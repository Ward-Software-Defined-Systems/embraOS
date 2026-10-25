use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tonic::Status;
use tracing::info;

use crate::db::WardsonDbClient;
use crate::provider::anthropic::AnthropicProvider;
use crate::provider::gemini::GeminiProvider;
use crate::provider::{LlmProvider, ProviderKind, ValidationResult};
use embra_common::proto::brain::*;

/// No `Default`, on purpose: a new field is written at every place a
/// config is constructed, where the compiler asks for it. A field that old
/// stored configs lack is optional and carries a serde default; that needs
/// no schema version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    pub name: String,
    pub api_key: String,
    pub timezone: String,
    pub deployment_mode: String,
    pub created_at: String,
    /// The embraOS build the config is running under
    /// (`CARGO_PKG_VERSION`, e.g. `0.19.0-phase1`). Refreshed to the running
    /// build on load and on save (F-7), so the stored doc never lags the
    /// binary. Not the build that first created the DATA: `created_at`
    /// carries the origin, and `system_status`/`GetSoulStatus` report the
    /// same live `env!` string.
    pub version: String,
    #[serde(default)]
    pub github_token: Option<String>,
    // Knowledge graph configuration (Sprint 2, schema v5)
    #[serde(default = "default_kg_temporal_window")]
    pub kg_temporal_window_secs: u64,
    #[serde(default = "default_kg_max_depth")]
    pub kg_max_traversal_depth: u32,
    #[serde(default = "default_kg_depth_ceiling")]
    pub kg_traversal_depth_ceiling: u32,
    #[serde(default = "default_kg_candidate_limit")]
    pub kg_edge_candidate_limit: u32,
    /// Per-hop edge fetch window for graph traversal (FIX-7, locked D3).
    /// Ranked `weight desc, created_at desc` so saturation prunes the
    /// weakest/oldest edges. 500 > the structural creation ceiling
    /// (~450 outgoing docs) at kg_edge_candidate_limit=50.
    #[serde(default = "default_kg_traversal_edge_limit")]
    pub kg_traversal_edge_limit: u32,
    /// BFS node budget for graph traversal (FIX-7, locked D3). Inert at
    /// current node counts; bounds dense-graph BFS cost below the depth
    /// ceiling. Budget hit → warn + `TraversalResult.truncated`.
    #[serde(default = "default_kg_traversal_node_budget")]
    pub kg_traversal_node_budget: u32,
    /// Active LLM provider — `"anthropic"` or `"gemini"` (Sprint 4
    /// schema v9). Missing on pre-v9 docs; defaults to anthropic for
    /// backward compatibility.
    #[serde(default = "default_api_provider")]
    pub api_provider: String,
    /// The Gemini chat model: the alias `/model` stores
    /// (`"gemini-3.1-pro"`, `"gemini-4-argon"`), which the brain's
    /// resolver maps to the API id. `None` means the default,
    /// `gemini-3.1-pro-preview`. Any other value is sent as typed, such
    /// as `gemini-3.1-pro-preview-customtools` (spec D8) when the
    /// standard model ignores custom tools in favor of bash invocations.
    /// The brain also honors the `EMBRA_GEMINI_MODEL` env var, which
    /// takes precedence over this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini_model: Option<String>,
    /// Active Anthropic model alias (`"opus-5.5"`, `"opus-5"`, `"opus-4.8"`,
    /// `"fable-5.1"`, `"fable-5"`; the legacy persisted `"opus-4.7"` keeps
    /// resolving). `None`
    /// (additive default) means the provider's
    /// `DEFAULT_MODEL` (`claude-opus-5`). The brain also honors the
    /// `EMBRA_ANTHROPIC_MODEL` env var, which takes precedence. Settable
    /// at runtime via `/model`; the request shape (adaptive thinking,
    /// tunable `effort`) is identical across supported models, so this
    /// only swaps the API `model` id. Serde-additive `Option` — no schema
    /// bump (same precedent as `gemini_model` / `max_tool_iterations` /
    /// `show_reasoning`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_model: Option<String>,
    /// Anthropic `output_config.effort` level (`"low"|"medium"|"high"|
    /// "xhigh"|"max"`). `None` (additive default) means `"max"`. The brain
    /// also honors the `EMBRA_ANTHROPIC_EFFORT` env var, which takes
    /// precedence. Settable at runtime via `/effort`; invalid values fall
    /// through to the default at the read site. Serde-additive `Option` —
    /// no schema bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_effort: Option<String>,
    /// Gemini reasoning posture, stored on the canonical embraOS effort
    /// ladder (`"low"|"medium"|"high"|"xhigh"|"max"`) and mapped to
    /// `thinkingLevel` at send time (`xhigh`/`max` clamp to `high`, the
    /// ceiling of both Gemini models). `None` = `"high"`, byte-identical to before the
    /// knob existed. Env `EMBRA_GEMINI_EFFORT` takes precedence. Settable
    /// via `/effort` while Gemini is active. Serde-additive — no schema
    /// bump; one more field on every `SystemConfig` literal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini_effort: Option<String>,
    /// Image-generation backend for the `image_generate` tool (media wave
    /// part 2). `None` (additive default) = Gemini when a Gemini key is
    /// resolvable, else unconfigured. Only `"gemini"` is valid today —
    /// deliberately a string + a small enum in `provider::image`, NOT an
    /// arm on `ProviderKind` (that enum drives LLM construction and
    /// session-compat checks). Set via `/image-provider gemini`.
    /// Serde-additive `Option` — no schema bump.
    /// Local embedding kill switch (KG-02). `None` = enabled — a boot with
    /// the model present should just work. Serde-additive: no schema bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_enabled: Option<bool>,

    /// Embedding model directory name under the model roots. `None` = the
    /// kind's default (`bge-small-en-v1.5`). Serde-additive: no schema bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_provider: Option<String>,
    /// Image-generation model id. `None` = the backend's default
    /// (`gemini-3-pro-image`). Allowlisted in `provider::image`; set via
    /// `/image-provider model <id>`. Serde-additive `Option`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_model: Option<String>,
    /// Per-host git tokens (host → PAT) for self-hosted git servers
    /// (GitLab/Gitea over HTTPS). Injected by the git tools as
    /// `url.https://oauth2:<token>@<host>/.insteadOf` rewrites — the
    /// github.com flow stays on `github_token` above. Set via
    /// `/git-token <host> <token>`. WardSONDB-only (no STATE file /
    /// env fallback: resolution happens at tool time when the DB is
    /// always up). `BTreeMap` for deterministic injection order.
    /// Serde-additive `Option` — no schema bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_tokens: Option<std::collections::BTreeMap<String, String>>,
    /// Per-host credentials the Guardian request guard adds to an outbound
    /// `http_request` whose host matches: host → header name → value. Set
    /// via `/guardian secret <host> <header> <value>`. The model never
    /// carries one in a tool's input and no tool returns this document.
    /// WardSONDB-only, like `git_tokens`. Serde-additive — no schema bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guardian_secrets:
        Option<std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>>,
    /// Private hosts the Guardian egress guard may reach: the operator's
    /// allowlist, a name (with its subdomains) or an IP literal. Loopback
    /// is refused whatever stands here. Set via `/guardian egress allow`.
    /// Serde-additive — no schema bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guardian_private_hosts: Option<Vec<String>>,
    /// Per-provider API keys (Sprint 4 schema v10, spec D2). The
    /// legacy `api_key` field above mirrors whichever of these is
    /// active so existing read paths keep working; new writes
    /// populate both. `/provider --setup <kind>` sets one of these
    /// without touching `api_provider`, letting an operator stash a
    /// key for later use without switching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini_api_key: Option<String>,
    /// Maximum tool-use iterations per user turn. Read fresh each turn
    /// at the loop driver's `loaded_config` site; falls back to
    /// `DEFAULT_MAX_TOOL_ITERATIONS` (100) when unset. Settable at
    /// runtime via `/iter-cap`. Clamped to 1..=1000 at the read site
    /// so a hand-edited bogus value can't break the loop driver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_iterations: Option<usize>,
    /// Whether to stream live reasoning / chain-of-thought to the
    /// expression panel. `None` (the additive default) and `Some(true)`
    /// both mean *show*; `Some(false)` suppresses panel reasoning and
    /// strips the request-body opt-ins (Anthropic `display: omitted`,
    /// Gemini `includeThoughts: false`) so providers don't spend
    /// tokens on summaries the operator won't see. Toggleable at
    /// runtime via `/show-reasoning`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_reasoning: Option<bool>,
    /// The provider gateway: WSDS Local AI Gateway's unified listener, set
    /// with `/gateway`. While `enabled`, every provider request is built on
    /// `"{url}/<prefix>"` (`provider::gateway::base_for`); `/gateway off`
    /// keeps the URL for `/gateway on`. Serde-additive `Option` — no schema
    /// bump; one more field on every `SystemConfig` literal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_gateway: Option<ProviderGateway>,
    /// OpenAI-compat preset configuration (Sprint 5 schema v11).
    /// Holds endpoint URL and selected model id per preset; bearer
    /// tokens are NOT here (STATE-only per Locked Decision #8).
    /// Empty strings on individual fields signal unconfigured; the
    /// presence of either preset's pair is unrelated to which provider
    /// is active (`api_provider`).
    #[serde(default)]
    pub openai_compat: OpenAiCompatConfig,
}

impl SystemConfig {
    /// The configuration a boot runs on when `load_config` fails (the
    /// first boot, before the wizard): the defaults, with the key embrad
    /// handed over and the timezone STATE holds. The one literal the three
    /// boot paths shared (#38); no `Default` is derived, so a new field is
    /// still named here and at every other construction.
    pub fn boot_fallback(api_key: String, timezone: String) -> Self {
        SystemConfig {
            name: "Embra".to_string(),
            api_key,
            timezone,
            deployment_mode: "phase1".into(),
            created_at: String::new(),
            version: env!("CARGO_PKG_VERSION").into(),
            github_token: None,
            kg_temporal_window_secs: 1800,
            kg_max_traversal_depth: 3,
            kg_traversal_depth_ceiling: 5,
            kg_edge_candidate_limit: 50,
            kg_traversal_edge_limit: 500,
            kg_traversal_node_budget: 1000,
            api_provider: "anthropic".to_string(),
            gemini_model: None,
            anthropic_model: None,
            anthropic_effort: None,
            gemini_effort: None,
            embedding_enabled: None,
            embedding_model: None,
            image_provider: None,
            image_model: None,
            git_tokens: None,
            guardian_secrets: None,
            guardian_private_hosts: None,
            anthropic_api_key: None,
            gemini_api_key: None,
            max_tool_iterations: None,
            show_reasoning: None,
            provider_gateway: None,
            openai_compat: OpenAiCompatConfig::default(),
        }
    }

