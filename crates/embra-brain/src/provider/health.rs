//! Provider endpoint health — the periodic reachability + model-presence
//! probe of the ACTIVE LLM provider (Sprint 6 close-out: S5-D4 endpoint
//! health, S5-D7 model validation after config load).
//!
//! The proactive health loop runs one probe per tick (5 min; the first
//! ~30 s after boot doubles as the config-load model check) and again on
//! demand after `/provider` switches and setup flows ([`request_probe`]).
//! The latest result is process-wide ([`latest`]) — read by `/status`,
//! the `system_status` tool and the `GetSystemStatus` RPC — the same
//! idea as the embedding provider's `OnceCell`, but mutable. The loop
//! decides what to tell the operator through the pure
//! [`transition_events`]: notifications fire on state CHANGES only, and
//! never for an unconfigured provider (pre-wizard boot).
//!
//! Model and key resolution is the caller's job ([`ProbeTarget`] is
//! built by `grpc_service::provider_probe_target` with the turn path's
//! own resolvers), so the probe checks exactly what a turn would use.
//!
//! On Ollama the probe also reads the context window the model is loaded
//! with (`/api/ps`, [`ContextWindow`]): a GGUF build trims a request that
//! does not fit and reports no error, so a window below
//! [`CONTEXT_FLOOR_TOKENS`] is announced once, and its fix once.

use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use super::anthropic::{API_VERSION as ANTHROPIC_API_VERSION, MODELS_URL as ANTHROPIC_MODELS_URL};
use super::gemini::API_BASE as GEMINI_API_BASE;
use super::openai_compat::{OpenAICompatProvider, OpenAiCompatPreset};
use super::{ProviderError, ProviderKind};
use crate::proactive::{Notification, Priority};

/// Bound on one probe request (mirrors the providers' key-validation
/// clients and `probe_models`).
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The smallest context window, in tokens, the docs ask for on a server
/// that trims (`docs/LOCAL-MODELS/RECOMMENDED-LOCAL-MODELS.md`; Ollama's own figure
/// for agents). A GGUF model Ollama loads with less is announced.
pub const CONTEXT_FLOOR_TOKENS: u64 = 64_000;

/// What to probe — resolved by the caller from `SystemConfig` exactly the
/// way a turn resolves it (env model overrides, per-provider key with the
/// boot-key fallback, the preset's bearer).
#[derive(Debug, Clone)]
pub struct ProbeTarget {
    pub kind: ProviderKind,
    /// Resolved model id.
    pub model: String,
    /// Anthropic/Gemini API key, or the OpenAI-compat bearer (may be empty).
    pub key: String,
    /// OpenAI-compat base URL; unused for the cloud providers.
    pub endpoint: String,
}

/// One probe result. Serializes into the `provider` block of
/// `system_status` / `/status`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProviderProbe {
    pub kind: String,
    pub model: String,
    /// Host for the cloud providers, base URL for the local presets.
    pub endpoint: String,
    /// False when no request was made (no provider configured yet).
    pub configured: bool,
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_present: Option<bool>,
    /// `valid` | `invalid` | `forbidden` once the endpoint answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    pub checked_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The window the model is loaded with — Ollama only, read from
    /// `/api/ps` while the model is loaded and kept while it is not
    /// ([`carry_context_window`]). Absent until first seen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<ContextWindow>,
}

/// What the model server says about the model's context window.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ContextWindow {
    /// Ollama trims a request longer than `tokens` to fit, without an
    /// error.
    Window {
        tokens: u64,
        below_floor: bool,
        seen_at: DateTime<Utc>,
    },
    /// An MLX build: the window is the model's own maximum, and Ollama
    /// never trims it.
    ModelMaximum { seen_at: DateTime<Utc> },
    /// The server does not report it: no `/api/ps`, or an Ollama that
    /// predates the field.
    Unavailable { reason: String },
}

impl ContextWindow {
    /// A window below [`CONTEXT_FLOOR_TOKENS`] that Ollama trims to.
    fn is_short(&self) -> bool {
        matches!(self, ContextWindow::Window { below_floor: true, .. })
    }

    fn summary(&self) -> String {
        match self {
            ContextWindow::Window {
                tokens,
                below_floor: true,
                ..
            } => format!(
                "context window {tokens} tokens, under {CONTEXT_FLOOR_TOKENS}: Ollama trims without an error"
            ),
            ContextWindow::Window { tokens, .. } => format!("context window {tokens} tokens"),
            ContextWindow::ModelMaximum { .. } => "context window: the model's maximum".to_string(),
            ContextWindow::Unavailable { reason } => format!("context window not reported ({reason})"),
        }
    }
}

impl ProviderProbe {
    fn unconfigured() -> Self {
        Self {
            kind: String::new(),
            model: String::new(),
            endpoint: String::new(),
            configured: false,
            reachable: false,
            model_present: None,
            key_state: None,
            latency_ms: None,
            checked_at: Utc::now(),
            error: Some("not configured".into()),
            context_window: None,
        }
    }

    fn base(target: &ProbeTarget, endpoint: String) -> Self {
        Self {
            kind: target.kind.as_str().to_string(),
            model: target.model.clone(),
            endpoint,
            configured: true,
            reachable: false,
            model_present: None,
            key_state: None,
            latency_ms: None,
            checked_at: Utc::now(),
            error: None,
            context_window: None,
        }
    }

