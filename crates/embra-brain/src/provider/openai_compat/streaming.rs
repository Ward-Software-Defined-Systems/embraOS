//! OpenAI Chat Completions SSE stream parser.
//!
//! Hand-rolled, line-buffered SSE consumer matching the Anthropic and
//! Gemini parser shapes but adapted to OpenAI's chunk format and
//! tool-call argument-shard assembly state machine:
//!
//! ```json
//! {
//!   "id": "...",
//!   "object": "chat.completion.chunk",
//!   "choices": [{
//!     "index": 0,
//!     "delta": {"content": "...", "tool_calls": [...], "reasoning": "..."},
//!     "finish_reason": null | "stop" | "tool_calls" | "length" | "content_filter"
//!   }]
//! }
//! ```
//!
//! Streaming semantics:
//! - `delta.content` shards append to a running text buffer; each
//!   chunk emits a [`StreamEvent::TextDelta`] for live TUI feedback.
//! - `delta.tool_calls[].index` correlates fragments across chunks.
//!   First fragment for an index typically carries `id` + `function.name`;
//!   subsequent fragments carry only `function.arguments` shards which
//!   concatenate into a per-index buffer. Final args parsed at terminal.
//! - `delta.reasoning` (cookbook primary) and `delta.reasoning_content`
//!   (LM Studio newer default per 0.3.23+ changelog) are both checked;
//!   `reasoning` wins when both present per Step 0 C1 decision.
//!   Reasoning shards append to a running buffer and are NEVER emitted
//!   as `TextDelta` — raw CoT must not reach the operator-facing UI.
//!
//! Terminal handling:
//! - The chunk carrying `finish_reason` (or `data: [DONE]`) finalizes
//!   the turn. We assemble neutral-IR `Vec<Block>`:
//!   - If reasoning buffer non-empty: `Block::ProviderOpaque(json!({
//!     "kind":"reasoning","content":"..."}))`
//!   - If text buffer non-empty: `Block::Text(text)`
//!   - For each `PartialToolCall`: parse args, sanitize name, emit
//!     `Block::ToolCall` with `provider_opaque: None`
//! - `finish_reason` mapping:
//!   - `"stop"` + tool calls present → `TurnOutcome::ToolUse`
//!   - `"stop"` + no tool calls → `TurnOutcome::EndTurn`
//!   - `"tool_calls"` → `TurnOutcome::ToolUse`
//!   - `"length"` → `TurnOutcome::MaxTokens`
//!   - `"content_filter"` → `TurnOutcome::EarlyStop(Other)`
//!   - missing → `TurnOutcome::EndTurn` (defensive; logs a warn)

use std::collections::BTreeMap;

use anyhow::Result;
use futures_util::StreamExt;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

use crate::provider::ir::{AssistantTurn, Block, EarlyStopReason, TurnOutcome};
use crate::provider::openai_compat::sanitize::sanitize_harmony_tokens;
use crate::provider::openai_compat::wire::OpenAIChatChunk;
use crate::provider::StreamEvent;

/// Drive an SSE stream from `/v1/chat/completions`, emitting neutral
/// [`StreamEvent`]s into `tx`. Terminates on `[DONE]`, on the chunk
/// carrying `finish_reason`, or when the byte stream ends.
/// `include_reasoning` gates `StreamEvent::ReasoningDelta` emission for
/// `delta.reasoning` / `delta.reasoning_content` shards. The reasoning
/// buffer is always assembled regardless (so the terminal
/// `Block::ProviderOpaque` round-trip stays intact); only the live
/// delta emission is gated.
pub async fn process_sse_stream(
    response: reqwest::Response,
    tx: mpsc::Sender<StreamEvent>,
    model_id: String,
    include_reasoning: bool,
) -> Result<()> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut state = ParserState::new(model_id, include_reasoning);

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let text = String::from_utf8_lossy(&chunk);
        buffer.push_str(&text);
        consume_sse_lines(&mut buffer, &mut state, &tx).await;
        if state.completed {
            return Ok(());
        }
    }

    if !state.completed {
        state.emit_complete(&tx).await;
    }
    Ok(())
}