    /// Effective `show_reasoning` decision: default-on when unset,
    /// otherwise honor the operator's explicit setting. Mirrors the
    /// `max_tool_iterations` precedent.
    pub fn show_reasoning(&self) -> bool {
        self.show_reasoning.unwrap_or(true)
    }
}

/// The provider gateway stored in [`SystemConfig::provider_gateway`]: the
/// URL of WSDS Local AI Gateway's unified listener and whether requests go
/// through it. `enabled` defaults to off, so a document that carries only
/// the URL reads as stored and off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderGateway {
    pub url: String,
    #[serde(default)]
    pub enabled: bool,
}

/// Per-preset OpenAI-compat config. Endpoint and model name only;
/// bearers live in STATE files at `/embra/state/bearer_<preset>` and
/// are sourced from env vars at brain startup (Stage 5).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenAiCompatConfig {
    #[serde(default)]
    pub ollama_endpoint: String,
    #[serde(default)]
    pub ollama_model: String,
    #[serde(default)]
    pub lm_studio_endpoint: String,
    #[serde(default)]
    pub lm_studio_model: String,
    /// Operator `reasoning_effort` override per preset (`/effort` while
    /// that preset is active), sent VERBATIM — the accepted set belongs
    /// to the model (Qwen3.8: low|medium|xhigh; gpt-oss: low|medium|high;
    /// the four Ollama Cloud models: low|high|max) and the server
    /// validates. `None` = the per-model auto-map
    /// (`reasoning_effort_for_model`). Serde-additive `Option`s — the four
    /// String fields above stay plain `default` so persisted docs keep
    /// their shape. Since levels are kept per model (below), these hold
    /// only a level stored before that, or before any model was chosen:
    /// it applies to the model in use until a switch files it under that
    /// model ([`Self::set_model`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ollama_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lm_studio_effort: Option<String>,
    /// The `/effort` level per model, keyed by the model id as configured.
    /// A switch never carries a level to a model that may not list it, and
    /// switching back brings each model's own level back; a model without
    /// an entry gets the auto-map. Serde-additive, absent when empty.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub ollama_effort_by_model: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub lm_studio_effort_by_model: std::collections::BTreeMap<String, String>,
}

impl OpenAiCompatConfig {
    /// Endpoint and model for the given preset. Returns `None` when
    /// either field is empty (unconfigured).
    pub fn for_preset(
        &self,
        preset: crate::provider::openai_compat::OpenAiCompatPreset,
    ) -> Option<(&str, &str)> {
        use crate::provider::openai_compat::OpenAiCompatPreset;
        let (endpoint, model) = match preset {
            OpenAiCompatPreset::Ollama => (&self.ollama_endpoint, &self.ollama_model),
            OpenAiCompatPreset::LmStudio => (&self.lm_studio_endpoint, &self.lm_studio_model),
        };
        if endpoint.is_empty() || model.is_empty() {
            None
        } else {
            Some((endpoint.as_str(), model.as_str()))
        }
    }

    /// The operator's stored effort override for the preset's active
    /// model, if any: its own entry, else a level stored the old way.
    pub fn effort_for_preset(
        &self,
        preset: crate::provider::openai_compat::OpenAiCompatPreset,
    ) -> Option<&str> {
        use crate::provider::openai_compat::OpenAiCompatPreset;
        let (model, legacy, by_model) = match preset {
            OpenAiCompatPreset::Ollama => {
                (&self.ollama_model, &self.ollama_effort, &self.ollama_effort_by_model)
            }
            OpenAiCompatPreset::LmStudio => {
                (&self.lm_studio_model, &self.lm_studio_effort, &self.lm_studio_effort_by_model)
            }
        };
        by_model.get(model).or(legacy.as_ref()).map(String::as_str)
    }

    /// Store (`Some`) or clear (`None`) the `/effort` level of the
    /// preset's active model. A level stored the old way belonged to that
    /// model and goes. With no model chosen yet the level is kept the old
    /// way, for the first model chosen.
    pub fn set_effort(
        &mut self,
        preset: crate::provider::openai_compat::OpenAiCompatPreset,
        level: Option<String>,
    ) {
        let (model, legacy, by_model) = self.effort_parts(preset);
        if model.is_empty() {
            *legacy = level;
            return;
        }
        *legacy = None;
        match level {
            Some(level) => {
                by_model.insert(model.clone(), level);
            }
            None => {
                by_model.remove(model.as_str());
            }
        }
    }

    /// Point the preset at `new_model`. A level stored the old way belonged
    /// to the model in use until now and is filed under it, so the new
    /// model starts at its own level, or its auto-map.
    pub fn set_model(
        &mut self,
        preset: crate::provider::openai_compat::OpenAiCompatPreset,
        new_model: &str,
    ) {
        let (model, legacy, by_model) = self.effort_parts(preset);
        if !model.is_empty()
            && let Some(level) = legacy.take()
        {
            by_model.entry(model.clone()).or_insert(level);
        }
        *model = new_model.to_string();
    }

    /// The preset's model, its old single level and its per-model levels.
    fn effort_parts(
        &mut self,
        preset: crate::provider::openai_compat::OpenAiCompatPreset,
    ) -> (&mut String, &mut Option<String>, &mut std::collections::BTreeMap<String, String>) {
        use crate::provider::openai_compat::OpenAiCompatPreset;
        match preset {
            OpenAiCompatPreset::Ollama => {
                (&mut self.ollama_model, &mut self.ollama_effort, &mut self.ollama_effort_by_model)
            }
            OpenAiCompatPreset::LmStudio => (
                &mut self.lm_studio_model,
                &mut self.lm_studio_effort,
                &mut self.lm_studio_effort_by_model,
            ),
        }
    }
}

