//! OpenAI Chat Completions SSE stream parser.
//!
//! Hand-rolled SSE consumer matching the Anthropic and Gemini parser
//! shapes but adapted to OpenAI's chunk format and tool-call
//! argument-shard assembly state machine. Lines are framed by the shared
//! [`LineBuffer`]:
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
use futures_util::{Stream, StreamExt};
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

use crate::provider::ir::{AssistantTurn, Block, EarlyStopReason, TurnOutcome};
use crate::provider::openai_compat::conv::reasoning_block;
use crate::provider::openai_compat::sanitize::sanitize_harmony_tokens;
use crate::provider::openai_compat::wire::OpenAIChatChunk;
use crate::provider::sse::LineBuffer;
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
    pump(response.bytes_stream(), tx, model_id, include_reasoning).await
}

/// The parser, over any stream of byte chunks: the response body in
/// production, a list of byte vectors in the tests. The tests run this
/// function; there is no second copy of the loop for them to drift from,
/// and none is to be written.
async fn pump<S, B, E>(
    mut stream: S,
    tx: mpsc::Sender<StreamEvent>,
    model_id: String,
    include_reasoning: bool,
) -> Result<()>
where
    S: Stream<Item = std::result::Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: Into<anyhow::Error>,
{
    let mut lines = LineBuffer::default();
    let mut state = ParserState::new(model_id, include_reasoning);

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(Into::into)?;
        lines.push(chunk.as_ref());
        consume_sse_lines(&mut lines, &mut state, &tx).await;
        if state.completed {
            return Ok(());
        }
        if state.receiver_gone {
            // Nobody is listening (consumer dropped the stream — e.g. an
            // operator /stop). Returning drops `response`, which closes
            // the TCP connection and makes the server stop generating.
            // Load-bearing for /stop: never downgrade back to fire-and-
            // forget sends.
            tracing::debug!(
                target: "provider::openai_compat::streaming",
                "receiver dropped mid-stream; aborting SSE pump"
            );
            return Ok(());
        }
    }

    if !state.completed {
        state.emit_complete(&tx).await;
    }
    Ok(())
}

/// Drain the complete lines into the parser. A line without its `\n`
/// stays in the buffer for the next chunk.
async fn consume_sse_lines(
    lines: &mut LineBuffer,
    state: &mut ParserState,
    tx: &mpsc::Sender<StreamEvent>,
) {
    while let Some(line) = lines.next_line() {
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
        if state.completed || state.receiver_gone {
            return;
        }
    }
}

