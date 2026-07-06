use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use tokio::sync::mpsc;
use tonic::Status;
use tracing::info;

use crate::db::WardsonDbClient;
use crate::provider::anthropic::AnthropicProvider;
use crate::provider::gemini::GeminiProvider;
use crate::provider::{LlmProvider, ProviderKind, ValidationResult};
use embra_common::proto::brain::*;
use embra_common::proto::common;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    pub name: String,
    pub api_key: String,
    pub timezone: String,
    pub deployment_mode: String,
    pub created_at: String,
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
    /// Optional override for the Gemini model id. Defaults to
    /// `gemini-3.1-pro-preview`. Operators set this to
    /// `gemini-3.1-pro-preview-customtools` (per spec D8) when the
    /// standard model is observed ignoring custom tools in favor of
    /// bash invocations. The brain also honors `EMBRA_GEMINI_MODEL`
    /// env var which takes precedence over this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini_model: Option<String>,
    /// Active Anthropic model alias (e.g. `"opus-4.8"`, `"opus-4.7"`,
    /// `"fable-5"`). `None` (additive default) means the provider's
    /// `DEFAULT_MODEL` (`claude-opus-4-8`). The brain also honors the
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
    /// Effective `show_reasoning` decision: default-on when unset,
    /// otherwise honor the operator's explicit setting. Mirrors the
    /// `max_tool_iterations` precedent.
    pub fn show_reasoning(&self) -> bool {
        self.show_reasoning.unwrap_or(true)
    }
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

fn default_kg_temporal_window() -> u64 { 1800 }
fn default_kg_max_depth() -> u32 { 3 }
fn default_kg_depth_ceiling() -> u32 { 5 }
fn default_kg_candidate_limit() -> u32 { 50 }
fn default_kg_traversal_edge_limit() -> u32 { 500 }
fn default_kg_traversal_node_budget() -> u32 { 1000 }
fn default_api_provider() -> String { "anthropic".to_string() }

const PROVIDER_ANTHROPIC_LABEL: &str = "Anthropic Claude Opus 4.7";
const PROVIDER_ANTHROPIC_48_LABEL: &str = "Anthropic Claude Opus 4.8";
const PROVIDER_ANTHROPIC_FABLE_LABEL: &str = "Anthropic Claude Fable 5";
const PROVIDER_GEMINI_LABEL: &str = "Google Gemini 3.1 Pro";
const PROVIDER_OLLAMA_LABEL: &str = "Ollama (OpenAI-compat)";
const PROVIDER_LM_STUDIO_LABEL: &str = "LM Studio (OpenAI-compat)";

fn provider_from_label(label: &str) -> ProviderKind {
    match label {
        PROVIDER_GEMINI_LABEL => ProviderKind::Gemini,
        PROVIDER_OLLAMA_LABEL => ProviderKind::Ollama,
        PROVIDER_LM_STUDIO_LABEL => ProviderKind::LmStudio,
        // All Anthropic labels (Opus 4.7 / 4.8, Fable 5) map here; the
        // chosen model is captured separately by `anthropic_model_from_label`.
        _ => ProviderKind::Anthropic,
    }
}

/// Which Anthropic model a provider-selection label implies, as the value
/// persisted to `SystemConfig.anthropic_model`. Every Anthropic label seeds
/// its model EXPLICITLY (never `None` = "ride the default") so a wizard
/// choice keeps meaning what the operator picked even if the provider's
/// `DEFAULT_MODEL` changes in a later build. Non-Anthropic labels yield
/// `None`. Later switchable at runtime via `/model`.
fn anthropic_model_from_label(label: &str) -> Option<String> {
    match label {
        PROVIDER_ANTHROPIC_LABEL => Some("opus-4.7".to_string()),
        PROVIDER_ANTHROPIC_48_LABEL => Some("opus-4.8".to_string()),
        PROVIDER_ANTHROPIC_FABLE_LABEL => Some("fable-5".to_string()),
        _ => None,
    }
}