/// Write a credential file to the STATE partition with mode `0600`
/// (read/write for owner only). Used for API keys and bearer tokens
/// per Locked Decision #8 — STATE files holding secrets must not be
/// world-readable, even on the otherwise-immutable embraOS rootfs.
///
/// Creates parent directories as needed. Writes content first, then
/// applies permissions. Returns the file's final mode on success for
/// caller-side telemetry / assertion in tests.
pub(crate) fn write_credential_state(path: &str, contents: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

impl SystemConfig {
    /// Look up the recorded key for the given provider. Per-provider
    /// fields are preferred; falls back to the legacy `api_key` field
    /// when the active provider matches `kind` (handles pre-v10 docs
    /// before migration runs).
    pub fn key_for(&self, kind: ProviderKind) -> Option<&str> {
        match kind {
            ProviderKind::Anthropic => self
                .anthropic_api_key
                .as_deref()
                .or_else(|| (self.api_provider == "anthropic").then_some(self.api_key.as_str()))
                .filter(|s| !s.is_empty()),
            ProviderKind::Gemini => self
                .gemini_api_key
                .as_deref()
                .or_else(|| (self.api_provider == "gemini").then_some(self.api_key.as_str()))
                .filter(|s| !s.is_empty()),
            // OpenAI-compat presets keep their bearer in STATE files only
            // (Stage 5 wiring); never in SystemConfig. `key_for` returns
            // None — callers fall through to env-var lookup.
            ProviderKind::Ollama | ProviderKind::LmStudio => None,
        }
    }
}

/// The configuration document as the `get` tool shows it: the fields that
/// hold a credential replaced by `"[redacted]"`, every other field as
/// stored. Guard `redact_replaces_the_credential_fields_and_keeps_the_rest`.
pub(crate) fn redact(mut doc: serde_json::Value) -> serde_json::Value {
    const REDACTED: &str = "[redacted]";
    if let Some(obj) = doc.as_object_mut() {
        for key in ["api_key", "anthropic_api_key", "gemini_api_key", "github_token"] {
            if let Some(v) = obj.get_mut(key)
                && v.as_str().is_some_and(|s| !s.is_empty())
            {
                *v = REDACTED.into();
            }
        }
        if let Some(tokens) = obj.get_mut("git_tokens").and_then(|v| v.as_object_mut()) {
            for v in tokens.values_mut() {
                *v = REDACTED.into();
            }
        }
        if let Some(hosts) = obj.get_mut("guardian_secrets").and_then(|v| v.as_object_mut()) {
            for headers in hosts.values_mut().filter_map(|h| h.as_object_mut()) {
                for v in headers.values_mut() {
                    *v = REDACTED.into();
                }
            }
        }
    }
    doc
}

/// A key as the setup wizard's summary shows it: its length and its last
/// four characters, by character, never a byte slice (a slice could cut a
/// character). Guard `the_key_summary_never_cuts_a_character`.
pub(crate) fn key_summary(key: &str) -> String {
    let n = key.chars().count();
    let tail: String = key.chars().skip(n.saturating_sub(4)).collect();
    format!("{n} characters, ending …{tail}")
}

fn default_kg_temporal_window() -> u64 { 1800 }
fn default_kg_max_depth() -> u32 { 3 }
fn default_kg_depth_ceiling() -> u32 { 5 }
fn default_kg_candidate_limit() -> u32 { 50 }
fn default_kg_traversal_edge_limit() -> u32 { 500 }
fn default_kg_traversal_node_budget() -> u32 { 1000 }
fn default_api_provider() -> String { "anthropic".to_string() }

const PROVIDER_ANTHROPIC_OPUS5_LABEL: &str = "Anthropic Claude Opus 5";
const PROVIDER_ANTHROPIC_OPUS55_LABEL: &str = "Anthropic Claude Opus 5.5";
const PROVIDER_ANTHROPIC_OPUS48_LABEL: &str = "Anthropic Claude Opus 4.8";
const PROVIDER_ANTHROPIC_FABLE51_LABEL: &str = "Anthropic Claude Fable 5.1";
const PROVIDER_ANTHROPIC_FABLE_LABEL: &str = "Anthropic Claude Fable 5";
const PROVIDER_GEMINI_LABEL: &str = "Google Gemini 3.1 Pro";
const PROVIDER_GEMINI_ARGON_LABEL: &str = "Google Gemini 4 Argon";
const PROVIDER_OLLAMA_LABEL: &str = "Ollama (OpenAI-compat)";
const PROVIDER_LM_STUDIO_LABEL: &str = "LM Studio (OpenAI-compat)";

/// The provider-selection labels, in display order. INDEX 0 IS THE
/// EFFECTIVE DEFAULT: the console builds its selector with the first
/// option pre-selected and does not read `default_value`
/// (`embra-console` `Selector::new`), so the label of the default model
/// must stay first.
pub(crate) const PROVIDER_WIZARD_LABELS: [&str; 9] = [
    PROVIDER_ANTHROPIC_OPUS5_LABEL,
    PROVIDER_ANTHROPIC_OPUS55_LABEL,
    PROVIDER_ANTHROPIC_OPUS48_LABEL,
    PROVIDER_ANTHROPIC_FABLE51_LABEL,
    PROVIDER_ANTHROPIC_FABLE_LABEL,
    PROVIDER_GEMINI_LABEL,
    PROVIDER_GEMINI_ARGON_LABEL,
    PROVIDER_OLLAMA_LABEL,
    PROVIDER_LM_STUDIO_LABEL,
];

/// The Anthropic and the Gemini labels, in wizard order: the options of
/// the model step that ends `/provider --setup <anthropic|gemini>`. A
/// test keeps them equal to the wizard's own.
pub(crate) const ANTHROPIC_WIZARD_LABELS: [&str; 5] = [
    PROVIDER_ANTHROPIC_OPUS5_LABEL,
    PROVIDER_ANTHROPIC_OPUS55_LABEL,
    PROVIDER_ANTHROPIC_OPUS48_LABEL,
    PROVIDER_ANTHROPIC_FABLE51_LABEL,
    PROVIDER_ANTHROPIC_FABLE_LABEL,
];
pub(crate) const GEMINI_WIZARD_LABELS: [&str; 2] =
    [PROVIDER_GEMINI_LABEL, PROVIDER_GEMINI_ARGON_LABEL];

fn provider_from_label(label: &str) -> ProviderKind {
    match label {
        PROVIDER_GEMINI_LABEL | PROVIDER_GEMINI_ARGON_LABEL => ProviderKind::Gemini,
        PROVIDER_OLLAMA_LABEL => ProviderKind::Ollama,
        PROVIDER_LM_STUDIO_LABEL => ProviderKind::LmStudio,
        // All Anthropic labels map here; the chosen model is captured
        // separately by `anthropic_model_from_label`.
        _ => ProviderKind::Anthropic,
    }
}

/// Which Anthropic model a provider-selection label implies, as the value
/// persisted to `SystemConfig.anthropic_model`. Every Anthropic label seeds
/// its model EXPLICITLY (never `None` = "ride the default") so a wizard
/// choice keeps meaning what the operator picked even if the provider's
/// `DEFAULT_MODEL` changes in a later build. Non-Anthropic labels yield
/// `None`. Later switchable at runtime via `/model`.
pub(crate) fn anthropic_model_from_label(label: &str) -> Option<String> {
    match label {
        PROVIDER_ANTHROPIC_OPUS5_LABEL => Some("opus-5".to_string()),
        PROVIDER_ANTHROPIC_OPUS55_LABEL => Some("opus-5.5".to_string()),
        PROVIDER_ANTHROPIC_OPUS48_LABEL => Some("opus-4.8".to_string()),
        PROVIDER_ANTHROPIC_FABLE51_LABEL => Some("fable-5.1".to_string()),
        PROVIDER_ANTHROPIC_FABLE_LABEL => Some("fable-5".to_string()),
        _ => None,
    }
}

/// Which Gemini model a label implies, as the alias persisted to
/// `SystemConfig.gemini_model` (the brain maps it to the API id). Like
/// the Anthropic labels, every Gemini label seeds its model explicitly,
/// so a wizard choice keeps meaning what the operator picked if the
/// default moves in a later build. Other labels yield `None`. Later
/// switchable via `/model`.
pub(crate) fn gemini_model_from_label(label: &str) -> Option<String> {
    match label {
        PROVIDER_GEMINI_LABEL => Some("gemini-3.1-pro".to_string()),
        PROVIDER_GEMINI_ARGON_LABEL => Some("gemini-4-argon".to_string()),
        _ => None,
    }
}

pub async fn save_config(db: &WardsonDbClient, config: &SystemConfig) -> Result<()> {
    if !db.collection_exists("config.system").await? {
        db.create_collection("config.system").await?;
    }
    let mut doc = serde_json::to_value(config)?;
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("_id".into(), serde_json::json!("config"));
        // F-7: stamp the writing build, so the persisted doc self-heals an
        // older `version` string on the next save (see load_config).
        obj.insert("version".into(), serde_json::json!(env!("CARGO_PKG_VERSION")));
    }
    match db.write("config.system", &doc).await {
        Ok(_) => Ok(()),
        // 409: the document exists, so it is updated. Any other failure is
        // the failure it is (#61: a timeout is not a conflict).
        Err(e) if e.downcast_ref::<crate::db::WardsonDbError>().is_some_and(|w| w.is_conflict()) => {
            db.update("config.system", "config", &doc).await?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Validate a proposed intelligence display name (the `set_name` tool
/// and the Phase-2 identity→config sync both route through this; the
/// wizard's Step-1 free-text answer predates it and stays permissive).
/// Returns the trimmed name. Bounds are display-driven: the name renders
/// in the status bar, the `[timestamp] Name:` message prefix, and the
/// `You are {name}` prompt line — single line, 1..=40 chars, no control
/// characters. It also rides the ModeTransition `"Name: X — "` token, so
/// the ` — ` separator sequence is rejected to keep that parse unambiguous.
pub fn validate_intelligence_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("name must not be empty".to_string());
    }
    if name.chars().count() > 40 {
        return Err("name must be at most 40 characters".to_string());
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("name must be a single line without control characters".to_string());
    }
    if name.contains(" — ") {
        return Err("name must not contain the ' — ' separator sequence".to_string());
    }
    Ok(name.to_string())
}

pub async fn load_config(db: &WardsonDbClient) -> Result<SystemConfig> {
    // Try direct GET by well-known ID first
    let mut config = match db.read("config.system", "config").await {
        Ok(doc) => serde_json::from_value::<SystemConfig>(doc)?,
        Err(_) => {
            // Fallback: query pattern (pre-migration data)
            let results = db
                .query("config.system", &crate::db::client::first_doc_query_body())
                .await?;
            let doc = results
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("No system config found"))?;
            serde_json::from_value::<SystemConfig>(doc)?
        }
    };
    // Always resolve timezone abbreviations (PDT → America/Los_Angeles) so chrono-tz can parse
    config.timezone = crate::tools::resolve_timezone(&config.timezone);
    // F-7: `version` is the build the config runs under, not the build that
    // first created the DATA. A long-lived instance kept its creation-time
    // string (e.g. 0.5.0-phase1) across image rebuilds while the binary moved
    // on; refresh it so a reader of the stored doc never sees a version that
    // disagrees with the running binary.
    config.version = env!("CARGO_PKG_VERSION").into();
    Ok(config)
}

enum ApiKeyCheck {
    Valid,
    Invalid(String),
}

/// Public-to-crate adapter that returns `Result<(), String>`.
/// Used by `/provider --setup` (Sprint 4 D2) to share the wizard's
/// validation path without exposing the module-private `ApiKeyCheck`.
pub(crate) async fn check_api_key(
    provider: ProviderKind,
    key: &str,
    gateway: Option<&ProviderGateway>,
) -> Result<(), String> {
    match validate_api_key_for(provider, key, gateway).await {
        ApiKeyCheck::Valid => Ok(()),
        ApiKeyCheck::Invalid(msg) => Err(msg),
    }
}

/// Probe the chosen provider's model-listing endpoint with the given
/// key. Single source of truth — the providers' own `validate_key`
/// impls (constructed with an empty key for the probe) carry the
/// HTTP shape and status mapping. The probe goes where the next turn
/// would (`provider::gateway::base_for`): through the provider gateway
/// when `gateway` is stored and on, to the provider's own host otherwise.
async fn validate_api_key_for(
    provider: ProviderKind,
    key: &str,
    gateway: Option<&ProviderGateway>,
) -> ApiKeyCheck {
    if key.is_empty() {
        return ApiKeyCheck::Invalid("API key is empty.".into());
    }
    let result = match provider {
        ProviderKind::Anthropic => {
            // Anthropic-specific local check the provider impl also enforces.
            if !key.starts_with("sk-") {
                return ApiKeyCheck::Invalid(
                    "Anthropic API key must start with 'sk-'.".into(),
                );
            }
            AnthropicProvider::new(String::new())
                .with_base_url(crate::provider::gateway::base_for(
                    gateway,
                    ProviderKind::Anthropic,
                    crate::provider::anthropic::DIRECT_BASE,
                ))
                .validate_key(key)
                .await
        }
        ProviderKind::Gemini => {
            GeminiProvider::new(String::new())
                .with_base_url(crate::provider::gateway::base_for(
                    gateway,
                    ProviderKind::Gemini,
                    crate::provider::gemini::DIRECT_BASE,
                ))
                .validate_key(key)
                .await
        }
        // OpenAI-compat presets validate via endpoint+bearer probe in the
        // wizard's Stage 4 flow, not via this api-key path. This arm is
        // unreachable from the current 2-way wizard but kept for
        // exhaustive-match completeness.
        ProviderKind::Ollama | ProviderKind::LmStudio => {
            return ApiKeyCheck::Invalid(
                "OpenAI-compat providers use the endpoint+bearer wizard flow.".into(),
            );
        }
    };
    match (result, provider) {
        (ValidationResult::Valid, _) => ApiKeyCheck::Valid,
        (ValidationResult::InvalidKey, ProviderKind::Anthropic) => ApiKeyCheck::Invalid(
            "Invalid Anthropic API key — check and try again.".into(),
        ),
        (ValidationResult::InvalidKey, ProviderKind::Gemini) => ApiKeyCheck::Invalid(
            "Invalid Gemini API key — check the key and try again.".into(),
        ),
        (ValidationResult::InvalidKey, ProviderKind::Ollama | ProviderKind::LmStudio) => {
            ApiKeyCheck::Invalid("OpenAI-compat bearer rejected.".into())
        }
        (ValidationResult::Forbidden, ProviderKind::Anthropic) => ApiKeyCheck::Invalid(
            "Anthropic API rejected the key (403). Verify it's active and try again.".into(),
        ),
        (ValidationResult::Forbidden, ProviderKind::Gemini) => ApiKeyCheck::Invalid(
            "Forbidden — likely missing billing on the Gemini API. \
             Verify at aistudio.google.com — Gemini 3.1 Pro and Gemini 4 Argon require a billed (paid-tier) account."
                .into(),
        ),
        (ValidationResult::Forbidden, ProviderKind::Ollama | ProviderKind::LmStudio) => {
            ApiKeyCheck::Invalid("OpenAI-compat endpoint refused the bearer (403).".into())
        }
        (ValidationResult::NetworkError, _) => ApiKeyCheck::Invalid(
            "Could not verify key — check network and try again.".into(),
        ),
        (ValidationResult::Unknown, _) => ApiKeyCheck::Invalid(
            "Could not verify key — provider returned an unexpected response.".into(),
        ),
    }
}

fn validate_timezone(input: &str) -> Result<String, String> {
    let resolved = crate::tools::resolve_timezone(input);
    resolved
        .parse::<chrono_tz::Tz>()
        .map(|_| resolved.clone())
        .map_err(|_| {
            format!(
                "Invalid timezone '{}' — expected an IANA name like 'America/Los_Angeles'.",
                input
            )
        })
}

/// Drive the config wizard over a gRPC Converse stream.
/// Sends SetupPrompt messages and waits for UserMessage responses.
pub async fn run_config_wizard_grpc(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
    db: &WardsonDbClient,
) -> Result<SystemConfig> {
    // Welcome
    let _ = tx.send(Ok(ConversationResponse {
        response_type: Some(conversation_response::ResponseType::ModeChange(
            ModeTransition {
                from_mode: OperatingMode::Unspecified as i32,
                to_mode: OperatingMode::Setup as i32,
                message: "Welcome to embraOS — First Run Setup — TZ: Etc/UTC".to_string(),
            }
        )),
    })).await;

    // Step 1: Name
    let _ = tx.send(Ok(ConversationResponse {
        response_type: Some(conversation_response::ResponseType::Setup(
            SetupPrompt {
                field_type: SetupFieldType::Text as i32,
                prompt: "What would you like to name your intelligence?".to_string(),
                options: vec![],
                default_value: "Embra".to_string(),
            }
        )),
    })).await;

    let name = match response_rx.recv().await {
        Some(input) if !input.is_empty() => input,
        Some(_) => "Embra".to_string(),
        // #61: a closed channel is the client gone, not an empty answer.
        None => anyhow::bail!("setup channel closed during the name step"),
    };
    info!("Config wizard: name = {}", name);

    // Step 2: Provider selection (Sprint 4 → Sprint 5 4-way, then the
    // Anthropic line-up, five models since 2026-09-27) —
    // Selector UI. The default is the first label (see
    // PROVIDER_WIZARD_LABELS) and tracks the provider's DEFAULT_MODEL.
    let _ = tx.send(Ok(ConversationResponse {
        response_type: Some(conversation_response::ResponseType::Setup(
            SetupPrompt {
                field_type: SetupFieldType::Selector as i32,
                prompt: "Which AI provider would you like to use?".to_string(),
                options: PROVIDER_WIZARD_LABELS.iter().map(|l| l.to_string()).collect(),
                default_value: PROVIDER_WIZARD_LABELS[0].to_string(),
            }
        )),
    })).await;
    let provider_choice = match response_rx.recv().await {
        Some(input) if !input.is_empty() => input,
        Some(_) => PROVIDER_WIZARD_LABELS[0].to_string(),
        None => anyhow::bail!("setup channel closed during the provider step"),
    };
    let provider_kind = provider_from_label(&provider_choice);
    // Capture the chosen Anthropic model for the Anthropic path; persisted
    // below as `SystemConfig.anthropic_model`.
    let anthropic_model = anthropic_model_from_label(&provider_choice);
    // And the Gemini model for the Gemini path (`SystemConfig.gemini_model`).
    let gemini_model = gemini_model_from_label(&provider_choice);
    info!(
        "Config wizard: provider = {}{}",
        provider_kind.as_str(),
        anthropic_model
            .as_deref()
            .or(gemini_model.as_deref())
            .map(|m| format!(" ({m})"))
            .unwrap_or_default()
    );

    // Sprint 5: OpenAI-compat presets dispatch into the new sub-flow
    // (Endpoint → Bearer → Probe-and-Select). Existing 2-way path
    // covers Anthropic/Gemini unchanged.
    let openai_compat_setup = match provider_kind {
        ProviderKind::Ollama => Some(
            crate::setup::wizard::run_openai_compat_subflow(
                crate::provider::openai_compat::OpenAiCompatPreset::Ollama,
                tx,
                response_rx,
            )
            .await?,
        ),
        ProviderKind::LmStudio => Some(
            crate::setup::wizard::run_openai_compat_subflow(
                crate::provider::openai_compat::OpenAiCompatPreset::LmStudio,
                tx,
                response_rx,
            )
            .await?,
        ),
        _ => None,
    };

    // Honor an env-var of the wrong shape with a clear info message
    // rather than silently accepting it.
    let env_var_name = match provider_kind {
        ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
        ProviderKind::Gemini => "GEMINI_API_KEY",
        ProviderKind::Ollama => "EMBRA_OLLAMA_BEARER",
        ProviderKind::LmStudio => "EMBRA_LM_STUDIO_BEARER",
    };
    let other_env_name = match provider_kind {
        ProviderKind::Anthropic => "GEMINI_API_KEY",
        ProviderKind::Gemini => "ANTHROPIC_API_KEY",
        // For OpenAI-compat presets there isn't a single "the other"
        // env var (the wizard 4-way flow handles its own prompts).
        // Use the cross-preset alternative as the placeholder.
        ProviderKind::Ollama => "EMBRA_LM_STUDIO_BEARER",
        ProviderKind::LmStudio => "EMBRA_OLLAMA_BEARER",
    };
    // Skip env-var "other" warning for OpenAI-compat — bearer flow
    // already gathered the relevant credential via prompts.
    if openai_compat_setup.is_none()
        && std::env::var(other_env_name).map(|v| !v.is_empty()).unwrap_or(false)
    {
        let _ = tx.send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(
                SystemMessage {
                    content: format!(
                        "{other_env_name} detected in environment but provider is {} — ignored.",
                        provider_choice
                    ),
                    msg_type: SystemMessageType::Info as i32,
                }
            )),
        })).await;
    }
    let _ = env_var_name; // silence unused-variable warning for OpenAI-compat path

    // Step 3: API Key — re-prompts until provider's validate_key returns Valid.
    // Skipped entirely for OpenAI-compat presets (Sprint 5): bearer was
    // already collected in the sub-flow and is held in `openai_compat_setup`.
    // The legacy `api_key` field stays empty for OpenAI-compat configs.
    let api_key = if openai_compat_setup.is_some() {
        String::new()
    } else {
        loop {
            let (candidate, from_env) = match std::env::var(env_var_name) {
                Ok(k) if !k.is_empty() => (k, true),
                _ => {
                    let prompt_text = match provider_kind {
                        ProviderKind::Anthropic => "Enter your Anthropic API key:",
                        ProviderKind::Gemini => "Enter your Gemini API key:",
                        // OpenAI-compat presets are handled via the
                        // sub-flow above and never reach this prompt.
                        // Arms exist for exhaustive-match completeness.
                        ProviderKind::Ollama | ProviderKind::LmStudio => {
                            unreachable!(
                                "openai_compat presets bypass api_key loop via openai_compat_setup branch"
                            )
                        }
                    };
                    let _ = tx.send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::Setup(
                            SetupPrompt {
                                field_type: SetupFieldType::Text as i32,
                                prompt: prompt_text.to_string(),
                                options: vec![],
                                default_value: String::new(),
                            }
                        )),
                    })).await;
                    // A closed channel is the client gone, not an empty
                    // answer: the step ends instead of looping (#61).
                    (
                        response_rx
                            .recv()
                            .await
                            .ok_or_else(|| anyhow::anyhow!("setup channel closed during the API key step"))?,
                        false,
                    )
                }
            };
            if from_env {
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: format!(
                                "{env_var_name} detected from environment."
                            ),
                            msg_type: SystemMessageType::Info as i32,
                        }
                    )),
                })).await;
            }
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage {
                        content: format!(
                            "Validating API key with {}…",
                            provider_choice
                        ),
                        msg_type: SystemMessageType::Info as i32,
                    }
                )),
            })).await;
            // The first boot has no provider gateway yet: the check goes
            // to the provider's own host.
            match validate_api_key_for(provider_kind, &candidate, None).await {
                ApiKeyCheck::Valid => break candidate,
                ApiKeyCheck::Invalid(msg) => {
                    let _ = tx.send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: msg,
                                msg_type: SystemMessageType::Error as i32,
                            }
                        )),
                    })).await;
                    if from_env {
                        // Stop trusting the env var; fall through to prompting next iteration.
                        // SAFETY: single-threaded access to env at wizard time.
                        unsafe { std::env::remove_var(env_var_name); }
                    }
                    continue;
                }
            }
        }
    };

    // Step 3: Timezone — re-prompts until chrono_tz can parse the resolved value.
    let timezone = loop {
        let tz_default = detect_timezone();
        let _ = tx.send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(
                SetupPrompt {
                    field_type: SetupFieldType::Text as i32,
                    prompt: format!("What timezone are you in? (detected: {})", tz_default),
                    options: vec![],
                    default_value: tz_default.clone(),
                }
            )),
        })).await;
        let input = response_rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("setup channel closed during the timezone step"))?;
        let candidate = if input.is_empty() { tz_default } else { input };
        match validate_timezone(&candidate) {
            Ok(tz) => break tz,
            Err(msg) => {
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: msg,
                            msg_type: SystemMessageType::Error as i32,
                        }
                    )),
                })).await;
                continue;
            }
        }
    };

    // Step 5: Confirmation
    let summary = format!(
        "Configuration summary:\n  Name: {}\n  Provider: {}\n  API Key: {}\n  Timezone: {}\n\nConfirm?",
        name,
        provider_choice,
        key_summary(&api_key),
        timezone,
    );
    let _ = tx.send(Ok(ConversationResponse {
        response_type: Some(conversation_response::ResponseType::Setup(
            SetupPrompt {
                field_type: SetupFieldType::Confirm as i32,
                prompt: summary,
                options: vec!["Yes, confirm".to_string(), "No, restart setup".to_string()],
                default_value: String::new(),
            }
        )),
    })).await;

    let confirm = response_rx
        .recv()
        .await
        .ok_or_else(|| anyhow::anyhow!("setup channel closed during the confirmation step"))?
        .to_lowercase();
    if confirm.contains("no") || confirm == "2" {
        // Restart — recursive call (simple approach for now)
        return Box::pin(run_config_wizard_grpc(tx, response_rx, db)).await;
    }

    // Save config — populate the legacy api_key (active provider's
    // key) AND the matching per-provider field so post-v10 reads
    // resolve correctly.
    let (anthropic_api_key, gemini_api_key) = match provider_kind {
        ProviderKind::Anthropic => (Some(api_key.clone()), None),
        ProviderKind::Gemini => (None, Some(api_key.clone())),
        // OpenAI-compat presets don't populate either api_key field;
        // bearer goes to STATE only (Stage 5 wiring).
        ProviderKind::Ollama | ProviderKind::LmStudio => (None, None),
    };
    // Populate OpenAI-compat fields per preset when the sub-flow ran.
    let mut openai_compat = OpenAiCompatConfig::default();
    if let Some(setup) = &openai_compat_setup {
        use crate::provider::openai_compat::OpenAiCompatPreset;
        match setup.preset {
            OpenAiCompatPreset::Ollama => {
                openai_compat.ollama_endpoint = setup.endpoint.clone();
            }
            OpenAiCompatPreset::LmStudio => {
                openai_compat.lm_studio_endpoint = setup.endpoint.clone();
            }
        }
        openai_compat.set_model(setup.preset, &setup.model_id);
    }
    let config = SystemConfig {
        name,
        api_key,
        timezone,
        deployment_mode: "phase1".into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        version: env!("CARGO_PKG_VERSION").into(),
        github_token: None,
        kg_temporal_window_secs: default_kg_temporal_window(),
        kg_max_traversal_depth: default_kg_max_depth(),
        kg_traversal_depth_ceiling: default_kg_depth_ceiling(),
        kg_edge_candidate_limit: default_kg_candidate_limit(),
        kg_traversal_edge_limit: default_kg_traversal_edge_limit(),
        kg_traversal_node_budget: default_kg_traversal_node_budget(),
        api_provider: provider_kind.as_str().to_string(),
        gemini_model,
        anthropic_model,
        anthropic_effort: None,
        gemini_effort: None,
        embedding_enabled: None,
        embedding_model: None,
        image_provider: None,
        image_model: None,
        git_tokens: None,
        guardian_secrets: None,
        guardian_private_hosts: None,
        anthropic_api_key,
        gemini_api_key,
        max_tool_iterations: None,
        show_reasoning: None,
        provider_gateway: None,
        openai_compat,
    };
    save_config(db, &config).await?;
    info!("Config wizard complete, saved to WardSONDB");

    // Write timezone to STATE partition so embrad can set TZ on subsequent boots
    let tz_path = "/embra/state/timezone";
    if let Err(e) = std::fs::write(tz_path, &config.timezone) {
        tracing::warn!("Could not write timezone to STATE: {}", e);
    } else {
        info!("Timezone written to {} ({})", tz_path, &config.timezone);
    }

    // Also write API key to STATE partition so embrad can pass it on
    // subsequent boots — only for Anthropic/Gemini (OpenAI-compat
    // bearer is written separately below at /embra/state/bearer_<preset>).
    // Mode 0600 per Locked Decision #8 (Sprint 5 retroactive fix to
    // existing api_key writes that previously used default umask).
    if openai_compat_setup.is_none() {
        let key_path = "/embra/state/api_key";
        if let Err(e) = write_credential_state(key_path, &config.api_key) {
            tracing::warn!("Could not write API key to STATE: {}", e);
        } else {
            info!("API key written to {} (mode 0600)", key_path);
        }
    }

    // Sprint 4: persist active provider to STATE so embrad picks the
    // right LlmProvider impl on subsequent boots.
    let provider_path = "/embra/state/api_provider";
    if let Err(e) = std::fs::write(provider_path, &config.api_provider) {
        tracing::warn!("Could not write api_provider to STATE: {}", e);
    } else {
        info!("Provider written to {} ({})", provider_path, &config.api_provider);
    }

    // OpenAI-compat bearer write: STATE file at /embra/state/bearer_<preset>
    // with mode 0600 per Locked Decision #8. Empty bearer → no file
    // written; existing file removed if present.
    if let Some(setup) = &openai_compat_setup {
        let path = match setup.preset {
            crate::provider::openai_compat::OpenAiCompatPreset::Ollama => {
                "/embra/state/bearer_ollama"
            }
            crate::provider::openai_compat::OpenAiCompatPreset::LmStudio => {
                "/embra/state/bearer_lm_studio"
            }
        };
        match &setup.bearer {
            Some(token) if !token.is_empty() => {
                if let Err(e) = write_credential_state(path, token) {
                    tracing::warn!("Could not write bearer to STATE: {}", e);
                } else {
                    info!("Bearer written to {} (mode 0600)", path);
                }
            }
            _ => {
                // Empty / None bearer — remove any existing file so a
                // re-run wizard with no auth doesn't leave stale creds.
                let _ = std::fs::remove_file(path);
            }
        }
    }

    // D2 schema v10: also write the per-provider key STATE file so
    // /provider switches between reboots find a recorded key for the
    // alternate provider. Wizard only writes the key for the active
    // provider; the alternate is added later via /provider --setup.
    // Skipped for OpenAI-compat — bearer landed at
    // /embra/state/bearer_<preset> above (Sprint 5).
    if openai_compat_setup.is_none() {
        let per_provider_state_path = match provider_kind {
            ProviderKind::Anthropic => "/embra/state/api_key_anthropic",
            ProviderKind::Gemini => "/embra/state/api_key_gemini",
            // Unreachable: gated by `openai_compat_setup.is_none()`.
            ProviderKind::Ollama | ProviderKind::LmStudio => unreachable!(
                "openai_compat presets bypass legacy per-provider api_key path"
            ),
        };
        if let Err(e) = write_credential_state(per_provider_state_path, &config.api_key) {
            tracing::warn!(
                "Could not write per-provider key to {}: {}",
                per_provider_state_path,
                e
            );
        } else {
            info!("Per-provider key written to {} (mode 0600)", per_provider_state_path);
        }
    }

    // Transition to next mode
    let soul_sealed = crate::learning::is_soul_sealed(db).await.unwrap_or(false);
    let next_mode = if soul_sealed {
        OperatingMode::Operational
    } else {
        OperatingMode::Learning
    };
    // Sprint 4: include the active model so the console status bar
    // refreshes from its first-paint default to whatever provider
    // the operator just selected. Inline match — display_model_for
    // lives in grpc_service.rs and we don't want a circular dep.
    // Sprint 5: OpenAI-compat presets show the operator-selected
    // model id (Ollama/LM Studio) populated from the sub-flow.
    let model: String = match config.api_provider.as_str() {
        // Gemini: the seeded alias, shown the way the turn path shows it.
        "gemini" => crate::provider::gemini::display_name_for(
            config
                .gemini_model
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(crate::provider::gemini::DEFAULT_MODEL),
        ),
        "ollama" => {
            if config.openai_compat.ollama_model.is_empty() {
                "ollama".to_string()
            } else {
                config.openai_compat.ollama_model.clone()
            }
        }
        "lm_studio" => {
            if config.openai_compat.lm_studio_model.is_empty() {
                "lm_studio".to_string()
            } else {
                config.openai_compat.lm_studio_model.clone()
            }
        }
        // Anthropic (and any unknown): reflect the chosen/persisted model
        // so the status bar shows what the operator selected (every
        // Anthropic wizard label now seeds it explicitly). (Inline rather
        // than calling grpc_service's resolver — that would be a circular
        // dep, per the note above.)
        _ => config
            .anthropic_model
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| crate::provider::anthropic::DEFAULT_DISPLAY_NAME.to_string()),
    };
    let _ = tx.send(Ok(ConversationResponse {
        response_type: Some(conversation_response::ResponseType::ModeChange(
            ModeTransition {
                from_mode: OperatingMode::Setup as i32,
                to_mode: next_mode as i32,
                message: format!(
                    "Setup complete — Name: {} — TZ: {} — Brain: {}",
                    config.name, config.timezone, model
                ),
            }
        )),
    })).await;

    Ok(config)
}

