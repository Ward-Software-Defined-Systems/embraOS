//! OpenAI Chat Completions-compatible provider.
//!
//! Single module covering both Ollama and LM Studio backends. The
//! presets share an HTTP client surface and wire format; the discriminator
//! is [`OpenAiCompatPreset`] which selects defaults and labels. Future
//! OpenAI-compat backends (vLLM, Together, Fireworks, OpenRouter) drop
//! in as additional `OpenAiCompatPreset` variants without new modules.
//!
//! Module layout:
//! - [`wire`] — request/response/streaming types (snake_case JSON).
//! - [`conv`] — neutral IR ↔ wire translators with reasoning round-trip.
//! - [`tool_schema`] — tool-schema translator (light passthrough using
//!   the shared `provider::schema_util::inline_refs`).
//! - [`sanitize`] — always-on harmony token sanitization for tool-call
//!   names per Locked Decision #11.
//! - [`streaming`] — SSE parser with `delta.tool_calls[]` argument-shard
//!   assembly and defensive multi-key reasoning accumulator.

pub mod conv;
pub mod sanitize;
pub mod streaming;
pub mod tool_schema;
pub mod wire;

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::Client;
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::error;

use crate::provider::{
    ApiMessage, LlmProvider, LlmRequestOptions, ProviderError, ProviderKind, StreamEvent,
    SystemPromptBundle, ToolManifest, ValidationResult,
};
use crate::tools::registry::ToolDescriptor;

use self::wire::{ModelsResponse, OpenAIChatRequest, OpenAIMessage, OpenAITool};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Preset variant within the OpenAI-compat module. Ollama and LM Studio
/// share wire format; presets carry preset-specific defaults (port, label).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OpenAiCompatPreset {
    Ollama,
    LmStudio,
}

impl OpenAiCompatPreset {
    /// Map to the cross-provider [`ProviderKind`] discriminator used by
    /// session attach checks and dispatch.
    pub fn as_kind(self) -> ProviderKind {
        match self {
            Self::Ollama => ProviderKind::Ollama,
            Self::LmStudio => ProviderKind::LmStudio,
        }
    }

    /// Short human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::LmStudio => "LM Studio",
        }
    }

    /// Default base URL when the wizard's Endpoint step receives no
    /// scheme + no port (operator typed only "localhost" or similar).
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Ollama => "http://localhost:11434",
            Self::LmStudio => "http://localhost:1234",
        }
    }
}

pub struct OpenAICompatProvider {
    preset: OpenAiCompatPreset,
    base_url: String,
    bearer_token: Option<String>,
    http: Client,
    model_id: String,
    display_name: String,
}

impl OpenAICompatProvider {
    pub fn ollama(base_url: String, bearer: Option<String>, model: String) -> Self {
        Self::new(OpenAiCompatPreset::Ollama, base_url, bearer, model)
    }

    pub fn lm_studio(base_url: String, bearer: Option<String>, model: String) -> Self {
        Self::new(OpenAiCompatPreset::LmStudio, base_url, bearer, model)
    }

    fn new(
        preset: OpenAiCompatPreset,
        base_url: String,
        bearer: Option<String>,
        model: String,
    ) -> Self {
        let display_name = model.clone();
        Self {
            preset,
            base_url,
            bearer_token: bearer,
            http: Client::new(),
            model_id: model,
            display_name,
        }
    }

    /// Probe the endpoint for available models. Used by the wizard's
    /// Probe-and-Select step (Stage 4) and by `validate_key`. Associated
    /// function — does not require a fully-constructed provider.
    ///
    /// Returns the list of model IDs sorted alphabetically. Empty Vec
    /// is a valid result (no models configured server-side).
    pub async fn probe_models(
        preset: OpenAiCompatPreset,
        base_url: &str,
        bearer: Option<&str>,
    ) -> Result<Vec<String>, ProviderError> {
        let _ = preset; // reserved for preset-specific behavior
        let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
        let client = Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        let mut req = client.get(&url);
        if let Some(token) = bearer.filter(|t| !t.is_empty()) {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                ProviderError::Network(format!("timeout connecting to {url}"))
            } else if e.is_connect() {
                ProviderError::Network(format!("connection refused: {url}"))
            } else {
                ProviderError::Network(e.to_string())
            }
        })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body,
            });
        }
        let parsed: ModelsResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::Decode(format!("/v1/models response: {e}")))?;
        let mut ids: Vec<String> = parsed.data.into_iter().map(|m| m.id).collect();
        ids.sort();
        Ok(ids)
    }

    async fn post_with_retry(
        &self,
        url: &str,
        body: &OpenAIChatRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let attempt = self.post_once(url, body).await;
        if should_retry(&attempt) {
            tokio::time::sleep(RETRY_DELAY).await;
            return self.post_once(url, body).await;
        }
        attempt
    }

    async fn post_once(
        &self,
        url: &str,
        body: &OpenAIChatRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let mut req = self
            .http
            .post(url)
            .header("content-type", "application/json")
            .json(body);
        if let Some(token) = self.bearer_token.as_deref().filter(|t| !t.is_empty()) {
            req = req.bearer_auth(token);
        }
        match req.send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    return Ok(resp);
                }
                let status = resp.status().as_u16();
                let body_text = resp.text().await.unwrap_or_default();
                Err(ProviderError::Http {
                    status,
                    body: body_text,
                })
            }
            Err(e) => Err(ProviderError::Network(e.to_string())),
        }
    }
}

