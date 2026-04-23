pub mod prompts;
mod streaming;
mod types;

pub use prompts::*;
pub use types::*;

use anyhow::Result;
use reqwest::Client;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::error;

const EMBRA_MODEL: &str = "claude-opus-4-7";
const EMBRA_MAX_TOKENS: u32 = 128_000;
const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const ANTHROPIC_BETA: &str = "prompt-caching-2024-07-31";

pub struct Brain {
    api_key: String,
    http_client: Client,
    pub system_prompt: String,
}

impl Brain {
    pub fn new(api_key: String, system_prompt: String) -> Self {
        Self {
            api_key,
            http_client: Client::new(),
            system_prompt,
        }
    }

    pub fn set_system_prompt(&mut self, prompt: String) {
        self.system_prompt = prompt;
    }

    pub async fn send_message_streaming(
        &self,
        messages: &[Message],
    ) -> Result<mpsc::Receiver<StreamEvent>> {
        let (tx, rx) = mpsc::channel(128);

        let body = json!({
            "model": EMBRA_MODEL,
            "max_tokens": EMBRA_MAX_TOKENS,
            "thinking": {"type": "adaptive", "display": "omitted"},
            "output_config": {"effort": "max"},
            "system": [{
                "type": "text",
                "text": self.system_prompt,
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": build_cached_messages(messages),
            "stream": true,
        });

        let request = self
            .http_client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("anthropic-beta", ANTHROPIC_BETA)
            .header("content-type", "application/json")
            .json(&body);

        let tx_clone = tx.clone();
        tokio::spawn(async move {
            match request.send().await {
                Ok(response) => {
                    if !response.status().is_success() {
                        let status = response.status().as_u16();
                        let body = response.text().await.unwrap_or_default();
                        let _ = tx_clone
                            .send(StreamEvent::Error(format!("API error {}: {}", status, body)))
                            .await;
                        return;
                    }
                    if let Err(e) = streaming::process_sse_stream(response, tx_clone.clone()).await
                    {
                        error!("SSE stream error: {}", e);
                        let _ = tx_clone
                            .send(StreamEvent::Error(e.to_string()))
                            .await;
                    }
                }
                Err(e) => {
                    let _ = tx_clone
                        .send(StreamEvent::Error(e.to_string()))
                        .await;
                }
            }
        });

        Ok(rx)
    }
}

/// Build messages array with prompt caching.
/// Places a cache breakpoint on the second-to-last message so all prior
/// conversation history is cached. The system prompt is cached separately.
fn build_cached_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    let len = messages.len();
    messages
        .iter()
        .enumerate()
        .map(|(i, msg)| {
            // Anthropic rejects cache_control on empty text blocks, so only
            // apply the breakpoint when the message actually has content.
            if len >= 2 && i == len - 2 && !msg.content.is_empty() {
                json!({
                    "role": msg.role,
                    "content": [{
                        "type": "text",
                        "text": msg.content,
                        "cache_control": {"type": "ephemeral"}
                    }]
                })
            } else {
                json!({
                    "role": msg.role,
                    "content": msg.content,
                })
            }
        })
        .collect()
}