#[cfg(test)]
mod key_lookup_tests {
    use super::*;

    fn cfg(api_key: &str, provider: &str, anth: Option<&str>, gem: Option<&str>) -> SystemConfig {
        let mut c = super::tests_support::minimal_cfg();
        c.api_key = api_key.into();
        c.api_provider = provider.into();
        c.anthropic_api_key = anth.map(str::to_string);
        c.gemini_api_key = gem.map(str::to_string);
        c
    }

    #[test]
    fn key_for_returns_per_provider_field_when_set() {
        let c = cfg("active-key", "anthropic", Some("sk-anth"), Some("ai-gem"));
        assert_eq!(c.key_for(ProviderKind::Anthropic), Some("sk-anth"));
        assert_eq!(c.key_for(ProviderKind::Gemini), Some("ai-gem"));
    }

    #[test]
    fn key_for_falls_back_to_legacy_for_active_provider() {
        // Pre-v10 doc — only legacy api_key + api_provider populated.
        let c = cfg("sk-legacy", "anthropic", None, None);
        assert_eq!(c.key_for(ProviderKind::Anthropic), Some("sk-legacy"));
        // Other provider has no key recorded.
        assert_eq!(c.key_for(ProviderKind::Gemini), None);
    }

    #[test]
    fn key_for_returns_none_when_inactive_provider_has_no_per_provider_key() {
        // Active = anthropic, gemini key was never set; legacy
        // api_key is the anthropic key. Gemini lookup must NOT
        // return the anthropic key as a fallback.
        let c = cfg("sk-anth-active", "anthropic", None, None);
        assert_eq!(c.key_for(ProviderKind::Gemini), None);
    }