/// Heuristic on `model_id` to decide whether `reasoning_effort` is
/// meaningful for this model. Per Locked Decision #4, the parameter
/// is sent only to reasoning-effort-aware models and omitted entirely
/// for everything else. False negatives (a reasoning model we miss)
/// degrade to no-effort-control output; false positives (sending to
/// a non-reasoning model) produce server-side log warnings without
/// affecting the response.
///
/// Pattern criteria — case-insensitive substring or prefix match:
/// - **gpt-oss family** (`gpt-oss:`, `*/gpt-oss-*`): OpenAI's
///   launch-partner reasoning model on Ollama.
/// - **OpenAI o-series** (`o1-mini`, `o1-preview`, `o3-mini`,
///   `o3-pro`, `o4-mini`): small reasoning models hosted via LM
///   Studio per its 0.3.23+ alignment-with-o3-mini changelog note.
/// - **`-thinking`**: Qwen3-thinking variants, Claude-thinking-style
///   models, etc.
/// - **`deepseek-r1`** / **`deepseek-r2`**: DeepSeek's reasoning family.
///
/// Standard non-reasoning models (Qwen3.6 base, Llama 3.x, Mistral)
/// fall through to `false`.
pub(crate) fn model_supports_reasoning_effort(model_id: &str) -> bool {
    let lower = model_id.to_lowercase();
    if lower.starts_with("gpt-oss") || lower.contains("/gpt-oss") {
        return true;
    }
    if lower.contains("o1-mini")
        || lower.contains("o1-preview")
        || lower.contains("o3-mini")
        || lower.contains("o3-pro")
        || lower.contains("o4-mini")
    {
        return true;
    }
    if lower.contains("-thinking") || lower.contains("_thinking") {
        return true;
    }
    if lower.contains("deepseek-r1") || lower.contains("deepseek-r2") {
        return true;
    }
    false
}

/// Decide whether a failed attempt warrants one retry. Connection
/// errors and 5xx responses retry once after a 1s delay; everything
/// else propagates immediately.
fn should_retry(attempt: &Result<reqwest::Response, ProviderError>) -> bool {
    match attempt {
        Err(ProviderError::Http { status, .. }) if (500..600).contains(status) => true,
        Err(ProviderError::Network(msg)) => msg.contains("connection refused") || msg.contains("timeout"),
        _ => false,
    }
}

#[async_trait]
impl LlmProvider for OpenAICompatProvider {
    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn kind(&self) -> ProviderKind {
        self.preset.as_kind()
    }