/// Drain complete SSE frames from `buffer` into the parser. Frames
/// without a trailing `\n` stay in the buffer for the next chunk.
async fn consume_sse_lines(
    buffer: &mut String,
    state: &mut ParserState,
    tx: &mpsc::Sender<StreamEvent>,
) {
    while let Some(newline_pos) = buffer.find('\n') {
        let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
        *buffer = buffer[newline_pos + 1..].to_string();

        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let Some(data) = line.strip_prefix("data: ").or_else(|| line.strip_prefix("data:")) else {
            continue;
        };
        let data = data.trim_start();
        if data == "[DONE]" {
            state.emit_complete(tx).await;
            return;
        }
        let Ok(chunk) = serde_json::from_str::<OpenAIChatChunk>(data) else {
            // Malformed JSON — log and continue. Real servers
            // occasionally emit keep-alive comments or junk; tolerate.
            tracing::warn!(
                target: "provider::openai_compat::streaming",
                data = %data,
                "could not parse SSE chunk; skipping"
            );
            continue;
        };
        state.process_chunk(chunk, tx).await;
        if state.completed {
            return;
        }
    }
}

/// In-flight assembly state. `pub(super)` so the test harness can
/// drive it without a real `reqwest::Response`.
pub(super) struct ParserState {
    pub(super) model_id: String,
    pub(super) text_buffer: String,
    pub(super) reasoning_buffer: String,
    /// Tool-call assembly keyed by `delta.tool_calls[].index`.
    /// BTreeMap so iteration order matches model-emitted index order.
    pub(super) tool_calls: BTreeMap<u32, PartialToolCall>,
    pub(super) finish_reason: Option<String>,
    pub(super) completed: bool,
    /// Mirror of `LlmRequestOptions.include_reasoning` — gates
    /// `StreamEvent::ReasoningDelta` emission. Reasoning buffer
    /// assembly continues unconditionally so the terminal
    /// `Block::ProviderOpaque` round-trip is unaffected.
    pub(super) include_reasoning: bool,
}

#[derive(Default, Debug)]
pub(super) struct PartialToolCall {
    pub id: Option<String>,
    pub name: Option<String>,
    pub args_buffer: String,
}

impl ParserState {
    pub(super) fn new(model_id: String, include_reasoning: bool) -> Self {
        Self {
            model_id,
            text_buffer: String::new(),
            reasoning_buffer: String::new(),
            tool_calls: BTreeMap::new(),
            finish_reason: None,
            completed: false,
            include_reasoning,
        }
    }