    #[test]
    fn key_for_per_provider_wins_over_legacy() {
        // Per-provider field set explicitly via /provider --setup;
        // legacy api_key is something else (active provider's key).
        let c = cfg("sk-active", "anthropic", None, Some("ai-stashed"));
        assert_eq!(c.key_for(ProviderKind::Gemini), Some("ai-stashed"));
        assert_eq!(c.key_for(ProviderKind::Anthropic), Some("sk-active"));
    }

    #[test]
    fn key_for_filters_empty_strings() {
        let c = cfg("", "anthropic", Some(""), None);
        assert!(c.key_for(ProviderKind::Anthropic).is_none());
    }
}

fn detect_timezone() -> String {
    // Try to read from system
    if let Ok(tz) = std::env::var("TZ")
        && !tz.is_empty()
    {
        return tz;
    }
    // Try reading /etc/timezone (Linux)
    if let Ok(tz) = std::fs::read_to_string("/etc/timezone") {
        let tz = tz.trim().to_string();
        if !tz.is_empty() {
            return tz;
        }
    }
    "UTC".into()
}

/// Test-only construction helper shared across the crate (every literal
/// must name every field — `SystemConfig` has no `Default`).
#[cfg(test)]
pub(crate) mod tests_support {
    use super::SystemConfig;

