//! Anthropic provider: `claude-opus-4-8` (default), `claude-opus-4-7`, or
//! `claude-fable-5` via `/v1/messages`. The model id is per-instance
//! (`with_model`); the request shape — adaptive thinking, `effort`
//! (default `max`), prompt-caching beta — is identical across supported
//! models, so switching models changes only the `model` field.
//!
//! Implements `LlmProvider` over the `/v1/messages` streaming endpoint.
//! Internal structure:
//! - [`wire`] — Anthropic-shaped block / message / response types.
//! - [`streaming`] — hand-rolled SSE parser that emits internal
//!   [`wire::AnthropicStreamEvent`]s.
//! - [`conv`] — neutral IR ↔ wire translators.
//! - [`tool_schema`] — Anthropic-specific tool manifest builder.

mod conv;
mod streaming;
mod tool_schema;
mod wire;

use anyhow::Result;
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, warn};

use crate::provider::{
    ApiMessage, AssistantTurn, LlmProvider, LlmRequestOptions, ProviderError, ProviderKind,
    StreamEvent, SystemPromptBundle, ToolManifest, ValidationResult,
};
use crate::tools::registry::ToolDescriptor;

/// Default Anthropic model when none is configured. `with_model` overrides
/// it (e.g. `claude-opus-4-7` or `claude-fable-5`); the resolver in
/// `grpc_service.rs` picks the active id from env/config.
pub const DEFAULT_MODEL: &str = "claude-opus-4-8";
const MAX_TOKENS: u32 = 128_000;
/// Default `output_config.effort`. Runtime-tunable via `/effort`
/// (`with_effort`); the full `low..max` range is valid on every
/// supported model (Opus 4.7/4.8, Fable 5).
const DEFAULT_EFFORT: &str = "max";
const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
const BETA: &str = "prompt-caching-2024-07-31";
const MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(10);
/// TCP + TLS establishment bound for the streaming client.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-read idle timeout for the streaming client. Resets on every byte
/// received; the API emits periodic SSE `ping` events well under this,
/// so a live stream can never trip it — it only unsticks a genuinely
/// dead connection. Do NOT replace with (or add) a total request
/// `.timeout()`: turns legitimately run many minutes at high effort
/// (especially Fable 5), and a total timeout would cut them mid-stream.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Exponential backoff ladder (seconds) for transient-error retries on
/// the initial POST (SDK-parity retryable set: 408/409/429/5xx — incl.
/// 529 overloaded — plus network send errors). Mid-stream errors are
/// never retried: a partial response cannot be recovered. Honors
/// `Retry-After` when present, capped at 60s. Ladder shared with the
/// Gemini provider precedent.
const RETRY_DELAYS_SECS: &[u64] = &[1, 2, 4, 8, 16, 32, 60];
/// Display name paired with [`DEFAULT_MODEL`] for the status bar.
pub const DEFAULT_DISPLAY_NAME: &str = "opus-4.8";

pub struct AnthropicProvider {
    api_key: String,
    /// API model id sent in the request body (e.g. `claude-opus-4-8`).
    model: String,
    /// Short display name (e.g. `opus-4.8`); backs the
    /// `LlmProvider::display_name` accessor, exercised in tests like the
    /// sibling Gemini / OpenAI-compat providers' equivalent field.
    display_name: String,
    /// `output_config.effort` sent in the request body. Defaults to
    /// [`DEFAULT_EFFORT`]; overridden per-instance via [`Self::with_effort`]
    /// from the `/effort`-persisted config value.
    effort: String,
    http: Client,
}

impl AnthropicProvider {
    /// Construct with the default model ([`DEFAULT_MODEL`]). Used by
    /// key-validation and tests where the model id is irrelevant.
    pub fn new(api_key: String) -> Self {
        Self::with_model(
            api_key,
            DEFAULT_MODEL.to_string(),
            DEFAULT_DISPLAY_NAME.to_string(),
        )
    }

