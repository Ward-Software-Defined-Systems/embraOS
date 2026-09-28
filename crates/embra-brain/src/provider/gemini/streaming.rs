//! Gemini SSE stream parser for `streamGenerateContent?alt=sse`.
//!
//! Hand-rolled SSE consumer matching the Anthropic parser's structure
//! but adapted to Gemini's chunk shape. Lines are framed by the shared
//! [`LineBuffer`]:
//!
//! ```json
//! {
//!   "candidates": [{
//!     "content": {"role": "model", "parts": [...]},
//!     "finishReason": "STOP" | null,
//!     "index": 0
//!   }],
//!   "usageMetadata": {...}   // terminal chunk only
//! }
//! ```
//!
//! Streaming semantics:
//! - Each chunk's `parts[]` is a delta over the running accumulator.
//! - A `text` part extends the last accumulated text part if the
//!   `thought` flag matches; otherwise pushes a new part.
//! - A `functionCall` part always pushes a new part; an inline
//!   `thoughtSignature` rides along on it.
//! - A signature-only part (no `text` / `functionCall`) attaches to
//!   the most recent non-text part (typically the prior
//!   `functionCall`); if none exists it is pushed as a standalone
//!   opaque part. The Gemini docs document this "signature arrives
//!   in its own chunk" pattern explicitly.
//! - `thought:true` text parts are accumulated for round-trip but
//!   NOT emitted as `StreamEvent::TextDelta` (chain-of-thought
//!   summaries are not user-visible).
//!
//! Terminal handling:
//! - The chunk carrying `finishReason` (or `usageMetadata`) finalizes
//!   the turn. We assemble neutral-IR `Vec<Block>` and emit
//!   `StreamEvent::Complete(AssistantTurn)`.
//! - `STOP` with ≥1 `Block::ToolCall` → `TurnOutcome::ToolUse`;
//!   `STOP` with no tool calls → `EndTurn`. Per Q4, Gemini does not
//!   emit a dedicated `TOOL_USE` finishReason on Gemini 3.1 Pro —
//!   the presence of `functionCall` parts is the continuation signal.

use anyhow::Result;
use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;

use crate::provider::ir::{AssistantTurn, Block, EarlyStopReason, TurnOutcome};
use crate::provider::sse::LineBuffer;
use crate::provider::StreamEvent;

use super::wire::{GeminiPart, GeminiStreamChunk};

/// Drive the SSE stream from `:streamGenerateContent?alt=sse`,
/// emitting neutral [`StreamEvent`]s into `tx`. Terminates on
/// `[DONE]`, on the chunk carrying `finishReason`, or when the byte
/// stream ends. `include_reasoning` gates `StreamEvent::ReasoningDelta`
/// emission for `thought:true` text parts (belt-and-suspenders against
/// the API returning thoughts when the request body had
/// `includeThoughts: false`).
pub async fn process_sse_stream(
    response: reqwest::Response,
    tx: mpsc::Sender<StreamEvent>,
    include_reasoning: bool,
) -> Result<()> {
    pump(response.bytes_stream(), tx, include_reasoning).await
}

/// The parser, over any stream of byte chunks: the response body in
/// production, a list of byte vectors in the tests. The tests run this
/// function; there is no second copy of it for them to drift from.
async fn pump<S, B, E>(
    mut stream: S,
    tx: mpsc::Sender<StreamEvent>,
    include_reasoning: bool,
) -> Result<()>
where
    S: Stream<Item = std::result::Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: Into<anyhow::Error>,
{
    let mut lines = LineBuffer::default();
    let mut state = ParserState::with_options(include_reasoning);

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(Into::into)?;
        lines.push(chunk.as_ref());

        while let Some(line) = lines.next_line() {
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if data == "[DONE]" {
                state.emit_complete(&tx).await;
                return Ok(());
            }
            let chunk_obj = match serde_json::from_str::<GeminiStreamChunk>(data) {
                Ok(chunk) => chunk,
                Err(e) => {
                    // Skipped, and said so: a chunk that does not fit the
                    // structs takes its text, its tool calls and its finish
                    // reason with it.
                    tracing::warn!(
                        target: "gemini::streaming",
                        error = %e,
                        data = %crate::tools::sessions::truncate_str(data, 512),
                        "could not parse SSE chunk; skipping"
                    );
                    continue;
                }
            };
            state.process_chunk(chunk_obj, &tx).await;
            if state.terminal {
                state.emit_complete(&tx).await;
                return Ok(());
            }
            if state.receiver_gone {
                // Consumer dropped the stream (operator /stop) — exit so
                // `response` drops and the connection closes.
                tracing::debug!(
                    target: "gemini::streaming",
                    "receiver dropped mid-stream; aborting SSE pump"
                );
                return Ok(());
            }
        }
    }

    // Stream ended without a terminal finishReason — emit Complete
    // anyway so consumers don't hang. Mirrors the Anthropic parser's
    // safety behavior.
    state.emit_complete(&tx).await;
    Ok(())
}