    pub(crate) fn minimal_cfg() -> SystemConfig {
        SystemConfig {
            name: "Embra".into(),
            api_key: "k".into(),
            timezone: "UTC".into(),
            deployment_mode: "phase1".into(),
            created_at: String::new(),
            version: "test".into(),
            github_token: None,
            kg_temporal_window_secs: 1800,
            kg_max_traversal_depth: 3,
            kg_traversal_depth_ceiling: 5,
            kg_edge_candidate_limit: 50,
            kg_traversal_edge_limit: 500,
            kg_traversal_node_budget: 1000,
            api_provider: "anthropic".into(),
            gemini_model: None,
            anthropic_model: None,
            anthropic_effort: None,
            gemini_effort: None,
            embedding_enabled: None,
            embedding_model: None,
            image_provider: None,
            image_model: None,
            git_tokens: None,
            guardian_secrets: None,
            guardian_private_hosts: None,
            anthropic_api_key: None,
            gemini_api_key: None,
            max_tool_iterations: None,
            show_reasoning: None,
            provider_gateway: None,
            openai_compat: crate::config::OpenAiCompatConfig::default(),
        }
    }
}

#[cfg(test)]
mod max_tool_iterations_serde_tests {
    use super::SystemConfig;
    use serde_json::json;

    fn minimal_cfg() -> SystemConfig {
        super::tests_support::minimal_cfg()
    }

    #[test]
    fn none_serializes_without_field() {
        let cfg = minimal_cfg();
        let json = serde_json::to_value(&cfg).unwrap();
        assert!(
            json.get("max_tool_iterations").is_none(),
            "None should serialize as absent (skip_serializing_if): {json:?}"
        );
    }

    #[test]
    fn some_serializes_value() {
        let mut cfg = minimal_cfg();
        cfg.max_tool_iterations = Some(250);
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json.get("max_tool_iterations"), Some(&json!(250)));
    }

    #[test]
    fn missing_field_deserializes_as_none() {
        // Pre-existing config docs in production WardSONDB lack the field
        // entirely. Serde must accept that and yield None (no migration
        // needed). Round-trip a minimal doc shape:
        let doc = json!({
            "name": "Embra",
            "api_key": "k",
            "timezone": "UTC",
            "deployment_mode": "phase1",
            "created_at": "",
            "version": "test",
            "kg_temporal_window_secs": 1800,
            "kg_max_traversal_depth": 3,
            "kg_traversal_depth_ceiling": 5,
            "kg_edge_candidate_limit": 50,
            "api_provider": "anthropic",
        });
        let cfg: SystemConfig = serde_json::from_value(doc).unwrap();
        assert!(cfg.max_tool_iterations.is_none());
    }

    #[test]
    fn image_config_fields_are_serde_additive() {
        // Media wave part 2: absent on the wire when None (pre-wave config
        // docs byte-identical), round-trip when set, absent → None.
        let cfg = minimal_cfg();
        let json = serde_json::to_value(&cfg).unwrap();
        assert!(json.get("image_provider").is_none());
        assert!(json.get("image_model").is_none());
        let mut cfg = minimal_cfg();
        cfg.image_provider = Some("gemini".into());
        cfg.image_model = Some("gemini-3-pro-image".into());
        let json = serde_json::to_value(&cfg).unwrap();
        let back: SystemConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.image_provider.as_deref(), Some("gemini"));
        assert_eq!(back.image_model.as_deref(), Some("gemini-3-pro-image"));
    }

    #[test]
    fn provider_gateway_is_serde_additive_and_absent_when_none() {
        // Absent on the wire when None, so a stored document keeps its
        // bytes; a round trip keeps the URL and the switch; a document that
        // carries only the URL reads as stored and off.
        let json = serde_json::to_value(minimal_cfg()).unwrap();
        assert!(json.get("provider_gateway").is_none());
        let mut cfg = minimal_cfg();
        cfg.provider_gateway = Some(super::ProviderGateway {
            url: "http://gw:4000".into(),
            enabled: true,
        });
        let json = serde_json::to_value(&cfg).unwrap();
        let back: SystemConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.provider_gateway, cfg.provider_gateway);
        let mut json = serde_json::to_value(minimal_cfg()).unwrap();
        json["provider_gateway"] = serde_json::json!({"url": "http://gw:4000"});
        let back: SystemConfig = serde_json::from_value(json).unwrap();
        assert_eq!(
            back.provider_gateway,
            Some(super::ProviderGateway {
                url: "http://gw:4000".into(),
                enabled: false,
            })
        );
    }

    #[test]
    fn explicit_value_round_trips() {
        let mut cfg = minimal_cfg();
        cfg.max_tool_iterations = Some(42);
        let json = serde_json::to_value(&cfg).unwrap();
        let back: SystemConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.max_tool_iterations, Some(42));
    }
}

