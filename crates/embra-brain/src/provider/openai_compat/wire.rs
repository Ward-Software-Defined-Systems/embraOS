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
        content: UserContent,
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

/// `user.content`: a bare string (every text-only turn — byte-identical
/// to the pre-media wire) or the parts array the vision models take.
/// Untagged: a `String` serializes as a JSON string, `Parts` as an array.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Parts(Vec<UserPart>),
}

impl From<String> for UserContent {
    fn from(s: String) -> Self {
        UserContent::Text(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrlRef },
}

/// `image_url.url` carries a `data:<mime>;base64,…` URL — embraOS never
/// sends remote URLs to a local model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrlRef {
    pub url: String,
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
    /// The string `"auto"` when tools are present; omitted when none. Not
    /// Anthropic's object form, which LM Studio rejects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<JsonValue>,
    /// The operator's `/effort` level as typed, or what the auto-map picks
    /// when none is set. Which values are accepted is the model's matter,
    /// not the server's; nothing here narrows them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

// ============================================================
// Streaming chunks (parsed in streaming.rs)
// ============================================================
//
// The receive-side structs name only the fields the parser reads; serde
// skips the rest. That is a contract, not an economy: a field declared
// here without a default is REQUIRED, and a chunk missing it fails to
// parse and is dropped whole (streaming.rs skips what it cannot parse).
// The envelope fields `id`, `object`, `created`, `model` and the choice
// `index` were once declared that way and never read, so a server that
// left one out lost every token. Servers differ in which they send.

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIChatChunk {
    /// Required. An in-stream `{"error": ...}` object has no `choices`,
    /// fails to parse, and is skipped with a warning.
    pub choices: Vec<OpenAIChoiceDelta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIChoiceDelta {
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
    pub data: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    pub id: String,
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
        assert_eq!(parsed.data.len(), 2);
        assert_eq!(parsed.data[0].id, "gpt-oss:20b");
        assert_eq!(parsed.data[1].id, "qwen3:8b");
    }

    #[test]
    fn models_response_needs_only_the_ids() {
        // The probe reads ids and nothing else; a server that omits the
        // list's `object` tag is still a server with models.
        let parsed: ModelsResponse =
            serde_json::from_value(json!({"data": [{"id": "qwen3:8b"}]})).unwrap();
        assert_eq!(parsed.data[0].id, "qwen3:8b");
        // An entry without an id is not a model.
        assert!(serde_json::from_value::<ModelsResponse>(json!({"data": [{"object": "model"}]}))
            .is_err());
    }

    #[test]
    fn chunk_parses_without_the_envelope_fields() {
        // Every token of a server that leaves out `id`, `object`,
        // `created`, `model` or the choice `index` used to be dropped:
        // the chunk failed to parse over a field nothing read.
        let chunk: OpenAIChatChunk = serde_json::from_value(json!({
            "choices": [{"delta": {"content": "hi"}}]
        }))
        .unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
        assert_eq!(chunk.choices[0].finish_reason, None);
        // The full envelope parses as before, null fields included.
        let chunk: OpenAIChatChunk = serde_json::from_value(json!({
            "id": null, "object": "chat.completion.chunk", "created": 1700000000u64,
            "model": "m", "usage": null,
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "hi"},
                         "finish_reason": "stop"}]
        }))
        .unwrap();
        assert_eq!(chunk.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn chunk_without_choices_is_rejected() {
        // What keeps an in-stream error object from being read as an
        // empty chunk: `choices` has no default.
        assert!(serde_json::from_value::<OpenAIChatChunk>(
            json!({"error": {"message": "model not loaded"}})
        )
        .is_err());
        // A usage-only final chunk carries an empty `choices` and parses.
        let chunk: OpenAIChatChunk =
            serde_json::from_value(json!({"choices": [], "usage": {"total_tokens": 9}})).unwrap();
        assert!(chunk.choices.is_empty());
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
