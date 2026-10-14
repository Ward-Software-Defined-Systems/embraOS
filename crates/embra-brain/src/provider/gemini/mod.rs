//! Gemini provider: `gemini-3.1-pro-preview` (the default) and
//! `gemini-4-argon`, via the public Generative Language API.
//!
//! Submodules:
//! - [`wire`] — Gemini-shaped request / response types.
//! - [`tool_schema`] — translator from registry descriptors to
//!   Gemini's OpenAPI-3.0 subset.
//! - [`streaming`] — SSE parser (wire → neutral StreamEvent stream).
//! - [`conv`] — neutral IR → Gemini wire converter.
//! - `cache` (Stage 6) — explicit Context Cache lifecycle manager, for
//!   the models that take it ([`explicit_cache_supported`]).

pub mod cache;
mod conv;
pub mod streaming;
pub mod tool_schema;
pub mod wire;

use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, warn};

use crate::db::WardsonDbClient;
#[cfg(test)]
use crate::provider::ProviderKind;
use crate::provider::{
    ApiMessage, LlmProvider, LlmRequestOptions, ProviderError, StreamEvent, SystemPromptBundle,
    ToolManifest, ValidationResult,
};
use crate::tools::registry::ToolDescriptor;

use cache::GeminiCacheManager;

use wire::{
    GeminiContent, GeminiFunctionCallingConfig, GeminiGenerateRequest, GeminiGenerationConfig,
    GeminiSystemInstruction, GeminiSystemPart, GeminiThinkingConfig, GeminiToolConfig,
};

/// The Gemini chat model an instance runs unless the operator picks
/// another, and the name it is shown under.
pub const DEFAULT_MODEL: &str = "gemini-3.1-pro-preview";
pub const DEFAULT_DISPLAY_NAME: &str = "gemini-3.1-pro";
pub(crate) const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(10);

/// 64,000 output tokens: under the 65,536 that a synchronous request of
/// Gemini 3.1 Pro or Gemini 4 Argon may produce (Argon's longer output
/// needs its long-decode mode, which embraOS does not use).
/// `thinking_level` is `high` unless the operator lowers it via
/// `/effort` (mapped by [`thinking_level_for`]). Thinking cannot be
/// disabled.
const MAX_OUTPUT_TOKENS: u32 = 64_000;
pub(crate) const THINKING_LEVEL: &str = "high";

/// Map the canonical embraOS effort ladder (`low|medium|high|xhigh|max`)
/// onto Gemini's `thinkingLevel` (`low|medium|high` on 3.1 Pro and on
/// 4 Argon, which also requires the field; thinking cannot be
/// disabled). `xhigh`/`max` have no Gemini equivalent above `high`, so
/// they clamp to it; anything unrecognized is the default.
pub(crate) fn thinking_level_for(level: &str) -> &'static str {
    match level.trim().to_ascii_lowercase().as_str() {
        "low" => "low",
        "medium" => "medium",
        _ => THINKING_LEVEL,
    }
}

/// The name a Gemini model id is shown under: the default's short form,
/// and any other id as itself (`gemini-4-argon`, the customtools id).
pub fn display_name_for(model_id: &str) -> String {
    if model_id == DEFAULT_MODEL {
        DEFAULT_DISPLAY_NAME.to_string()
    } else {
        model_id.to_string()
    }
}

/// Whether a model takes the explicit context cache (`cachedContents`).
/// Google: it "can be manually enabled on Gemini 3 and 2.5 models", and
/// "is not supported for Gemini 4 Argon (`gemini-4-argon`) and onward".
/// Every other id, `*-latest` aliases included, relies on the implicit
/// caching all models have.
pub(crate) fn explicit_cache_supported(model_id: &str) -> bool {
    ["gemini-3.", "gemini-3-", "gemini-2.5-"]
        .iter()
        .any(|family| model_id.starts_with(family))
}

