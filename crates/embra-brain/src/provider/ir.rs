//! Provider-neutral intermediate representation.
//!
//! The loop driver in `grpc_service.rs` and the system-prompt layer only
//! touch these types. Each `LlmProvider` translates IR ↔ its own wire
//! shape internally. Anthropic's `MessageBlock` / `ApiMessage` (in
//! `provider/anthropic/wire.rs` after Stage 2) and Gemini's
//! `GeminiContent` / `GeminiPart` (Stage 3) are wire types private to
//! their respective providers.
//!
//! Round-trip invariants:
//! - `Block::ProviderOpaque` and `Block::ToolCall.provider_opaque` are
//!   never inspected by the loop driver. Providers emit them verbatim
//!   on the next request.
//! - `Vec<Block>` order is load-bearing. For Gemini the first parallel
//!   `ToolCall.provider_opaque` carries the only `thoughtSignature`; for
//!   Anthropic, a `ProviderOpaque` (thinking block) must precede its
//!   paired `ToolCall` on the wire.

use serde_json::Value as JsonValue;

#[derive(Debug, Clone)]
pub enum Block {
    /// Plain text — user input, model prose, or stringified tool output.
    Text(String),

    /// Model-emitted tool invocation. `id` is the provider's call id
    /// (Anthropic `tool_use.id`, Gemini `functionCall.id`).
    ///
    /// `provider_opaque` carries any reasoning state the provider has
    /// associated with this call. Anthropic stores the preceding
    /// `{type: "thinking", thinking, signature}` JSON; Gemini stores the
    /// `thoughtSignature` string. The loop driver never inspects it.
    ToolCall {
        id: String,
        name: String,
        args: JsonValue,
        provider_opaque: Option<JsonValue>,
    },

    /// Tool's result, replayed on the next user turn. `call_id` matches
    /// the prior `ToolCall.id`.
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },

    /// Standalone provider reasoning state with no paired `ToolCall`.
    /// Used when a model emits a thinking/signature block on a turn that
    /// terminates without invoking any tool. Kept for verbatim replay.
    ProviderOpaque(JsonValue),
}

#[derive(Debug, Clone)]
pub enum ApiMessage {
    User { content: Vec<Block> },
    Assistant { content: Vec<Block> },
}

impl ApiMessage {
    pub fn user_text(s: impl Into<String>) -> Self {
        Self::User {
            content: vec![Block::Text(s.into())],
        }
    }

    pub fn user_tool_results(blocks: Vec<Block>) -> Self {
        debug_assert!(blocks.iter().all(|b| matches!(b, Block::ToolResult { .. })));
        Self::User { content: blocks }
    }

    pub fn assistant_blocks(blocks: Vec<Block>) -> Self {
        Self::Assistant { content: blocks }
    }

    pub fn content(&self) -> &[Block] {
        match self {
            ApiMessage::User { content } | ApiMessage::Assistant { content } => content,
        }
    }
}

/// Why the model stopped emitting tokens on a turn.
///
/// `Pause` is Anthropic's `pause_turn` (loop driver resends conversation
/// unchanged). `EarlyStop` covers the union of Anthropic refusals/stop
/// sequences and Gemini safety/recitation/malformed reasons — all are
/// terminal for the loop driver but distinguishable for telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    EndTurn,
    ToolUse,
    MaxTokens,
    Pause,
    EarlyStop(EarlyStopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EarlyStopReason {
    /// Anthropic `stop_sequence`.
    StopSequence,
    /// Anthropic `refusal`.
    Refusal,
    /// Gemini `SAFETY`.
    Safety,
    /// Gemini `RECITATION`.
    Recitation,
    /// Gemini `MALFORMED_FUNCTION_CALL`.
    Malformed,
    Other,
}

#[derive(Debug, Clone)]
pub struct AssistantTurn {
    pub content: Vec<Block>,
    pub outcome: TurnOutcome,
    /// Provider-specific usage JSON (token counts, cache stats). Used
    /// only for tracing — never for control flow.
    pub usage: Option<JsonValue>,
}

impl AssistantTurn {
    pub fn has_tool_call(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, Block::ToolCall { .. }))
    }

    pub fn has_text(&self) -> bool {
        self.content.iter().any(|b| matches!(b, Block::Text(_)))
    }
}