    /// `up` / `down` / `unknown` — the value the RPC `services` map and
    /// the web console's pill carry.
    pub fn state(&self) -> &'static str {
        if !self.configured {
            "unknown"
        } else if self.reachable {
            "up"
        } else {
            "down"
        }
    }

    /// Seconds since the probe ran.
    pub fn age_secs(&self) -> u64 {
        (Utc::now() - self.checked_at).num_seconds().max(0) as u64
    }

    /// True when nothing about the result needs the operator's attention.
    pub fn healthy(&self) -> bool {
        self.configured
            && self.reachable
            && self.model_present != Some(false)
            && self.key_state.as_deref().unwrap_or("valid") == "valid"
    }

    /// One-line summary for the RPC detail entry / pill tooltip.
    pub fn summary_line(&self) -> String {
        if !self.configured {
            return "no LLM provider configured yet".to_string();
        }
        let mut parts = vec![self.kind.clone(), self.model.clone(), self.endpoint.clone()];
        parts.push(
            match (self.reachable, self.model_present) {
                (false, _) => "unreachable",
                (true, Some(true)) => "model present",
                (true, Some(false)) => "model NOT found",
                (true, None) => "reachable",
            }
            .to_string(),
        );
        if let Some(w) = &self.context_window {
            parts.push(w.summary());
        }
        if let Some(k) = self.key_state.as_deref().filter(|k| *k != "valid") {
            parts.push(format!("key {k}"));
        }
        if let Some(ms) = self.latency_ms {
            parts.push(format!("{ms} ms"));
        }
        parts.push(format!("checked {} s ago", self.age_secs()));
        if let Some(e) = &self.error {
            parts.push(e.clone());
        }
        parts.join(" · ")
    }
}

static LATEST: RwLock<Option<ProviderProbe>> = RwLock::new(None);
static PROBE_REQUESTED: OnceLock<Notify> = OnceLock::new();

fn probe_notify() -> &'static Notify {
    PROBE_REQUESTED.get_or_init(Notify::new)
}

/// The most recent probe, if the loop has run one.
pub fn latest() -> Option<ProviderProbe> {
    LATEST.read().ok().and_then(|g| g.clone())
}

/// Publish a probe result for `latest()` readers.
pub fn record(probe: ProviderProbe) {
    if let Ok(mut g) = LATEST.write() {
        *g = Some(probe);
    }
}

/// Ask the health loop to re-probe now (provider switch, setup completed).
/// A request that arrives mid-probe is kept as a permit, so the loop runs
/// once more right after.
pub fn request_probe() {
    probe_notify().notify_one();
}

/// Resolves when a re-probe was requested — the loop `select!`s this
/// against its interval sleep.
pub async fn probe_requested() {
    probe_notify().notified().await;
}

/// Run one probe. `None` = nothing configured yet (recorded as such,
/// never notified).
pub async fn probe(target: Option<&ProbeTarget>) -> ProviderProbe {
    let Some(t) = target else {
        return ProviderProbe::unconfigured();
    };
    match t.kind {
        ProviderKind::Anthropic => {
            let url = format!("{ANTHROPIC_MODELS_URL}/{}", t.model);
            probe_model_url(t, &url, "api.anthropic.com").await
        }
        ProviderKind::Gemini => {
            let url = format!("{GEMINI_API_BASE}/models/{}", t.model);
            probe_model_url(t, &url, "generativelanguage.googleapis.com").await
        }
        ProviderKind::Ollama | ProviderKind::LmStudio => probe_openai_compat(t).await,
    }
}

/// Per-model GET against a cloud provider's models endpoint. `url` is a
/// parameter (not derived) so tests can point it at a mock server.
async fn probe_model_url(t: &ProbeTarget, url: &str, endpoint_label: &str) -> ProviderProbe {
    let mut out = ProviderProbe::base(t, endpoint_label.to_string());
    let client = match crate::net::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            out.error = Some(format!("http client: {e}"));
            return out;
        }
    };
    let mut req = client.get(url);
    req = match t.kind {
        ProviderKind::Anthropic => req
            .header("x-api-key", &t.key)
            .header("anthropic-version", ANTHROPIC_API_VERSION),
        ProviderKind::Gemini => req.header("x-goog-api-key", &t.key),
        ProviderKind::Ollama | ProviderKind::LmStudio => req,
    };
    let started = Instant::now();
    match req.send().await {
        Ok(resp) => {
            out.reachable = true;
            out.latency_ms = Some(started.elapsed().as_millis() as u64);
            classify_model_status(&mut out, resp.status().as_u16());
        }
        Err(e) => out.error = Some(network_error_text(&e)),
    }
    out
}

