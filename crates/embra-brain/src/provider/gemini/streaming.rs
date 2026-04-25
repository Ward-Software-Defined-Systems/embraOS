//! Gemini SSE stream parser for `streamGenerateContent?alt=sse`.
//!
//! Hand-rolled, line-buffered SSE consumer matching the Anthropic
//! parser's structure but adapted to Gemini's chunk shape:
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
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::provider::ir::{AssistantTurn, Block, EarlyStopReason, TurnOutcome};
use crate::provider::StreamEvent;

use super::wire::{GeminiPart, GeminiStreamChunk};

/// Drive the SSE stream from `:streamGenerateContent?alt=sse`,
/// emitting neutral [`StreamEvent`]s into `tx`. Terminates on
/// `[DONE]`, on the chunk carrying `finishReason`, or when the byte
/// stream ends.
pub async fn process_sse_stream(
    response: reqwest::Response,
    tx: mpsc::Sender<StreamEvent>,
) -> Result<()> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut state = ParserState::default();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let text = String::from_utf8_lossy(&chunk);
        buffer.push_str(&text);

        while let Some(newline_pos) = buffer.find('\n') {
            let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
            buffer = buffer[newline_pos + 1..].to_string();

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
            let Ok(chunk_obj) = serde_json::from_str::<GeminiStreamChunk>(data) else {
                continue;
            };
            state.process_chunk(chunk_obj, &tx).await;
            if state.terminal {
                state.emit_complete(&tx).await;
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

/// Mutable in-flight assembly state. Public-via-this-module only so
/// the test harness can drive it without a real `reqwest::Response`.
#[derive(Default)]
struct ParserState {
    parts: Vec<GeminiPart>,
    finish_reason: Option<String>,
    usage_metadata: Option<serde_json::Value>,
    /// Set true once `process_chunk` sees a finishReason so the
    /// driver loop knows to emit Complete and exit.
    terminal: bool,
    /// Tracks whether we've already emitted Complete so a trailing
    /// `[DONE]` line after a terminal chunk doesn't double-fire.
    completed: bool,
}

impl ParserState {
    async fn process_chunk(
        &mut self,
        chunk: GeminiStreamChunk,
        tx: &mpsc::Sender<StreamEvent>,
    ) {
        if let Some(usage) = chunk.usage_metadata {
            self.usage_metadata = Some(usage);
        }
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
                });
            }
            // Only user-visible text fires TextDelta; chain-of-
            // thought summaries are accumulated for round-trip but
            // not surfaced.
            if !is_thought {
                let _ = tx.send(StreamEvent::TextDelta(text)).await;
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
                usage: self.usage_metadata.take(),
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
    } = part;
    let is_thought = thought.unwrap_or(false);
    let mut out = Vec::new();

    if let Some(call) = function_call {
        // Signature (if any) rides on the ToolCall via provider_opaque.
        let opaque = thought_signature
            .clone()
            .map(|sig| serde_json::json!({"thought_signature": sig}));
        out.push(Block::ToolCall {
            id: call.id,
            name: call.name,
            args: call.args,
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

    /// Drive the parser against an in-memory SSE body without going
    /// through `reqwest::Response` (which is opaque). Pulls the
    /// state-machine logic into the test directly.
    async fn run_stream(events: &[&str]) -> Vec<StreamEvent> {
        let mut body = String::new();
        for e in events {
            body.push_str("data: ");
            body.push_str(e);
            body.push_str("\n\n");
        }
        let (tx, mut rx) = mpsc::channel(128);
        tokio::spawn(async move {
            drive_fake(body, tx).await;
        });
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev);
        }
        out
    }

    async fn drive_fake(body: String, tx: mpsc::Sender<StreamEvent>) {
        let mut state = ParserState::default();
        for line in body.lines() {
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if data == "[DONE]" {
                state.emit_complete(&tx).await;
                return;
            }
            let Ok(chunk) = serde_json::from_str::<GeminiStreamChunk>(data) else {
                continue;
            };
            state.process_chunk(chunk, &tx).await;
            if state.terminal {
                state.emit_complete(&tx).await;
                return;
            }
        }
        state.emit_complete(&tx).await;
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
        assert!(turn.usage.is_some());
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
}