    /// Validate a candidate bearer (or no auth) against the endpoint
    /// by probing /v1/models. Mirrors the existing trait contract;
    /// the wizard uses [`Self::probe_models`] directly to also obtain
    /// the model list.
    async fn validate_key(&self, key: &str) -> ValidationResult {
        let bearer = (!key.is_empty()).then_some(key);
        match Self::probe_models(self.preset, &self.base_url, bearer).await {
            Ok(_) => ValidationResult::Valid,
            Err(ProviderError::Http { status: 401, .. }) => ValidationResult::InvalidKey,
            Err(ProviderError::Http { status: 403, .. }) => ValidationResult::Forbidden,
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
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );

        // Translate IR conversation → wire messages, prepending system.
        let mut wire_messages = vec![OpenAIMessage::System {
            content: system.text.clone(),
        }];
        let body_messages = conv::ir_messages_to_wire(messages)
            .map_err(|e| ProviderError::Decode(e.to_string()))?;
        wire_messages.extend(body_messages);

        // Tool manifest → Vec<OpenAITool>. Empty manifest → omit tools.
        let tools_vec: Vec<OpenAITool> = match &tools.wire_json {
            JsonValue::Array(a) if !a.is_empty() => serde_json::from_value(JsonValue::Array(a.clone()))
                .map_err(|e| ProviderError::Decode(format!("tool manifest: {e}")))?,
            _ => Vec::new(),
        };
        let has_tools = !tools_vec.is_empty();

        let body = OpenAIChatRequest {
            model: self.model_id.clone(),
            messages: wire_messages,
            stream: true,
            tools: if has_tools { Some(tools_vec) } else { None },
            // OpenAI canonical `tool_choice`: plain string `"auto"`,
            // NOT Anthropic's `{"type":"auto"}` object form. LM Studio
            // emits `Invalid tool_choice type: 'object'. Supported
            // string values: none, auto, required` for the object form.
            // Stage 3 copied Anthropic's syntax by mistake.
            tool_choice: if has_tools {
                Some(json!("auto"))
            } else {
                None
            },
            // Locked Decision #4: send "high" only when the active
            // model is reasoning-effort-aware. Sending to a non-
            // reasoning model produces a `No valid custom reasoning
            // fields found` warning on the server side (LM Studio)
            // and is silently dropped (Ollama). Omit entirely when
            // unsupported — matches the spec's "parameter omitted
            // entirely when it doesn't" wording verbatim.
            reasoning_effort: if model_supports_reasoning_effort(&self.model_id) {
                Some("high".to_string())
            } else {
                None
            },
            max_tokens: None,
        };

        let response = self.post_with_retry(&url, &body).await?;

        // Spawn the SSE consumer; events flow through an mpsc channel
        // which is wrapped as a BoxStream<StreamEvent>.
        let (tx, rx) = mpsc::channel::<StreamEvent>(256);
        let model_id = self.model_id.clone();
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = streaming::process_sse_stream(
                response,
                tx_clone.clone(),
                model_id,
                options.include_reasoning,
            )
            .await
            {
                error!(target: "provider::openai_compat", "SSE stream error: {e}");
                let _ = tx_clone.send(StreamEvent::Error(e.to_string())).await;
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn build_tool_manifest(&self, descriptors: &[&'static ToolDescriptor]) -> ToolManifest {
        let wire_json = match tool_schema::translate(descriptors) {
            Ok(v) => v,
            Err(e) => {
                error!(
                    target: "provider::openai_compat",
                    error = %e,
                    "tool schema translation failed; sending empty manifest"
                );
                JsonValue::Array(Vec::new())
            }
        };
        let canonical = serde_json::to_string(&wire_json).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        let digest = hasher.finalize();
        let fingerprint = hex::encode(&digest[..8]);
        ToolManifest {
            wire_json,
            fingerprint,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ir::{Block, EarlyStopReason, TurnOutcome};
    use crate::provider::StreamEvent as ProviderStreamEvent;
    use futures::StreamExt;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn dead_url() -> String {
        // Port 1 is reserved (tcpmux) and not listening on test runners.
        // Reqwest reports this as a connection refused.
        "http://127.0.0.1:1".to_string()
    }

    fn system_bundle() -> SystemPromptBundle {
        SystemPromptBundle {
            text: "you are a test".to_string(),
            fingerprint: "abc".to_string(),
            session_name: "test".to_string(),
        }
    }

    fn empty_manifest() -> ToolManifest {
        ToolManifest {
            wire_json: json!([]),
            fingerprint: "0".to_string(),
        }
    }

    // ===========================================================
    // probe_models — 8 cases × 2 presets = 16 tests
    // ===========================================================

    async fn probe_success_n(preset: OpenAiCompatPreset) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [
                    {"id": "qwen3:8b"},
                    {"id": "gpt-oss:20b"},
                    {"id": "gpt-oss:120b"}
                ]
            })))
            .mount(&server)
            .await;
        let result = OpenAICompatProvider::probe_models(preset, &server.uri(), None)
            .await
            .unwrap();
        // Sorted alphabetically.
        assert_eq!(
            result,
            vec![
                "gpt-oss:120b".to_string(),
                "gpt-oss:20b".to_string(),
                "qwen3:8b".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn probe_success_n_ollama() {
        probe_success_n(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn probe_success_n_lm_studio() {
        probe_success_n(OpenAiCompatPreset::LmStudio).await;
    }

    async fn probe_success_one(preset: OpenAiCompatPreset) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [{"id": "only-model"}]
            })))
            .mount(&server)
            .await;
        let result = OpenAICompatProvider::probe_models(preset, &server.uri(), None)
            .await
            .unwrap();
        assert_eq!(result, vec!["only-model".to_string()]);
    }

