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
use std::sync::Arc;

/// One image on the wire, already normalized by the media ingest path
/// (≤ `media::MEDIA_LONG_EDGE_MAX` px long edge, ≤ `media::MEDIA_INLINE_MAX`
/// bytes). `data_b64` is the base64 of the ENCODED file — shared by `Arc`
/// because the whole history is re-converted into IR on every turn and
/// `Block: Clone` must stay cheap. Each provider emits it in its own
/// shape: Anthropic `image.source.base64`, Gemini `inlineData`,
/// OpenAI-compat `image_url` data URL.
#[derive(Debug, Clone)]
pub struct ImageData {
    /// `image/png` | `image/jpeg` | `image/gif` | `image/webp`.
    pub media_type: String,
    pub data_b64: Arc<str>,
    pub width: u32,
    pub height: u32,
    /// Operator-facing name (original filename or media id) — used for
    /// the `Image N (name):` labels and text fallbacks, never sent as a
    /// wire field on its own.
    pub name: String,
}

impl ImageData {
    /// `data:<media_type>;base64,<payload>` — the OpenAI-compat shape.
    pub fn to_data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data_b64)
    }
}

#[derive(Debug, Clone)]
pub enum Block {
    /// Plain text — user input, model prose, or stringified tool output.
    Text(String),

    /// Operator-attached image on a user turn. Emitted BEFORE the turn's
    /// text (images-then-text is the documented-best ordering); never
    /// appears on assistant turns (the APIs reject it there).
    Image(ImageData),

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
    /// the prior `ToolCall.id`. `images` are what a media tool
    /// (`image_view`, `image_generate`) hands the model alongside its
    /// text; empty for every other tool. Each provider has its own
    /// placement (Anthropic: inside the `tool_result` content array;
    /// Gemini/OpenAI-compat: see their `conv.rs`).
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
        images: Vec<ImageData>,
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

    /// A user turn carrying operator-attached images followed by text.
    /// Images come first (documented-best ordering); when there are two
    /// or more, each is introduced by an `Image N (name):` text label so
    /// the model and the operator can refer to them by number. `text`
    /// must be non-empty — the loop driver substitutes a placeholder for
    /// image-only messages so no provider ever sees a text-less turn.
    pub fn user_with_images(images: Vec<ImageData>, text: impl Into<String>) -> Self {
        let text = text.into();
        let mut content = Vec::with_capacity(images.len() * 2 + 1);
        let label = images.len() > 1;
        for (i, img) in images.into_iter().enumerate() {
            if label {
                content.push(Block::Text(format!("Image {} ({}):", i + 1, img.name)));
            }
            content.push(Block::Image(img));
        }
        content.push(Block::Text(text));
        Self::User { content }
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
    /// Operator-requested interrupt (`/stop` → the StopTurn RPC). Never
    /// produced by a provider wire mapper — the loop driver synthesizes it
    /// when the stop generation advances mid-turn.
    OperatorStop,
    Other,
}

/// Structured detail accompanying a terminal early stop. Anthropic
/// populates it only on `stop_reason: "refusal"` (category values seen:
/// `cyber`, `bio`, `reasoning_extraction`, `frontier_llm`; both fields
/// are optional even then — guard every read). Other providers leave it
/// `None` today; Gemini safety detail could ride here later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopDetails {
    pub category: Option<String>,
    pub explanation: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AssistantTurn {
    pub content: Vec<Block>,
    pub outcome: TurnOutcome,
    /// Provider-specific usage JSON (token counts, cache stats). Used
    /// only for tracing — never for control flow.
    pub usage: Option<JsonValue>,
    /// Populated only alongside `TurnOutcome::EarlyStop(Refusal)` — the
    /// loop driver folds it into the operator-facing refusal notice.
    pub stop_details: Option<StopDetails>,
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

#[cfg(test)]
mod image_message_tests {
    use super::*;

    fn img(name: &str) -> ImageData {
        ImageData {
            media_type: "image/png".into(),
            data_b64: Arc::from("AAAA"),
            width: 1,
            height: 1,
            name: name.into(),
        }
    }

    #[test]
    fn user_with_images_images_precede_text() {
        let msg = ApiMessage::user_with_images(vec![img("a.png")], "describe");
        let c = msg.content();
        assert_eq!(c.len(), 2);
        assert!(matches!(c[0], Block::Image(_)));
        assert!(matches!(&c[1], Block::Text(t) if t == "describe"));
    }

    #[test]
    fn user_with_images_labels_only_when_multiple() {
        let msg = ApiMessage::user_with_images(vec![img("a.png"), img("b.png")], "compare");
        let c = msg.content();
        assert_eq!(c.len(), 5);
        assert!(matches!(&c[0], Block::Text(t) if t == "Image 1 (a.png):"));
        assert!(matches!(c[1], Block::Image(_)));
        assert!(matches!(&c[2], Block::Text(t) if t == "Image 2 (b.png):"));
        assert!(matches!(c[3], Block::Image(_)));
        assert!(matches!(&c[4], Block::Text(t) if t == "compare"));
    }

    #[test]
    fn data_url_shape() {
        assert_eq!(img("x").to_data_url(), "data:image/png;base64,AAAA");
    }
}
