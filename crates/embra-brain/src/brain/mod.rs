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
    /// Pre-built `tools` array for native tool-use requests. Constructed once
    /// at [`Brain::new`] from `crate::tools::registry::all_descriptors()`.
    /// The last entry carries `cache_control: ephemeral` so the entire tools
    /// block is a cache breakpoint shared across every turn.
    tools_json: Vec<serde_json::Value>,
}

impl Brain {
    pub fn new(api_key: String, system_prompt: String) -> Self {
        Self {
            api_key,
            http_client: Client::new(),
            system_prompt,
            tools_json: build_tools_snapshot(),
        }
    }

    pub fn set_system_prompt(&mut self, prompt: String) {
        self.system_prompt = prompt;
    }

    pub fn tool_count(&self) -> usize {
        self.tools_json.len()
    }

    /// Text-only streaming call — no tools declared. Used by the learning
    /// flow and slash-command synthetic turns where tool invocation is not
    /// desired.
    pub async fn send_message_streaming(
        &self,
        messages: &[Message],
    ) -> Result<mpsc::Receiver<StreamEvent>> {
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
        self.dispatch_stream(body).await
    }

    /// Native tool-use streaming call. `messages` carries typed blocks
    /// (thinking / tool_use / tool_result preserved verbatim). The
    /// request body includes the pre-built `tools` array and
    /// `tool_choice: "auto"` — the only `tool_choice` value legal under
    /// extended thinking.
    pub async fn send_message_streaming_with_tools(
        &self,
        messages: &[ApiMessage],
    ) -> Result<mpsc::Receiver<StreamEvent>> {
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
            "tools": self.tools_json,
            "tool_choice": {"type": "auto"},
            "messages": build_cached_api_messages(messages),
            "stream": true,
        });
        self.dispatch_stream(body).await
    }

    async fn dispatch_stream(
        &self,
        body: serde_json::Value,
    ) -> Result<mpsc::Receiver<StreamEvent>> {
        let (tx, rx) = mpsc::channel(128);
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
                        let _ = tx_clone.send(StreamEvent::Error(e.to_string())).await;
                    }
                }
                Err(e) => {
                    let _ = tx_clone.send(StreamEvent::Error(e.to_string())).await;
                }
            }
        });
        Ok(rx)
    }
}

/// Snapshot every registered `ToolDescriptor` into the Anthropic
/// tools-array JSON shape `{name, description, input_schema}`. The last
/// entry gets `cache_control: {"type": "ephemeral"}` so the entire tools
/// block becomes a cache breakpoint — stable across turns since the
/// registry is static after process start.
fn build_tools_snapshot() -> Vec<serde_json::Value> {
    use crate::tools::registry::all_descriptors;
    let mut tools: Vec<serde_json::Value> = all_descriptors()
        .map(|d| {
            json!({
                "name": d.name,
                "description": d.description,
                "input_schema": (d.input_schema)(),
            })
        })
        .collect();
    // Sort by name so the array order is deterministic across builds — keeps
    // the prompt cache key stable even if inventory iteration order shifts.
    tools.sort_by(|a, b| {
        a.get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .cmp(b.get("name").and_then(|n| n.as_str()).unwrap_or(""))
    });
    if let Some(last) = tools.last_mut() {
        if let Some(obj) = last.as_object_mut() {
            obj.insert("cache_control".into(), json!({"type": "ephemeral"}));
        }
    }
    tools
}

/// Build messages array (legacy String content) with prompt caching.
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

/// Serialize typed `ApiMessage` array and stamp a cache breakpoint on the
/// last text block of the second-to-last message. Thinking/tool blocks
/// serialize verbatim — critical for the API's verbatim thinking-sequence
/// requirement on follow-up tool_result turns.
fn build_cached_api_messages(messages: &[ApiMessage]) -> Vec<serde_json::Value> {
    let len = messages.len();
    messages
        .iter()
        .enumerate()
        .map(|(i, msg)| {
            let mut v = serde_json::to_value(msg).unwrap_or(serde_json::json!({}));
            if len >= 2 && i == len - 2 {
                // Prefer the LAST text block for the cache_control marker;
                // fall back to any block if no text block is present. This
                // avoids putting cache_control on a tool_result (legal but
                // unusual) whenever possible.
                if let Some(content) = v.get_mut("content").and_then(|c| c.as_array_mut()) {
                    let text_idx = content
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, b)| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                        .map(|(i, _)| i);
                    let target_idx = text_idx.unwrap_or_else(|| content.len().saturating_sub(1));
                    if let Some(block) = content.get_mut(target_idx) {
                        if let Some(obj) = block.as_object_mut() {
                            obj.insert("cache_control".into(), json!({"type": "ephemeral"}));
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

    #[test]
    fn tools_snapshot_is_sorted_and_last_has_cache_control() {
        let snapshot = build_tools_snapshot();
        if snapshot.is_empty() {
            return;
        }
        // Sorted by name ascending
        let names: Vec<&str> = snapshot
            .iter()
            .map(|t| t.get("name").and_then(|v| v.as_str()).unwrap_or(""))
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);

        // Last entry has cache_control
        let last = snapshot.last().unwrap();
        assert_eq!(last["cache_control"]["type"], "ephemeral");
        // Earlier entries do not
        if snapshot.len() > 1 {
            let earlier = &snapshot[snapshot.len() - 2];
            assert!(
                earlier.get("cache_control").is_none(),
                "only the last tool should carry cache_control"
            );
        }
    }

    #[test]
    fn tools_snapshot_is_nonempty_and_includes_known_tools() {
        let snapshot = build_tools_snapshot();
        assert!(
            snapshot.len() >= 70,
            "registry should have >=70 tools, got {}",
            snapshot.len()
        );
        let names: Vec<&str> = snapshot
            .iter()
            .map(|t| t.get("name").and_then(|v| v.as_str()).unwrap_or(""))
            .collect();
        for known in [
            "system_status",
            "recall",
            "remember",
            "git_status",
            "cron_add",
            "knowledge_query",
        ] {
            assert!(
                names.contains(&known),
                "expected {} in tool snapshot",
                known
            );
        }
    }

    #[test]
    fn build_cached_api_messages_marks_penultimate_text_block() {
        let msgs = vec![
            ApiMessage::user_text("first"),
            ApiMessage::assistant_blocks(vec![
                MessageBlock::Thinking {
                    thinking: String::new(),
                    signature: "sig".into(),
                },
                MessageBlock::Text {
                    text: "hello".into(),
                },
            ]),
            ApiMessage::user_text("second"),
        ];
        let out = build_cached_api_messages(&msgs);
        // Second-to-last (index 1, the assistant) — cache_control on the
        // Text block, not the Thinking block.
        let assistant_content = out[1]["content"].as_array().unwrap();
        assert_eq!(assistant_content.len(), 2);
        assert!(assistant_content[0].get("cache_control").is_none());
        assert_eq!(assistant_content[1]["cache_control"]["type"], "ephemeral");

        // Last message untouched
        assert!(out[2]["content"][0].get("cache_control").is_none());
    }
}