/// HTTP status of a per-model GET → model presence + key state. A 404
/// means the key was accepted and the model id is wrong — exactly the
/// stale-model case S5-D7 is about.
fn classify_model_status(out: &mut ProviderProbe, status: u16) {
    match status {
        200..=299 => {
            out.model_present = Some(true);
            out.key_state = Some("valid".into());
        }
        404 => {
            out.model_present = Some(false);
            out.key_state = Some("valid".into());
            out.error = Some(format!("model '{}' not found (404)", out.model));
        }
        401 => {
            out.key_state = Some("invalid".into());
            out.error = Some("API key rejected (401)".into());
        }
        403 => {
            out.key_state = Some("forbidden".into());
            out.error = Some("API key not authorized (403)".into());
        }
        s => out.error = Some(format!("HTTP {s}")),
    }
}

fn network_error_text(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".to_string()
    } else if e.is_connect() {
        "connection refused".to_string()
    } else {
        e.to_string()
    }
}

/// Ollama / LM Studio: `GET {base}/v1/models` through the wizard's own
/// `probe_models`, then an exact-id membership check (the wizard stores
/// the id the server listed, so exact is right).
async fn probe_openai_compat(t: &ProbeTarget) -> ProviderProbe {
    let preset = match t.kind {
        ProviderKind::Ollama => OpenAiCompatPreset::Ollama,
        _ => OpenAiCompatPreset::LmStudio,
    };
    let mut out = ProviderProbe::base(t, t.endpoint.clone());
    let started = Instant::now();
    match OpenAICompatProvider::probe_models(preset, &t.endpoint, Some(&t.key)).await {
        Ok(models) => {
            out.reachable = true;
            out.latency_ms = Some(started.elapsed().as_millis() as u64);
            out.key_state = Some("valid".into());
            let present = models.iter().any(|m| m == &t.model);
            out.model_present = Some(present);
            if !present {
                out.error = Some(format!(
                    "model '{}' is not in the server's list ({} models)",
                    t.model,
                    models.len()
                ));
            } else if t.kind == ProviderKind::Ollama && !is_cloud_model(&t.model) {
                out.context_window = read_context_window(t).await;
            }
        }
        Err(ProviderError::Http { status, .. }) => {
            out.reachable = true;
            out.latency_ms = Some(started.elapsed().as_millis() as u64);
            match status {
                401 => {
                    out.key_state = Some("invalid".into());
                    out.error = Some("bearer rejected (401)".into());
                }
                403 => {
                    out.key_state = Some("forbidden".into());
                    out.error = Some("bearer not authorized (403)".into());
                }
                s => out.error = Some(format!("HTTP {s} from /v1/models")),
            }
        }
        Err(e) => out.error = Some(e.to_string()),
    }
    out
}

/// Ollama's cloud models (`:cloud`, `:<tag>-cloud`) run on ollama.com and
/// are never loaded locally, so `/api/ps` has nothing to say about them.
fn is_cloud_model(id: &str) -> bool {
    id.rsplit_once(':')
        .is_some_and(|(_, tag)| tag == "cloud" || tag.ends_with("-cloud"))
}

/// `GET /api/ps`: the fields read, nothing more. `models` is required, so
/// a 200 that is not this answer (a proxy's page) reads as `Unavailable`,
/// never as "nothing loaded".
#[derive(Deserialize)]
struct PsAnswer {
    models: Vec<PsModel>,
}

#[derive(Deserialize)]
struct PsModel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    model: String,
    /// The loaded window; absent on an Ollama that predates the field.
    context_length: Option<u64>,
    /// `mlx` | `ggml` | `llamacpp`; absent on an older Ollama.
    runner: Option<String>,
    details: Option<PsDetails>,
}

#[derive(Deserialize)]
struct PsDetails {
    /// `gguf`, or `safetensors` for an MLX build.
    format: Option<String>,
}

/// The window Ollama loaded the configured model with: the `CONTEXT`
/// column of `ollama ps`. Built like `probe_models`: a fresh client under
/// `PROBE_TIMEOUT`, the bearer only when one is set.
///
/// `None` when the model is not loaded, and when the server could not
/// answer now (a network error, a 5xx): a proxy's 502 while Ollama
/// restarts is not "not reported". `Unavailable` when the server does not
/// serve the list: a 4xx, or a body that is not one.
async fn read_context_window(t: &ProbeTarget) -> Option<ContextWindow> {
    let url = format!("{}/api/ps", t.endpoint.trim_end_matches('/'));
    let client = crate::net::builder().timeout(PROBE_TIMEOUT).build().ok()?;
    let mut req = client.get(&url);
    if !t.key.is_empty() {
        req = req.bearer_auth(&t.key);
    }
    let resp = req.send().await.ok()?;
    let status = resp.status();
    if status.is_client_error() {
        return Some(ContextWindow::Unavailable {
            reason: format!("HTTP {} from /api/ps", status.as_u16()),
        });
    }
    if !status.is_success() {
        return None;
    }
    let body = resp.bytes().await.ok()?;
    let Ok(answer) = serde_json::from_slice::<PsAnswer>(&body) else {
        return Some(ContextWindow::Unavailable {
            reason: "the answer from /api/ps is not a list of models".into(),
        });
    };
    answer
        .models
        .iter()
        .find(|m| m.name == t.model || m.model == t.model)
        .map(|m| window_of(m, Utc::now()))
}