/// Mutable in-flight assembly state.
#[derive(Default)]
struct ParserState {
    parts: Vec<GeminiPart>,
    finish_reason: Option<String>,
    /// Set true once `process_chunk` sees a finishReason so the
    /// driver loop knows to emit Complete and exit.
    terminal: bool,
    /// Tracks whether we've already emitted Complete so a trailing
    /// `[DONE]` line after a terminal chunk doesn't double-fire.
    completed: bool,
    /// Mirror of `LlmRequestOptions.include_reasoning` — gates
    /// `StreamEvent::ReasoningDelta` emission. Default `false` keeps
    /// `Default::default()` callers (test harnesses) reasoning-off.
    include_reasoning: bool,
    /// Set when a send fails (receiver dropped — consumer aborted the
    /// turn, e.g. operator /stop). The pump loop checks it and exits,
    /// dropping the `reqwest::Response` (severs the connection). Never
    /// downgrade sends back to fire-and-forget.
    receiver_gone: bool,
}

impl ParserState {
    fn with_options(include_reasoning: bool) -> Self {
        Self {
            include_reasoning,
            ..Self::default()
        }
    }

    /// Send an event, recording receiver loss instead of ignoring it.
    async fn send(&mut self, tx: &mpsc::Sender<StreamEvent>, event: StreamEvent) {
        if tx.send(event).await.is_err() {
            self.receiver_gone = true;
        }
    }
}

impl ParserState {
    async fn process_chunk(
        &mut self,
        chunk: GeminiStreamChunk,
        tx: &mpsc::Sender<StreamEvent>,
    ) {
        let Some(candidate) = chunk.candidates.into_iter().next() else {
            return;
        };
        if let Some(reason) = candidate.finish_reason {
            self.finish_reason = Some(reason);
            self.terminal = true;
        }
        for incoming in candidate.content.parts {
            self.absorb_part(incoming, tx).await;
        }
    }

    async fn absorb_part(&mut self, incoming: GeminiPart, tx: &mpsc::Sender<StreamEvent>) {
        let has_text = incoming.text.is_some();
        let has_call = incoming.function_call.is_some();
        let has_resp = incoming.function_response.is_some();
        let has_sig = incoming.thought_signature.is_some();
        let is_thought = incoming.thought.unwrap_or(false);

        // Signature-only chunk: attach to the most recent non-text
        // part (the typical sibling-chunk pattern from the docs).
        if !has_text && !has_call && !has_resp && has_sig {
            self.attach_signature(incoming.thought_signature.unwrap());
            return;
        }

        // Text delta path.
        if has_text && !has_call && !has_resp {
            let text = incoming.text.unwrap();
            // Append to last text part if it shares the thought
            // flag; otherwise push a new part.
            let appendable = self.parts.last().is_some_and(|p| {
                p.text.is_some()
                    && p.function_call.is_none()
                    && p.function_response.is_none()
                    && p.thought.unwrap_or(false) == is_thought
            });
            if appendable {
                let last = self.parts.last_mut().unwrap();
                if let Some(existing) = last.text.as_mut() {
                    existing.push_str(&text);
                }
                if has_sig {
                    last.thought_signature = incoming.thought_signature;
                }
            } else {
                self.parts.push(GeminiPart {
                    text: Some(text.clone()),
                    function_call: None,
                    function_response: None,
                    thought_signature: incoming.thought_signature,
                    thought: incoming.thought,
                    inline_data: None,
                });
            }
            // Only user-visible text fires TextDelta; chain-of-
            // thought summaries route to ReasoningDelta when the
            // operator has opted in (gated by include_reasoning,
            // mirroring `LlmRequestOptions`). The ReasoningDelta
            // privacy contract (provider/mod.rs) keeps this off the
            // text accumulators / persistence path.
            if is_thought {
                if self.include_reasoning {
                    self.send(tx, StreamEvent::ReasoningDelta(text)).await;
                }
            } else {
                self.send(tx, StreamEvent::TextDelta(text)).await;
            }
            return;
        }

        // Function call (and any sibling signature) — push verbatim.
        if has_call {
            self.parts.push(GeminiPart {
                text: None,
                function_call: incoming.function_call,
                function_response: None,
                thought_signature: incoming.thought_signature,
                thought: incoming.thought,
                inline_data: None,
            });
            return;
        }

        // Function response (rare on assistant turns — handled
        // defensively).
        if has_resp {
            self.parts.push(GeminiPart {
                text: None,
                function_call: None,
                function_response: incoming.function_response,
                thought_signature: incoming.thought_signature,
                thought: incoming.thought,
                inline_data: None,
            });
        }
    }