#[cfg(test)]
mod gateway_key_check_tests {
    //! The key check after boot goes where the next turn would.
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn check_api_key_asks_the_base_the_gateway_rule_gives() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/anthropic/v1/models"))
            .and(header("x-api-key", "sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/gemini/v1beta/models"))
            .and(header("x-goog-api-key", "g-test"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;
        let gateway = ProviderGateway {
            url: server.uri(),
            enabled: true,
        };
        assert!(check_api_key(ProviderKind::Anthropic, "sk-test", Some(&gateway)).await.is_ok());
        assert!(check_api_key(ProviderKind::Gemini, "g-test", Some(&gateway)).await.is_ok());
        // Off, the same check goes to the provider's own host: a key the
        // mock never saw is not reported valid by it.
        let off = ProviderGateway {
            url: server.uri(),
            enabled: false,
        };
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        assert!(check_api_key(ProviderKind::Anthropic, "", Some(&off)).await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}

#[cfg(test)]
mod kg_traversal_config_tests {
    //! FIX-7 traversal knobs (locked D3): serde-additive concrete fields,
    //! same pattern as the existing kg_* block — pre-existing config docs
    //! lack them and must deserialize to the defaults (no schema bump).
    use super::*;
    use serde_json::json;

    fn minimal_doc() -> serde_json::Value {
        // Same minimal doc shape as production pre-fix config docs.
        json!({
            "name": "Embra",
            "api_key": "k",
            "timezone": "UTC",
            "deployment_mode": "phase1",
            "created_at": "",
            "version": "test",
            "kg_temporal_window_secs": 1800,
            "kg_max_traversal_depth": 3,
            "kg_traversal_depth_ceiling": 5,
            "kg_edge_candidate_limit": 50,
            "api_provider": "anthropic",
        })
    }

    #[test]
    fn kg_traversal_fields_default_when_missing() {
        let cfg: SystemConfig = serde_json::from_value(minimal_doc()).unwrap();
        assert_eq!(cfg.kg_traversal_edge_limit, 500);
        assert_eq!(cfg.kg_traversal_node_budget, 1000);
    }

    #[test]
    fn kg_traversal_fields_round_trip() {
        let mut cfg: SystemConfig = serde_json::from_value(minimal_doc()).unwrap();
        cfg.kg_traversal_edge_limit = 750;
        cfg.kg_traversal_node_budget = 2500;
        let json = serde_json::to_value(&cfg).unwrap();
        let back: SystemConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.kg_traversal_edge_limit, 750);
        assert_eq!(back.kg_traversal_node_budget, 2500);
    }
}

#[cfg(test)]
mod provider_label_tests {
    //! Wizard provider-selection labels → (ProviderKind, anthropic_model).
    //! All Anthropic entries resolve to the Anthropic provider and
    //! each seeds `anthropic_model` EXPLICITLY (a wizard choice must keep
    //! meaning what the operator picked even if `DEFAULT_MODEL` moves —
    //! it did, 4.7 → 4.8 → 5). The selector itself is server-driven, so
    //! the console and chat-mobile UIs render whichever labels appear here.
    use super::{
        anthropic_model_from_label, gemini_model_from_label, provider_from_label,
        ANTHROPIC_WIZARD_LABELS, GEMINI_WIZARD_LABELS,
        PROVIDER_ANTHROPIC_FABLE51_LABEL, PROVIDER_ANTHROPIC_FABLE_LABEL,
        PROVIDER_ANTHROPIC_OPUS48_LABEL, PROVIDER_ANTHROPIC_OPUS55_LABEL,
        PROVIDER_ANTHROPIC_OPUS5_LABEL, PROVIDER_GEMINI_ARGON_LABEL, PROVIDER_GEMINI_LABEL,
        PROVIDER_WIZARD_LABELS,
    };
    use crate::provider::ProviderKind;

    /// The console pre-selects index 0 and ignores `default_value`, so the
    /// first label is what an operator gets by pressing Enter. It must
    /// name the provider's default model.
    #[test]
    fn first_wizard_label_is_the_default_model() {
        let first = PROVIDER_WIZARD_LABELS[0];
        assert_eq!(provider_from_label(first), ProviderKind::Anthropic);
        assert_eq!(
            anthropic_model_from_label(first).as_deref(),
            Some(crate::provider::anthropic::DEFAULT_DISPLAY_NAME)
        );
    }

    /// `provider_from_label` has a catch-all, so an Anthropic label with
    /// no arm in `anthropic_model_from_label` would persist `None` and run
    /// the default model under another model's name.
    #[test]
    fn every_anthropic_wizard_label_seeds_a_model() {
        for label in PROVIDER_WIZARD_LABELS {
            match provider_from_label(label) {
                ProviderKind::Anthropic => assert!(
                    anthropic_model_from_label(label).is_some(),
                    "'{label}' seeds no model"
                ),
                _ => assert_eq!(anthropic_model_from_label(label), None, "{label}"),
            }
        }
    }

    /// Like the Anthropic labels: a Gemini label without a seeded model
    /// would persist `None` and run the default under another model's
    /// name.
    #[test]
    fn every_gemini_wizard_label_seeds_a_model() {
        for label in PROVIDER_WIZARD_LABELS {
            match provider_from_label(label) {
                ProviderKind::Gemini => assert!(
                    gemini_model_from_label(label).is_some(),
                    "'{label}' seeds no model"
                ),
                _ => assert_eq!(gemini_model_from_label(label), None, "{label}"),
            }
        }
    }

    #[test]
    fn gemini_labels_seed_expected_models() {
        assert_eq!(
            gemini_model_from_label(PROVIDER_GEMINI_LABEL).as_deref(),
            Some("gemini-3.1-pro")
        );
        assert_eq!(
            gemini_model_from_label(PROVIDER_GEMINI_ARGON_LABEL).as_deref(),
            Some("gemini-4-argon")
        );
    }

    /// The setup flow's model step offers the per-provider lists; they
    /// must stay the wizard's cloud labels, in its order.
    #[test]
    fn every_wizard_label_is_in_its_providers_setup_list() {
        let cloud: Vec<&str> = PROVIDER_WIZARD_LABELS
            .iter()
            .copied()
            .filter(|l| matches!(provider_from_label(l), ProviderKind::Anthropic | ProviderKind::Gemini))
            .collect();
        let lists: Vec<&str> = ANTHROPIC_WIZARD_LABELS
            .iter()
            .chain(GEMINI_WIZARD_LABELS.iter())
            .copied()
            .collect();
        assert_eq!(cloud, lists);
        for label in ANTHROPIC_WIZARD_LABELS {
            assert_eq!(provider_from_label(label), ProviderKind::Anthropic, "{label}");
        }
        for label in GEMINI_WIZARD_LABELS {
            assert_eq!(provider_from_label(label), ProviderKind::Gemini, "{label}");
        }
    }

    #[test]
    fn all_anthropic_labels_map_to_anthropic() {
        for label in [
            PROVIDER_ANTHROPIC_OPUS5_LABEL,
            PROVIDER_ANTHROPIC_OPUS55_LABEL,
            PROVIDER_ANTHROPIC_OPUS48_LABEL,
            PROVIDER_ANTHROPIC_FABLE51_LABEL,
            PROVIDER_ANTHROPIC_FABLE_LABEL,
        ] {
            assert_eq!(provider_from_label(label), ProviderKind::Anthropic);
        }
    }

    #[test]
    fn anthropic_labels_seed_expected_models() {
        assert_eq!(
            anthropic_model_from_label(PROVIDER_ANTHROPIC_OPUS5_LABEL),
            Some("opus-5".to_string())
        );
        assert_eq!(
            anthropic_model_from_label(PROVIDER_ANTHROPIC_OPUS48_LABEL),
            Some("opus-4.8".to_string())
        );
        assert_eq!(
            anthropic_model_from_label(PROVIDER_ANTHROPIC_FABLE_LABEL),
            Some("fable-5".to_string())
        );
        assert_eq!(
            anthropic_model_from_label(PROVIDER_ANTHROPIC_OPUS55_LABEL),
            Some("opus-5.5".to_string())
        );
        assert_eq!(
            anthropic_model_from_label(PROVIDER_ANTHROPIC_FABLE51_LABEL),
            Some("fable-5.1".to_string())
        );
        assert_eq!(anthropic_model_from_label(PROVIDER_GEMINI_LABEL), None);
    }
}

#[cfg(test)]
mod intelligence_name_tests {
    //! Shared validator behind the `set_name` tool and the Phase-2
    //! identity→config sync. Display-driven bounds — see the fn doc.
    use super::validate_intelligence_name;

    #[test]
    fn accepts_and_trims_reasonable_names() {
        assert_eq!(validate_intelligence_name("Embra"), Ok("Embra".to_string()));
        assert_eq!(
            validate_intelligence_name("  Ada Lovelace  "),
            Ok("Ada Lovelace".to_string())
        );
        // Unicode is fine — the bound is char count, not bytes.
        assert_eq!(validate_intelligence_name("Émbra"), Ok("Émbra".to_string()));
    }

    #[test]
    fn rejects_empty_overlong_and_control() {
        assert!(validate_intelligence_name("").is_err());
        assert!(validate_intelligence_name("   ").is_err());
        assert!(validate_intelligence_name(&"x".repeat(41)).is_err());
        assert!(validate_intelligence_name("two\nlines").is_err());
        assert!(validate_intelligence_name("tab\there").is_err());
        // 40 chars exactly is allowed.
        assert!(validate_intelligence_name(&"x".repeat(40)).is_ok());
    }

    #[test]
    fn rejects_the_mode_transition_separator() {
        // "Name: X — Session: …" is parsed by splitting on " — "; a name
        // containing the sequence would truncate at the console parser.
        assert!(validate_intelligence_name("Embra — the second").is_err());
        // A plain em-dash without the padded sequence is fine.
        assert!(validate_intelligence_name("Embra—2").is_ok());
    }
}

#[cfg(test)]
mod anthropic_effort_serde_tests {
    //! `/effort` config field: serde-additive `Option<String>`, same
    //! pattern as `max_tool_iterations` — pre-existing config docs lack
    //! it and must deserialize to `None` (no schema bump).
    use super::{OpenAiCompatConfig, SystemConfig};
    use serde_json::json;

    #[test]
    fn anthropic_effort_absent_from_json_when_none() {
        let doc = json!({
            "name": "Embra",
            "api_key": "k",
            "timezone": "UTC",
            "deployment_mode": "phase1",
            "created_at": "",
            "version": "test",
            "kg_temporal_window_secs": 1800,
            "kg_max_traversal_depth": 3,
            "kg_traversal_depth_ceiling": 5,
            "kg_edge_candidate_limit": 50,
            "api_provider": "anthropic",
        });
        let cfg: SystemConfig = serde_json::from_value(doc).unwrap();
        assert!(cfg.anthropic_effort.is_none());

        let json = serde_json::to_value(&cfg).unwrap();
        assert!(
            json.get("anthropic_effort").is_none(),
            "None should serialize as absent (skip_serializing_if): {json:?}"
        );

        let mut cfg = cfg;
        cfg.anthropic_effort = Some("high".into());
        let json = serde_json::to_value(&cfg).unwrap();
        let back: SystemConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.anthropic_effort.as_deref(), Some("high"));
    }

    #[test]
    fn provider_effort_fields_absent_from_json_when_none() {
        // gemini_effort + the two OpenAiCompatConfig overrides follow the
        // same serde-additive contract: absent → None, None → absent.
        let doc = json!({
            "name": "Embra",
            "api_key": "k",
            "timezone": "UTC",
            "deployment_mode": "phase1",
            "created_at": "",
            "version": "test",
            "kg_temporal_window_secs": 1800,
            "kg_max_traversal_depth": 3,
            "kg_traversal_depth_ceiling": 5,
            "kg_edge_candidate_limit": 50,
            "api_provider": "lm_studio",
            "openai_compat": {"lm_studio_endpoint": "http://localhost:1234", "lm_studio_model": "qwen/qwen3.8-27b"},
        });
        let cfg: SystemConfig = serde_json::from_value(doc).unwrap();
        assert!(cfg.gemini_effort.is_none());
        assert!(cfg.openai_compat.ollama_effort.is_none());
        assert!(cfg.openai_compat.lm_studio_effort.is_none());

        let json = serde_json::to_value(&cfg).unwrap();
        assert!(json.get("gemini_effort").is_none(), "{json:?}");
        let oc = json.get("openai_compat").unwrap();
        assert!(oc.get("ollama_effort").is_none(), "{oc:?}");
        assert!(oc.get("lm_studio_effort").is_none(), "{oc:?}");
        assert!(oc.get("ollama_effort_by_model").is_none(), "{oc:?}");
        assert!(oc.get("lm_studio_effort_by_model").is_none(), "{oc:?}");
        // The pre-existing String leaves keep serializing (shape stable).
        assert_eq!(oc.get("ollama_endpoint").and_then(|v| v.as_str()), Some(""));

        let mut cfg = cfg;
        cfg.gemini_effort = Some("low".into());
        cfg.openai_compat.lm_studio_effort = Some("xhigh".into());
        let back: SystemConfig = serde_json::from_value(serde_json::to_value(&cfg).unwrap()).unwrap();
        assert_eq!(back.gemini_effort.as_deref(), Some("low"));
        assert_eq!(back.openai_compat.lm_studio_effort.as_deref(), Some("xhigh"));
        assert_eq!(
            back.openai_compat.effort_for_preset(crate::provider::openai_compat::OpenAiCompatPreset::LmStudio),
            Some("xhigh")
        );
        assert!(back.openai_compat.ollama_effort.is_none());

        // A per-model level round-trips under its model.
        let mut cfg = back;
        cfg.openai_compat.ollama_effort_by_model.insert("gpt-oss:20b".into(), "low".into());
        let back: SystemConfig = serde_json::from_value(serde_json::to_value(&cfg).unwrap()).unwrap();
        assert_eq!(
            back.openai_compat.ollama_effort_by_model.get("gpt-oss:20b").map(String::as_str),
            Some("low")
        );
    }

    #[test]
    fn a_level_is_kept_per_model_and_a_switch_brings_back_each_ones_own() {
        use crate::provider::openai_compat::OpenAiCompatPreset::{LmStudio, Ollama};
        let mut oc = OpenAiCompatConfig::default();
        oc.set_model(Ollama, "qwen3.8:27b-mxfp8");
        oc.set_effort(Ollama, Some("low".into()));
        assert_eq!(oc.effort_for_preset(Ollama), Some("low"));
        // The switch leaves Qwen3.8's level behind: V4 Pro gets its auto-map.
        oc.set_model(Ollama, "deepseek-v4-pro:cloud");
        assert_eq!(oc.effort_for_preset(Ollama), None);
        oc.set_effort(Ollama, Some("high".into()));
        oc.set_model(Ollama, "qwen3.8:27b-mxfp8");
        assert_eq!(oc.effort_for_preset(Ollama), Some("low"));
        oc.set_model(Ollama, "deepseek-v4-pro:cloud");
        assert_eq!(oc.effort_for_preset(Ollama), Some("high"));
        // LM Studio keeps its own.
        assert_eq!(oc.effort_for_preset(LmStudio), None);
    }

    #[test]
    fn a_level_stored_the_old_way_stays_with_the_model_it_was_set_for() {
        use crate::provider::openai_compat::OpenAiCompatPreset::Ollama;
        // An instance from before levels were kept per model.
        let mut oc = OpenAiCompatConfig {
            ollama_model: "qwen3.8:27b-mxfp8".into(),
            ollama_effort: Some("low".into()),
            ..Default::default()
        };
        assert_eq!(oc.effort_for_preset(Ollama), Some("low"));
        oc.set_model(Ollama, "deepseek-v4-pro:cloud");
        assert_eq!(oc.effort_for_preset(Ollama), None);
        assert!(oc.ollama_effort.is_none());
        oc.set_model(Ollama, "qwen3.8:27b-mxfp8");
        assert_eq!(oc.effort_for_preset(Ollama), Some("low"));
    }

    #[test]
    fn effort_reset_clears_the_active_models_level_only() {
        use crate::provider::openai_compat::OpenAiCompatPreset::Ollama;
        let mut oc = OpenAiCompatConfig::default();
        oc.set_model(Ollama, "a");
        oc.set_effort(Ollama, Some("low".into()));
        oc.set_model(Ollama, "b");
        oc.set_effort(Ollama, Some("high".into()));
        oc.set_effort(Ollama, None);
        assert_eq!(oc.effort_for_preset(Ollama), None);
        oc.set_model(Ollama, "a");
        assert_eq!(oc.effort_for_preset(Ollama), Some("low"));
    }

    #[test]
    fn a_level_set_before_any_model_applies_to_the_first_one_chosen() {
        use crate::provider::openai_compat::OpenAiCompatPreset::Ollama;
        let mut oc = OpenAiCompatConfig::default();
        oc.set_effort(Ollama, Some("medium".into()));
        oc.set_model(Ollama, "qwen3.8:27b-mxfp8");
        assert_eq!(oc.effort_for_preset(Ollama), Some("medium"));
        // The next switch files it under that model.
        oc.set_model(Ollama, "glm-5.3:cloud");
        assert_eq!(oc.effort_for_preset(Ollama), None);
        oc.set_model(Ollama, "qwen3.8:27b-mxfp8");
        assert_eq!(oc.effort_for_preset(Ollama), Some("medium"));
    }

    #[test]
    fn guardian_secrets_and_private_hosts_are_serde_additive() {
        // Absent on an old document; present and round-tripping when set.
        let old = json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
            "created_at": "", "version": "test", "kg_temporal_window_secs": 1800,
            "kg_max_traversal_depth": 3, "kg_traversal_depth_ceiling": 5, "kg_edge_candidate_limit": 50,
        });
        let cfg: SystemConfig = serde_json::from_value(old).unwrap();
        assert!(cfg.guardian_secrets.is_none() && cfg.guardian_private_hosts.is_none());
        let text = serde_json::to_string(&cfg).unwrap();
        assert!(!text.contains("guardian_secrets") && !text.contains("guardian_private_hosts"));
        let mut cfg = cfg;
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("authorization".to_string(), "Bearer t".to_string());
        let mut secrets = std::collections::BTreeMap::new();
        secrets.insert("gitlab.ops.wsds".to_string(), headers);
        cfg.guardian_secrets = Some(secrets);
        cfg.guardian_private_hosts = Some(vec!["gitlab.ops.wsds".into()]);
        let back: SystemConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.guardian_secrets.unwrap()["gitlab.ops.wsds"]["authorization"], "Bearer t");
        assert_eq!(back.guardian_private_hosts.unwrap(), vec!["gitlab.ops.wsds".to_string()]);
    }

    #[test]
    fn git_tokens_serde_additive_round_trip() {
        // `/git-token` config field: same additive-Option pattern.
        let doc = json!({
            "name": "Embra",
            "api_key": "k",
            "timezone": "UTC",
            "deployment_mode": "phase1",
            "created_at": "",
            "version": "test",
            "kg_temporal_window_secs": 1800,
            "kg_max_traversal_depth": 3,
            "kg_traversal_depth_ceiling": 5,
            "kg_edge_candidate_limit": 50,
            "api_provider": "anthropic",
        });
        let cfg: SystemConfig = serde_json::from_value(doc).unwrap();
        assert!(cfg.git_tokens.is_none());

        let json = serde_json::to_value(&cfg).unwrap();
        assert!(
            json.get("git_tokens").is_none(),
            "None should serialize as absent (skip_serializing_if): {json:?}"
        );

        let mut cfg = cfg;
        let mut tokens = std::collections::BTreeMap::new();
        tokens.insert("gitlab.example.com".to_string(), "glpat-x".to_string());
        cfg.git_tokens = Some(tokens);
        let json = serde_json::to_value(&cfg).unwrap();
        let back: SystemConfig = serde_json::from_value(json).unwrap();
        assert_eq!(
            back.git_tokens.unwrap().get("gitlab.example.com").map(|s| s.as_str()),
            Some("glpat-x")
        );
    }
}