    /// Construct with an explicit API model id + display name. The request
    /// shape is identical regardless of model — only the `model` field and
    /// the reported `display_name` differ.
    pub fn with_model(api_key: String, model: String, display_name: String) -> Self {
        // Hardened streaming client: bounded connect + per-read idle
        // timeouts only (see the const docs — a total timeout is
        // deliberately absent). Builder failure is not expected with
        // these options; fall back to the stock client rather than
        // panic in PID-1-adjacent code.
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_IDLE_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            api_key,
            model,
            display_name,
            effort: DEFAULT_EFFORT.to_string(),
            http,
        }
    }

    /// Override `output_config.effort` (builder-style, chained after
    /// [`Self::with_model`]). The caller validates the value against the
    /// API allowlist (`low|medium|high|xhigh|max`) — see
    /// `parse_anthropic_effort_choice` in `grpc_service.rs`.
    pub fn with_effort(mut self, effort: String) -> Self {
        self.effort = effort;
        self
    }

    /// Build the `/v1/messages` request body. Pure (no I/O) so the exact
    /// shape is unit-testable per model — the shape is identical across
    /// supported models (Opus 4.7/4.8, Fable 5); only `model` and the
    /// configured `effort` vary per instance.
    ///
    /// Request body matches the pre-refactor send_message_streaming_with_tools
    /// exactly — same model id, max_tokens, thinking config,
    /// output_config, system-as-content-block-with-cache,
    /// tool_choice: auto, prompt-caching beta header (set by the caller).
    ///
    /// `display: "summarized"` opts the API into emitting human-
    /// readable `thinking_delta` SSE events (translated to
    /// `StreamEvent::ReasoningDelta` for the live panel).
    /// `"omitted"` suppresses those deltas entirely. The API
    /// rejects any other value with 400 invalid_request_error
    /// ("Input should be 'summarized', 'omitted'") — do NOT change
    /// these strings without re-checking against the live API.
    /// Signature round-trip (signed `thinking` block carrying
    /// `signature_delta`) is unaffected by either setting and
    /// still rides via `Block::ProviderOpaque`.
    ///
    /// Fable 5 note: `claude-fable-5` rejects `thinking:{type:"disabled"}`,
    /// `budget_tokens`, and `temperature`/`top_p`/`top_k` with 400 — this
    /// body sends none of them, and `{type:"adaptive", display:…}` +
    /// `output_config.effort` are valid on it unchanged.
    fn request_body(
        &self,
        system_text: &str,
        wire_messages_json: Vec<serde_json::Value>,
        tools: &ToolManifest,
        options: &LlmRequestOptions,
    ) -> serde_json::Value {
        // Empty tool manifest → omit `tools` and `tool_choice` from
        // the request body. Anthropic accepts the request without them
        // (legacy text-only path used this shape for the learning
        // flow).
        let tools_empty = matches!(&tools.wire_json, serde_json::Value::Array(a) if a.is_empty());
        let thinking_display = if options.include_reasoning {
            "summarized"
        } else {
            "omitted"
        };
        let mut body = json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "thinking": {"type": "adaptive", "display": thinking_display},
            "output_config": {"effort": self.effort},
            "system": [{
                "type": "text",
                "text": system_text,
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": wire_messages_json,
            "stream": true,
        });
        if !tools_empty {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("tools".into(), tools.wire_json.clone());
                obj.insert("tool_choice".into(), json!({"type": "auto"}));
            }
        }
        body
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    async fn validate_key(&self, key: &str) -> ValidationResult {
        if key.is_empty() {
            return ValidationResult::InvalidKey;
        }
        if !key.starts_with("sk-") {
            return ValidationResult::InvalidKey;
        }
        let client = match Client::builder().timeout(VALIDATE_TIMEOUT).build() {
            Ok(c) => c,
            Err(_) => return ValidationResult::Unknown,
        };
        let resp = client
            .get(MODELS_URL)
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION)
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
        // Translate neutral IR → Anthropic wire shape.
        let wire_messages = conv::ir_messages_to_wire(messages);
        let wire_messages_json = build_cached_messages(&wire_messages);

        // Body construction lives in `request_body` (pure, unit-tested);
        // see its doc comment for the load-bearing shape notes.
        let body = self.request_body(&system.text, wire_messages_json, tools, &options);

        // Spawn the request + SSE consumer; events flow through an
        // mpsc channel to keep the parser code unchanged. The
        // ReceiverStream + map adapter translates wire events to
        // neutral StreamEvents. The POST goes through `send_with_retry`
        // INSIDE the spawn: terminal failures keep arriving in-stream
        // (`AnthropicStreamEvent::Error`) — the error path every caller
        // is built around — and the caller gets the stream back
        // immediately while any backoff runs.
        let http = self.http.clone();
        let api_key = self.api_key.clone();

        let (tx, rx) = mpsc::channel::<wire::AnthropicStreamEvent>(128);
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            match send_with_retry(&http, API_URL, &api_key, &body, RETRY_DELAYS_SECS).await {
                Ok(response) => {
                    if let Err(e) =
                        streaming::process_sse_stream(response, tx_clone.clone()).await
                    {
                        error!("SSE stream error: {}", e);
                        let _ = tx_clone
                            .send(wire::AnthropicStreamEvent::Error(e.to_string()))
                            .await;
                    }
                }
                Err(err) => {
                    let _ = tx_clone
                        .send(wire::AnthropicStreamEvent::Error(err.into_wire_message()))
                        .await;
                }
            }
        });

        // Translate wire events → neutral StreamEvents. Capture
        // `include_reasoning` so the closure can suppress thinking-delta
        // emission belt-and-suspenders even if the API returns it
        // (paranoid against operator opt-out being respected only at
        // the request body).
        let include_reasoning = options.include_reasoning;
        let stream = ReceiverStream::new(rx).filter_map(move |ev| {
            let include_reasoning = include_reasoning;
            async move {
                match ev {
                    wire::AnthropicStreamEvent::Token(s) => Some(StreamEvent::TextDelta(s)),
                    wire::AnthropicStreamEvent::ThinkingDelta(s) => {
                        if include_reasoning {
                            Some(StreamEvent::ReasoningDelta(s))
                        } else {
                            None
                        }
                    }
                    wire::AnthropicStreamEvent::Done(_) => {
                        // Anthropic emits a Done after Complete carrying
                        // the full accumulated text. The neutral stream
                        // surfaces all text via Complete(turn) — drop the
                        // Done duplicate to keep the contract clean.
                        None
                    }
                    wire::AnthropicStreamEvent::Error(s) => Some(StreamEvent::Error(s)),
                    wire::AnthropicStreamEvent::BlockComplete { .. } => {
                        Some(StreamEvent::BlockComplete)
                    }
                    wire::AnthropicStreamEvent::Complete { response } => {
                        let outcome = conv::stop_reason_to_outcome(response.stop_reason);
                        let stop_details =
                            response.stop_details.map(conv::wire_stop_details_to_ir);
                        let content = conv::wire_blocks_to_ir(response.content);
                        Some(StreamEvent::Complete(AssistantTurn {
                            content,
                            outcome,
                            usage: None,
                            stop_details,
                        }))
                    }
                }
            }
        });

        Ok(Box::pin(stream))
    }

    fn build_tool_manifest(&self, descriptors: &[&'static ToolDescriptor]) -> ToolManifest {
        let wire_json = tool_schema::build_tools_snapshot(descriptors);
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

/// Stamp `cache_control: ephemeral` on the last text block of the
/// second-to-last message. Preserves the pre-refactor caching
/// breakpoint logic verbatim. Thinking and tool blocks serialize as-is
/// — the API requires verbatim thinking-sequence preservation.
fn build_cached_messages(messages: &[wire::AnthropicWireMessage]) -> Vec<serde_json::Value> {
    let len = messages.len();
    messages
        .iter()
        .enumerate()
        .map(|(i, msg)| {
            let mut v = serde_json::to_value(msg).unwrap_or(json!({}));
            if len >= 2 && i == len - 2 {
                if let Some(content) = v.get_mut("content").and_then(|c| c.as_array_mut()) {
                    // Prefer the last text block; fall back to any
                    // block. Avoids placing cache_control on a
                    // tool_result whenever a text block exists.
                    let text_idx = content
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, b)| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                        .map(|(i, _)| i);
                    let target_idx = text_idx.unwrap_or_else(|| content.len().saturating_sub(1));
                    if let Some(block) = content.get_mut(target_idx) {
                        if let Some(obj) = block.as_object_mut() {
                            // Refuse to stamp on an empty text block —
                            // Anthropic rejects cache_control on empty
                            // content. (Empty assistant turns can
                            // happen when the model ends silently.)
                            let is_empty_text = obj.get("type").and_then(|t| t.as_str())
                                == Some("text")
                                && obj
                                    .get("text")
                                    .and_then(|t| t.as_str())
                                    .map(str::is_empty)
                                    .unwrap_or(false);
                            if !is_empty_text {
                                obj.insert("cache_control".into(), json!({"type": "ephemeral"}));
                            }
                        }
                    }
                }
            }
            v
        })
        .collect()
}

/// Terminal failure from [`send_with_retry`]. Private — it exists only
/// to be formatted into the in-stream [`wire::AnthropicStreamEvent::Error`]
/// string via [`Self::into_wire_message`]; it never crosses the
/// provider trait boundary.
#[derive(Debug)]
enum SendError {
    /// Non-2xx response: either non-retryable (`attempts == 1`) or
    /// retryable after exhausting the ladder. Status and body come from
    /// the SAME (last) response — tracked as one value so a mixed
    /// failure sequence can never pair a stale status with a later
    /// network-error message.
    Http {
        status: u16,
        body: String,
        attempts: usize,
    },
    /// reqwest send error (connect/write failure) on the last attempt.
    Network { msg: String, attempts: usize },
}

impl SendError {
    /// Render for the in-stream error event. Keeps the historical
    /// `"API error {status}: {body}"` family recognizable — and
    /// byte-identical to the pre-retry format when only one attempt was
    /// made (non-retryable status / lone network failure).
    fn into_wire_message(self) -> String {
        match self {
            Self::Http {
                status,
                body,
                attempts: 1,
            } => format!("API error {}: {}", status, body),
            Self::Http {
                status,
                body,
                attempts,
            } => format!("API error {} after {} attempts: {}", status, attempts, body),
            Self::Network { msg, attempts: 1 } => msg,
            Self::Network { msg, attempts } => {
                format!("network error after {} attempts: {}", attempts, msg)
            }
        }
    }
}

/// SDK-parity retryable set: 408 Request Timeout, 409 Conflict, 429
/// rate limit, and all 5xx (incl. 529 overloaded) — what Anthropic's
/// official clients retry.
fn is_retryable_status(code: u16) -> bool {
    matches!(code, 408 | 409 | 429) || (500..=599).contains(&code)
}

/// Parse an integer-seconds `Retry-After` value, capped at 60s. The
/// HTTP-date form parses as `None` — the ladder alone paces the retry.
fn parse_retry_after_secs(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok().map(|s| s.min(60))
}

/// POST `body` to `url`, retrying transient failures with the `delays`
/// backoff ladder ([`RETRY_DELAYS_SECS`] in production; tests pass
/// zeros). Mirrors the Gemini provider's helper: the sleep precedes
/// each retry attempt, a `Retry-After` (capped 60s) sleeps additively
/// on top of the next ladder step, and a retryable response's headers
/// are read before `text()` consumes it. Returns the first 2xx response
/// untouched — the SSE stream is consumed by the caller and is never
/// retried here (see [`RETRY_DELAYS_SECS`]).
async fn send_with_retry(
    http: &Client,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
    delays: &[u64],
) -> Result<reqwest::Response, SendError> {
    // Placeholder is unreachable: the iterator below always yields the
    // first (delay-0) attempt, and every arm either returns or
    // overwrites `last_err`.
    let mut last_err = SendError::Network {
        msg: "no attempts made".to_string(),
        attempts: 0,
    };

    for (attempt, delay_secs) in std::iter::once(0).chain(delays.iter().copied()).enumerate() {
        if delay_secs > 0 {
            tokio::time::sleep(Duration::from_secs(delay_secs)).await;
        }

        // RequestBuilder is consumed by `.send()` — rebuild the full
        // request each attempt (the in-crate pattern; the body bytes
        // are identical across attempts, so no prompt-cache impact).
        let send_result = http
            .post(url)
            .header("x-api-key", api_key)
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", BETA)
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
                // Read Retry-After BEFORE text() consumes the response.
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_retry_after_secs);
                let body_text = resp.text().await.unwrap_or_default();
                if !is_retryable_status(code) {
                    return Err(SendError::Http {
                        status: code,
                        body: body_text,
                        attempts: attempt + 1,
                    });
                }
                last_err = SendError::Http {
                    status: code,
                    body: body_text,
                    attempts: attempt + 1,
                };
                warn!(
                    target: "anthropic::http",
                    attempt = attempt + 1,
                    status = code,
                    retry_after_secs = retry_after,
                    "retryable Anthropic error; backing off"
                );
                if let Some(secs) = retry_after {
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                }
            }
            Err(e) => {
                last_err = SendError::Network {
                    msg: e.to_string(),
                    attempts: attempt + 1,
                };
                warn!(target: "anthropic::http", attempt = attempt + 1, "network error: {e}");
            }
        }
    }

    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ir::Block;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn new_defaults_to_opus_4_8() {
        let p = AnthropicProvider::new(String::new());
        assert_eq!(p.model, "claude-opus-4-8");
        assert_eq!(p.display_name(), "opus-4.8");
        assert_eq!(p.effort, "max");
    }

    #[test]
    fn with_model_sets_opus_4_7() {
        let p = AnthropicProvider::with_model(
            String::new(),
            "claude-opus-4-7".to_string(),
            "opus-4.7".to_string(),
        );
        assert_eq!(p.model, "claude-opus-4-7");
        assert_eq!(p.display_name(), "opus-4.7");
    }

    #[test]
    fn with_model_sets_fable_5() {
        let p = AnthropicProvider::with_model(
            String::new(),
            "claude-fable-5".to_string(),
            "fable-5".to_string(),
        );
        assert_eq!(p.model, "claude-fable-5");
        assert_eq!(p.display_name(), "fable-5");
    }

    // ===========================================================
    // send_with_retry — transient-error backoff (wiremock; zero-delay
    // ladders so the suite never sleeps)

    #[test]
    fn is_retryable_status_matches_sdk_set() {
        for code in [408u16, 409, 429, 500, 502, 503, 529, 599] {
            assert!(is_retryable_status(code), "{code} should be retryable");
        }
        for code in [400u16, 401, 403, 404, 413, 422] {
            assert!(!is_retryable_status(code), "{code} should not be retryable");
        }
    }

    #[test]
    fn parse_retry_after_secs_parses_and_caps() {
        assert_eq!(parse_retry_after_secs("5"), Some(5));
        assert_eq!(parse_retry_after_secs("0"), Some(0));
        assert_eq!(parse_retry_after_secs(" 7 "), Some(7));
        assert_eq!(parse_retry_after_secs("120"), Some(60)); // capped
        assert_eq!(parse_retry_after_secs(""), None);
        assert_eq!(parse_retry_after_secs("-1"), None);
        // HTTP-date form falls back to the ladder alone.
        assert_eq!(
            parse_retry_after_secs("Wed, 21 Oct 2026 07:28:00 GMT"),
            None
        );
    }

    #[test]
    fn send_error_wire_message_formats() {
        // attempts == 1 stays byte-identical to the pre-retry formats.
        let single_http = SendError::Http {
            status: 400,
            body: "bad".into(),
            attempts: 1,
        };
        assert_eq!(single_http.into_wire_message(), "API error 400: bad");
        let single_net = SendError::Network {
            msg: "connection refused".into(),
            attempts: 1,
        };
        assert_eq!(single_net.into_wire_message(), "connection refused");
        // Exhausted ladders carry the attempt count.
        let multi_http = SendError::Http {
            status: 529,
            body: "overloaded".into(),
            attempts: 8,
        };
        assert_eq!(
            multi_http.into_wire_message(),
            "API error 529 after 8 attempts: overloaded"
        );
        let multi_net = SendError::Network {
            msg: "timed out".into(),
            attempts: 3,
        };
        assert_eq!(
            multi_net.into_wire_message(),
            "network error after 3 attempts: timed out"
        );
    }

    #[tokio::test]
    async fn send_with_retry_recovers_after_529s() {
        // Two 529s, then a 200 that requires the auth/version/beta
        // headers — proving the per-attempt rebuild carries the full
        // header set. The expired first mock falls through to the
        // second (wiremock mount-order sequencing).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(529).set_body_string("overloaded"))
            .up_to_n_times(2)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "k"))
            .and(header("anthropic-version", API_VERSION))
            .and(header("anthropic-beta", BETA))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;
        let url = format!("{}/v1/messages", server.uri());
        let resp = send_with_retry(&Client::new(), &url, "k", &json!({"m": 1}), &[0, 0, 0])
            .await
            .expect("expected recovery on the third attempt");
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn send_with_retry_exhaustion_returns_last_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(529).set_body_string("overloaded"))
            .expect(3)
            .mount(&server)
            .await;
        let url = format!("{}/v1/messages", server.uri());
        let err = send_with_retry(&Client::new(), &url, "k", &json!({}), &[0, 0])
            .await
            .err()
            .expect("expected exhaustion");
        match &err {
            SendError::Http {
                status,
                body,
                attempts,
            } => {
                assert_eq!(*status, 529);
                assert_eq!(*attempts, 3);
                assert!(body.contains("overloaded"));
            }
            other => panic!("expected Http, got {other:?}"),
        }
        assert_eq!(
            err.into_wire_message(),
            "API error 529 after 3 attempts: overloaded"
        );
    }

    #[tokio::test]
    async fn send_with_retry_400_fails_after_single_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .expect(1)
            .mount(&server)
            .await;
        let url = format!("{}/v1/messages", server.uri());
        let err = send_with_retry(&Client::new(), &url, "k", &json!({}), &[0, 0])
            .await
            .err()
            .expect("expected immediate failure");
        // Byte-identical to the pre-retry error format.
        assert_eq!(err.into_wire_message(), "API error 400: bad request");
    }

    #[tokio::test]
    async fn send_with_retry_network_error_exhausts() {
        // Port 1 is never listening — connection refused on every
        // attempt, no server involved.
        let err = send_with_retry(
            &Client::new(),
            "http://127.0.0.1:1/v1/messages",
            "k",
            &json!({}),
            &[0],
        )
        .await
        .err()
        .expect("expected network failure");
        match &err {
            SendError::Network { attempts, .. } => assert_eq!(*attempts, 2),
            other => panic!("expected Network, got {other:?}"),
        }
        assert!(
            err.into_wire_message()
                .starts_with("network error after 2 attempts:")
        );
    }

    fn empty_manifest() -> ToolManifest {
        ToolManifest {
            wire_json: json!([]),
            fingerprint: String::new(),
        }
    }

    /// The request shape must be byte-identical across supported models
    /// (only `model` differs) and must never contain the fields Fable 5
    /// rejects with 400 (`temperature`/`top_p`/`top_k`, `budget_tokens`,
    /// `thinking.type: disabled`).
    #[test]
    fn fable_5_request_body_shape_matches_opus() {
        let opts = LlmRequestOptions {
            include_reasoning: true,
        };
        let tools = ToolManifest {
            wire_json: json!([{"name": "time", "description": "d", "input_schema": {}}]),
            fingerprint: String::new(),
        };
        let fable = AnthropicProvider::with_model(
            String::new(),
            "claude-fable-5".to_string(),
            "fable-5".to_string(),
        );
        let body = fable.request_body("sys", vec![json!({"role": "user"})], &tools, &opts);

        assert_eq!(body["model"], "claude-fable-5");
        assert_eq!(body["max_tokens"], MAX_TOKENS);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert_eq!(body["output_config"]["effort"], "max");
        assert_eq!(body["stream"], true);
        assert_eq!(body["tool_choice"]["type"], "auto");
        for forbidden in ["temperature", "top_p", "top_k"] {
            assert!(body.get(forbidden).is_none(), "{forbidden} must be absent");
        }
        assert!(body["thinking"].get("budget_tokens").is_none());

        // Same shape as an Opus instance modulo the model id.
        let opus = AnthropicProvider::with_model(
            String::new(),
            "claude-opus-4-8".to_string(),
            "opus-4.8".to_string(),
        );
        let mut opus_body =
            opus.request_body("sys", vec![json!({"role": "user"})], &tools, &opts);
        opus_body["model"] = json!("claude-fable-5");
        assert_eq!(body, opus_body);

        // display gating: include_reasoning=false → "omitted"; empty
        // manifest → tools/tool_choice omitted entirely.
        let body = fable.request_body(
            "sys",
            vec![],
            &empty_manifest(),
            &LlmRequestOptions {
                include_reasoning: false,
            },
        );
        assert_eq!(body["thinking"]["display"], "omitted");
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn request_body_carries_configured_effort() {
        let p = AnthropicProvider::new(String::new()).with_effort("high".to_string());
        let body = p.request_body(
            "sys",
            vec![],
            &empty_manifest(),
            &LlmRequestOptions {
                include_reasoning: true,
            },
        );
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn build_cached_api_messages_marks_penultimate_text_block() {
        let msgs = vec![
            ApiMessage::user_text("first"),
            ApiMessage::assistant_blocks(vec![
                Block::ProviderOpaque(json!({
                    "type": "thinking",
                    "thinking": "",
                    "signature": "sig"
                })),
                Block::Text("hello".into()),
            ]),
            ApiMessage::user_text("second"),
        ];
        let wire = conv::ir_messages_to_wire(&msgs);
        let out = build_cached_messages(&wire);
        let assistant_content = out[1]["content"].as_array().unwrap();
        assert_eq!(assistant_content.len(), 2);
        // Thinking comes first, no cache_control on it.
        assert_eq!(assistant_content[0]["type"], "thinking");
        assert!(assistant_content[0].get("cache_control").is_none());
        // Text gets the cache breakpoint.
        assert_eq!(assistant_content[1]["type"], "text");
        assert_eq!(assistant_content[1]["cache_control"]["type"], "ephemeral");

        // Last message untouched.
        assert!(out[2]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn validate_key_rejects_empty_and_non_sk_prefix() {
        let p = AnthropicProvider::new(String::new());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            assert_eq!(p.validate_key("").await, ValidationResult::InvalidKey);
            assert_eq!(p.validate_key("not-sk-shape").await, ValidationResult::InvalidKey);
        });
    }
}