/// In-flight assembly state.
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
    /// Set when a send fails (receiver dropped — consumer aborted the
    /// turn). The pump loops check it and exit, which drops the
    /// `reqwest::Response` and severs the connection — load-bearing for
    /// the operator /stop.
    pub(super) receiver_gone: bool,
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
            receiver_gone: false,
        }
    }

    /// Send an event, recording receiver loss instead of ignoring it —
    /// the fire-and-forget `let _ = tx.send(...)` shape left the pump
    /// draining a stream nobody consumed (the pre-/stop leak).
    async fn send(&mut self, tx: &mpsc::Sender<StreamEvent>, event: StreamEvent) {
        if tx.send(event).await.is_err() {
            self.receiver_gone = true;
        }
    }

    pub(super) async fn process_chunk(
        &mut self,
        chunk: OpenAIChatChunk,
        tx: &mpsc::Sender<StreamEvent>,
    ) {
        for choice in chunk.choices {
            // Text content — emit live + accumulate.
            if let Some(content) = choice.delta.content
                && !content.is_empty()
            {
                self.text_buffer.push_str(&content);
                self.send(tx, StreamEvent::TextDelta(content)).await;
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
                    self.send(tx, StreamEvent::ReasoningDelta(r)).await;
                }
            } else if let Some(r) = choice.delta.reasoning_content {
                self.reasoning_buffer.push_str(&r);
                if self.include_reasoning {
                    self.send(tx, StreamEvent::ReasoningDelta(r)).await;
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
            content.push(reasoning_block(&self.reasoning_buffer));
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
        self.send(
            tx,
            StreamEvent::Complete(AssistantTurn {
                content,
                outcome,
                stop_details: None,
            }),
        )
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
/// path naturally.
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

    /// The body in one chunk, reasoning off. Reasoning-on tests call
    /// `drive_fake_with_options`.
    async fn drive_fake(sse_text: &str, model_id: &str) -> Vec<StreamEvent> {
        drive_fake_with_options(sse_text, model_id, false).await
    }

    async fn drive_fake_with_options(
        sse_text: &str,
        model_id: &str,
        include_reasoning: bool,
    ) -> Vec<StreamEvent> {
        run_chunks(vec![sse_text.as_bytes().to_vec()], model_id, include_reasoning).await
    }

    /// Run the parser over the chunks and collect what it emits.
    async fn run_chunks(
        chunks: Vec<Vec<u8>>,
        model_id: &str,
        include_reasoning: bool,
    ) -> Vec<StreamEvent> {
        let (tx, mut rx) = mpsc::channel(256);
        let stream = futures_util::stream::iter(
            chunks.into_iter().map(Ok::<_, std::convert::Infallible>),
        );
        let parser = tokio::spawn(pump(stream, tx, model_id.to_string(), include_reasoning));
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        parser.await.expect("parser task").expect("parser result");
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

    #[tokio::test]
    async fn chunks_without_envelope_fields_are_processed() {
        // Only `choices[].delta` and `finish_reason` are read, so only
        // they are needed. Before the receive-side structs were trimmed
        // to that, each of these chunks failed to parse and was skipped:
        // the turn completed empty.
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({"choices": [{"delta": {"content": "Hel"}}]})));
        sse.push_str(&data_frame(json!({"choices": [{"delta": {"content": "lo"}}]})));
        sse.push_str(&data_frame(json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}]
        })));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "test-model").await;
        assert_eq!(text_deltas(&events), vec!["Hel", "lo"]);
        let turn = complete_event(&events);
        assert_eq!(turn.outcome, TurnOutcome::EndTurn);
        let Block::Text(t) = &turn.content[0] else {
            panic!("expected Text");
        };
        assert_eq!(t, "Hello");
    }

    #[tokio::test]
    async fn in_stream_error_object_is_skipped() {
        // No `choices`: not a chunk. Skipped like any frame that does not
        // parse; the stream goes on.
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({"error": {"message": "slot busy"}})));
        sse.push_str(&data_frame(assistant_chunk_with_text("ok", Some("stop"))));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "test-model").await;
        assert_eq!(text_deltas(&events), vec!["ok"]);
        assert_eq!(complete_event(&events).content.len(), 1);
    }

    #[tokio::test]
    async fn empty_reasoning_shards_produce_no_block() {
        // Servers that always send the key send it empty on text chunks.
        // An empty buffer at the terminal must not become a reasoning
        // block that is then replayed to the model.
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "choices": [{"delta": {"content": "answer", "reasoning": ""}}]
        })));
        sse.push_str(&data_frame(json!({
            "choices": [{"delta": {"reasoning_content": ""}, "finish_reason": "stop"}]
        })));
        sse.push_str("data: [DONE]\n\n");

        let events = drive_fake(&sse, "test-model").await;
        let turn = complete_event(&events);
        assert_eq!(turn.content.len(), 1);
        assert!(matches!(turn.content[0], Block::Text(_)));
    }

    #[tokio::test]
    async fn dropped_receiver_sets_receiver_gone() {
        // The /stop contract: when the consumer drops the stream, the
        // next send must record receiver loss so the pump loops exit and
        // drop the reqwest::Response (severing the connection). The old
        // fire-and-forget sends left the pump draining forever.
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(4);
        drop(rx);
        let mut state = ParserState::new("test-model".to_string(), false);
        let chunk: OpenAIChatChunk =
            serde_json::from_value(assistant_chunk_with_text("token", None)).unwrap();
        state.process_chunk(chunk, &tx).await;
        assert!(state.receiver_gone, "send to a dropped receiver must flag receiver_gone");
    }

    /// The same contract one level up: with the flag set the parser
    /// returns, and with it goes the response body — the connection. It
    /// must not read on.
    #[tokio::test]
    async fn a_dropped_receiver_ends_the_parser_at_its_next_send() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let text = data_frame(assistant_chunk_with_text("one", None)).into_bytes();
        let reasoning = data_frame(json!({
            "choices": [{"delta": {"reasoning": "hm"}, "finish_reason": null}]
        }))
        .into_bytes();
        for (checkpoint, first) in [("text", &text), ("reasoning", &reasoning)] {
            let read = Arc::new(AtomicUsize::new(0));
            let counter = read.clone();
            let chunks = vec![first.clone(), text.clone(), text.clone(), text.clone()];
            let stream = futures_util::stream::iter(
                chunks.into_iter().map(Ok::<_, std::convert::Infallible>),
            )
            .inspect(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            });
            let (tx, rx) = mpsc::channel(8);
            drop(rx);
            pump(stream, tx, "test-model".to_string(), true)
                .await
                .expect("a dropped receiver is not an error");
            assert_eq!(read.load(Ordering::SeqCst), 1, "read on after {checkpoint}");
        }
    }

    #[tokio::test]
    async fn a_line_is_read_whole_wherever_the_chunks_were_cut() {
        let mut sse = String::new();
        sse.push_str(&data_frame(assistant_chunk_with_text("I'll write it.", None)));
        sse.push_str(&data_frame(json!({
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "file_write", "arguments": "{\"path\":\"notes.md\","}
                }]},
                "finish_reason": null
            }]
        })));
        sse.push_str(&data_frame(json!({
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "\"content\":\"plain text\"}"}
                }]},
                "finish_reason": "tool_calls"
            }]
        })));
        sse.push_str("data: [DONE]\n\n");
        let body = sse.into_bytes();

        let whole = run_chunks(vec![body.clone()], "test-model", false).await;
        let turn = complete_event(&whole);
        assert_eq!(turn.outcome, TurnOutcome::ToolUse);
        let Block::ToolCall { args, .. } = &turn.content[1] else {
            panic!("expected ToolCall, got {:?}", turn.content);
        };
        assert_eq!(args, &json!({"path": "notes.md", "content": "plain text"}));
        // No `PartialEq` on the turn; its `Debug` form says it all.
        let want = format!("{turn:?}");
        for cut in 1..body.len() {
            let chunks = vec![body[..cut].to_vec(), body[cut..].to_vec()];
            let events = run_chunks(chunks, "test-model", false).await;
            assert_eq!(text_deltas(&events), ["I'll write it."], "cut at byte {cut}");
            assert_eq!(format!("{:?}", complete_event(&events)), want, "cut at byte {cut}");
        }
    }

    #[tokio::test]
    async fn crlf_and_a_data_prefix_without_its_space_are_read() {
        let chunk = assistant_chunk_with_text("hi", Some("stop"));
        let sse = format!("event: message\r\ndata:{chunk}\r\n\r\ndata: [DONE]\r\n\r\n");
        let events = drive_fake(&sse, "test-model").await;
        assert_eq!(text_deltas(&events), ["hi"]);
        assert_eq!(complete_event(&events).outcome, TurnOutcome::EndTurn);
    }

    #[tokio::test]
    async fn a_bare_end_of_stream_still_completes_once() {
        // No finish_reason and no [DONE]: the body just ends.
        let sse = data_frame(assistant_chunk_with_text("hi", None));
        let events = drive_fake(&sse, "test-model").await;
        let completes = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Complete(_)))
            .count();
        assert_eq!(completes, 1);
        assert_eq!(complete_event(&events).outcome, TurnOutcome::EndTurn);

        // A finish_reason, then [DONE], then more: one Complete, and
        // nothing after the end is read.
        let mut sse = data_frame(assistant_chunk_with_text("hi", Some("stop")));
        sse.push_str("data: [DONE]\n\n");
        sse.push_str(&data_frame(assistant_chunk_with_text(" after", None)));
        let events = drive_fake(&sse, "test-model").await;
        assert_eq!(text_deltas(&events), ["hi"]);
        let completes = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Complete(_)))
            .count();
        assert_eq!(completes, 1);
    }

    /// The network cuts where it cuts, and that can be inside a character.
    /// Both halves are bytes of the same line and have to be decoded
    /// together: decoded chunk by chunk, each half becomes U+FFFD - in
    /// the text the operator reads, and in what a tool call writes.
    #[tokio::test]
    async fn a_character_cut_in_two_by_the_network_stays_whole() {
        // 2, 3 and 4 bytes a character.
        let said = "caf\u{e9} \u{2014} \u{6f22}\u{5b57} \u{1f600}";
        let args = json!({"path": "notes.md", "content": said}).to_string();
        let mut sse = String::new();
        sse.push_str(&data_frame(json!({
            "choices": [{"delta": {"reasoning": said}, "finish_reason": null}]
        })));
        sse.push_str(&data_frame(assistant_chunk_with_text(said, None)));
        sse.push_str(&data_frame(json!({
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "file_write", "arguments": args}
                }]},
                "finish_reason": "tool_calls"
            }]
        })));
        sse.push_str("data: [DONE]\n\n");
        let body = sse.into_bytes();
        assert!(body.len() > body.iter().filter(|b| b.is_ascii()).count(), "raw UTF-8 on the wire");

        for cut in 1..body.len() {
            let chunks = vec![body[..cut].to_vec(), body[cut..].to_vec()];
            let events = run_chunks(chunks, "test-model", true).await;
            assert_eq!(text_deltas(&events), [said], "text, cut at byte {cut}");
            let reasoning: Vec<&str> = events
                .iter()
                .filter_map(|e| match e {
                    StreamEvent::ReasoningDelta(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(reasoning, [said], "reasoning, cut at byte {cut}");
            match &complete_event(&events).content[..] {
                [Block::ProviderOpaque(thought), Block::Text(text), Block::ToolCall { args, .. }] => {
                    assert_eq!(thought["content"], said, "reasoning block, cut at byte {cut}");
                    assert_eq!(text, said, "text block, cut at byte {cut}");
                    assert_eq!(args["content"], said, "tool args, cut at byte {cut}");
                    assert_eq!(args["path"], "notes.md", "cut at byte {cut}");
                }
                other => panic!("cut at byte {cut}: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_last_line_without_its_newline_is_not_processed() {
        let mut sse = data_frame(assistant_chunk_with_text("whole", None));
        sse.push_str(&format!("data: {}", assistant_chunk_with_text(" cut off", None)));
        let events = drive_fake(&sse, "test-model").await;
        assert_eq!(text_deltas(&events), ["whole"]);
        let Block::Text(t) = &complete_event(&events).content[0] else {
            panic!("expected Text");
        };
        assert_eq!(t, "whole");
    }

    #[tokio::test]
    async fn a_transport_error_is_returned_to_the_caller() {
        let first = data_frame(assistant_chunk_with_text("one", None)).into_bytes();
        let stream = futures_util::stream::iter(vec![
            Ok(first),
            Err(std::io::Error::other("connection reset")),
        ]);
        let (tx, mut rx) = mpsc::channel(8);
        let err = pump(stream, tx, "test-model".to_string(), false)
            .await
            .expect_err("the read error");
        assert!(err.to_string().contains("connection reset"));
        // What arrived before it was delivered; nothing is made up after
        // it. The caller turns the error into the Error event.
        assert!(matches!(rx.recv().await, Some(StreamEvent::TextDelta(t)) if t == "one"));
        assert!(rx.recv().await.is_none());
    }
}
