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
}

fn default_kg_temporal_window() -> u64 { 1800 }
fn default_kg_max_depth() -> u32 { 3 }
fn default_kg_depth_ceiling() -> u32 { 5 }
fn default_kg_candidate_limit() -> u32 { 50 }
fn default_api_provider() -> String { "anthropic".to_string() }

const PROVIDER_ANTHROPIC_LABEL: &str = "Anthropic Claude Opus 4.7";
const PROVIDER_GEMINI_LABEL: &str = "Google Gemini 3.1 Pro";

fn provider_label(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => PROVIDER_ANTHROPIC_LABEL,
        ProviderKind::Gemini => PROVIDER_GEMINI_LABEL,
    }
}

fn provider_from_label(label: &str) -> ProviderKind {
    match label {
        PROVIDER_GEMINI_LABEL => ProviderKind::Gemini,
        _ => ProviderKind::Anthropic,
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
        api_provider: default_api_provider(),
        gemini_model: None,
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
    };
    match (result, provider) {
        (ValidationResult::Valid, _) => ApiKeyCheck::Valid,
        (ValidationResult::InvalidKey, ProviderKind::Anthropic) => ApiKeyCheck::Invalid(
            "Invalid Anthropic API key — check and try again.".into(),
        ),
        (ValidationResult::InvalidKey, ProviderKind::Gemini) => ApiKeyCheck::Invalid(
            "Invalid Gemini API key — check the key and try again.".into(),
        ),
        (ValidationResult::Forbidden, ProviderKind::Anthropic) => ApiKeyCheck::Invalid(
            "Anthropic API rejected the key (403). Verify it's active and try again.".into(),
        ),
        (ValidationResult::Forbidden, ProviderKind::Gemini) => ApiKeyCheck::Invalid(
            "Forbidden — likely missing billing on the Gemini API. \
             Verify at aistudio.google.com — gemini-3.1-pro-preview requires a billed account."
                .into(),
        ),
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

    // Step 2: Provider selection (Sprint 4) — Selector UI.
    let _ = tx.send(Ok(ConversationResponse {
        response_type: Some(conversation_response::ResponseType::Setup(
            SetupPrompt {
                field_type: SetupFieldType::Selector as i32,
                prompt: "Which AI provider would you like to use?".to_string(),
                options: vec![
                    PROVIDER_ANTHROPIC_LABEL.to_string(),
                    PROVIDER_GEMINI_LABEL.to_string(),
                ],
                default_value: PROVIDER_ANTHROPIC_LABEL.to_string(),
            }
        )),
    })).await;
    let provider_choice = match response_rx.recv().await {
        Some(input) if !input.is_empty() => input,
        _ => PROVIDER_ANTHROPIC_LABEL.to_string(),
    };
    let provider_kind = provider_from_label(&provider_choice);
    info!(
        "Config wizard: provider = {}",
        provider_kind.as_str()
    );

    // Honor an env-var of the wrong shape with a clear info message
    // rather than silently accepting it.
    let env_var_name = match provider_kind {
        ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
        ProviderKind::Gemini => "GEMINI_API_KEY",
    };
    let other_env_name = match provider_kind {
        ProviderKind::Anthropic => "GEMINI_API_KEY",
        ProviderKind::Gemini => "ANTHROPIC_API_KEY",
    };
    if std::env::var(other_env_name).map(|v| !v.is_empty()).unwrap_or(false) {
        let _ = tx.send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(
                SystemMessage {
                    content: format!(
                        "{other_env_name} detected in environment but provider is {} — ignored.",
                        provider_label(provider_kind)
                    ),
                    msg_type: SystemMessageType::Info as i32,
                }
            )),
        })).await;
    }

    // Step 3: API Key — re-prompts until provider's validate_key returns Valid.
    let api_key = loop {
        let (candidate, from_env) = match std::env::var(env_var_name) {
            Ok(k) if !k.is_empty() => (k, true),
            _ => {
                let prompt_text = match provider_kind {
                    ProviderKind::Anthropic => "Enter your Anthropic API key:",
                    ProviderKind::Gemini => "Enter your Gemini API key:",
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
                        provider_label(provider_kind)
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
        provider_label(provider_kind),
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

    // Save config
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
        api_provider: provider_kind.as_str().to_string(),
        gemini_model: None,
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

    // Also write API key to STATE partition so embrad can pass it on subsequent boots
    let key_path = "/embra/state/api_key";
    if let Some(parent) = std::path::Path::new(key_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(key_path, &config.api_key) {
        tracing::warn!("Could not write API key to STATE: {}", e);
    } else {
        info!("API key written to {}", key_path);
    }

    // Sprint 4: persist active provider to STATE so embrad picks the
    // right LlmProvider impl on subsequent boots.
    let provider_path = "/embra/state/api_provider";
    if let Err(e) = std::fs::write(provider_path, &config.api_provider) {
        tracing::warn!("Could not write api_provider to STATE: {}", e);
    } else {
        info!("Provider written to {} ({})", provider_path, &config.api_provider);
    }

    // Transition to next mode
    let soul_sealed = crate::learning::is_soul_sealed(db).await.unwrap_or(false);
    let next_mode = if soul_sealed {
        OperatingMode::Operational
    } else {
        OperatingMode::Learning
    };
    let _ = tx.send(Ok(ConversationResponse {
        response_type: Some(conversation_response::ResponseType::ModeChange(
            ModeTransition {
                from_mode: OperatingMode::Setup as i32,
                to_mode: next_mode as i32,
                message: format!("Setup complete — Name: {} — TZ: {}", config.name, config.timezone),
            }
        )),
    })).await;

    Ok(config)
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
