//! Anthropic-specific wire types.
//!
//! These shapes mirror the `/v1/messages` request and SSE streaming
//! response. They are private to the Anthropic provider — the loop
//! driver works exclusively with `crate::provider::ir` (neutral IR).

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// A structural content block on a native-tool-use conversation turn.
///
/// Thinking blocks carry the model's extended-thinking signature. Per
/// the Anthropic spec, thinking blocks (including their signatures)
/// MUST be re-sent verbatim in every follow-up request that carries
/// tool_result blocks — the API rejects altered or reordered thinking
/// sequences.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageBlock {
    /// Plain text. Used for user input and assistant prose.
    Text { text: String },
    /// Extended-thinking block. `thinking` is empty under
    /// `display: omitted`, but `signature` is always populated and
    /// must round-trip unchanged.
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

/// In-flight conversation message in the Anthropic wire shape.
/// Used internally by the Anthropic provider; the loop driver consumes
/// `crate::provider::ir::ApiMessage` instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum AnthropicWireMessage {
    User { content: Vec<MessageBlock> },
    Assistant { content: Vec<MessageBlock> },
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
#[derive(Debug, Clone, Deserialize)]
pub struct AssistantResponse {
    #[serde(default)]
    pub id: Option<String>,
    pub content: Vec<MessageBlock>,
    pub stop_reason: StopReason,
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

/// Internal-only event type used between the SSE parser and the
/// `LlmProvider` adapter. The neutral `crate::provider::StreamEvent`
/// is what callers see.
#[derive(Debug, Clone)]
pub enum AnthropicStreamEvent {
    /// Text token from `text_delta` — forwarded to the TUI for live UX.
    Token(String),
    /// Full accumulated text on stream end. The provider synthesizes
    /// a gRPC `Done` from this.
    Done(String),
    Error(String),
    /// One block finalized.
    BlockComplete {
        block_index: usize,
        block: MessageBlock,
    },
    /// Stream end with full typed response ready for IR conversion.
    Complete { response: AssistantResponse },
}