pub async fn run_config_wizard() -> Result<SystemConfig> {
    println!();
    println!("╔══════════════════════════════════════════╗");
    println!("║     embraOS Phase 1 — First Run Setup    ║");
    println!("╚══════════════════════════════════════════╝");
    println!();

    // Name
    let name = prompt_with_default(
        "What would you like to name your intelligence?",
        "Embra",
    )?;

    // API Key
    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.is_empty() => {
            println!("✓ Anthropic API key detected from environment.");
            key
        }
        _ => {
            let key = prompt_required("Enter your Anthropic API key:")?;
            if !key.starts_with("sk-") {
                println!("⚠ Warning: API key doesn't start with 'sk-'. Proceeding anyway.");
            }
            key
        }
    };

    // Timezone
    let tz_default = detect_timezone();
    let timezone = prompt_with_default(
        &format!("Timezone (detected: {}):", tz_default),
        &tz_default,
    )?;

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
        api_provider: default_api_provider(),
        gemini_model: None,
        anthropic_model: None,
        anthropic_effort: None,
        anthropic_api_key: None,
        gemini_api_key: None,
        max_tool_iterations: None,
        show_reasoning: None,
        openai_compat: crate::config::OpenAiCompatConfig::default(),
    };

    println!();
    println!("Configuration complete:");
    println!("  Name: {}", config.name);
    println!("  Timezone: {}", config.timezone);
    println!("  Mode: {}", config.deployment_mode);
    println!("  Version: {}", config.version);
    println!();

    Ok(config)
}

pub async fn save_config(db: &WardsonDbClient, config: &SystemConfig) -> Result<()> {
    if !db.collection_exists("config.system").await? {
        db.create_collection("config.system").await?;
    }
    let mut doc = serde_json::to_value(config)?;
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("_id".into(), serde_json::json!("config"));
    }
    match db.write("config.system", &doc).await {
        Ok(_) => Ok(()),
        Err(_) => {
            // 409 conflict means doc already exists — update instead
            db.update("config.system", "config", &doc).await?;
            Ok(())
        }
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
                .query("config.system", &serde_json::json!({}))
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
    Ok(config)
}