    pub(super) async fn process_chunk(
        &mut self,
        chunk: OpenAIChatChunk,
        tx: &mpsc::Sender<StreamEvent>,
    ) {
        for choice in chunk.choices {
            // Text content — emit live + accumulate.
            if let Some(content) = choice.delta.content {
                if !content.is_empty() {
                    self.text_buffer.push_str(&content);
                    let _ = tx.send(StreamEvent::TextDelta(content)).await;
                }
            }
            // Reasoning content — accumulate for round-trip and (when
            // operator opted in) emit as ReasoningDelta for the live
            // expression panel. NEVER emitted as TextDelta — the
            // ReasoningDelta privacy contract keeps it off
            // `full_response` / session history.
            // C1 ordering: reasoning primary, reasoning_content fallback.
            if let Some(r) = choice.delta.reasoning {
                self.reasoning_buffer.push_str(&r);
                if self.include_reasoning {
                    let _ = tx.send(StreamEvent::ReasoningDelta(r)).await;
                }
            } else if let Some(r) = choice.delta.reasoning_content {
                self.reasoning_buffer.push_str(&r);
                if self.include_reasoning {
                    let _ = tx.send(StreamEvent::ReasoningDelta(r)).await;
                }
            }
            // Tool-call deltas — accumulate per index.
            if let Some(deltas) = choice.delta.tool_calls {
                for d in deltas {
                    let entry = self.tool_calls.entry(d.index).or_default();
                    if let Some(id) = d.id {
                        entry.id.get_or_insert(id);
                    }
                    if let Some(func) = d.function {
                        if let Some(name) = func.name {
                            // First-fragment-wins for name: subsequent
                            // chunks should not carry name; if they do
                            // (server bug), preserve the first.
                            entry.name.get_or_insert(name);
                        }
                        if let Some(args_shard) = func.arguments {
                            entry.args_buffer.push_str(&args_shard);
                        }
                    }
                }
            }
            // Finish reason — mark terminal; the Complete will emit
            // when consume_sse_lines drains and we re-enter.
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
                self.emit_complete(tx).await;
                return;
            }
        }
    }

    pub(super) async fn emit_complete(&mut self, tx: &mpsc::Sender<StreamEvent>) {
        if self.completed {
            return;
        }
        self.completed = true;

        let mut content: Vec<Block> = Vec::new();

        // Reasoning first (cookbook recommendation: round-trip CoT
        // before the visible answer in our IR ordering).
        if !self.reasoning_buffer.is_empty() {
            content.push(Block::ProviderOpaque(serde_json::json!({
                "kind": "reasoning",
                "content": self.reasoning_buffer,
            })));
        }

        // Visible text.
        if !self.text_buffer.is_empty() {
            content.push(Block::Text(std::mem::take(&mut self.text_buffer)));
        }

        // Tool calls in index order.
        for (_idx, partial) in std::mem::take(&mut self.tool_calls) {
            let Some(id) = partial.id else {
                tracing::warn!(
                    target: "provider::openai_compat::streaming",
                    "tool_call delta accumulated without id; dropping"
                );
                continue;
            };
            let Some(name) = partial.name else {
                tracing::warn!(
                    target: "provider::openai_compat::streaming",
                    call_id = %id,
                    "tool_call delta accumulated without name; dropping"
                );
                continue;
            };
            let sanitized_name = sanitize_harmony_tokens(&name, &self.model_id).into_owned();
            let args = parse_tool_args(&partial.args_buffer);
            content.push(Block::ToolCall {
                id,
                name: sanitized_name,
                args,
                provider_opaque: None,
            });
        }

        let outcome = map_finish_reason(self.finish_reason.as_deref(), &content);
        let _ = tx
            .send(StreamEvent::Complete(AssistantTurn {
                content,
                outcome,
                usage: None,
            }))
            .await;
    }
}

fn map_finish_reason(reason: Option<&str>, content: &[Block]) -> TurnOutcome {
    let has_tool_calls = content.iter().any(|b| matches!(b, Block::ToolCall { .. }));
    match reason {
        Some("tool_calls") => TurnOutcome::ToolUse,
        Some("stop") => {
            if has_tool_calls {
                // Some servers emit "stop" with tool_calls present.
                TurnOutcome::ToolUse
            } else {
                TurnOutcome::EndTurn
            }
        }
        Some("length") => TurnOutcome::MaxTokens,
        Some("content_filter") => TurnOutcome::EarlyStop(EarlyStopReason::Other),
        Some(other) => {
            tracing::warn!(
                target: "provider::openai_compat::streaming",
                finish_reason = %other,
                "unrecognized finish_reason; treating as EndTurn"
            );
            TurnOutcome::EndTurn
        }
        None => {
            tracing::warn!(
                target: "provider::openai_compat::streaming",
                "stream ended with no finish_reason; treating as EndTurn"
            );
            TurnOutcome::EndTurn
        }
    }
}