/// One `/api/ps` entry → its reading. An MLX build's figure is only the
/// number `ollama ps` shows: the window is the model's maximum, and Ollama
/// never trims it. `runner` decides; on an Ollama too old to send it, the
/// weight format does (`safetensors` is MLX).
fn window_of(m: &PsModel, seen_at: DateTime<Utc>) -> ContextWindow {
    let format = m.details.as_ref().and_then(|d| d.format.as_deref());
    let mlx = match m.runner.as_deref() {
        Some(runner) => runner == "mlx",
        None => format == Some("safetensors"),
    };
    if mlx {
        return ContextWindow::ModelMaximum { seen_at };
    }
    match m.context_length {
        Some(tokens) if tokens > 0 => ContextWindow::Window {
            tokens,
            below_floor: tokens < CONTEXT_FLOOR_TOKENS,
            seen_at,
        },
        _ => ContextWindow::Unavailable {
            reason: "this Ollama does not report the context window".into(),
        },
    }
}

/// About how many tokens the tool list takes on the OpenAI-compatible
/// wire: its JSON bytes over four. Computed once. A translation that
/// fails sends no tools (`OpenAICompatProvider::build_tool_manifest`), so
/// it counts as 0.
fn tool_list_tokens() -> u64 {
    static TOKENS: OnceLock<u64> = OnceLock::new();
    *TOKENS.get_or_init(|| {
        let descriptors: Vec<&'static crate::tools::registry::ToolDescriptor> =
            crate::tools::registry::all_descriptors().collect();
        super::openai_compat::tool_schema::translate(&descriptors)
            .map(|manifest| manifest.to_string().len() as u64 / 4)
            .unwrap_or(0)
    })
}

/// The announcement of a window below [`CONTEXT_FLOOR_TOKENS`]. Critical
/// when it cannot hold even the tool list: then every request is cut.
fn short_window_notice(model: &str, tokens: u64) -> Notification {
    let remedy = format!(
        "Set OLLAMA_CONTEXT_LENGTH to {CONTEXT_FLOOR_TOKENS} or more on the Ollama server and restart it."
    );
    if tokens < tool_list_tokens() {
        Notification::new(
            Priority::Critical,
            format!(
                "Ollama is running {model} with a context window of {tokens} tokens, less than \
                 embraOS's tool list alone. Ollama cuts every request to fit and reports no \
                 error, so the model works without its identity, most of its tools and its \
                 earlier turns. {remedy}"
            ),
        )
    } else {
        Notification::new(
            Priority::Normal,
            format!(
                "Ollama is running {model} with a context window of {tokens} tokens, below the \
                 {CONTEXT_FLOOR_TOKENS} embraOS needs. A request that does not fit is cut to fit \
                 with no error, its earlier turns first. {remedy}"
            ),
        )
    }
}

/// The two results are about the same provider, model and endpoint: the
/// comparison [`transition_events`] and [`carry_context_window`] make.
fn same_target(prev: &ProviderProbe, next: &ProviderProbe) -> bool {
    prev.configured && prev.kind == next.kind && prev.model == next.model && prev.endpoint == next.endpoint
}

/// Keep the last context-window reading while the model is unloaded.
/// Ollama lists loaded models only, and it unloads an idle one after
/// `OLLAMA_KEEP_ALIVE` (5 minutes by default); without the carry, every
/// reload would read as a first sighting and repeat the warning. Only for
/// the same target, and never over a fresh reading.
pub fn carry_context_window(prev: Option<&ProviderProbe>, next: &mut ProviderProbe) {
    if next.context_window.is_some() {
        return;
    }
    if let Some(p) = prev
        && same_target(p, next)
    {
        next.context_window = p.context_window.clone();
    }
}

