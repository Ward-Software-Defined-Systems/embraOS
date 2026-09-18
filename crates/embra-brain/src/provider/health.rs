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

use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Serialize;
use tokio::sync::Notify;

use super::anthropic::{API_VERSION as ANTHROPIC_API_VERSION, MODELS_URL as ANTHROPIC_MODELS_URL};
use super::gemini::API_BASE as GEMINI_API_BASE;
use super::openai_compat::{OpenAICompatProvider, OpenAiCompatPreset};
use super::{ProviderError, ProviderKind};
use crate::proactive::{Notification, Priority};

/// Bound on one probe request (mirrors the providers' key-validation
/// clients and `probe_models`).
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

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
    let client = match Client::builder().timeout(PROBE_TIMEOUT).build() {
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

/// Operator notifications for a probe result, given the previous one.
/// Pure — fires only on CHANGES of the same target, never for an
/// unconfigured provider, so a headless instance never floods the
/// bounded channel and a steady failure is announced once.
pub fn transition_events(prev: Option<&ProviderProbe>, next: &ProviderProbe) -> Vec<Notification> {
    let mut out = Vec::new();
    if !next.configured {
        return out;
    }
    let prev = prev.filter(|p| {
        p.configured && p.kind == next.kind && p.model == next.model && p.endpoint == next.endpoint
    });
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
        out.push(Notification::new(
            Priority::Critical,
            format!(
                "Configured model '{}' was not found on {} ({}) — run /provider --setup {} \
                 to pick an available model.",
                next.model, next.kind, next.endpoint, next.kind
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
        assert!(n[0].message.contains("/provider --setup lm_studio"));
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
}