/// A manifest with nothing to declare: `[]`, or the wrapper with an
/// empty `functionDeclarations` list that `build_tool_manifest(&[])`
/// gives Learning Mode. A request built from one carries no `tools` and
/// no `toolConfig`.
fn declares_no_tools(wire_json: &serde_json::Value) -> bool {
    match wire_json {
        serde_json::Value::Array(entries) => entries.iter().all(|entry| {
            entry
                .get("functionDeclarations")
                .and_then(|d| d.as_array())
                .is_some_and(|d| d.is_empty())
        }),
        _ => false,
    }
}

/// Exponential backoff ladder (seconds) for 429 / 5xx retries on the
/// initial request. Mid-stream errors are not retried — a partial
/// response cannot be recovered. Honors `Retry-After` if the server
/// sent one.
const RETRY_DELAYS_SECS: &[u64] = &[1, 2, 4, 8, 16, 32, 60];

pub struct GeminiProvider {
    api_key: String,
    http: Client,
    model_id: String,
    display_name: String,
    /// `thinkingLevel` sent on every request (see [`thinking_level_for`]).
    thinking_level: String,
    /// Optional context-cache lifecycle manager. `None` when the
    /// builder passed no WardSONDB handle, or when the model takes no
    /// explicit cache ([`explicit_cache_supported`]); every request
    /// then carries the system prompt and the tools, which Google's
    /// implicit caching may still serve.
    cache: Option<Arc<GeminiCacheManager>>,
}