    #[tokio::test]
    async fn probe_success_one_ollama() {
        probe_success_one(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn probe_success_one_lm_studio() {
        probe_success_one(OpenAiCompatPreset::LmStudio).await;
    }

    async fn probe_success_zero(preset: OpenAiCompatPreset) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": []
            })))
            .mount(&server)
            .await;
        let result = OpenAICompatProvider::probe_models(preset, &server.uri(), None)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn probe_success_zero_ollama() {
        probe_success_zero(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn probe_success_zero_lm_studio() {
        probe_success_zero(OpenAiCompatPreset::LmStudio).await;
    }

    async fn probe_unauthorized(preset: OpenAiCompatPreset) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;
        let err = OpenAICompatProvider::probe_models(preset, &server.uri(), None)
            .await
            .unwrap_err();
        match err {
            ProviderError::Http { status, .. } => assert_eq!(status, 401),
            other => panic!("expected Http 401, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_unauthorized_ollama() {
        probe_unauthorized(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn probe_unauthorized_lm_studio() {
        probe_unauthorized(OpenAiCompatPreset::LmStudio).await;
    }

    async fn probe_not_found(preset: OpenAiCompatPreset) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = OpenAICompatProvider::probe_models(preset, &server.uri(), None)
            .await
            .unwrap_err();
        match err {
            ProviderError::Http { status, .. } => assert_eq!(status, 404),
            other => panic!("expected Http 404, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_not_found_ollama() {
        probe_not_found(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn probe_not_found_lm_studio() {
        probe_not_found(OpenAiCompatPreset::LmStudio).await;
    }

    async fn probe_connection_refused(preset: OpenAiCompatPreset) {
        let err = OpenAICompatProvider::probe_models(preset, &dead_url(), None)
            .await
            .unwrap_err();
        match err {
            ProviderError::Network(_) => {}
            other => panic!("expected Network error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_connection_refused_ollama() {
        probe_connection_refused(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn probe_connection_refused_lm_studio() {
        probe_connection_refused(OpenAiCompatPreset::LmStudio).await;
    }

    async fn probe_timeout(preset: OpenAiCompatPreset) {
        // Use a routable but black-hole address. 192.0.2.0/24 is
        // TEST-NET-1 — guaranteed not assigned. Connection times out
        // rather than refusing immediately.
        let url = "http://192.0.2.1:80";
        let err = OpenAICompatProvider::probe_models(preset, url, None)
            .await
            .unwrap_err();
        match err {
            ProviderError::Network(_) => {}
            other => panic!("expected Network error, got {other:?}"),
        }
    }

    #[tokio::test]
    #[ignore = "takes 10s due to PROBE_TIMEOUT; opt in with --ignored"]
    async fn probe_timeout_ollama() {
        probe_timeout(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    #[ignore = "takes 10s due to PROBE_TIMEOUT; opt in with --ignored"]
    async fn probe_timeout_lm_studio() {
        probe_timeout(OpenAiCompatPreset::LmStudio).await;
    }

    async fn probe_malformed(preset: OpenAiCompatPreset) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("not json")
                    .insert_header("content-type", "application/json"),
            )
            .mount(&server)
            .await;
        let err = OpenAICompatProvider::probe_models(preset, &server.uri(), None)
            .await
            .unwrap_err();
        match err {
            ProviderError::Decode(_) => {}
            other => panic!("expected Decode error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_malformed_ollama() {
        probe_malformed(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn probe_malformed_lm_studio() {
        probe_malformed(OpenAiCompatPreset::LmStudio).await;
    }

    // probe_models with bearer token — verifies Authorization header.
    #[tokio::test]
    async fn probe_with_bearer_includes_authorization_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(wiremock::matchers::header("authorization", "Bearer secret123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list", "data": [{"id": "m1"}]
            })))
            .mount(&server)
            .await;
        let result = OpenAICompatProvider::probe_models(
            OpenAiCompatPreset::Ollama,
            &server.uri(),
            Some("secret123"),
        )
        .await
        .unwrap();
        assert_eq!(result, vec!["m1".to_string()]);
    }

    #[tokio::test]
    async fn probe_with_empty_bearer_succeeds_without_auth_header() {
        // Empty bearer must not produce an Authorization header.
        // We verify the call succeeds against a mock that doesn't
        // require auth; the source-level guard
        // `bearer.filter(|t| !t.is_empty())` skips the bearer_auth
        // call when the token is empty.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list", "data": [{"id": "m1"}]
            })))
            .mount(&server)
            .await;
        let result = OpenAICompatProvider::probe_models(
            OpenAiCompatPreset::Ollama,
            &server.uri(),
            Some(""),
        )
        .await
        .unwrap();
        assert_eq!(result, vec!["m1".to_string()]);
    }

    // ===========================================================
    // validate_key — delegates to probe_models
    // ===========================================================

    async fn validate_key_delegates(preset: OpenAiCompatPreset) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list", "data": [{"id": "m1"}]
            })))
            .mount(&server)
            .await;
        let provider = match preset {
            OpenAiCompatPreset::Ollama => {
                OpenAICompatProvider::ollama(server.uri(), None, "m1".to_string())
            }
            OpenAiCompatPreset::LmStudio => {
                OpenAICompatProvider::lm_studio(server.uri(), None, "m1".to_string())
            }
        };
        let result = provider.validate_key("").await;
        assert_eq!(result, ValidationResult::Valid);
    }

    #[tokio::test]
    async fn validate_key_ollama_delegates_to_probe() {
        validate_key_delegates(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn validate_key_lm_studio_delegates_to_probe() {
        validate_key_delegates(OpenAiCompatPreset::LmStudio).await;
    }

    #[tokio::test]
    async fn validate_key_maps_401_to_invalid_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let provider =
            OpenAICompatProvider::ollama(server.uri(), Some("bad".into()), "m1".to_string());
        assert_eq!(
            provider.validate_key("bad").await,
            ValidationResult::InvalidKey
        );
    }

    // ===========================================================
    // stream_turn — text-only / single tool / multi-tool / harmony /
    // reasoning / 5xx retry / mid-stream error
    // ===========================================================

    fn sse_body(frames: &[&str]) -> String {
        let mut out = String::new();
        for frame in frames {
            out.push_str(&format!("data: {frame}\n\n"));
        }
        out.push_str("data: [DONE]\n\n");
        out
    }

    async fn collect_events(
        mut stream: BoxStream<'static, StreamEvent>,
    ) -> Vec<ProviderStreamEvent> {
        let mut out = Vec::new();
        while let Some(ev) = stream.next().await {
            out.push(ev);
        }
        out
    }

    fn complete_block<'a>(events: &'a [ProviderStreamEvent]) -> &'a Block {
        let turn = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Complete(t) => Some(t),
                _ => None,
            })
            .expect("expected Complete event");
        turn.content.first().expect("expected at least one block")
    }

    async fn stream_text_only(preset: OpenAiCompatPreset) {
        let server = MockServer::start().await;
        let body = sse_body(&[
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#,
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":"stop"}]}"#,
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider = match preset {
            OpenAiCompatPreset::Ollama => {
                OpenAICompatProvider::ollama(server.uri(), None, "m".to_string())
            }
            OpenAiCompatPreset::LmStudio => {
                OpenAICompatProvider::lm_studio(server.uri(), None, "m".to_string())
            }
        };
        let stream = provider
            .stream_turn(&[ApiMessage::user_text("hi")], &system_bundle(), &empty_manifest(), LlmRequestOptions::default())
            .await
            .unwrap();
        let events = collect_events(stream).await;
        let Block::Text(t) = complete_block(&events) else {
            panic!("expected Text");
        };
        assert_eq!(t, "hello world");
    }

    #[tokio::test]
    async fn stream_text_only_ollama() {
        stream_text_only(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn stream_text_only_lm_studio() {
        stream_text_only(OpenAiCompatPreset::LmStudio).await;
    }

    async fn stream_single_tool_call(preset: OpenAiCompatPreset) {
        let server = MockServer::start().await;
        let body = sse_body(&[
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"git_status","arguments":"{\"path\":\".\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider = match preset {
            OpenAiCompatPreset::Ollama => {
                OpenAICompatProvider::ollama(server.uri(), None, "m".to_string())
            }
            OpenAiCompatPreset::LmStudio => {
                OpenAICompatProvider::lm_studio(server.uri(), None, "m".to_string())
            }
        };
        let stream = provider
            .stream_turn(&[ApiMessage::user_text("status")], &system_bundle(), &empty_manifest(), LlmRequestOptions::default())
            .await
            .unwrap();
        let events = collect_events(stream).await;
        let turn = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Complete(t) => Some(t),
                _ => None,
            })
            .unwrap();
        assert_eq!(turn.outcome, TurnOutcome::ToolUse);
        let Block::ToolCall { name, args, .. } = &turn.content[0] else {
            panic!("expected ToolCall");
        };
        assert_eq!(name, "git_status");
        assert_eq!(args, &json!({"path": "."}));
    }

    #[tokio::test]
    async fn stream_single_tool_call_ollama() {
        stream_single_tool_call(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn stream_single_tool_call_lm_studio() {
        stream_single_tool_call(OpenAiCompatPreset::LmStudio).await;
    }

    async fn stream_multi_tool_call(preset: OpenAiCompatPreset) {
        let server = MockServer::start().await;
        let body = sse_body(&[
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"tool_a","arguments":"{}"}},{"index":1,"id":"b","type":"function","function":{"name":"tool_b","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider = match preset {
            OpenAiCompatPreset::Ollama => {
                OpenAICompatProvider::ollama(server.uri(), None, "m".to_string())
            }
            OpenAiCompatPreset::LmStudio => {
                OpenAICompatProvider::lm_studio(server.uri(), None, "m".to_string())
            }
        };
        let stream = provider
            .stream_turn(&[ApiMessage::user_text("multi")], &system_bundle(), &empty_manifest(), LlmRequestOptions::default())
            .await
            .unwrap();
        let events = collect_events(stream).await;
        let turn = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Complete(t) => Some(t),
                _ => None,
            })
            .unwrap();
        assert_eq!(turn.content.len(), 2);
    }

    #[tokio::test]
    async fn stream_multi_tool_call_ollama() {
        stream_multi_tool_call(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn stream_multi_tool_call_lm_studio() {
        stream_multi_tool_call(OpenAiCompatPreset::LmStudio).await;
    }

    async fn stream_harmony_leak_sanitized(preset: OpenAiCompatPreset) {
        let server = MockServer::start().await;
        let body = sse_body(&[
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","type":"function","function":{"name":"git_status<|channel|>analysis","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider = match preset {
            OpenAiCompatPreset::Ollama => {
                OpenAICompatProvider::ollama(server.uri(), None, "gpt-oss:120b".to_string())
            }
            OpenAiCompatPreset::LmStudio => {
                OpenAICompatProvider::lm_studio(server.uri(), None, "gpt-oss:120b".to_string())
            }
        };
        let stream = provider
            .stream_turn(&[ApiMessage::user_text("x")], &system_bundle(), &empty_manifest(), LlmRequestOptions::default())
            .await
            .unwrap();
        let events = collect_events(stream).await;
        let turn = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Complete(t) => Some(t),
                _ => None,
            })
            .unwrap();
        let Block::ToolCall { name, .. } = &turn.content[0] else {
            panic!("expected ToolCall");
        };
        assert_eq!(name, "git_status", "harmony token must be stripped");
    }

    #[tokio::test]
    async fn stream_harmony_leak_sanitized_ollama() {
        stream_harmony_leak_sanitized(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn stream_harmony_leak_sanitized_lm_studio() {
        stream_harmony_leak_sanitized(OpenAiCompatPreset::LmStudio).await;
    }

    async fn stream_reasoning_via_primary(preset: OpenAiCompatPreset) {
        let server = MockServer::start().await;
        let body = sse_body(&[
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"reasoning":"thinking..."},"finish_reason":null}]}"#,
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"stop"}]}"#,
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider = match preset {
            OpenAiCompatPreset::Ollama => {
                OpenAICompatProvider::ollama(server.uri(), None, "gpt-oss:20b".to_string())
            }
            OpenAiCompatPreset::LmStudio => {
                OpenAICompatProvider::lm_studio(server.uri(), None, "gpt-oss:20b".to_string())
            }
        };
        let stream = provider
            .stream_turn(&[ApiMessage::user_text("think")], &system_bundle(), &empty_manifest(), LlmRequestOptions::default())
            .await
            .unwrap();
        let events = collect_events(stream).await;
        let turn = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Complete(t) => Some(t),
                _ => None,
            })
            .unwrap();
        // Reasoning block first, text block second.
        assert_eq!(turn.content.len(), 2);
        let Block::ProviderOpaque(opaque) = &turn.content[0] else {
            panic!("expected ProviderOpaque first");
        };
        assert_eq!(opaque["kind"], "reasoning");
        assert_eq!(opaque["content"], "thinking...");
    }

    #[tokio::test]
    async fn stream_reasoning_via_primary_ollama() {
        stream_reasoning_via_primary(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn stream_reasoning_via_primary_lm_studio() {
        stream_reasoning_via_primary(OpenAiCompatPreset::LmStudio).await;
    }

    async fn stream_reasoning_via_alias(preset: OpenAiCompatPreset) {
        let server = MockServer::start().await;
        let body = sse_body(&[
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"reasoning_content":"alias thinking"},"finish_reason":null}]}"#,
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"stop"}]}"#,
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider = match preset {
            OpenAiCompatPreset::Ollama => {
                OpenAICompatProvider::ollama(server.uri(), None, "qwen3.6:35b".to_string())
            }
            OpenAiCompatPreset::LmStudio => {
                OpenAICompatProvider::lm_studio(server.uri(), None, "qwen3.6:35b".to_string())
            }
        };
        let stream = provider
            .stream_turn(&[ApiMessage::user_text("think")], &system_bundle(), &empty_manifest(), LlmRequestOptions::default())
            .await
            .unwrap();
        let events = collect_events(stream).await;
        let turn = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Complete(t) => Some(t),
                _ => None,
            })
            .unwrap();
        let Block::ProviderOpaque(opaque) = &turn.content[0] else {
            panic!("expected ProviderOpaque");
        };
        assert_eq!(opaque["content"], "alias thinking");
    }

    #[tokio::test]
    async fn stream_reasoning_via_alias_ollama() {
        stream_reasoning_via_alias(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn stream_reasoning_via_alias_lm_studio() {
        stream_reasoning_via_alias(OpenAiCompatPreset::LmStudio).await;
    }

    async fn stream_5xx_retry_succeeds(preset: OpenAiCompatPreset) {
        // First call returns 502; second succeeds. Wiremock's
        // up_to_n_times sequencing handles this.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(502))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let body = sse_body(&[
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"recovered"},"finish_reason":"stop"}]}"#,
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider = match preset {
            OpenAiCompatPreset::Ollama => {
                OpenAICompatProvider::ollama(server.uri(), None, "m".to_string())
            }
            OpenAiCompatPreset::LmStudio => {
                OpenAICompatProvider::lm_studio(server.uri(), None, "m".to_string())
            }
        };
        let stream = provider
            .stream_turn(&[ApiMessage::user_text("hi")], &system_bundle(), &empty_manifest(), LlmRequestOptions::default())
            .await
            .unwrap();
        let events = collect_events(stream).await;
        let Block::Text(t) = complete_block(&events) else {
            panic!("expected Text");
        };
        assert_eq!(t, "recovered");
    }

    #[tokio::test]
    async fn stream_5xx_retry_succeeds_ollama() {
        stream_5xx_retry_succeeds(OpenAiCompatPreset::Ollama).await;
    }
    #[tokio::test]
    async fn stream_5xx_retry_succeeds_lm_studio() {
        stream_5xx_retry_succeeds(OpenAiCompatPreset::LmStudio).await;
    }

    #[tokio::test]
    async fn stream_4xx_does_not_retry() {
        // 4xx other than 5xx must not retry. Bad-bearer 401 case.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .expect(1)
            .mount(&server)
            .await;
        let provider =
            OpenAICompatProvider::ollama(server.uri(), Some("bad".into()), "m".to_string());
        let err = provider
            .stream_turn(&[ApiMessage::user_text("hi")], &system_bundle(), &empty_manifest(), LlmRequestOptions::default())
            .await
            .err()
            .expect("expected error");
        match err {
            ProviderError::Http { status, .. } => assert_eq!(status, 401),
            other => panic!("expected Http 401, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_persistent_5xx_propagates_after_retry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(503))
            .expect(2)
            .mount(&server)
            .await;
        let provider = OpenAICompatProvider::ollama(server.uri(), None, "m".to_string());
        let err = provider
            .stream_turn(&[ApiMessage::user_text("hi")], &system_bundle(), &empty_manifest(), LlmRequestOptions::default())
            .await
            .err()
            .expect("expected error");
        match err {
            ProviderError::Http { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Http 503, got {other:?}"),
        }
    }

    // ===========================================================
    // Provider trait basics + tool manifest
    // ===========================================================

    #[test]
    fn ollama_kind_and_display_name() {
        let p = OpenAICompatProvider::ollama(
            "http://localhost:11434".to_string(),
            None,
            "gpt-oss:20b".to_string(),
        );
        assert_eq!(p.kind(), ProviderKind::Ollama);
        assert_eq!(p.display_name(), "gpt-oss:20b");
    }

    #[test]
    fn lm_studio_kind_and_display_name() {
        let p = OpenAICompatProvider::lm_studio(
            "http://localhost:1234".to_string(),
            None,
            "qwen3.6:35b".to_string(),
        );
        assert_eq!(p.kind(), ProviderKind::LmStudio);
        assert_eq!(p.display_name(), "qwen3.6:35b");
    }

    #[test]
    fn build_tool_manifest_produces_sorted_array_with_fingerprint() {
        let p = OpenAICompatProvider::ollama(
            "http://x".to_string(),
            None,
            "m".to_string(),
        );
        let descriptors: Vec<&'static ToolDescriptor> =
            crate::tools::registry::all_descriptors().collect();
        let manifest = p.build_tool_manifest(&descriptors);
        // wire_json is a Vec<{type:"function",function:...}>
        let arr = manifest.wire_json.as_array().expect("array");
        assert!(arr.len() >= 70);
        for tool in arr {
            assert_eq!(tool["type"], "function");
        }
        assert_eq!(manifest.fingerprint.len(), 16, "16 hex chars");
    }

    #[test]
    fn preset_default_base_url_matches_documented_ports() {
        assert_eq!(
            OpenAiCompatPreset::Ollama.default_base_url(),
            "http://localhost:11434"
        );
        assert_eq!(
            OpenAiCompatPreset::LmStudio.default_base_url(),
            "http://localhost:1234"
        );
    }

    #[test]
    fn model_supports_reasoning_effort_recognizes_gpt_oss() {
        assert!(model_supports_reasoning_effort("gpt-oss:20b"));
        assert!(model_supports_reasoning_effort("gpt-oss:120b"));
        assert!(model_supports_reasoning_effort("openai/gpt-oss-120b"));
    }

    #[test]
    fn model_supports_reasoning_effort_recognizes_o_series() {
        assert!(model_supports_reasoning_effort("o1-mini"));
        assert!(model_supports_reasoning_effort("o1-preview"));
        assert!(model_supports_reasoning_effort("o3-mini"));
        assert!(model_supports_reasoning_effort("o3-pro-2026"));
        assert!(model_supports_reasoning_effort("o4-mini-preview"));
    }

    #[test]
    fn model_supports_reasoning_effort_recognizes_thinking_variants() {
        assert!(model_supports_reasoning_effort(
            "qwen3.6-32b-thinking-mlx-4bit"
        ));
        assert!(model_supports_reasoning_effort("Claude-3.5-thinking"));
        assert!(model_supports_reasoning_effort(
            "unsloth/Qwen3-thinking-32b"
        ));
    }

    #[test]
    fn model_supports_reasoning_effort_recognizes_deepseek_r() {
        assert!(model_supports_reasoning_effort("deepseek-r1:32b"));
        assert!(model_supports_reasoning_effort("DeepSeek-r2-7B"));
    }

    #[test]
    fn model_supports_reasoning_effort_rejects_standard_models() {
        // Non-reasoning models — sending reasoning_effort to these
        // produces "No valid custom reasoning fields found" warnings
        // in LM Studio's server logs.
        assert!(!model_supports_reasoning_effort(
            "unsloth/Qwen3.6-35B-A3B-UD-MLX-4bit"
        ));
        assert!(!model_supports_reasoning_effort("qwen3.6:35b"));
        assert!(!model_supports_reasoning_effort("llama3.2:8b"));
        assert!(!model_supports_reasoning_effort("mistral:7b"));
        assert!(!model_supports_reasoning_effort(
            "mlx-community/Llama-3.3-70B-Instruct"
        ));
        // Bare `o1` / `o3` / `o4` substrings shouldn't false-positive
        // (only the canonical OpenAI o-series-mini/pro/preview names).
        assert!(!model_supports_reasoning_effort("hello1"));
        assert!(!model_supports_reasoning_effort("o3"));
    }

    #[tokio::test]
    async fn stream_turn_sends_tool_choice_as_string_not_object() {
        // LM Studio rejects `tool_choice: {"type":"auto"}` with
        // `Invalid tool_choice type: 'object'`. OpenAI canonical
        // form is the plain string `"auto"`. This test asserts the
        // wire body sends the string form when tools are present.
        let server = MockServer::start().await;
        let body = sse_body(&[
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#,
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider =
            OpenAICompatProvider::lm_studio(server.uri(), None, "qwen3.6:35b".to_string());
        // Manifest with one tool so has_tools is true and tool_choice
        // is emitted.
        let manifest = ToolManifest {
            wire_json: json!([{
                "type": "function",
                "function": {"name": "test", "description": "x", "parameters": {"type":"object","properties":{}}}
            }]),
            fingerprint: "0".to_string(),
        };
        let _stream = provider
            .stream_turn(
                &[ApiMessage::user_text("hi")],
                &system_bundle(),
                &manifest,
                LlmRequestOptions::default(),
            )
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let body_json: JsonValue = serde_json::from_slice(&requests[0].body).unwrap();
        // Must be the string "auto", NOT an object {"type":"auto"}.
        assert_eq!(
            body_json["tool_choice"],
            json!("auto"),
            "tool_choice must be the canonical OpenAI string form"
        );
        assert!(
            !body_json["tool_choice"].is_object(),
            "tool_choice must not be an object (LM Studio rejects)"
        );
    }

    #[tokio::test]
    async fn stream_turn_omits_tool_choice_when_no_tools() {
        // No tools → tool_choice must be absent, not "none".
        let server = MockServer::start().await;
        let body = sse_body(&[
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#,
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider = OpenAICompatProvider::ollama(server.uri(), None, "m".to_string());
        let _stream = provider
            .stream_turn(
                &[ApiMessage::user_text("hi")],
                &system_bundle(),
                &empty_manifest(),
                LlmRequestOptions::default(),
            )
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let body_json: JsonValue = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(
            body_json.get("tool_choice").is_none(),
            "tool_choice must be omitted when no tools are present"
        );
        assert!(
            body_json.get("tools").is_none(),
            "tools array must also be omitted"
        );
    }

    #[tokio::test]
    async fn stream_turn_omits_reasoning_effort_for_non_reasoning_model() {
        // Capture the request body LM Studio receives and assert the
        // reasoning_effort field is absent for a standard model.
        let server = MockServer::start().await;
        let body = sse_body(&[
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#,
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(json!({
                "stream": true,
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider = OpenAICompatProvider::lm_studio(
            server.uri(),
            None,
            "unsloth/Qwen3.6-35B-A3B-UD-MLX-4bit".to_string(),
        );
        let _stream = provider
            .stream_turn(
                &[ApiMessage::user_text("hi")],
                &system_bundle(),
                &empty_manifest(),
                LlmRequestOptions::default(),
            )
            .await
            .unwrap();
        // Inspect the captured request via wiremock's received_requests.
        let requests = server.received_requests().await.unwrap();
        let req = requests.first().expect("expected one request");
        let body_str = std::str::from_utf8(&req.body).unwrap();
        let body_json: JsonValue = serde_json::from_str(body_str).unwrap();
        assert!(
            body_json.get("reasoning_effort").is_none(),
            "reasoning_effort must be omitted for non-reasoning models, got: {}",
            body_str
        );
    }

    #[tokio::test]
    async fn stream_turn_sends_reasoning_effort_for_gpt_oss() {
        let server = MockServer::start().await;
        let body = sse_body(&[
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#,
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider =
            OpenAICompatProvider::ollama(server.uri(), None, "gpt-oss:20b".to_string());
        let _stream = provider
            .stream_turn(
                &[ApiMessage::user_text("hi")],
                &system_bundle(),
                &empty_manifest(),
                LlmRequestOptions::default(),
            )
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let req = requests.first().expect("expected one request");
        let body_json: JsonValue = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(
            body_json.get("reasoning_effort").and_then(|v| v.as_str()),
            Some("high")
        );
    }

    #[test]
    fn should_retry_handles_5xx_and_connect_only() {
        // 5xx → retry.
        assert!(should_retry(&Err(ProviderError::Http {
            status: 503,
            body: String::new()
        })));
        // 4xx → no retry.
        assert!(!should_retry(&Err(ProviderError::Http {
            status: 401,
            body: String::new()
        })));
        // Network "connection refused" → retry.
        assert!(should_retry(&Err(ProviderError::Network(
            "connection refused: x".to_string()
        ))));
        // Network "timeout" → retry.
        assert!(should_retry(&Err(ProviderError::Network(
            "timeout connecting to x".to_string()
        ))));
        // Other network error → no retry.
        assert!(!should_retry(&Err(ProviderError::Network(
            "DNS resolution failed".to_string()
        ))));
        // Decode error → no retry.
        assert!(!should_retry(&Err(ProviderError::Decode(
            "json".to_string()
        ))));
    }

    // EarlyStopReason is referenced via the imports for clarity.
    #[allow(dead_code)]
    fn _early_stop_compile_check(r: EarlyStopReason) -> EarlyStopReason {
        r
    }
}