#[cfg(test)]
mod redact_tests {
    use super::{key_summary, redact};

    #[test]
    fn redact_replaces_the_credential_fields_and_keeps_the_rest() {
        let doc = serde_json::json!({
            "name": "Embra", "api_provider": "gemini", "api_key": "sk-0123456789",
            "anthropic_api_key": "", "gemini_api_key": "AIza0123456789", "github_token": "ghp_0123456789",
            "git_tokens": {"a.example": "glpat-1", "b.example": "glpat-2"},
            "guardian_secrets": {"h.example": {"Authorization": "Bearer t", "X-Key": "k"}},
            "kg_temporal_window_secs": 1800,
        });
        let out = redact(doc);
        assert_eq!(out["name"], "Embra");
        assert_eq!(out["api_provider"], "gemini");
        assert_eq!(out["kg_temporal_window_secs"], 1800);
        assert_eq!(out["api_key"], "[redacted]");
        assert_eq!(out["anthropic_api_key"], "", "an empty field hides nothing and stays empty");
        assert_eq!(out["gemini_api_key"], "[redacted]");
        assert_eq!(out["github_token"], "[redacted]");
        assert_eq!(out["git_tokens"]["a.example"], "[redacted]");
        assert_eq!(out["git_tokens"]["b.example"], "[redacted]");
        assert_eq!(out["guardian_secrets"]["h.example"]["Authorization"], "[redacted]");
        assert_eq!(out["guardian_secrets"]["h.example"]["X-Key"], "[redacted]");
        let text = out.to_string();
        for secret in ["sk-0123", "AIza0123", "ghp_0123", "glpat-", "Bearer t", "\"k\""] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }

    #[test]
    fn the_key_summary_never_cuts_a_character() {
        assert_eq!(key_summary("sk-ant-api03-abcd"), "17 characters, ending …abcd");
        assert_eq!(key_summary("ключ"), "4 characters, ending …ключ");
        assert_eq!(key_summary("é"), "1 characters, ending …é");
        assert_eq!(key_summary(""), "0 characters, ending …");
    }
}

#[cfg(test)]
mod save_config_tests {
    use super::{save_config, tests_support::minimal_cfg};
    use crate::db::WardsonDbClient;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The fall-through to `update` is for a 409 alone; any other write
    /// failure is returned as itself, and no update is attempted (#61).
    #[tokio::test]
    async fn a_conflict_updates_and_any_other_write_failure_is_returned() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/config.system")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/config.system/docs"))
            .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/config.system/docs/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .expect(0)
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());
        let err = save_config(&db, &minimal_cfg()).await.unwrap_err();
        assert!(err.to_string().contains("503"), "{err}");
        server.verify().await;

        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/config.system")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/config.system/docs"))
            .respond_with(ResponseTemplate::new(409).set_body_string("DOCUMENT_CONFLICT"))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/config.system/docs/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());
        save_config(&db, &minimal_cfg()).await.unwrap();
        server.verify().await;
    }

    /// F-7: the document written carries the running build whatever the
    /// struct says (`minimal_cfg` says `"test"`), so no caller persists a
    /// stale string.
    #[tokio::test]
    async fn a_save_stamps_the_running_build_into_version() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/config.system")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/config.system/docs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "data": {"_id": "config"}, "meta": {}})))
            .mount(&server)
            .await;
        save_config(&WardsonDbClient::from_url(&server.uri()), &minimal_cfg()).await.unwrap();
        let written = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|r| !r.body.is_empty())
            .expect("one document was written");
        let body: serde_json::Value = serde_json::from_slice(&written.body).unwrap();
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["_id"], "config");
    }
}

#[cfg(test)]
mod load_config_tests {
    use super::{load_config, tests_support::minimal_cfg};
    use crate::db::WardsonDbClient;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// F-7: a stored config whose `version` predates the running build is
    /// refreshed to `CARGO_PKG_VERSION` on load, so no reader sees a version
    /// that disagrees with the binary it configures.
    #[tokio::test]
    async fn load_refreshes_a_stale_version_to_the_running_build() {
        let mut doc = serde_json::to_value(minimal_cfg()).unwrap();
        doc["version"] = serde_json::json!("0.5.0-phase1");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/config.system/docs/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "data": doc, "meta": {}})))
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());
        let cfg = load_config(&db).await.unwrap();
        assert_eq!(cfg.version, env!("CARGO_PKG_VERSION"));
    }
}

#[cfg(test)]
mod wizard_channel_tests {
    use super::run_config_wizard_grpc;
    use crate::db::WardsonDbClient;
    use tokio::sync::mpsc;

    /// #61: a client that disconnects during setup ends the wizard with an
    /// error naming the step it was on; it neither defaults its way through
    /// the steps nor spins on a closed channel. Nothing before the first
    /// answer reaches the database, so the address is never dialed.
    #[tokio::test]
    async fn a_dropped_receiver_ends_the_wizard_at_the_step_it_was_on() {
        let (tx, _prompts) = mpsc::channel(8);
        let (answers, mut response_rx) = mpsc::channel::<String>(8);
        drop(answers);
        let db = WardsonDbClient::from_url("http://127.0.0.1:9");
        let err = run_config_wizard_grpc(&tx, &mut response_rx, &db).await.unwrap_err().to_string();
        assert!(err.contains("setup channel closed during the name step"), "{err}");
    }

    /// The same past the name and the provider steps: a client gone at the
    /// API-key step ends the wizard naming that step (the verification of
    /// 2026-10-24 found the guard above exercised the name step alone).
    #[tokio::test]
    async fn a_dropped_receiver_ends_the_wizard_at_the_api_key_step() {
        let (tx, _prompts) = mpsc::channel(8);
        let (answers, mut response_rx) = mpsc::channel::<String>(8);
        answers.send("Embra".to_string()).await.unwrap();
        answers.send(super::PROVIDER_WIZARD_LABELS[0].to_string()).await.unwrap();
        drop(answers);
        let db = WardsonDbClient::from_url("http://127.0.0.1:9");
        let err = run_config_wizard_grpc(&tx, &mut response_rx, &db).await.unwrap_err().to_string();
        assert!(err.contains("setup channel closed during the API key step"), "{err}");
    }
}