/// Operator notifications for a probe result, given the previous one.
/// Pure — fires only on CHANGES of the same target, never for an
/// unconfigured provider, so a headless instance never floods the
/// bounded channel and a steady failure is announced once.
pub fn transition_events(prev: Option<&ProviderProbe>, next: &ProviderProbe) -> Vec<Notification> {
    let mut out = Vec::new();
    if !next.configured {
        return out;
    }
    let prev = prev.filter(|p| same_target(p, next));
    let prev_reachable = prev.map(|p| p.reachable);
    let prev_model_present = prev.and_then(|p| p.model_present);
    let prev_key_state = prev.and_then(|p| p.key_state.as_deref());

    if !next.reachable {
        if prev_reachable != Some(false) {
            let why = next
                .error
                .as_deref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default();
            out.push(Notification::new(
                Priority::Critical,
                format!(
                    "LLM provider {} ({}) is unreachable{why} — turns will fail until it is \
                     back. Check the server, or switch with /provider.",
                    next.kind, next.endpoint
                ),
            ));
        }
        return out; // nothing else is knowable while unreachable
    }
    if prev_reachable == Some(false) {
        out.push(Notification::new(
            Priority::Normal,
            format!("LLM provider {} ({}) is reachable again.", next.kind, next.endpoint),
        ));
    }
    if next.model_present == Some(false) && prev_model_present != Some(false) {
        // Every provider's model is chosen with `/model`.
        let remedy = "run /model to pick an available model";
        out.push(Notification::new(
            Priority::Critical,
            format!(
                "Configured model '{}' was not found on {} ({}) — {remedy}.",
                next.model, next.kind, next.endpoint
            ),
        ));
    }
    if let Some(k) = next.key_state.as_deref().filter(|k| *k != "valid")
        && prev_key_state != Some(k)
    {
        out.push(Notification::new(
            Priority::Critical,
            format!(
                "{} rejected the API key ({k}) — run /provider --setup {}.",
                next.kind, next.kind
            ),
        ));
    }
    // A short window once, and its fix once. An MLX build and an
    // unreported window never notify.
    let prev_short = prev
        .and_then(|p| p.context_window.as_ref())
        .is_some_and(ContextWindow::is_short);
    match &next.context_window {
        Some(ContextWindow::Window {
            tokens,
            below_floor: true,
            ..
        }) if !prev_short => out.push(short_window_notice(&next.model, *tokens)),
        Some(ContextWindow::Window {
            tokens,
            below_floor: false,
            ..
        }) if prev_short => out.push(Notification::new(
            Priority::Normal,
            format!(
                "Ollama now runs {} with a context window of {tokens} tokens.",
                next.model
            ),
        )),
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn target(kind: ProviderKind, model: &str, endpoint: &str) -> ProbeTarget {
        ProbeTarget {
            kind,
            model: model.into(),
            key: "sk-test".into(),
            endpoint: endpoint.into(),
        }
    }

    fn result(reachable: bool, model_present: Option<bool>, key_state: Option<&str>) -> ProviderProbe {
        let mut p = ProviderProbe::base(&target(ProviderKind::LmStudio, "m", "http://x:1234"), "http://x:1234".into());
        p.reachable = reachable;
        p.model_present = model_present;
        p.key_state = key_state.map(str::to_string);
        p
    }

    #[test]
    fn unconfigured_probe_never_notifies_and_reads_unknown() {
        let p = ProviderProbe::unconfigured();
        assert_eq!(p.state(), "unknown");
        assert!(transition_events(None, &p).is_empty());
        assert!(transition_events(Some(&result(true, Some(true), Some("valid"))), &p).is_empty());
        assert_eq!(p.summary_line(), "no LLM provider configured yet");
    }

    #[test]
    fn transition_table_fires_on_changes_only() {
        let ok = result(true, Some(true), Some("valid"));
        let down = result(false, None, None);
        let absent = result(true, Some(false), Some("valid"));
        let bad_key = result(true, None, Some("invalid"));

        // Boot: healthy → silent; unreachable → one Critical.
        assert!(transition_events(None, &ok).is_empty());
        let n = transition_events(None, &down);
        assert_eq!(n.len(), 1);
        assert!(matches!(n[0].priority, Priority::Critical));
        assert!(n[0].message.contains("unreachable"));
        // Still down → silent. Recovered → one Normal.
        assert!(transition_events(Some(&down), &down).is_empty());
        let n = transition_events(Some(&down), &ok);
        assert_eq!(n.len(), 1);
        assert!(matches!(n[0].priority, Priority::Normal));
        // Model missing: once, then silent while unchanged.
        let n = transition_events(Some(&ok), &absent);
        assert_eq!(n.len(), 1);
        assert!(n[0].message.contains("run /model to pick"));
        assert!(transition_events(Some(&absent), &absent).is_empty());
        // Key rejected: once.
        let n = transition_events(Some(&ok), &bad_key);
        assert_eq!(n.len(), 1);
        assert!(n[0].message.contains("rejected the API key (invalid)"));
        assert!(transition_events(Some(&bad_key), &bad_key).is_empty());
        // A different target resets the comparison: a first-seen failure
        // on the new provider is announced even if the old one was down.
        let mut other_down = down.clone();
        other_down.kind = "ollama".into();
        assert_eq!(transition_events(Some(&down), &other_down).len(), 1);
        assert_eq!(ok.state(), "up");
        assert_eq!(down.state(), "down");
        assert!(ok.healthy() && !down.healthy() && !absent.healthy() && !bad_key.healthy());
    }

    /// A model the API does not serve (a selectable id the key has no
    /// access to, or an env/config passthrough id) must point the operator
    /// at the command that changes it. For Anthropic that is `/model`.
    #[test]
    fn missing_model_hint_names_the_command_that_fixes_it() {
        let mut absent = ProviderProbe::base(
            &target(ProviderKind::Anthropic, "claude-opus-5-5", ""),
            "api.anthropic.com".into(),
        );
        absent.reachable = true;
        absent.model_present = Some(false);
        absent.key_state = Some("valid".into());
        let n = transition_events(None, &absent);
        assert_eq!(n.len(), 1);
        assert!(n[0].message.contains("'claude-opus-5-5' was not found"), "{}", n[0].message);
        assert!(n[0].message.contains("run /model to pick"), "{}", n[0].message);
        assert!(!n[0].message.contains("--setup"), "{}", n[0].message);

        // Gemini, Ollama and LM Studio pick their model with `/model` too.
        for kind in [ProviderKind::Gemini, ProviderKind::Ollama, ProviderKind::LmStudio] {
            let mut absent = ProviderProbe::base(&target(kind, "m", ""), "host".into());
            absent.reachable = true;
            absent.model_present = Some(false);
            absent.key_state = Some("valid".into());
            let n = transition_events(None, &absent);
            assert_eq!(n.len(), 1, "{kind:?}");
            assert!(n[0].message.contains("run /model to pick"), "{}", n[0].message);
            assert!(!n[0].message.contains("--setup"), "{}", n[0].message);
        }
    }

    #[test]
    fn classify_model_status_maps_presence_and_key_state() {
        let mut p = result(true, None, None);
        classify_model_status(&mut p, 200);
        assert_eq!((p.model_present, p.key_state.as_deref()), (Some(true), Some("valid")));
        let mut p = result(true, None, None);
        classify_model_status(&mut p, 404);
        assert_eq!((p.model_present, p.key_state.as_deref()), (Some(false), Some("valid")));
        assert!(p.error.as_deref().unwrap().contains("not found"));
        let mut p = result(true, None, None);
        classify_model_status(&mut p, 401);
        assert_eq!((p.model_present, p.key_state.as_deref()), (None, Some("invalid")));
        let mut p = result(true, None, None);
        classify_model_status(&mut p, 403);
        assert_eq!(p.key_state.as_deref(), Some("forbidden"));
        let mut p = result(true, None, None);
        classify_model_status(&mut p, 529);
        assert_eq!(p.error.as_deref(), Some("HTTP 529"));
        assert!(p.model_present.is_none() && p.key_state.is_none());
    }

    #[tokio::test]
    async fn cloud_probe_reports_present_absent_and_unreachable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models/claude-opus-5"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models/claude-nope"))
            .respond_with(ResponseTemplate::new(404).set_body_string("{}"))
            .mount(&server)
            .await;
        let t = target(ProviderKind::Anthropic, "claude-opus-5", "");
        let p = probe_model_url(&t, &format!("{}/v1/models/claude-opus-5", server.uri()), "mock").await;
        assert!(p.reachable && p.model_present == Some(true) && p.latency_ms.is_some());
        assert_eq!(p.kind, "anthropic");
        let req = &server.received_requests().await.unwrap()[0];
        assert_eq!(req.headers.get("x-api-key").unwrap(), "sk-test");
        assert_eq!(req.headers.get("anthropic-version").unwrap(), ANTHROPIC_API_VERSION);

        let t = target(ProviderKind::Anthropic, "claude-nope", "");
        let p = probe_model_url(&t, &format!("{}/v1/models/claude-nope", server.uri()), "mock").await;
        assert!(p.reachable && p.model_present == Some(false));

        // Nothing listens on this port → unreachable, nothing else claimed.
        let p = probe_model_url(&t, "http://127.0.0.1:1/v1/models/x", "mock").await;
        assert!(!p.reachable && p.model_present.is_none() && p.key_state.is_none());
        assert!(p.error.is_some());
        assert_eq!(p.state(), "down");
    }

    #[tokio::test]
    async fn openai_compat_probe_checks_exact_model_membership() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "qwen/qwen3.8-27b"}, {"id": "openai/gpt-oss-20b"}]
            })))
            .mount(&server)
            .await;
        let p = probe(Some(&target(ProviderKind::LmStudio, "qwen/qwen3.8-27b", &server.uri()))).await;
        assert!(p.reachable && p.model_present == Some(true));
        assert_eq!(p.kind, "lm_studio");
        let p = probe(Some(&target(ProviderKind::LmStudio, "qwen/qwen3.8", &server.uri()))).await;
        assert!(p.reachable && p.model_present == Some(false), "prefix must not count as present");
        assert!(p.error.as_deref().unwrap().contains("2 models"));
        let p = probe(Some(&target(ProviderKind::Ollama, "x", "http://127.0.0.1:1"))).await;
        assert!(!p.reachable);
        assert!(probe(None).await.state() == "unknown");
    }

    #[tokio::test]
    async fn record_latest_and_request_probe_round_trip() {
        let p = result(true, Some(true), Some("valid"));
        record(p.clone());
        assert_eq!(latest().map(|l| l.model), Some("m".into()));
        request_probe();
        // The permit is kept, so this resolves without a waiter having
        // been parked first — the mid-probe request case.
        tokio::time::timeout(Duration::from_secs(1), probe_requested())
            .await
            .expect("request_probe must wake the loop");
    }

    /// A healthy Ollama probe of `model` holding `window`.
    fn ollama(model: &str, window: Option<ContextWindow>) -> ProviderProbe {
        let endpoint = "http://ollama:11434";
        let mut p = ProviderProbe::base(&target(ProviderKind::Ollama, model, endpoint), endpoint.into());
        p.reachable = true;
        p.model_present = Some(true);
        p.key_state = Some("valid".into());
        p.context_window = window;
        p
    }

    fn window(tokens: u64) -> Option<ContextWindow> {
        Some(ContextWindow::Window {
            tokens,
            below_floor: tokens < CONTEXT_FLOOR_TOKENS,
            seen_at: Utc::now(),
        })
    }

    async fn ps_requests(server: &MockServer) -> Vec<wiremock::Request> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == "/api/ps")
            .collect()
    }

    /// An Ollama that lists `ids` and answers `/api/ps` with `ps`.
    async fn ollama_server(ids: &[&str], ps: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        let data: Vec<_> = ids.iter().map(|id| serde_json::json!({ "id": id })).collect();
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "object": "list", "data": data })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/api/ps")).respond_with(ps).mount(&server).await;
        server
    }

    #[tokio::test]
    async fn ollama_probe_reads_the_window_of_the_loaded_model() {
        let ps = ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [
                {"name": "gemma4:12b", "model": "gemma4:12b", "context_length": 4096,
                 "runner": "llamacpp", "details": {"format": "gguf"}},
                {"name": "gemma4:12b-mlx", "model": "gemma4:12b-mlx", "context_length": 4096,
                 "runner": "mlx", "details": {"format": "safetensors"}},
                {"name": "qwen3.8:27b", "model": "qwen3.8:27b", "context_length": 65536,
                 "runner": "llamacpp", "details": {"format": "gguf"}}
            ]
        }));
        let ids = ["gemma4:12b", "gemma4:12b-mlx", "qwen3.8:27b", "qwen3.6:27b", "glm-5.3:cloud"];
        let server = ollama_server(&ids, ps).await;
        let uri = server.uri();

        let p = probe(Some(&target(ProviderKind::Ollama, "gemma4:12b", &uri))).await;
        assert!(
            matches!(
                p.context_window,
                Some(ContextWindow::Window { tokens: 4096, below_floor: true, .. })
            ),
            "{p:?}"
        );
        assert!(p.healthy() && p.error.is_none(), "a short window is not an endpoint failure: {p:?}");
        let asked = ps_requests(&server).await;
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].headers.get("authorization").unwrap(), "Bearer sk-test");

        // MLX: the figure is only what `ollama ps` shows.
        let p = probe(Some(&target(ProviderKind::Ollama, "gemma4:12b-mlx", &uri))).await;
        assert!(matches!(p.context_window, Some(ContextWindow::ModelMaximum { .. })), "{p:?}");
        let p = probe(Some(&target(ProviderKind::Ollama, "qwen3.8:27b", &uri))).await;
        assert!(
            matches!(
                p.context_window,
                Some(ContextWindow::Window { tokens: 65536, below_floor: false, .. })
            ),
            "{p:?}"
        );
        // Listed by /v1/models but not loaded: nothing to read yet.
        let p = probe(Some(&target(ProviderKind::Ollama, "qwen3.6:27b", &uri))).await;
        assert!(p.model_present == Some(true) && p.context_window.is_none(), "{p:?}");

        // A cloud model and an LM Studio target never ask /api/ps.
        let before = ps_requests(&server).await.len();
        let p = probe(Some(&target(ProviderKind::Ollama, "glm-5.3:cloud", &uri))).await;
        assert!(p.model_present == Some(true) && p.context_window.is_none(), "{p:?}");
        let p = probe(Some(&target(ProviderKind::LmStudio, "gemma4:12b", &uri))).await;
        assert!(p.model_present == Some(true) && p.context_window.is_none(), "{p:?}");
        assert_eq!(ps_requests(&server).await.len(), before);
    }

    #[tokio::test]
    async fn a_server_that_does_not_report_the_window_reads_as_unavailable() {
        let probe_of = |server: &MockServer| {
            let t = target(ProviderKind::Ollama, "m:7b", &server.uri());
            async move { probe(Some(&t)).await }
        };

        // No /api/ps (a server behind the preset that is not Ollama).
        let server = ollama_server(&["m:7b"], ResponseTemplate::new(404)).await;
        let p = probe_of(&server).await;
        match &p.context_window {
            Some(ContextWindow::Unavailable { reason }) => assert!(reason.contains("404"), "{reason}"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
        assert!(p.healthy() && p.error.is_none(), "{p:?}");

        // An Ollama too old to send the window.
        let ps = ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({"models": [{"name": "m:7b", "model": "m:7b"}]}));
        let p = probe_of(&ollama_server(&["m:7b"], ps).await).await;
        assert!(matches!(p.context_window, Some(ContextWindow::Unavailable { .. })), "{p:?}");

        // Too old to send the runner: the weight format decides.
        let ps = ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": [
            {"name": "m:7b", "model": "m:7b", "context_length": 4096, "details": {"format": "safetensors"}}
        ]}));
        let p = probe_of(&ollama_server(&["m:7b"], ps).await).await;
        assert!(matches!(p.context_window, Some(ContextWindow::ModelMaximum { .. })), "{p:?}");
        let ps = ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": [
            {"name": "m:7b", "model": "m:7b", "context_length": 8192, "details": {"format": "gguf"}}
        ]}));
        let p = probe_of(&ollama_server(&["m:7b"], ps).await).await;
        assert!(
            matches!(p.context_window, Some(ContextWindow::Window { tokens: 8192, .. })),
            "{p:?}"
        );

        // A 200 that is not the list is not "nothing loaded".
        let ps = ResponseTemplate::new(200).set_body_json(serde_json::json!({}));
        let p = probe_of(&ollama_server(&["m:7b"], ps).await).await;
        assert!(matches!(p.context_window, Some(ContextWindow::Unavailable { .. })), "{p:?}");

        // A server error is transient: no reading, so a held one is kept.
        let p = probe_of(&ollama_server(&["m:7b"], ResponseTemplate::new(500)).await).await;
        assert!(p.healthy() && p.context_window.is_none(), "{p:?}");
    }

    #[test]
    fn the_window_reading_is_kept_while_the_model_is_unloaded() {
        let seen = ollama("gemma4:12b", window(4096));

        let mut unloaded = ollama("gemma4:12b", None);
        carry_context_window(Some(&seen), &mut unloaded);
        assert_eq!(unloaded.context_window, seen.context_window);
        assert!(transition_events(Some(&seen), &unloaded).is_empty());

        // A fresh reading wins.
        let mut fresh = ollama("gemma4:12b", window(65536));
        carry_context_window(Some(&seen), &mut fresh);
        assert!(matches!(
            fresh.context_window,
            Some(ContextWindow::Window { tokens: 65536, .. })
        ));

        // Another model, or no configured provider before, carries nothing.
        let mut other = ollama("qwen3.8:27b", None);
        carry_context_window(Some(&seen), &mut other);
        assert!(other.context_window.is_none());
        let mut first = ollama("gemma4:12b", None);
        carry_context_window(Some(&ProviderProbe::unconfigured()), &mut first);
        assert!(first.context_window.is_none());
        carry_context_window(None, &mut first);
        assert!(first.context_window.is_none());
    }

    #[test]
    fn a_short_window_is_announced_once_and_its_fix_once() {
        let tools = tool_list_tokens();
        assert!(tools > 4096 && tools <= 60_000, "tool list ≈ {tools} tokens");
        let m = "gemma4:12b";

        // First sight of a window the tool list does not fit: Critical.
        for prev in [None, Some(ollama(m, None))] {
            let n = transition_events(prev.as_ref(), &ollama(m, window(4096)));
            assert_eq!(n.len(), 1);
            assert!(matches!(n[0].priority, Priority::Critical));
            for needle in ["gemma4:12b", "4096 tokens", "OLLAMA_CONTEXT_LENGTH", "64000"] {
                assert!(n[0].message.contains(needle), "{needle}: {}", n[0].message);
            }
        }
        // Short, but it holds the tools: Normal.
        let n = transition_events(None, &ollama(m, window(60_000)));
        assert_eq!(n.len(), 1);
        assert!(matches!(n[0].priority, Priority::Normal));
        assert!(n[0].message.contains("60000 tokens"), "{}", n[0].message);

        // Unchanged: silent. Fixed: one Normal.
        assert!(transition_events(Some(&ollama(m, window(4096))), &ollama(m, window(4096))).is_empty());
        let n = transition_events(Some(&ollama(m, window(4096))), &ollama(m, window(65536)));
        assert_eq!(n.len(), 1);
        assert!(matches!(n[0].priority, Priority::Normal));
        assert!(
            n[0].message.contains("now runs gemma4:12b with a context window of 65536 tokens"),
            "{}",
            n[0].message
        );

        // A window at the floor, an MLX build and an unreported window are silent.
        let mlx = Some(ContextWindow::ModelMaximum { seen_at: Utc::now() });
        let unreported = Some(ContextWindow::Unavailable { reason: "HTTP 404 from /api/ps".into() });
        for w in [window(65536), mlx.clone(), unreported.clone()] {
            assert!(transition_events(None, &ollama(m, w.clone())).is_empty(), "{w:?}");
            assert!(transition_events(Some(&ollama(m, None)), &ollama(m, w.clone())).is_empty(), "{w:?}");
        }
        // Another model resets the comparison.
        assert_eq!(
            transition_events(Some(&ollama("qwen3.8:27b", window(4096))), &ollama(m, window(4096))).len(),
            1
        );

        // The summary line says it; the endpoint state does not change.
        let short = ollama(m, window(4096));
        assert!(
            short
                .summary_line()
                .contains("context window 4096 tokens, under 64000: Ollama trims without an error"),
            "{}",
            short.summary_line()
        );
        assert_eq!(short.state(), "up");
        assert!(short.healthy());
        let fine = ollama(m, window(65536)).summary_line();
        assert!(fine.contains("context window 65536 tokens") && !fine.contains("under"), "{fine}");
        assert!(ollama(m, mlx.clone()).summary_line().contains("context window: the model's maximum"));
        assert!(
            ollama(m, unreported)
                .summary_line()
                .contains("context window not reported (HTTP 404 from /api/ps)")
        );

        // `/status` and `system_status` carry it as `provider.context_window`.
        let status = |p: ProviderProbe| serde_json::to_value(crate::tools::ProviderStatus::from(p)).unwrap();
        let v = status(short);
        assert_eq!(v["context_window"]["state"], "window");
        assert_eq!(v["context_window"]["tokens"], 4096);
        assert_eq!(v["context_window"]["below_floor"], true);
        assert_eq!(status(ollama(m, mlx))["context_window"]["state"], "model_maximum");
        assert!(status(ollama(m, None)).get("context_window").is_none());
    }

    #[test]
    fn cloud_tags_are_recognized() {
        for cloud in ["glm-5.3:cloud", "gpt-oss:120b-cloud", "deepseek-v4-pro:cloud"] {
            assert!(is_cloud_model(cloud), "{cloud}");
        }
        for local in ["cloudy:7b", "gemma4:12b-mlx", "qwen3.8:27b", "cloud"] {
            assert!(!is_cloud_model(local), "{local}");
        }
    }
}
