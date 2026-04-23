use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Legacy message shape — still used by sessions on disk (SessionHistory
/// wraps `Vec<Message>`) and by the gRPC conversation save path. Stage 8
/// introduces a `format_version` bump and typed-block persistence; until
/// then, tool calls and thinking blocks are not reflected in history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}

// ── Native tool-use types (NATIVE-TOOLS-01 Stage 4) ──

/// A structural content block on a native-tool-use conversation turn.
///
/// Thinking blocks carry the model's extended-thinking signature. Per the
/// Anthropic spec, thinking blocks (including their signatures) MUST be
/// re-sent verbatim in every follow-up request that carries tool_result
/// blocks — the API rejects altered or reordered thinking sequences.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageBlock {
    /// Plain text. Used for user input and assistant prose.
    Text { text: String },
    /// Extended-thinking block. `thinking` is empty under display: omitted,
    /// but `signature` is always populated and must round-trip unchanged.
    Thinking {
        #[serde(default)]
        thinking: String,
        signature: String,
    },
    /// Assistant-emitted tool invocation. `input` is structured JSON
    /// matching the tool's input_schema.
    ToolUse {
        id: String,
        name: String,
        input: JsonValue,
    },
    /// Tool-result block sent back on a user turn, correlated by id.
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// In-flight conversation message for native tool-use flow.
///
/// The gRPC loop (Stage 5) builds a `Vec<ApiMessage>` for each Brain call,
/// appending the assistant's verbatim response (thinking blocks included)
/// between iterations. Distinct from the on-disk `Message` until Stage 8
/// schema-migrates sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ApiMessage {
    User { content: Vec<MessageBlock> },
    Assistant { content: Vec<MessageBlock> },
}

impl ApiMessage {
    pub fn user_text(s: impl Into<String>) -> Self {
        Self::User {
            content: vec![MessageBlock::Text { text: s.into() }],
        }
    }

    pub fn user_tool_results(blocks: Vec<MessageBlock>) -> Self {
        debug_assert!(blocks
            .iter()
            .all(|b| matches!(b, MessageBlock::ToolResult { .. })));
        Self::User { content: blocks }
    }

    pub fn assistant_blocks(blocks: Vec<MessageBlock>) -> Self {
        Self::Assistant { content: blocks }
    }
}

/// Reasons the model stopped producing tokens on a turn.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    Refusal,
    PauseTurn,
}

/// Fully assembled assistant response from a single API call.
/// Stage 5's loop inspects `stop_reason` to decide whether to continue.
#[derive(Debug, Clone, Deserialize)]
pub struct AssistantResponse {
    #[serde(default)]
    pub id: Option<String>,
    pub content: Vec<MessageBlock>,
    pub stop_reason: StopReason,
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    // Legacy UX streaming — gRPC forwards these to the console verbatim.
    Token(String),
    Done(String), // full accumulated text
    Error(String),
    // Native tool-use (Stage 4+).
    /// A single content block finished assembling.
    BlockComplete {
        block_index: usize,
        block: MessageBlock,
    },
    /// Stream ended; full typed response ready for the native loop.
    Complete {
        response: AssistantResponse,
    },
}

#[derive(Debug, Deserialize)]
pub struct ApiResponse {
    #[serde(default)]
    pub id: Option<String>,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
pub struct ContentBlock {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(default)]
    pub text: String,
}