    /// Attach a signature to the most recent non-text part if one
    /// exists; otherwise push a standalone signature-only part.
    fn attach_signature(&mut self, sig: String) {
        let target_idx = self
            .parts
            .iter()
            .enumerate()
            .rev()
            .find(|(_, p)| p.function_call.is_some() || p.function_response.is_some())
            .map(|(i, _)| i);
        match target_idx {
            Some(i) => {
                self.parts[i].thought_signature = Some(sig);
            }
            None => {
                self.parts.push(GeminiPart {
                    text: None,
                    function_call: None,
                    function_response: None,
                    thought_signature: Some(sig),
                    thought: None,
                    inline_data: None,
                });
            }
        }
    }

    async fn emit_complete(&mut self, tx: &mpsc::Sender<StreamEvent>) {
        if self.completed {
            return;
        }
        self.completed = true;

        let mut content: Vec<Block> = Vec::with_capacity(self.parts.len());
        for part in std::mem::take(&mut self.parts) {
            content.extend(part_to_blocks(part));
        }

        let has_tool_call = content
            .iter()
            .any(|b| matches!(b, Block::ToolCall { .. }));
        let outcome = match self.finish_reason.as_deref() {
            None => {
                tracing::warn!(
                    target: "gemini::streaming",
                    "stream closed without finishReason; defaulting to EndTurn"
                );
                if has_tool_call {
                    TurnOutcome::ToolUse
                } else {
                    TurnOutcome::EndTurn
                }
            }
            Some("STOP") => {
                if has_tool_call {
                    TurnOutcome::ToolUse
                } else {
                    TurnOutcome::EndTurn
                }
            }
            Some("MAX_TOKENS") => TurnOutcome::MaxTokens,
            Some("SAFETY") => TurnOutcome::EarlyStop(EarlyStopReason::Safety),
            Some("RECITATION") => TurnOutcome::EarlyStop(EarlyStopReason::Recitation),
            Some("MALFORMED_FUNCTION_CALL") => TurnOutcome::EarlyStop(EarlyStopReason::Malformed),
            Some(_other) => TurnOutcome::EarlyStop(EarlyStopReason::Other),
        };

        let _ = tx
            .send(StreamEvent::Complete(AssistantTurn {
                content,
                outcome,
                stop_details: None,
            }))
            .await;
    }
}

