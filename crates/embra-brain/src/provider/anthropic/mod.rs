//! Anthropic provider: `claude-opus-4-7` via `/v1/messages`.
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
use tracing::error;

use crate::provider::{
    ApiMessage, AssistantTurn, LlmProvider, ProviderError, ProviderKind, StreamEvent,
    SystemPromptBundle, ToolManifest, ValidationResult,
};
use crate::tools::registry::ToolDescriptor;

const MODEL: &str = "claude-opus-4-7";
const MAX_TOKENS: u32 = 128_000;
const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
const BETA: &str = "prompt-caching-2024-07-31";
const MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(10);
const DISPLAY_NAME: &str = "opus-4.7";

pub struct AnthropicProvider {
    api_key: String,
    http: Client,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            http: Client::new(),
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn display_name(&self) -> &str {
        DISPLAY_NAME
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
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        // Translate neutral IR → Anthropic wire shape.
        let wire_messages = conv::ir_messages_to_wire(messages);
        let wire_messages_json = build_cached_messages(&wire_messages);

        // Empty tool manifest → omit `tools` and `tool_choice` from
        // the request body. Anthropic accepts the request without them
        // (legacy text-only path used this shape for the learning
        // flow).
        let tools_empty = matches!(&tools.wire_json, serde_json::Value::Array(a) if a.is_empty());

        // Request body matches the pre-refactor send_message_streaming_with_tools
        // exactly — same model id, max_tokens, thinking config,
        // output_config, system-as-content-block-with-cache,
        // tool_choice: auto, prompt-caching beta header.
        let mut body = json!({
            "model": MODEL,
            "max_tokens": MAX_TOKENS,
            "thinking": {"type": "adaptive", "display": "omitted"},
            "output_config": {"effort": "max"},
            "system": [{
                "type": "text",
                "text": &system.text,
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

        // Spawn the request + SSE consumer; events flow through an
        // mpsc channel to keep the parser code unchanged. The
        // ReceiverStream + map adapter translates wire events to
        // neutral StreamEvents.
        let request = self
            .http
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", BETA)
            .header("content-type", "application/json")
            .json(&body);

        let (tx, rx) = mpsc::channel::<wire::AnthropicStreamEvent>(128);
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            match request.send().await {
                Ok(response) => {
                    if !response.status().is_success() {
                        let status = response.status().as_u16();
                        let body = response.text().await.unwrap_or_default();
                        let _ = tx_clone
                            .send(wire::AnthropicStreamEvent::Error(format!(
                                "API error {}: {}",
                                status, body
                            )))
                            .await;
                        return;
                    }
                    if let Err(e) =
                        streaming::process_sse_stream(response, tx_clone.clone()).await
                    {
                        error!("SSE stream error: {}", e);
                        let _ = tx_clone
                            .send(wire::AnthropicStreamEvent::Error(e.to_string()))
                            .await;
                    }
                }
                Err(e) => {
                    let _ = tx_clone
                        .send(wire::AnthropicStreamEvent::Error(e.to_string()))
                        .await;
                }
            }
        });

        // Translate wire events → neutral StreamEvents.
        let stream = ReceiverStream::new(rx).filter_map(|ev| async move {
            match ev {
                wire::AnthropicStreamEvent::Token(s) => Some(StreamEvent::TextDelta(s)),
                wire::AnthropicStreamEvent::Done(_) => {
                    // Anthropic emits a Done after Complete carrying
                    // the full accumulated text. The neutral stream
                    // surfaces all text via Complete(turn) — drop the
                    // Done duplicate to keep the contract clean.
                    None
                }
                wire::AnthropicStreamEvent::Error(s) => Some(StreamEvent::Error(s)),
                wire::AnthropicStreamEvent::BlockComplete { .. } => Some(StreamEvent::BlockComplete),
                wire::AnthropicStreamEvent::Complete { response } => {
                    let outcome = conv::stop_reason_to_outcome(response.stop_reason);
                    let content = conv::wire_blocks_to_ir(response.content);
                    Some(StreamEvent::Complete(AssistantTurn {
                        content,
                        outcome,
                        usage: None,
                    }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ir::Block;

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
