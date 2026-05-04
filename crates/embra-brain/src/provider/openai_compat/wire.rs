//! OpenAI Chat Completions wire types (`/v1/chat/completions`).
//!
//! Shared shape across Ollama and LM Studio OpenAI-compat backends.
//! These types are private to the OpenAI-compat provider; the loop
//! driver works exclusively with `crate::provider::ir`.
//!
//! Field naming notes per Step 0 verification:
//! - `function.arguments` is `String` (string-encoded JSON), confirmed
//!   verbatim from the OpenAI Python SDK source.
//! - `tool_call_id` is the canonical correlator on `role:"tool"`
//!   messages, NOT `tool_name`.
//! - Reasoning content has TWO field names in production: `reasoning`
//!   (cookbook-recommended primary, used by Ollama and older LM Studio)
//!   and `reasoning_content` (LM Studio newer default per 0.3.23+
//!   changelog). Both are deserialized; serialization emits
//!   `reasoning` (cookbook primary). See conv.rs for IR ↔ wire mapping.
//! - `finish_reason` enum values from SDK source: `stop`, `length`,
//!   `tool_calls`, `content_filter`, `function_call` (deprecated path).

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

// ============================================================
// Tool definitions (request body)
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAITool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: OpenAIToolFunction,
}

impl OpenAITool {
    pub fn function(name: String, description: String, parameters: JsonValue) -> Self {
        Self {
            tool_type: "function".to_string(),
            function: OpenAIToolFunction {
                name,
                description,
                parameters,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: JsonValue,
}

// ============================================================
// Tool calls (assistant-emitted, in messages and responses)
// ============================================================

/// Tool call emitted by the model. `function.arguments` is a
/// STRING-encoded JSON object, confirmed verbatim from the OpenAI
/// Python SDK source (`ChatCompletionMessageFunctionToolCall`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: OpenAIToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIToolCallFunction {
    pub name: String,
    /// String-encoded JSON. Per SDK source: "the model does not always
    /// generate valid JSON, and may hallucinate parameters not defined
    /// by your function schema. Validate the arguments in your code
    /// before calling your function."
    pub arguments: String,
}

// ============================================================
// Messages (request and response, tagged on `role`)
// ============================================================

/// Conversation message in OpenAI Chat Completions wire shape.
///
/// Both Ollama and LM Studio accept this exact shape. The discriminator
/// is `role`. Fields skipped on serialize when `None` keep the wire
/// payload small and avoid sending nulls that older servers might
/// reject.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum OpenAIMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<OpenAIToolCall>>,
        /// Cookbook-recommended primary field for raw CoT. Emitted on
        /// serialize when round-tripping reasoning blocks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        /// LM Studio newer-default alias. Deserialized for
        /// compatibility but never emitted on serialize (we always
        /// send the cookbook-recommended `reasoning` name).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

// ============================================================
// Request body
// ============================================================

#[derive(Debug, Clone, Serialize)]
pub struct OpenAIChatRequest {
    pub model: String,
    pub messages: Vec<OpenAIMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OpenAITool>>,
    /// `{"type": "auto"}` when tools are present; omit when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<JsonValue>,
    /// `"high"`, `"medium"`, `"low"`, `"none"` per Ollama docs (Q3.4).
    /// LM Studio accepts the same string values per #1250 resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

// ============================================================
// Non-streaming response
// ============================================================

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIChatResponse {
    pub id: String,
    pub choices: Vec<OpenAIChoice>,
    #[serde(default)]
    pub usage: Option<JsonValue>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIChoice {
    pub index: u32,
    pub message: OpenAIMessageOut,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// Out-of-band message variant used for non-streaming response parsing.
/// The full streaming/non-streaming roundtrip uses [`OpenAIMessage`];
/// this is the receive-side shape with both reasoning field aliases
/// flattened for defensive parsing.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIMessageOut {
    pub role: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<OpenAIToolCall>>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
}

// ============================================================
// Streaming chunks (Stage 2 will lean on these in streaming.rs)
// ============================================================

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIChatChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<OpenAIChoiceDelta>,
    #[serde(default)]
    pub usage: Option<JsonValue>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIChoiceDelta {
    pub index: u32,
    pub delta: OpenAIDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// Streaming delta. Fields are sparse — most chunks carry one of
/// `content`, `tool_calls`, `reasoning`/`reasoning_content`.
/// Reasoning has two field aliases per Step 0 C1; defensive accumulator
/// in `streaming.rs` checks `reasoning` first (cookbook primary), then
/// `reasoning_content` (LM Studio newer default).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenAIDelta {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<OpenAIToolCallDelta>>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIToolCallDelta {
    /// Correlator across chunks. First chunk for an `index` typically
    /// carries `id` and `function.name`; subsequent chunks carry only
    /// `function.arguments` shards which concatenate per-index.
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "type")]
    pub call_type: Option<String>,
    #[serde(default)]
    pub function: Option<OpenAIToolCallFunctionDelta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenAIToolCallFunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    /// Fragment of the string-encoded JSON. Concatenate across
    /// matching-index chunks; parse the accumulated buffer at
    /// `finish_reason` arrival.
    #[serde(default)]
    pub arguments: Option<String>,
}

// ============================================================
// GET /v1/models response (probe)
// ============================================================

#[derive(Debug, Clone, Deserialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub created: Option<i64>,
    #[serde(default)]
    pub owned_by: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn assistant_message_with_tool_calls_skips_null_content_on_serialize() {
        let msg = OpenAIMessage::Assistant {
            content: None,
            tool_calls: Some(vec![OpenAIToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: OpenAIToolCallFunction {
                    name: "git_status".to_string(),
                    arguments: "{\"path\":\".\"}".to_string(),
                },
            }]),
            reasoning: None,
            reasoning_content: None,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "assistant");
        assert!(v.get("content").is_none(), "content should be omitted, got {v}");
        assert_eq!(v["tool_calls"][0]["id"], "call_1");
        assert_eq!(v["tool_calls"][0]["function"]["name"], "git_status");
        // arguments must be a STRING in the wire, not an object.
        assert!(v["tool_calls"][0]["function"]["arguments"].is_string());
    }