impl GeminiProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_model(api_key, DEFAULT_MODEL.to_string())
    }

    /// Construct with a model id: the default, `gemini-4-argon`, or an
    /// id set by hand, such as `gemini-3.1-pro-preview-customtools`
    /// (spec D8: the default ignoring custom tools).
    pub fn with_model(api_key: String, model_id: String) -> Self {
        let display_name = display_name_for(&model_id);
        Self {
            api_key,
            http: Client::new(),
            model_id,
            display_name,
            thinking_level: THINKING_LEVEL.to_string(),
            cache: None,
        }
    }

    /// Operator effort posture from `/effort` (canonical embraOS ladder);
    /// `None` keeps Gemini's default `high`. The context cache is
    /// unaffected — `thinkingLevel` rides `generationConfig` per request,
    /// never the cached content.
    pub fn with_effort(mut self, level: Option<String>) -> Self {
        self.thinking_level = level
            .as_deref()
            .map(thinking_level_for)
            .unwrap_or(THINKING_LEVEL)
            .to_string();
        self
    }

    /// Attach a Context Cache lifecycle manager when the model takes the
    /// explicit cache ([`explicit_cache_supported`]); any other model
    /// gets none. The turn and learning builders call this when the
    /// WardSONDB handle is available. Idempotent — replaces any
    /// existing cache.
    pub fn with_cache(mut self, db: Arc<WardsonDbClient>) -> Self {
        self.cache = explicit_cache_supported(&self.model_id).then(|| {
            Arc::new(GeminiCacheManager::new(
                self.api_key.clone(),
                db,
                self.model_id.clone(),
            ))
        });
        self
    }

    fn stream_url(&self) -> String {
        format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            API_BASE, self.model_id
        )
    }

    fn models_url() -> String {
        format!("{}/models", API_BASE)
    }
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    fn display_name(&self) -> &str {
        &self.display_name
    }

    #[cfg(test)]
    fn kind(&self) -> ProviderKind {
        ProviderKind::Gemini
    }

    async fn validate_key(&self, key: &str) -> ValidationResult {
        if key.is_empty() {
            return ValidationResult::InvalidKey;
        }
        let client = match Client::builder().timeout(VALIDATE_TIMEOUT).build() {
            Ok(c) => c,
            Err(_) => return ValidationResult::Unknown,
        };
        let resp = client
            .get(Self::models_url())
            .header("x-goog-api-key", key)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => ValidationResult::Valid,
            Ok(r) => match r.status().as_u16() {
                401 => ValidationResult::InvalidKey,
                403 => ValidationResult::Forbidden,
                _ => ValidationResult::Unknown,
            },
            Err(_) => ValidationResult::NetworkError,
        }
    }

    async fn stream_turn(
        &self,
        messages: &[ApiMessage],
        system: &SystemPromptBundle,
        tools: &ToolManifest,
        options: LlmRequestOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        // Translate neutral IR → Gemini wire shape.
        let contents = conv::ir_messages_to_wire(messages);

        // Tools manifest is `[{functionDeclarations: [...]}]` from the
        // tool_schema translator. A manifest that declares nothing
        // skips the tools / tool_config fields entirely.
        let tools_empty = declares_no_tools(&tools.wire_json);

        // Try to reuse a cached system+tools handle. On hit, omit
        // systemInstruction and tools from the request body — the
        // server prepends them from the cache. Errors and 4xx
        // ineligibility fall through to per-request system+tools.
        let cache_handle = match &self.cache {
            Some(cache) => match cache.ensure_cache(system, tools).await {
                Ok(maybe) => maybe,
                Err(e) => {
                    warn!(target: "gemini::cache", "ensure_cache failed: {e}; falling back uncached");
                    None
                }
            },
            None => None,
        };

        let url = self.stream_url();
        let api_key = self.api_key.clone();
        let http = self.http.clone();

        // First attempt: with cache (if active). On a stale-cache
        // error from the server, invalidate the local handle and
        // retry uncached. The server can delete a cachedContents
        // resource ahead of our recorded TTL (its own GC, billing
        // boundary, etc.); when it does, our handle still says the
        // cache is fresh but using it returns
        //   403 PERMISSION_DENIED "CachedContent not found"
        // (or a similar 404). Defensive recovery — never propagate
        // a cache-stale error to the user when we can just resend
        // with systemInstruction + tools inline.
        let body_with_cache = build_request_body(
            &contents,
            system,
            tools,
            tools_empty,
            cache_handle.as_ref(),
            options,
            &self.thinking_level,
        );
        let body_with_cache_json = serde_json::to_value(&body_with_cache)
            .map_err(|e| ProviderError::Decode(format!("request serialization: {e}")))?;

        let response = match send_with_retry(&http, &url, &api_key, &body_with_cache_json).await {
            Ok(r) => r,
            Err(ProviderError::Http { status, body })
                if cache_handle.is_some() && is_cache_stale_error(status, &body) =>
            {
                warn!(
                    target: "gemini::cache",
                    status,
                    "cached_content rejected by server; invalidating local handle and retrying uncached"
                );
                if let Some(cache) = &self.cache {
                    cache.invalidate_local().await;
                }
                let body_no_cache = build_request_body(
                    &contents,
                    system,
                    tools,
                    tools_empty,
                    None,
                    options,
                    &self.thinking_level,
                );
                let body_no_cache_json = serde_json::to_value(&body_no_cache).map_err(|e| {
                    ProviderError::Decode(format!("retry serialization: {e}"))
                })?;
                send_with_retry(&http, &url, &api_key, &body_no_cache_json).await?
            }
            Err(e) => return Err(e),
        };

        let (tx, rx) = mpsc::channel::<StreamEvent>(128);
        let include_reasoning = options.include_reasoning;
        tokio::spawn(async move {
            if let Err(e) =
                streaming::process_sse_stream(response, tx.clone(), include_reasoning).await
            {
                error!("Gemini SSE stream error: {}", e);
                let _ = tx.send(StreamEvent::Error(e.to_string())).await;
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn build_tool_manifest(&self, descriptors: &[&'static ToolDescriptor]) -> ToolManifest {
        let wire_json = match tool_schema::translate(descriptors) {
            Ok(v) => v,
            Err(e) => {
                error!("Gemini tool schema translation failed: {e}");
                serde_json::Value::Array(vec![])
            }
        };
        ToolManifest { wire_json }
    }
}

/// Build the Gemini request body with or without a cache handle.
/// Extracted so `stream_turn` can call it twice when the first
/// attempt fails with a stale-cache error and we need to retry
/// uncached.
fn build_request_body<'a>(
    contents: &'a [GeminiContent],
    system: &SystemPromptBundle,
    tools: &'a ToolManifest,
    tools_empty: bool,
    cache_handle: Option<&cache::CacheHandle>,
    options: LlmRequestOptions,
    thinking_level: &str,
) -> GeminiGenerateRequest<'a> {
    let cache_active = cache_handle.is_some();
    GeminiGenerateRequest {
        contents,
        system_instruction: if cache_active {
            None
        } else {
            Some(GeminiSystemInstruction {
                parts: vec![GeminiSystemPart {
                    text: system.text.clone(),
                }],
            })
        },
        tools: if cache_active || tools_empty {
            None
        } else {
            Some(&tools.wire_json)
        },
        tool_config: if cache_active || tools_empty {
            None
        } else {
            Some(GeminiToolConfig {
                function_calling_config: GeminiFunctionCallingConfig {
                    mode: "AUTO".to_string(),
                },
            })
        },
        generation_config: Some(generation_config(thinking_level, options.include_reasoning)),
        cached_content: cache_handle.map(|h| h.cache_name.clone()),
    }
}

/// The per-request `generationConfig`. Extracted so the wire shape is
/// unit-testable without assembling a full request.
fn generation_config(thinking_level: &str, include_reasoning: bool) -> GeminiGenerationConfig {
    GeminiGenerationConfig {
        max_output_tokens: MAX_OUTPUT_TOKENS,
        thinking_config: GeminiThinkingConfig {
            thinking_level: thinking_level.to_string(),
            include_thoughts: include_reasoning,
        },
    }
}

/// Detect the "you sent a cached_content reference but the server
/// no longer has that cache" error class. Server returns either
/// 403 PERMISSION_DENIED or 404 with a `CachedContent`-mentioning
/// body. We only treat 4xx as cache-stale when the body explicitly
/// names cached content — a plain 403 with no cache reference is
/// a real auth/billing problem and should propagate.
fn is_cache_stale_error(status: u16, body: &str) -> bool {
    if !matches!(status, 403 | 404) {
        return false;
    }
    body.contains("CachedContent")
        || body.contains("cached_content")
        || body.contains("cachedContent")
}

/// POST the request body and retry on 429 / 5xx with the documented
/// backoff ladder. Returns the streaming `reqwest::Response` on the
/// first 2xx status; surfaces 4xx (other than 429) and exhausted
/// retries as `ProviderError`.
async fn send_with_retry(
    http: &Client,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
) -> Result<reqwest::Response, ProviderError> {
    let mut last_status: u16 = 0;
    let mut last_body = String::new();

    for (attempt, delay_secs) in
        std::iter::once(0).chain(RETRY_DELAYS_SECS.iter().copied()).enumerate()
    {
        if delay_secs > 0 {
            tokio::time::sleep(Duration::from_secs(delay_secs)).await;
        }

        let send_result = http
            .post(url)
            .header("x-goog-api-key", api_key)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await;

        match send_result {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp);
                }
                let code = status.as_u16();
                last_status = code;
                // 429 and 5xx are retryable. Honor Retry-After when
                // present by deferring this iteration's body read.
                let retryable = code == 429 || (500..=599).contains(&code);
                if !retryable {
                    let body_text = resp.text().await.unwrap_or_default();
                    return Err(ProviderError::Http {
                        status: code,
                        body: body_text,
                    });
                }
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());
                last_body = resp.text().await.unwrap_or_default();
                warn!(
                    target: "gemini::http",
                    attempt = attempt + 1,
                    status = code,
                    retry_after_secs = retry_after,
                    "retryable Gemini error; backing off"
                );
                if let Some(secs) = retry_after {
                    tokio::time::sleep(Duration::from_secs(secs.min(60))).await;
                }
            }
            Err(e) => {
                last_body = e.to_string();
                warn!(target: "gemini::http", attempt = attempt + 1, "network error: {e}");
            }
        }
    }

    if last_status >= 400 {
        Err(ProviderError::Http {
            status: last_status,
            body: last_body,
        })
    } else {
        Err(ProviderError::Network(last_body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_level_for_clamps_to_the_gemini_ladder() {
        assert_eq!(thinking_level_for("low"), "low");
        assert_eq!(thinking_level_for(" Medium "), "medium");
        assert_eq!(thinking_level_for("high"), "high");
        assert_eq!(thinking_level_for("xhigh"), "high");
        assert_eq!(thinking_level_for("max"), "high");
        assert_eq!(thinking_level_for("bogus"), "high");
    }

    #[test]
    fn generation_config_default_is_high_and_carries_a_configured_level() {
        let v = serde_json::to_value(generation_config(THINKING_LEVEL, true)).unwrap();
        assert_eq!(v["maxOutputTokens"], 64_000);
        assert_eq!(v["thinkingConfig"]["thinkingLevel"], "high");
        assert_eq!(v["thinkingConfig"]["includeThoughts"], true);
        let v = serde_json::to_value(generation_config(thinking_level_for("low"), false)).unwrap();
        assert_eq!(v["thinkingConfig"]["thinkingLevel"], "low");
        assert_eq!(v["thinkingConfig"]["includeThoughts"], false);
    }

    #[test]
    fn with_effort_maps_the_ladder_and_none_restores_the_default() {
        let p = GeminiProvider::new("k".into());
        assert_eq!(p.thinking_level, "high");
        let p = p.with_effort(Some("max".into()));
        assert_eq!(p.thinking_level, "high");
        let p = p.with_effort(Some("medium".into()));
        assert_eq!(p.thinking_level, "medium");
        let p = p.with_effort(None);
        assert_eq!(p.thinking_level, "high");
    }

    #[test]
    fn validate_key_rejects_empty() {
        let p = GeminiProvider::new(String::new());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert_eq!(p.validate_key("").await, ValidationResult::InvalidKey);
        });
    }

    #[test]
    fn display_name_uses_short_form_for_default_model() {
        let p = GeminiProvider::new("k".into());
        assert_eq!(p.display_name(), DEFAULT_DISPLAY_NAME);
        assert_eq!(p.kind(), ProviderKind::Gemini);
    }

    #[test]
    fn display_name_uses_full_id_for_custom_model() {
        let p = GeminiProvider::with_model(
            "k".into(),
            "gemini-3.1-pro-preview-customtools".into(),
        );
        assert_eq!(p.display_name(), "gemini-3.1-pro-preview-customtools");
    }

    #[test]
    fn display_name_for_shortens_the_default_only() {
        assert_eq!(display_name_for(DEFAULT_MODEL), DEFAULT_DISPLAY_NAME);
        assert_eq!(display_name_for("gemini-4-argon"), "gemini-4-argon");
        assert_eq!(
            display_name_for("gemini-3.1-pro-preview-customtools"),
            "gemini-3.1-pro-preview-customtools"
        );
    }

    #[test]
    fn the_explicit_cache_is_for_gemini_3_and_2_5_only() {
        for id in [
            DEFAULT_MODEL,
            "gemini-3.1-pro-preview-customtools",
            "gemini-3-flash-preview",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
        ] {
            assert!(explicit_cache_supported(id), "{id} takes the explicit cache");
        }
        for id in [
            "gemini-4-argon",
            "gemini-4-argon-cyber-permissive",
            "gemini-pro-latest",
            "gemini-2.0-flash",
            "gemini-30-x",
            "",
        ] {
            assert!(!explicit_cache_supported(id), "{id} takes no explicit cache");
        }
    }

    #[test]
    fn an_argon_provider_carries_no_cache_manager() {
        let db = Arc::new(WardsonDbClient::from_url("http://127.0.0.1:1"));
        let argon = GeminiProvider::with_model("k".into(), "gemini-4-argon".into()).with_cache(db.clone());
        assert!(argon.cache.is_none());
        let default = GeminiProvider::new("k".into()).with_cache(db);
        assert!(default.cache.is_some());
    }

    #[test]
    fn a_manifest_without_declarations_sends_no_tools_or_tool_config() {
        for (wire, declares_none) in [
            (serde_json::json!([]), true),
            (serde_json::json!([{"functionDeclarations": []}]), true),
            (serde_json::json!([{"functionDeclarations": [{"name": "a"}]}]), false),
            (serde_json::json!([{"googleSearch": {}}]), false),
            (serde_json::json!({}), false),
        ] {
            assert_eq!(declares_no_tools(&wire), declares_none, "{wire}");
        }

        let system = SystemPromptBundle {
            text: "sys".into(),
            session_name: "main".into(),
        };
        let options = LlmRequestOptions {
            include_reasoning: true,
        };
        let body_of = |tools: &ToolManifest| {
            let empty = declares_no_tools(&tools.wire_json);
            serde_json::to_value(build_request_body(
                &[],
                &system,
                tools,
                empty,
                None,
                options,
                THINKING_LEVEL,
            ))
            .unwrap()
        };
        let p = GeminiProvider::new("k".into());

        // Learning Mode's manifest: the system prompt, nothing to call.
        let v = body_of(&p.build_tool_manifest(&[]));
        assert!(v.get("tools").is_none() && v.get("toolConfig").is_none(), "{v}");
        assert!(v.get("systemInstruction").is_some(), "{v}");

        let descriptors: Vec<&'static ToolDescriptor> =
            crate::tools::registry::all_descriptors().collect();
        let v = body_of(&p.build_tool_manifest(&descriptors));
        assert!(v.get("tools").is_some(), "the full manifest is sent");
        assert!(v.get("toolConfig").is_some() && v.get("systemInstruction").is_some());
    }

    #[test]
    fn build_tool_manifest_returns_function_declarations_array() {
        let p = GeminiProvider::new("k".into());
        let descriptors: Vec<&'static ToolDescriptor> =
            crate::tools::registry::all_descriptors().collect();
        let manifest = p.build_tool_manifest(&descriptors);
        match &manifest.wire_json {
            serde_json::Value::Array(arr) => {
                assert_eq!(arr.len(), 1);
                let decls = arr[0]["functionDeclarations"].as_array().unwrap();
                assert!(decls.len() >= 70);
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn build_tool_manifest_with_empty_descriptors_yields_empty_function_declarations() {
        let p = GeminiProvider::new("k".into());
        let manifest = p.build_tool_manifest(&[]);
        let arr = manifest.wire_json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert!(arr[0]["functionDeclarations"].as_array().unwrap().is_empty());
    }

    #[test]
    fn is_cache_stale_error_matches_documented_403_message() {
        let body = r#"{"error":{"code":403,"message":"CachedContent not found (or permission denied)","status":"PERMISSION_DENIED"}}"#;
        assert!(is_cache_stale_error(403, body));
    }

    #[test]
    fn is_cache_stale_error_matches_404_with_cache_reference() {
        let body = r#"{"error":{"code":404,"message":"cachedContents/abc123 not found"}}"#;
        assert!(is_cache_stale_error(404, body));
    }

    #[test]
    fn is_cache_stale_error_rejects_403_without_cache_reference() {
        // Plain auth/billing 403 — must propagate, not be silently
        // retried as a cache miss.
        let body = r#"{"error":{"code":403,"message":"API key not authorized","status":"PERMISSION_DENIED"}}"#;
        assert!(!is_cache_stale_error(403, body));
    }

    #[test]
    fn is_cache_stale_error_rejects_other_status_codes() {
        let body = r#"{"error":{"message":"CachedContent not found"}}"#;
        assert!(!is_cache_stale_error(401, body));
        assert!(!is_cache_stale_error(500, body));
        assert!(!is_cache_stale_error(429, body));
    }
}