fn prompt_with_default(prompt: &str, default: &str) -> Result<String> {
    print!("{} [{}] ", prompt, default);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn prompt_required(prompt: &str) -> Result<String> {
    loop {
        print!("{} ", prompt);
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let trimmed = input.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
        println!("This field is required.");
    }
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
) -> Result<(), String> {
    match validate_api_key_for(provider, key).await {
        ApiKeyCheck::Valid => Ok(()),
        ApiKeyCheck::Invalid(msg) => Err(msg),
    }
}

/// Probe the chosen provider's model-listing endpoint with the given
/// key. Single source of truth — the providers' own `validate_key`
/// impls (constructed with an empty key for the probe) carry the
/// HTTP shape and status mapping.
async fn validate_api_key_for(provider: ProviderKind, key: &str) -> ApiKeyCheck {
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
            AnthropicProvider::new(String::new()).validate_key(key).await
        }
        ProviderKind::Gemini => GeminiProvider::new(String::new()).validate_key(key).await,
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
             Verify at aistudio.google.com — gemini-3.1-pro-preview requires a billed account."
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
        _ => "Embra".to_string(),
    };
    info!("Config wizard: name = {}", name);

    // Step 2: Provider selection (Sprint 4 → Sprint 5 4-way, +Fable 5) —
    // Selector UI. Default tracks the provider's DEFAULT_MODEL (Opus 4.8).
    let _ = tx.send(Ok(ConversationResponse {
        response_type: Some(conversation_response::ResponseType::Setup(
            SetupPrompt {
                field_type: SetupFieldType::Selector as i32,
                prompt: "Which AI provider would you like to use?".to_string(),
                options: vec![
                    PROVIDER_ANTHROPIC_LABEL.to_string(),
                    PROVIDER_ANTHROPIC_48_LABEL.to_string(),
                    PROVIDER_ANTHROPIC_FABLE_LABEL.to_string(),
                    PROVIDER_GEMINI_LABEL.to_string(),
                    PROVIDER_OLLAMA_LABEL.to_string(),
                    PROVIDER_LM_STUDIO_LABEL.to_string(),
                ],
                default_value: PROVIDER_ANTHROPIC_48_LABEL.to_string(),
            }
        )),
    })).await;
    let provider_choice = match response_rx.recv().await {
        Some(input) if !input.is_empty() => input,
        _ => PROVIDER_ANTHROPIC_48_LABEL.to_string(),
    };
    let provider_kind = provider_from_label(&provider_choice);
    // Capture the chosen Anthropic model (Opus 4.7/4.8, Fable 5) for the
    // Anthropic path; persisted below as `SystemConfig.anthropic_model`.
    let anthropic_model = anthropic_model_from_label(&provider_choice);
    info!(
        "Config wizard: provider = {}{}",
        provider_kind.as_str(),
        anthropic_model
            .as_deref()
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
                    (response_rx.recv().await.unwrap_or_default(), false)
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
            match validate_api_key_for(provider_kind, &candidate).await {
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
        let input = response_rx.recv().await.unwrap_or_default();
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
        "Configuration summary:\n  Name: {}\n  Provider: {}\n  API Key: {}...\n  Timezone: {}\n\nConfirm?",
        name,
        provider_choice,
        &api_key[..std::cmp::min(10, api_key.len())],
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

    let confirm = response_rx.recv().await.unwrap_or_default().to_lowercase();
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
                openai_compat.ollama_model = setup.model_id.clone();
            }
            OpenAiCompatPreset::LmStudio => {
                openai_compat.lm_studio_endpoint = setup.endpoint.clone();
                openai_compat.lm_studio_model = setup.model_id.clone();
            }
        }
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
        gemini_model: None,
        anthropic_model,
        anthropic_effort: None,
        anthropic_api_key,
        gemini_api_key,
        max_tool_iterations: None,
        show_reasoning: None,
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
    // refreshes from its default ("opus-4.8") to whatever provider
    // the operator just selected. Inline match — display_model_for
    // lives in grpc_service.rs and we don't want a circular dep.
    // Sprint 5: OpenAI-compat presets show the operator-selected
    // model id (Ollama/LM Studio) populated from the sub-flow.
    let model: String = match config.api_provider.as_str() {
        "gemini" => "gemini-3.1-pro".to_string(),
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
            .unwrap_or_else(|| "opus-4.8".to_string()),
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
        SystemConfig {
            name: "Embra".into(),
            api_key: api_key.into(),
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
            api_provider: provider.into(),
            gemini_model: None,
            anthropic_model: None,
            anthropic_effort: None,
            anthropic_api_key: anth.map(str::to_string),
            gemini_api_key: gem.map(str::to_string),
            max_tool_iterations: None,
            show_reasoning: None,
            openai_compat: crate::config::OpenAiCompatConfig::default(),
        }
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
    if let Ok(tz) = std::env::var("TZ") {
        if !tz.is_empty() {
            return tz;
        }
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

#[cfg(test)]
mod max_tool_iterations_serde_tests {
    use super::SystemConfig;
    use serde_json::json;

    fn minimal_cfg() -> SystemConfig {
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
            anthropic_api_key: None,
            gemini_api_key: None,
            max_tool_iterations: None,
            show_reasoning: None,
            openai_compat: crate::config::OpenAiCompatConfig::default(),
        }
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
    fn explicit_value_round_trips() {
        let mut cfg = minimal_cfg();
        cfg.max_tool_iterations = Some(42);
        let json = serde_json::to_value(&cfg).unwrap();
        let back: SystemConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.max_tool_iterations, Some(42));
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
    //! All three Anthropic entries resolve to the Anthropic provider and
    //! each seeds `anthropic_model` EXPLICITLY (a wizard choice must keep
    //! meaning what the operator picked even if `DEFAULT_MODEL` moves —
    //! it did, 4.7 → 4.8). The selector itself is server-driven, so the
    //! console and chat-mobile UIs render whichever labels appear here.
    use super::{
        anthropic_model_from_label, provider_from_label, PROVIDER_ANTHROPIC_48_LABEL,
        PROVIDER_ANTHROPIC_FABLE_LABEL, PROVIDER_ANTHROPIC_LABEL, PROVIDER_GEMINI_LABEL,
    };
    use crate::provider::ProviderKind;

    #[test]
    fn all_anthropic_labels_map_to_anthropic() {
        for label in [
            PROVIDER_ANTHROPIC_LABEL,
            PROVIDER_ANTHROPIC_48_LABEL,
            PROVIDER_ANTHROPIC_FABLE_LABEL,
        ] {
            assert_eq!(provider_from_label(label), ProviderKind::Anthropic);
        }
    }

    #[test]
    fn anthropic_labels_seed_expected_models() {
        assert_eq!(
            anthropic_model_from_label(PROVIDER_ANTHROPIC_LABEL),
            Some("opus-4.7".to_string())
        );
        assert_eq!(
            anthropic_model_from_label(PROVIDER_ANTHROPIC_48_LABEL),
            Some("opus-4.8".to_string())
        );
        assert_eq!(
            anthropic_model_from_label(PROVIDER_ANTHROPIC_FABLE_LABEL),
            Some("fable-5".to_string())
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
    use super::SystemConfig;
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
}