/// Parse the accumulated `arguments` string into a JsonValue. On
/// parse failure (malformed JSON from the model), returns `{}` and
/// logs a warning; downstream tool dispatch will surface the bad-args
/// path naturally. Mirrors `conv::parse_tool_args` behavior.
fn parse_tool_args(raw: &str) -> JsonValue {
    if raw.is_empty() {
        return JsonValue::Object(serde_json::Map::new());
    }
    match serde_json::from_str::<JsonValue>(raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "provider::openai_compat::streaming",
                error = %e,
                raw = %raw,
                "accumulated tool args are not valid JSON; using empty object"
            );
            JsonValue::Object(serde_json::Map::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// In-memory test harness that drives the parser without an HTTP
    /// transport. Returns all StreamEvents and the final state.
    /// `include_reasoning: false` matches the existing tests'
    /// expectation that reasoning never streams; reasoning-on tests
    /// call `drive_fake_with_reasoning` instead.
    async fn drive_fake(sse_text: &str, model_id: &str) -> Vec<StreamEvent> {
        drive_fake_with_options(sse_text, model_id, false).await
    }

    async fn drive_fake_with_options(
        sse_text: &str,
        model_id: &str,
        include_reasoning: bool,
    ) -> Vec<StreamEvent> {
        let (tx, mut rx) = mpsc::channel(256);
        let mut buffer = sse_text.to_string();
        let mut state = ParserState::new(model_id.to_string(), include_reasoning);
        consume_sse_lines(&mut buffer, &mut state, &tx).await;
        if !state.completed {
            state.emit_complete(&tx).await;
        }
        drop(tx);
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        events
    }

    fn data_frame(payload: JsonValue) -> String {
        format!("data: {}\n\n", payload)
    }

    fn assistant_chunk_with_text(content: &str, finish_reason: Option<&str>) -> JsonValue {
        json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 1700000000u64,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "delta": {"content": content},
                "finish_reason": finish_reason,
            }]
        })
    }

    fn complete_event(events: &[StreamEvent]) -> &AssistantTurn {
        events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Complete(t) => Some(t),
                _ => None,
            })
            .expect("expected a Complete event")
    }

    fn text_deltas(events: &[StreamEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn text_only_response_assembles_to_end_turn() {
        let mut sse = String::new();
        sse.push_str(&data_frame(assistant_chunk_with_text("Hello, ", None)));
        sse.push_str(&data_frame(assistant_chunk_with_text("world!", Some("stop"))));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "test-model").await;
        assert_eq!(text_deltas(&events), vec!["Hello, ", "world!"]);
        let turn = complete_event(&events);
        assert_eq!(turn.outcome, TurnOutcome::EndTurn);
        assert_eq!(turn.content.len(), 1);
        let Block::Text(t) = &turn.content[0] else {
            panic!("expected Text");
        };
        assert_eq!(t, "Hello, world!");
    }

    #[tokio::test]
    async fn single_tool_call_with_shards() {
        // Args split mid-key, mid-value.
        let mut sse = String::new();
        // First chunk: id + name + opening brace.
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "git_status", "arguments": "{\"pa"}
                }]},
                "finish_reason": null
            }]
        })));
        // Second chunk: middle of arguments.
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "th\":\""}
                }]},
                "finish_reason": null
            }]
        })));
        // Third chunk: end.
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": ".\"}"}
                }]},
                "finish_reason": "tool_calls"
            }]
        })));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "test-model").await;
        let turn = complete_event(&events);
        assert_eq!(turn.outcome, TurnOutcome::ToolUse);
        let Block::ToolCall { id, name, args, .. } = &turn.content[0] else {
            panic!("expected ToolCall, got {:?}", turn.content);
        };
        assert_eq!(id, "call_1");
        assert_eq!(name, "git_status");
        assert_eq!(args, &json!({"path": "."}));
    }

    #[tokio::test]
    async fn multiple_tool_calls_correlate_by_index() {
        // Two interleaved tool calls — first chunk announces both,
        // subsequent chunks fill different args buffers.
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [
                    {"index": 0, "id": "call_a", "type": "function",
                     "function": {"name": "tool_a", "arguments": "{"}},
                    {"index": 1, "id": "call_b", "type": "function",
                     "function": {"name": "tool_b", "arguments": "{"}}
                ]},
                "finish_reason": null
            }]
        })));
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [
                    {"index": 0, "function": {"arguments": "\"k\":1}"}},
                    {"index": 1, "function": {"arguments": "\"k\":2}"}}
                ]},
                "finish_reason": "tool_calls"
            }]
        })));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "test-model").await;
        let turn = complete_event(&events);
        assert_eq!(turn.outcome, TurnOutcome::ToolUse);
        assert_eq!(turn.content.len(), 2);
        let Block::ToolCall { name: n1, args: a1, .. } = &turn.content[0] else {
            panic!("expected ToolCall");
        };
        let Block::ToolCall { name: n2, args: a2, .. } = &turn.content[1] else {
            panic!("expected ToolCall");
        };
        assert_eq!(n1, "tool_a");
        assert_eq!(a1, &json!({"k": 1}));
        assert_eq!(n2, "tool_b");
        assert_eq!(a2, &json!({"k": 2}));
    }

    #[tokio::test]
    async fn args_split_inside_quoted_string_assemble_correctly() {
        // The hardest shard split case: mid-quoted-string with embedded
        // braces in the value. Buffer must concatenate verbatim, not
        // try to parse incrementally.
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0, "id": "call_q", "type": "function",
                    "function": {"name": "echo", "arguments": "{\"msg\":\"a {b"}
                }]},
                "finish_reason": null
            }]
        })));
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "} c\"}"}
                }]},
                "finish_reason": "tool_calls"
            }]
        })));

        let events = drive_fake(&sse, "test-model").await;
        let turn = complete_event(&events);
        let Block::ToolCall { args, .. } = &turn.content[0] else {
            panic!("expected ToolCall");
        };
        assert_eq!(args["msg"], "a {b} c");
    }

    #[tokio::test]
    async fn done_terminator_without_finish_reason_still_emits_complete() {
        // Servers that emit [DONE] without a prior finish_reason —
        // we still produce a Complete with EndTurn (defensive).
        let mut sse = String::new();
        sse.push_str(&data_frame(assistant_chunk_with_text("hi", None)));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "test-model").await;
        let turn = complete_event(&events);
        assert_eq!(turn.outcome, TurnOutcome::EndTurn);
        assert_eq!(turn.content.len(), 1);
    }

    #[tokio::test]
    async fn malformed_json_chunk_is_skipped_without_panicking() {
        // Real servers occasionally emit comments or partially-flushed
        // garbage; the parser must tolerate.
        let mut sse = String::new();
        sse.push_str("data: not-valid-json\n\n");
        sse.push_str(&data_frame(assistant_chunk_with_text("ok", Some("stop"))));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "test-model").await;
        let turn = complete_event(&events);
        assert_eq!(turn.outcome, TurnOutcome::EndTurn);
        let Block::Text(t) = &turn.content[0] else {
            panic!("expected Text");
        };
        assert_eq!(t, "ok");
    }

    #[tokio::test]
    async fn empty_response_completes_with_end_turn_and_no_blocks() {
        // No content, no tool calls, no reasoning — just a finish_reason.
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        })));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "test-model").await;
        let turn = complete_event(&events);
        assert_eq!(turn.outcome, TurnOutcome::EndTurn);
        assert!(turn.content.is_empty());
    }

    #[tokio::test]
    async fn reasoning_via_primary_field_accumulates_to_provider_opaque() {
        // C1 path 1: delta.reasoning (cookbook primary, Ollama).
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"reasoning": "step 1\n"},
                "finish_reason": null
            }]
        })));
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"reasoning": "step 2", "content": "answer"},
                "finish_reason": "stop"
            }]
        })));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "gpt-oss:20b").await;
        // Reasoning should NOT have produced TextDelta events.
        assert_eq!(text_deltas(&events), vec!["answer"]);
        let turn = complete_event(&events);
        assert_eq!(turn.content.len(), 2);
        let Block::ProviderOpaque(opaque) = &turn.content[0] else {
            panic!("expected ProviderOpaque first");
        };
        assert_eq!(opaque["kind"], "reasoning");
        assert_eq!(opaque["content"], "step 1\nstep 2");
        let Block::Text(t) = &turn.content[1] else {
            panic!("expected Text second");
        };
        assert_eq!(t, "answer");
    }

    #[tokio::test]
    async fn reasoning_via_alias_field_accumulates_to_provider_opaque() {
        // C1 path 2: delta.reasoning_content (LM Studio newer default).
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"reasoning_content": "lm studio thoughts"},
                "finish_reason": null
            }]
        })));
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{"index": 0, "delta": {"content": "ans"}, "finish_reason": "stop"}]
        })));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "qwen3.6:35b").await;
        let turn = complete_event(&events);
        let Block::ProviderOpaque(opaque) = &turn.content[0] else {
            panic!("expected ProviderOpaque");
        };
        assert_eq!(opaque["content"], "lm studio thoughts");
    }

    #[tokio::test]
    async fn reasoning_primary_wins_when_both_keys_present() {
        // Defensive: server emitting both keys — reasoning wins per
        // C1 ordering decision.
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"reasoning": "primary", "reasoning_content": "alias"},
                "finish_reason": "stop"
            }]
        })));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "test-model").await;
        let turn = complete_event(&events);
        let Block::ProviderOpaque(opaque) = &turn.content[0] else {
            panic!("expected ProviderOpaque");
        };
        assert_eq!(opaque["content"], "primary");
    }

    #[tokio::test]
    async fn reasoning_emits_delta_when_include_reasoning_enabled() {
        // include_reasoning=true: each delta.reasoning shard fires a
        // StreamEvent::ReasoningDelta in order. The reasoning buffer
        // STILL assembles into the terminal Block::ProviderOpaque (so
        // the round-trip stays intact); the deltas are an ADDITIONAL
        // channel, not a replacement.
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"reasoning": "first shard"},
                "finish_reason": null
            }]
        })));
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"reasoning": " then more"},
                "finish_reason": null
            }]
        })));
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": "stop"}]
        })));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake_with_options(&sse, "gpt-oss:20b", true).await;
        let reasoning: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ReasoningDelta(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, vec!["first shard", " then more"]);

        // Still no TextDelta from reasoning content.
        assert_eq!(text_deltas(&events), vec!["ok"]);

        // ProviderOpaque carries assembled buffer for IR round-trip.
        let turn = complete_event(&events);
        let Block::ProviderOpaque(opaque) = &turn.content[0] else {
            panic!("expected ProviderOpaque first");
        };
        assert_eq!(opaque["content"], "first shard then more");
    }

    #[tokio::test]
    async fn reasoning_alias_emits_delta_when_enabled() {
        // delta.reasoning_content (LM Studio newer default) also fires
        // ReasoningDelta when include_reasoning=true.
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"reasoning_content": "lm studio cot"},
                "finish_reason": null
            }]
        })));
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{"index": 0, "delta": {"content": "done"}, "finish_reason": "stop"}]
        })));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake_with_options(&sse, "qwen3.6:35b", true).await;
        let reasoning: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ReasoningDelta(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, vec!["lm studio cot"]);
    }

    #[tokio::test]
    async fn reasoning_suppressed_when_include_reasoning_disabled() {
        // Default path (include_reasoning=false): reasoning shards
        // STILL assemble into the buffer (so the IR round-trip works)
        // but the parser MUST NOT emit a single ReasoningDelta. This is
        // the load-bearing privacy guard — operator opt-out at the
        // brain level still works even if the model sends reasoning.
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"reasoning": "leaked"},
                "finish_reason": "stop"
            }]
        })));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "gpt-oss:20b").await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, StreamEvent::ReasoningDelta(_))),
            "ReasoningDelta must not fire when include_reasoning=false"
        );
        // Still assembled for round-trip.
        let turn = complete_event(&events);
        let Block::ProviderOpaque(opaque) = &turn.content[0] else {
            panic!("expected ProviderOpaque");
        };
        assert_eq!(opaque["content"], "leaked");
    }

    #[tokio::test]
    async fn reasoning_interleaved_with_tool_calls() {
        // Reasoning shards arrive between tool-call shards — both
        // must accumulate into the right buffers.
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"reasoning": "considering options"},
                "finish_reason": null
            }]
        })));
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0, "id": "call_x", "type": "function",
                    "function": {"name": "git_status", "arguments": "{}"}
                }]},
                "finish_reason": null
            }]
        })));
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"reasoning": "; need to check"},
                "finish_reason": "tool_calls"
            }]
        })));

        let events = drive_fake(&sse, "test-model").await;
        let turn = complete_event(&events);
        assert_eq!(turn.outcome, TurnOutcome::ToolUse);
        assert_eq!(turn.content.len(), 2);
        let Block::ProviderOpaque(opaque) = &turn.content[0] else {
            panic!("expected ProviderOpaque");
        };
        assert_eq!(opaque["content"], "considering options; need to check");
        let Block::ToolCall { name, .. } = &turn.content[1] else {
            panic!("expected ToolCall");
        };
        assert_eq!(name, "git_status");
    }

    #[tokio::test]
    async fn harmony_leak_in_streamed_tool_name_sanitized() {
        // Tool-call name field arrives polluted with harmony tokens —
        // sanitize at terminal.
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0, "id": "c", "type": "function",
                    "function": {
                        "name": "exec<|channel|>analysis",
                        "arguments": "{}"
                    }
                }]},
                "finish_reason": "tool_calls"
            }]
        })));

        let events = drive_fake(&sse, "gpt-oss:120b").await;
        let turn = complete_event(&events);
        let Block::ToolCall { name, .. } = &turn.content[0] else {
            panic!("expected ToolCall");
        };
        assert_eq!(name, "exec");
    }

    #[tokio::test]
    async fn finish_reason_length_maps_to_max_tokens() {
        let mut sse = String::new();
        sse.push_str(&data_frame(assistant_chunk_with_text("part", Some("length"))));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "test-model").await;
        let turn = complete_event(&events);
        assert_eq!(turn.outcome, TurnOutcome::MaxTokens);
    }

    #[tokio::test]
    async fn finish_reason_content_filter_maps_to_early_stop() {
        let mut sse = String::new();
        sse.push_str(&data_frame(assistant_chunk_with_text("", Some("content_filter"))));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "test-model").await;
        let turn = complete_event(&events);
        assert!(matches!(
            turn.outcome,
            TurnOutcome::EarlyStop(EarlyStopReason::Other)
        ));
    }

    #[tokio::test]
    async fn finish_reason_stop_with_tool_calls_maps_to_tool_use() {
        // Some servers emit finish_reason: "stop" even with tool calls
        // present. The parser must still drive the loop.
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0, "id": "c", "type": "function",
                    "function": {"name": "git_status", "arguments": "{}"}
                }]},
                "finish_reason": "stop"
            }]
        })));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "test-model").await;
        let turn = complete_event(&events);
        assert_eq!(turn.outcome, TurnOutcome::ToolUse);
    }

    #[tokio::test]
    async fn malformed_args_yield_empty_object_at_terminal() {
        // Args buffer contains malformed JSON — graceful fallback to {}.
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0, "id": "c", "type": "function",
                    "function": {"name": "tool", "arguments": "{not valid json"}
                }]},
                "finish_reason": "tool_calls"
            }]
        })));

        let events = drive_fake(&sse, "test-model").await;
        let turn = complete_event(&events);
        let Block::ToolCall { args, .. } = &turn.content[0] else {
            panic!("expected ToolCall");
        };
        assert_eq!(args, &json!({}));
    }

    #[tokio::test]
    async fn comment_lines_and_keepalives_skipped() {
        // SSE comment lines (`:keepalive`) and empty lines are noise.
        let sse = format!(
            ":\n: keep alive\n\n{}data: [DONE]\n\n",
            data_frame(assistant_chunk_with_text("hi", Some("stop")))
        );
        let events = drive_fake(&sse, "test-model").await;
        let turn = complete_event(&events);
        let Block::Text(t) = &turn.content[0] else {
            panic!("expected Text");
        };
        assert_eq!(t, "hi");
    }
}