    #[test]
    fn tool_message_uses_tool_call_id() {
        let msg = OpenAIMessage::Tool {
            tool_call_id: "call_1".to_string(),
            content: "ok".to_string(),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "tool");
        assert_eq!(v["tool_call_id"], "call_1");
        assert_eq!(v["content"], "ok");
    }

    #[test]
    fn reasoning_field_emits_on_serialize() {
        // Round-tripping reasoning content requires sending the
        // `reasoning` field (cookbook primary).
        let msg = OpenAIMessage::Assistant {
            content: Some("answer".to_string()),
            tool_calls: None,
            reasoning: Some("step 1...".to_string()),
            reasoning_content: None,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["reasoning"], "step 1...");
        assert!(v.get("reasoning_content").is_none());
    }

    #[test]
    fn deserialize_accepts_reasoning_alias() {
        // Servers that emit `reasoning_content` (LM Studio 0.3.23+)
        // must deserialize cleanly into the alias field.
        let raw = json!({
            "role": "assistant",
            "content": null,
            "reasoning_content": "thoughts here"
        });
        let msg: OpenAIMessage = serde_json::from_value(raw).unwrap();
        let OpenAIMessage::Assistant {
            reasoning,
            reasoning_content,
            ..
        } = msg
        else {
            panic!("expected assistant variant");
        };
        assert_eq!(reasoning, None);
        assert_eq!(reasoning_content, Some("thoughts here".to_string()));
    }

    #[test]
    fn deserialize_accepts_reasoning_primary() {
        // Servers that emit `reasoning` (cookbook primary, Ollama).
        let raw = json!({
            "role": "assistant",
            "content": "x",
            "reasoning": "think"
        });
        let msg: OpenAIMessage = serde_json::from_value(raw).unwrap();
        let OpenAIMessage::Assistant {
            reasoning,
            reasoning_content,
            ..
        } = msg
        else {
            panic!("expected assistant variant");
        };
        assert_eq!(reasoning, Some("think".to_string()));
        assert_eq!(reasoning_content, None);
    }

    #[test]
    fn streaming_delta_default_constructs_empty() {
        // Sparse deltas are common; default-construction must work
        // without unwrap_or chains in the parser.
        let d = OpenAIDelta::default();
        assert!(d.content.is_none());
        assert!(d.tool_calls.is_none());
        assert!(d.reasoning.is_none());
    }

    #[test]
    fn tool_call_delta_index_correlates_chunks() {
        // First chunk carries id+name; second carries only args shard.
        let first: OpenAIToolCallDelta = serde_json::from_value(json!({
            "index": 0,
            "id": "call_a",
            "type": "function",
            "function": {"name": "foo", "arguments": "{\"k\":"}
        }))
        .unwrap();
        let second: OpenAIToolCallDelta = serde_json::from_value(json!({
            "index": 0,
            "function": {"arguments": "\"v\"}"}
        }))
        .unwrap();
        assert_eq!(first.index, 0);
        assert_eq!(first.id.as_deref(), Some("call_a"));
        assert_eq!(first.function.as_ref().unwrap().name.as_deref(), Some("foo"));
        assert_eq!(second.index, 0);
        assert_eq!(second.id, None);
        assert_eq!(
            second.function.as_ref().unwrap().arguments.as_deref(),
            Some("\"v\"}")
        );
    }

    #[test]
    fn models_response_parses_minimal_shape() {
        let raw = json!({
            "object": "list",
            "data": [
                {"id": "gpt-oss:20b", "object": "model", "created": 1700000000, "owned_by": "library"},
                {"id": "qwen3:8b"}
            ]
        });
        let parsed: ModelsResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.object, "list");
        assert_eq!(parsed.data.len(), 2);
        assert_eq!(parsed.data[0].id, "gpt-oss:20b");
        assert_eq!(parsed.data[1].id, "qwen3:8b");
        assert_eq!(parsed.data[1].object, None);
    }

    #[test]
    fn finish_reason_string_round_trip() {
        let raw = json!({
            "index": 0,
            "delta": {"content": "hi"},
            "finish_reason": "tool_calls"
        });
        let parsed: OpenAIChoiceDelta = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.finish_reason.as_deref(), Some("tool_calls"));
    }
}