/// Translate a single accumulated `GeminiPart` into 0+ neutral
/// blocks. The split is needed because a text part with a stand-alone
/// signature could in principle yield both a `Block::Text` AND a
/// sibling `Block::ProviderOpaque` — though in practice signatures
/// rarely accompany text parts in Gemini 3.1 Pro responses.
fn part_to_blocks(part: GeminiPart) -> Vec<Block> {
    let GeminiPart {
        text,
        function_call,
        function_response,
        thought_signature,
        thought,
        // LLM models never emit inline media on the response side; a
        // stray blob is dropped rather than replayed on a model turn.
        inline_data: _,
    } = part;
    let is_thought = thought.unwrap_or(false);
    let mut out = Vec::new();

    if let Some(call) = function_call {
        // Signature (if any) rides on the ToolCall via provider_opaque.
        let opaque = thought_signature
            .clone()
            .map(|sig| serde_json::json!({"thought_signature": sig}));
        out.push(Block::ToolCall {
            // The API may send no id. A result has to find its call, among
            // parallel calls and across turns, so one is assigned here; it
            // goes back to the API on the call and on its response alike.
            id: if call.id.is_empty() {
                format!("gemini-{}", uuid::Uuid::new_v4().simple())
            } else {
                call.id
            },
            name: call.name,
            // No arguments for a tool that takes none: an empty object, which
            // the tool's typed arguments can be read from, where `null` cannot.
            args: if call.args.is_null() {
                serde_json::json!({})
            } else {
                call.args
            },
            provider_opaque: opaque,
        });
        return out;
    }

    if let Some(resp) = function_response {
        // Defensive — assistant turns shouldn't carry tool results,
        // but if one slips through preserve it as a tool result.
        let payload = match resp.response.get("result") {
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => resp.response.to_string(),
        };
        out.push(Block::ToolResult {
            call_id: resp.id,
            content: payload,
            is_error: false,
            images: Vec::new(),
        });
        return out;
    }

    if let Some(t) = text {
        if is_thought {
            // Round-trip the thought summary as opaque so signature
            // ordering survives. Loop driver doesn't surface it.
            out.push(Block::ProviderOpaque(serde_json::json!({
                "thought": true,
                "text": t,
                "thought_signature": thought_signature,
            })));
        } else {
            out.push(Block::Text(t));
            if let Some(sig) = thought_signature {
                out.push(Block::ProviderOpaque(serde_json::json!({
                    "thought_signature": sig,
                })));
            }
        }
        return out;
    }

    // Signature-only part with no text/call/response.
    if let Some(sig) = thought_signature {
        out.push(Block::ProviderOpaque(serde_json::json!({
            "thought_signature": sig,
        })));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse_body(events: &[&str]) -> Vec<u8> {
        let mut body = String::new();
        for e in events {
            body.push_str("data: ");
            body.push_str(e);
            body.push_str("\n\n");
        }
        body.into_bytes()
    }

    /// The events as one SSE body, arriving in one chunk; reasoning off.
    async fn run_stream(events: &[&str]) -> Vec<StreamEvent> {
        run_chunks(vec![sse_body(events)], false).await
    }

    /// Run the parser over the chunks and collect what it emits.
    async fn run_chunks(chunks: Vec<Vec<u8>>, include_reasoning: bool) -> Vec<StreamEvent> {
        let (tx, mut rx) = mpsc::channel(128);
        let stream = futures_util::stream::iter(
            chunks.into_iter().map(Ok::<_, std::convert::Infallible>),
        );
        let parser = tokio::spawn(pump(stream, tx, include_reasoning));
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev);
        }
        parser.await.expect("parser task").expect("parser result");
        out
    }

    fn text_deltas(out: &[StreamEvent]) -> Vec<&str> {
        out.iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn complete_turn(out: &[StreamEvent]) -> AssistantTurn {
        out.iter()
            .find_map(|e| match e {
                StreamEvent::Complete(t) => Some(t.clone()),
                _ => None,
            })
            .expect("Complete event")
    }

    #[tokio::test]
    async fn single_text_turn() {
        let events = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":" world"}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":10}}"#,
        ];
        let out = run_stream(&events).await;
        // Two TextDelta events fired before the terminal chunk.
        let deltas: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["Hello", " world"]);

        let turn = complete_turn(&out);
        assert_eq!(turn.outcome, TurnOutcome::EndTurn);
        assert_eq!(turn.content.len(), 1);
        match &turn.content[0] {
            Block::Text(t) => assert_eq!(t, "Hello world"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn thought_text_emits_reasoning_delta_when_enabled() {
        // With include_reasoning=true, thought:true text parts emit
        // StreamEvent::ReasoningDelta but NEVER StreamEvent::TextDelta.
        let events = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Considering the question.","thought":true}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Visible answer.","thought":false}]},"finishReason":"STOP","index":0}]}"#,
        ];
        let out = run_chunks(vec![sse_body(&events)], true).await;

        let reasoning: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ReasoningDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, vec!["Considering the question."]);

        let text_deltas: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text_deltas,
            vec!["Visible answer."],
            "thought:true text must NOT leak into TextDelta"
        );
    }

    #[tokio::test]
    async fn thought_text_suppressed_when_reasoning_disabled() {
        // With include_reasoning=false (the default), thought parts are
        // accumulated for IR round-trip but NEVER emitted as
        // ReasoningDelta or TextDelta.
        let events = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Internal monologue.","thought":true}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Answer."}]},"finishReason":"STOP","index":0}]}"#,
        ];
        let out = run_stream(&events).await;
        assert!(
            !out
                .iter()
                .any(|e| matches!(e, StreamEvent::ReasoningDelta(_))),
            "ReasoningDelta must not be emitted when include_reasoning=false"
        );
        let text_deltas: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text_deltas, vec!["Answer."]);
    }

    #[tokio::test]
    async fn tool_call_with_signature() {
        let events = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"fc1","name":"system_status","args":{}},"thoughtSignature":"sig-abc"}]},"finishReason":"STOP","index":0}]}"#,
        ];
        let out = run_stream(&events).await;
        let turn = complete_turn(&out);
        assert_eq!(turn.outcome, TurnOutcome::ToolUse);
        assert_eq!(turn.content.len(), 1);
        match &turn.content[0] {
            Block::ToolCall { id, name, provider_opaque, .. } => {
                assert_eq!(id, "fc1");
                assert_eq!(name, "system_status");
                let opaque = provider_opaque.as_ref().expect("signature carried");
                assert_eq!(opaque["thought_signature"], "sig-abc");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parallel_tool_calls_first_has_signature() {
        let events = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"fc1","name":"a","args":{}},"thoughtSignature":"only-on-first"}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"fc2","name":"b","args":{}}}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"fc3","name":"c","args":{}}}]},"finishReason":"STOP","index":0}]}"#,
        ];
        let out = run_stream(&events).await;
        let turn = complete_turn(&out);
        assert_eq!(turn.outcome, TurnOutcome::ToolUse);
        assert_eq!(turn.content.len(), 3);

        // First call has signature.
        match &turn.content[0] {
            Block::ToolCall { id, provider_opaque, .. } => {
                assert_eq!(id, "fc1");
                let opaque = provider_opaque.as_ref().expect("first call has signature");
                assert_eq!(opaque["thought_signature"], "only-on-first");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        // Subsequent parallel calls have no signature.
        for (i, expected_id) in [(1, "fc2"), (2, "fc3")] {
            match &turn.content[i] {
                Block::ToolCall { id, provider_opaque, .. } => {
                    assert_eq!(id, expected_id);
                    assert!(provider_opaque.is_none(), "parallel call must not synthesize signature");
                }
                other => panic!("expected ToolCall at index {i}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn signature_in_separate_terminal_chunk() {
        // The docs explicitly call out the case where the signature
        // arrives in its own chunk after the function_call chunk.
        let events = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"fc1","name":"x","args":{}}}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"thoughtSignature":"late-sig"}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP","index":0}]}"#,
        ];
        let out = run_stream(&events).await;
        let turn = complete_turn(&out);
        assert_eq!(turn.content.len(), 1);
        match &turn.content[0] {
            Block::ToolCall { provider_opaque, .. } => {
                let opaque = provider_opaque.as_ref().expect("late signature attached");
                assert_eq!(opaque["thought_signature"], "late-sig");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn text_then_tool_call_preserves_order() {
        let events = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"I'll check."}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"fc1","name":"system_status","args":{}}}]},"finishReason":"STOP","index":0}]}"#,
        ];
        let out = run_stream(&events).await;
        let turn = complete_turn(&out);
        assert_eq!(turn.outcome, TurnOutcome::ToolUse);
        assert_eq!(turn.content.len(), 2);
        assert!(matches!(&turn.content[0], Block::Text(t) if t == "I'll check."));
        assert!(matches!(&turn.content[1], Block::ToolCall { .. }));
    }

    #[tokio::test]
    async fn thought_text_is_filtered_from_text_deltas() {
        let events = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Reasoning...","thought":true}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Visible."}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP","index":0}]}"#,
        ];
        let out = run_stream(&events).await;
        // Only the non-thought text fires TextDelta.
        let deltas: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["Visible."]);

        let turn = complete_turn(&out);
        // Both parts survive in the assembled IR — thought as
        // ProviderOpaque, normal text as Block::Text.
        assert!(turn
            .content
            .iter()
            .any(|b| matches!(b, Block::ProviderOpaque(_))));
        assert!(turn
            .content
            .iter()
            .any(|b| matches!(b, Block::Text(t) if t == "Visible.")));
    }

    #[tokio::test]
    async fn safety_finish_reason_maps_to_early_stop() {
        let events = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"partial"}]},"finishReason":"SAFETY","index":0}]}"#,
        ];
        let out = run_stream(&events).await;
        let turn = complete_turn(&out);
        assert_eq!(turn.outcome, TurnOutcome::EarlyStop(EarlyStopReason::Safety));
    }

    // What follows had no test while the tests ran a copy of the line
    // loop: the copy read a whole body at once and never looked at the
    // receiver.

    /// `id` and `args` are optional in the API. A tool that takes no
    /// parameters can be called without `args`.
    #[tokio::test]
    async fn a_tool_call_without_args_or_id_is_read() {
        let events = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"system_status"}},{"functionCall":{"name":"time_now"}}]},"finishReason":"STOP","index":0}]}"#,
        ];
        let out = run_stream(&events).await;
        let turn = complete_turn(&out);
        assert_eq!(turn.outcome, TurnOutcome::ToolUse);
        let calls: Vec<_> = turn
            .content
            .iter()
            .filter_map(|b| match b {
                Block::ToolCall { id, name, args, .. } => Some((id.clone(), name.clone(), args.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, "system_status");
        assert_eq!(calls[1].1, "time_now");
        // Arguments the tool's typed struct can be read from.
        assert_eq!(calls[0].2, serde_json::json!({}));
        // An id of our own, so a result finds its call: not empty, not shared.
        assert!(!calls[0].0.is_empty());
        assert_ne!(calls[0].0, calls[1].0);
    }

    /// Gemini leaves `parts` out of a content that has nothing to say — a
    /// turn that ends on `MAX_TOKENS` or `SAFETY` is the usual case — and
    /// the same chunk carries the finish reason.
    #[tokio::test]
    async fn a_content_without_parts_keeps_its_finish_reason() {
        let events = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"partial"}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"role":"model"},"finishReason":"MAX_TOKENS","index":0}]}"#,
        ];
        let out = run_stream(&events).await;
        assert_eq!(text_deltas(&out), ["partial"]);
        assert_eq!(complete_turn(&out).outcome, TurnOutcome::MaxTokens);
    }

    #[tokio::test]
    async fn a_content_without_a_role_is_read() {
        let events = [
            r#"{"candidates":[{"content":{"parts":[{"text":"no role"}]},"finishReason":"STOP","index":0}]}"#,
        ];
        let out = run_stream(&events).await;
        assert_eq!(text_deltas(&out), ["no role"]);
    }

    #[tokio::test]
    async fn a_line_is_read_whole_wherever_the_chunks_were_cut() {
        let events = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"I'll write it."}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"fc1","name":"file_write","args":{"path":"notes.md","content":"plain text"}},"thoughtSignature":"sig-abc"}]},"finishReason":"STOP","index":0}]}"#,
        ];
        let body = sse_body(&events);
        let whole = complete_turn(&run_chunks(vec![body.clone()], false).await);
        assert_eq!(whole.outcome, TurnOutcome::ToolUse);
        assert_eq!(whole.content.len(), 2);
        // No `PartialEq` on the turn; its `Debug` form says it all.
        let want = format!("{whole:?}");
        for cut in 1..body.len() {
            let out = run_chunks(vec![body[..cut].to_vec(), body[cut..].to_vec()], false).await;
            assert_eq!(text_deltas(&out), ["I'll write it."], "cut at byte {cut}");
            assert_eq!(format!("{:?}", complete_turn(&out)), want, "cut at byte {cut}");
        }
    }

    #[tokio::test]
    async fn framing_noise_is_skipped() {
        let body = concat!(
            ": ping\r\n",
            "event: message\r\n",
            "data: not json\n\n",
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"kept"}]},"index":0}]}"#,
            "\r\n\r\n",
            r#"data: {"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP","index":0}]}"#,
            "\n\n",
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":" after the end"}]},"index":0}]}"#,
            "\n\n",
        );
        let out = run_chunks(vec![body.as_bytes().to_vec()], false).await;
        assert_eq!(text_deltas(&out), ["kept"]);
        let turn = complete_turn(&out);
        assert!(matches!(&turn.content[..], [Block::Text(t)] if t == "kept"));
    }

    #[tokio::test]
    async fn done_sentinel_and_a_bare_end_of_stream_both_complete() {
        let text = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"index":0}]}"#,
        ];
        // The body just ends: no finishReason.
        let turn = complete_turn(&run_stream(&text).await);
        assert_eq!(turn.outcome, TurnOutcome::EndTurn);
        assert!(matches!(&turn.content[..], [Block::Text(t)] if t == "hi"));

        let mut body = sse_body(&text);
        body.extend_from_slice(b"data: [DONE]\n\n");
        body.extend_from_slice(&sse_body(&[
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":" after"}]},"index":0}]}"#,
        ]));
        let out = run_chunks(vec![body], false).await;
        assert_eq!(text_deltas(&out), ["hi"]);
        let completes = out
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
        let events = [
            serde_json::json!({"candidates":[{"content":{"role":"model","parts":[
                {"text": said, "thought": true}]},"index":0}]}).to_string(),
            serde_json::json!({"candidates":[{"content":{"role":"model","parts":[
                {"text": said}]},"index":0}]}).to_string(),
            serde_json::json!({"candidates":[{"content":{"role":"model","parts":[
                {"functionCall":{"id":"fc1","name":"file_write",
                    "args":{"path":"notes.md","content": said}}}]},
                "finishReason":"STOP","index":0}]}).to_string(),
        ];
        let events: Vec<&str> = events.iter().map(String::as_str).collect();
        let body = sse_body(&events);
        assert!(body.len() > body.iter().filter(|b| b.is_ascii()).count(), "raw UTF-8 on the wire");

        for cut in 1..body.len() {
            let out = run_chunks(vec![body[..cut].to_vec(), body[cut..].to_vec()], true).await;
            assert_eq!(text_deltas(&out), [said], "text, cut at byte {cut}");
            let reasoning: Vec<&str> = out
                .iter()
                .filter_map(|e| match e {
                    StreamEvent::ReasoningDelta(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(reasoning, [said], "reasoning, cut at byte {cut}");
            let turn = complete_turn(&out);
            match &turn.content[..] {
                [Block::ProviderOpaque(thought), Block::Text(text), Block::ToolCall { args, .. }] => {
                    assert_eq!(thought["text"], said, "thought, cut at byte {cut}");
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
        let mut body = sse_body(&[
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"whole"}]},"index":0}]}"#,
        ]);
        body.extend_from_slice(
            br#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":" cut off"}]},"index":0}]}"#,
        );
        let out = run_chunks(vec![body], false).await;
        assert_eq!(text_deltas(&out), ["whole"]);
        assert!(matches!(&complete_turn(&out).content[..], [Block::Text(t)] if t == "whole"));
    }

    /// The `/stop` contract. The consumer drops the stream; the next send
    /// fails; the parser returns, and with it goes the response body —
    /// the connection. It must not read on.
    #[tokio::test]
    async fn a_dropped_receiver_ends_the_parser_at_its_next_send() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let text = sse_body(&[
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"one"}]},"index":0}]}"#,
        ]);
        let thought = sse_body(&[
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hm","thought":true}]},"index":0}]}"#,
        ]);
        for (checkpoint, first) in [("text", &text), ("thought", &thought)] {
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
            pump(stream, tx, true)
                .await
                .expect("a dropped receiver is not an error");
            assert_eq!(read.load(Ordering::SeqCst), 1, "read on after {checkpoint}");
        }
    }

    #[tokio::test]
    async fn a_transport_error_is_returned_to_the_caller() {
        let first = sse_body(&[
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"one"}]},"index":0}]}"#,
        ]);
        let stream = futures_util::stream::iter(vec![
            Ok(first),
            Err(std::io::Error::other("connection reset")),
        ]);
        let (tx, mut rx) = mpsc::channel(8);
        let err = pump(stream, tx, false).await.expect_err("the read error");
        assert!(err.to_string().contains("connection reset"));
        // What arrived before it was delivered; nothing is made up after
        // it. The caller turns the error into the Error event.
        assert!(matches!(rx.recv().await, Some(StreamEvent::TextDelta(t)) if t == "one"));
        assert!(rx.recv().await.is_none());
    }
}
