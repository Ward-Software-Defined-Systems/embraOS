//! Anthropic SSE stream parser.
//!
//! Accumulates per-block state across `content_block_start`,
//! `content_block_delta`, and `content_block_stop` events to
//! reconstruct structured [`MessageBlock`]s. At `message_stop`, emits
//! a [`AnthropicStreamEvent::Complete`] carrying the full typed
//! [`AssistantResponse`].
//!
//! This module is internal to the Anthropic provider. The provider's
//! `stream_turn` translates these wire events into the neutral
//! [`crate::provider::StreamEvent`].

use anyhow::Result;
use futures_util::{Stream, StreamExt};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

use super::wire::{AnthropicStreamEvent, AssistantResponse, MessageBlock, StopDetails, StopReason};
use crate::provider::sse::LineBuffer;

#[derive(Debug)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse,
    /// Any `content_block` type we don't explicitly handle. Finalized
    /// as Text with whatever body arrived so we never drop silently.
    Unknown,
}

#[derive(Debug)]
struct BlockAccumulator {
    kind: BlockKind,
    text: String,
    thinking: String,
    signature: Option<String>,
    id: Option<String>,
    name: Option<String>,
    /// Partial JSON for tool_use input. Assembled across
    /// `input_json_delta` events and parsed on block_stop.
    input_json: String,
}

impl BlockAccumulator {
    fn new(kind: BlockKind) -> Self {
        Self {
            kind,
            text: String::new(),
            thinking: String::new(),
            signature: None,
            id: None,
            name: None,
            input_json: String::new(),
        }
    }

    fn finalize(self) -> MessageBlock {
        match self.kind {
            BlockKind::Text | BlockKind::Unknown => MessageBlock::Text { text: self.text },
            BlockKind::Thinking => MessageBlock::Thinking {
                thinking: self.thinking,
                // A thinking block without a signature would be
                // rejected by the API on the follow-up request. We
                // preserve whatever we got; if it's empty the
                // downstream request will fail and produce a clear
                // error.
                signature: self.signature.unwrap_or_default(),
            },
            BlockKind::ToolUse => {
                let input = if self.input_json.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&self.input_json).unwrap_or(serde_json::json!({}))
                };
                MessageBlock::ToolUse {
                    id: self.id.unwrap_or_default(),
                    name: self.name.unwrap_or_default(),
                    input,
                }
            }
        }
    }

    /// Rehydrate an accumulator from an already-finalized block so we
    /// can re-emit it as part of the final `Complete` event without
    /// cloning the finalization logic.
    fn from_finalized(block: MessageBlock) -> Self {
        let mut acc = Self::new(BlockKind::Text);
        match block {
            MessageBlock::Text { text } => {
                acc.kind = BlockKind::Text;
                acc.text = text;
            }
            MessageBlock::Thinking { thinking, signature } => {
                acc.kind = BlockKind::Thinking;
                acc.thinking = thinking;
                acc.signature = Some(signature);
            }
            MessageBlock::ToolUse { id, name, input } => {
                acc.kind = BlockKind::ToolUse;
                acc.id = Some(id);
                acc.name = Some(name);
                acc.input_json = serde_json::to_string(&input).unwrap_or_default();
            }
            MessageBlock::ToolResult { .. } | MessageBlock::Image { .. } => {
                // Tool results and images are client-originated; they
                // should never appear in an assistant stream. Fall back
                // to Text.
                acc.kind = BlockKind::Text;
            }
        }
        acc
    }
}

pub async fn process_sse_stream(
    response: reqwest::Response,
    tx: mpsc::Sender<AnthropicStreamEvent>,
) -> Result<()> {
    pump(response.bytes_stream(), tx).await
}

/// The parser, over any stream of byte chunks: the response body in
/// production, a list of byte vectors in the tests. The tests run this
/// function; there is no second copy of it for them to drift from.
async fn pump<S, B, E>(mut stream: S, tx: mpsc::Sender<AnthropicStreamEvent>) -> Result<()>
where
    S: Stream<Item = std::result::Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: Into<anyhow::Error>,
{
    let mut lines = LineBuffer::default();
    let mut blocks: BTreeMap<usize, BlockAccumulator> = BTreeMap::new();
    let mut stop_reason: Option<StopReason> = None;
    let mut stop_details: Option<StopDetails> = None;

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
                emit_complete(&tx, &mut blocks, stop_reason, stop_details).await;
                return Ok(());
            }

            let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

            match event_type {
                "content_block_start" => {
                    let index = event
                        .get("index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;
                    if let Some(cb) = event.get("content_block") {
                        let btype = cb.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                        let kind = match btype {
                            "text" => BlockKind::Text,
                            "thinking" => BlockKind::Thinking,
                            "tool_use" => BlockKind::ToolUse,
                            _ => BlockKind::Unknown,
                        };
                        let mut acc = BlockAccumulator::new(kind);
                        acc.id = cb
                            .get("id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        acc.name = cb
                            .get("name")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        // Thinking blocks may carry the signature on
                        // the initial block or via signature_delta —
                        // capture both paths.
                        if let Some(sig) = cb.get("signature").and_then(|v| v.as_str()) {
                            acc.signature = Some(sig.to_string());
                        }
                        // Initial tool_use input may arrive inline as
                        // `{}`; only seed when non-empty so the delta
                        // accumulator path doesn't have to undo it.
                        if let Some(input) = cb.get("input")
                            && let Ok(s) = serde_json::to_string(input)
                            && s != "{}"
                        {
                            acc.input_json = s;
                        }
                        blocks.insert(index, acc);
                    }
                }
                "content_block_delta" => {
                    let index = event
                        .get("index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;
                    let Some(delta) = event.get("delta") else {
                        continue;
                    };
                    let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let Some(acc) = blocks.get_mut(&index) else {
                        continue;
                    };
                    match delta_type {
                        "text_delta" => {
                            if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                                acc.text.push_str(t);
                                // Receiver dropped = consumer aborted the
                                // turn (operator /stop): exit so `stream`
                                // (and the connection) drop. Load-bearing
                                // — never downgrade to fire-and-forget.
                                if tx.send(AnthropicStreamEvent::Token(t.to_string())).await.is_err() {
                                    return Ok(());
                                }
                            }
                        }
                        "thinking_delta" => {
                            if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) {
                                acc.thinking.push_str(t);
                                if tx
                                    .send(AnthropicStreamEvent::ThinkingDelta(t.to_string()))
                                    .await
                                    .is_err()
                                {
                                    return Ok(());
                                }
                            }
                        }
                        "signature_delta" => {
                            if let Some(s) = delta.get("signature").and_then(|v| v.as_str()) {
                                match acc.signature.as_mut() {
                                    Some(existing) => existing.push_str(s),
                                    None => acc.signature = Some(s.to_string()),
                                }
                            }
                        }
                        "input_json_delta" => {
                            if let Some(s) = delta.get("partial_json").and_then(|v| v.as_str()) {
                                acc.input_json.push_str(s);
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let index = event
                        .get("index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;
                    if let Some(acc) = blocks.remove(&index) {
                        let block = acc.finalize();
                        // A /stop checkpoint: the event has no payload,
                        // the send is what notices a dropped receiver.
                        if tx.send(AnthropicStreamEvent::BlockComplete).await.is_err() {
                            return Ok(());
                        }
                        // Reinsert the finalized block so the final
                        // Complete event carries every block in order.
                        blocks.insert(index, BlockAccumulator::from_finalized(block));
                    }
                }
                "message_delta" => {
                    if let Some(delta) = event.get("delta") {
                        if let Some(sr) = delta.get("stop_reason").and_then(|v| v.as_str()) {
                            stop_reason = parse_stop_reason(sr);
                        }
                        if let Some(sd) = parse_stop_details(delta) {
                            stop_details = Some(sd);
                        }
                    }
                }
                "message_stop" => {
                    emit_complete(&tx, &mut blocks, stop_reason, stop_details).await;
                    return Ok(());
                }
                "error" => {
                    let msg = event
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("Unknown stream error");
                    // The one kind of send whose result is not read: the
                    // pump returns on the next line, receiver or no receiver.
                    let _ = tx.send(AnthropicStreamEvent::Error(msg.to_string())).await;
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    // Stream ended without message_stop — emit Complete anyway so
    // consumers don't hang.
    emit_complete(&tx, &mut blocks, stop_reason, stop_details).await;
    Ok(())
}

async fn emit_complete(
    tx: &mpsc::Sender<AnthropicStreamEvent>,
    blocks: &mut BTreeMap<usize, BlockAccumulator>,
    stop_reason: Option<StopReason>,
    stop_details: Option<StopDetails>,
) {
    let content: Vec<MessageBlock> = std::mem::take(blocks)
        .into_values()
        .map(|acc| acc.finalize())
        .collect();
    let effective_stop = stop_reason.unwrap_or_else(|| {
        // A missing message_delta means the SSE stream ended without
        // the final message_delta/message_stop pair — could be a
        // dropped connection, a truncated response, or an API-side
        // hiccup. Default to EndTurn for UX safety (otherwise the loop
        // hangs), but surface the condition for debug.
        tracing::warn!(
            target: "streaming",
            "stream closed without message_delta; defaulting stop_reason to EndTurn"
        );
        StopReason::EndTurn
    });
    let response = AssistantResponse {
        content,
        stop_reason: effective_stop,
        stop_details,
    };
    // The last send of the stream; nothing follows that a dropped receiver
    // would have to stop.
    let _ = tx
        .send(AnthropicStreamEvent::Complete { response })
        .await;
}

fn parse_stop_reason(s: &str) -> Option<StopReason> {
    Some(match s {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        "refusal" => StopReason::Refusal,
        "pause_turn" => StopReason::PauseTurn,
        "model_context_window_exceeded" => StopReason::ModelContextWindowExceeded,
        _ => return None,
    })
}

/// Extract `stop_details` from a `message_delta`'s `delta` object. The
/// API includes it only alongside `stop_reason: "refusal"`; absent,
/// null, or malformed → `None`.
fn parse_stop_details(delta: &serde_json::Value) -> Option<StopDetails> {
    delta
        .get("stop_details")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
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

    /// The events as one SSE body, arriving in one chunk.
    async fn run_stream(events: &[&str]) -> Vec<AnthropicStreamEvent> {
        run_chunks(vec![sse_body(events)]).await
    }

    /// Run the parser over the chunks and collect what it emits.
    async fn run_chunks(chunks: Vec<Vec<u8>>) -> Vec<AnthropicStreamEvent> {
        let (tx, mut rx) = mpsc::channel(128);
        let stream = futures_util::stream::iter(
            chunks.into_iter().map(Ok::<_, std::convert::Infallible>),
        );
        let parser = tokio::spawn(pump(stream, tx));
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev);
        }
        parser.await.expect("parser task").expect("parser result");
        out
    }

    fn tokens(out: &[AnthropicStreamEvent]) -> Vec<&str> {
        out.iter()
            .filter_map(|e| match e {
                AnthropicStreamEvent::Token(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn complete(out: &[AnthropicStreamEvent]) -> Option<AssistantResponse> {
        out.iter().find_map(|e| match e {
            AnthropicStreamEvent::Complete { response } => Some(response.clone()),
            _ => None,
        })
    }

    #[tokio::test]
    async fn text_only_stream_produces_text_block() {
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let out = run_stream(&events).await;
        let tokens: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                AnthropicStreamEvent::Token(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(tokens, vec!["Hello", " world"]);

        let complete = out
            .iter()
            .find_map(|e| match e {
                AnthropicStreamEvent::Complete { response } => Some(response.clone()),
                _ => None,
            })
            .expect("Complete event");
        assert_eq!(complete.stop_reason, StopReason::EndTurn);
        assert_eq!(complete.content.len(), 1);
        match &complete.content[0] {
            MessageBlock::Text { text } => assert_eq!(text, "Hello world"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn thinking_delta_emits_thinking_delta_wire_event() {
        // Verify the parser emits an `AnthropicStreamEvent::ThinkingDelta`
        // for every `thinking_delta` SSE event in order — these become
        // `StreamEvent::ReasoningDelta` for the live expression panel.
        // Signature-deltas must NOT emit a wire event (they ride only on
        // `BlockAccumulator::signature` for IR round-trip).
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"First thought."}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" More."}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-abc"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let out = run_stream(&events).await;
        let deltas: Vec<&str> = out
            .iter()
            .filter_map(|e| match e {
                AnthropicStreamEvent::ThinkingDelta(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            deltas,
            vec!["First thought.", " More."],
            "expected one ThinkingDelta per thinking_delta event"
        );
        // No ThinkingDelta events leak from signature_delta.
        assert!(
            !out.iter().any(|e| matches!(
                e,
                AnthropicStreamEvent::ThinkingDelta(s) if s.contains("sig")
            )),
            "signatures must not be emitted as ThinkingDelta wire events"
        );
    }

    #[tokio::test]
    async fn thinking_block_preserves_signature() {
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me reason..."}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-abc"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"-xyz"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let out = run_stream(&events).await;
        let complete = out
            .iter()
            .find_map(|e| match e {
                AnthropicStreamEvent::Complete { response } => Some(response.clone()),
                _ => None,
            })
            .expect("Complete event");
        assert_eq!(complete.content.len(), 1);
        match &complete.content[0] {
            MessageBlock::Thinking { thinking, signature } => {
                assert_eq!(thinking, "Let me reason...");
                assert_eq!(signature, "sig-abc-xyz");
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_use_block_assembles_input_json() {
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"recall","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":":\"alerts\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let out = run_stream(&events).await;
        let complete = out
            .iter()
            .find_map(|e| match e {
                AnthropicStreamEvent::Complete { response } => Some(response.clone()),
                _ => None,
            })
            .expect("Complete event");
        assert_eq!(complete.stop_reason, StopReason::ToolUse);
        assert_eq!(complete.content.len(), 1);
        match &complete.content[0] {
            MessageBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "recall");
                assert_eq!(input["query"], "alerts");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn multiple_blocks_preserve_order() {
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"I'll check."}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"time"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let out = run_stream(&events).await;
        let complete = out
            .iter()
            .find_map(|e| match e {
                AnthropicStreamEvent::Complete { response } => Some(response.clone()),
                _ => None,
            })
            .expect("Complete event");
        assert_eq!(complete.content.len(), 3);
        assert!(matches!(complete.content[0], MessageBlock::Thinking { .. }));
        assert!(matches!(complete.content[1], MessageBlock::Text { .. }));
        assert!(matches!(complete.content[2], MessageBlock::ToolUse { .. }));
    }

    /// An unrecognized stop reason parses to `None`, which the stream end
    /// turns into `EndTurn`. That is how `model_context_window_exceeded`
    /// used to end a turn silently; it has its own variant now.
    #[tokio::test]
    async fn context_window_stop_reason_is_not_folded_into_end_turn() {
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"model_context_window_exceeded"}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let out = run_stream(&events).await;
        let complete = out
            .iter()
            .find_map(|e| match e {
                AnthropicStreamEvent::Complete { response } => Some(response.clone()),
                _ => None,
            })
            .expect("Complete event");
        assert_eq!(complete.stop_reason, StopReason::ModelContextWindowExceeded);
        assert!(complete.stop_details.is_none());
        assert_eq!(parse_stop_reason("something_new"), None);
    }

    #[tokio::test]
    async fn refusal_message_delta_carries_stop_details() {
        // A pre-output refusal: HTTP 200, no content blocks, final
        // message_delta carries stop_reason + stop_details.
        let events = [
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"Request declined by safety classifier."}}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let out = run_stream(&events).await;
        let complete = out
            .iter()
            .find_map(|e| match e {
                AnthropicStreamEvent::Complete { response } => Some(response.clone()),
                _ => None,
            })
            .expect("Complete event");
        assert_eq!(complete.stop_reason, StopReason::Refusal);
        assert!(complete.content.is_empty());
        let details = complete.stop_details.expect("stop_details parsed");
        assert_eq!(details.category.as_deref(), Some("cyber"));
        assert_eq!(
            details.explanation.as_deref(),
            Some("Request declined by safety classifier.")
        );
    }

    #[tokio::test]
    async fn refusal_stop_details_with_null_category_and_missing_explanation_parses() {
        // category is nullable and explanation is not guaranteed —
        // both must parse without erroring (guarded reads downstream).
        let events = [
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":null}}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let out = run_stream(&events).await;
        let complete = out
            .iter()
            .find_map(|e| match e {
                AnthropicStreamEvent::Complete { response } => Some(response.clone()),
                _ => None,
            })
            .expect("Complete event");
        assert_eq!(complete.stop_reason, StopReason::Refusal);
        let details = complete.stop_details.expect("stop_details parsed");
        assert_eq!(details.category, None);
        assert_eq!(details.explanation, None);

        // And a plain non-refusal delta leaves stop_details None.
        let events = [
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let out = run_stream(&events).await;
        let complete = out
            .iter()
            .find_map(|e| match e {
                AnthropicStreamEvent::Complete { response } => Some(response.clone()),
                _ => None,
            })
            .expect("Complete event");
        assert_eq!(complete.stop_details, None);
    }

    // What follows had no test while the tests ran a copy of the parser:
    // the copy had none of these paths.

    #[tokio::test]
    async fn tool_input_given_whole_at_block_start_is_kept() {
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"recall","input":{"query":"alerts"}}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let response = complete(&run_stream(&events).await).expect("Complete event");
        match &response.content[0] {
            MessageBlock::ToolUse { input, .. } => assert_eq!(input["query"], "alerts"),
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn error_event_is_reported_and_ends_the_stream() {
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" never read"}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let out = run_stream(&events).await;
        assert_eq!(tokens(&out), ["partial"]);
        assert!(
            matches!(out.last(), Some(AnthropicStreamEvent::Error(m)) if m == "Overloaded"),
            "the error is the last event: {out:?}"
        );
        // No Complete: a turn that ended in an error is not a finished turn.
        assert!(complete(&out).is_none());
    }

    #[tokio::test]
    async fn done_sentinel_and_a_bare_end_of_stream_both_complete() {
        let text = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        ];
        // The body just ends: no message_delta, no message_stop.
        let response = complete(&run_stream(&text).await).expect("Complete event");
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert!(matches!(&response.content[0], MessageBlock::Text { text } if text == "hi"));

        let mut body = sse_body(&text);
        body.extend_from_slice(b"data: [DONE]\n\n");
        body.extend_from_slice(&sse_body(&[
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" after"}}"#,
        ]));
        let out = run_chunks(vec![body]).await;
        assert_eq!(tokens(&out), ["hi"]);
        assert!(complete(&out).is_some());
    }

    #[tokio::test]
    async fn framing_noise_is_skipped() {
        // Comments, `event:` lines, CRLF line ends, a frame that is not
        // JSON, a delta for a block that never started.
        let body = concat!(
            ": ping\r\n",
            "event: content_block_start\r\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\r\n\r\n",
            "data: not json\n\n",
            r#"data: {"type":"content_block_delta","index":7,"delta":{"type":"text_delta","text":"stray"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"kept"}}"#,
            "\r\n\r\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );
        let out = run_chunks(vec![body.as_bytes().to_vec()]).await;
        assert_eq!(tokens(&out), ["kept"]);
        let response = complete(&out).expect("Complete event");
        assert!(matches!(&response.content[0], MessageBlock::Text { text } if text == "kept"));
    }

    #[tokio::test]
    async fn a_line_is_read_whole_wherever_the_chunks_were_cut() {
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"file_write","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"notes.md\","}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"content\":\"plain text\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let body = sse_body(&events);
        let whole = complete(&run_chunks(vec![body.clone()]).await).expect("Complete event");
        // No `PartialEq` on the wire types; their `Debug` form says it all.
        let want = format!("{whole:?}");
        for cut in 1..body.len() {
            let out = run_chunks(vec![body[..cut].to_vec(), body[cut..].to_vec()]).await;
            let got = complete(&out).expect("Complete event");
            assert_eq!(format!("{got:?}"), want, "cut at byte {cut}");
        }
        match &whole.content[0] {
            MessageBlock::ToolUse { input, .. } => {
                assert_eq!(input["path"], "notes.md");
                assert_eq!(input["content"], "plain text");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    /// The network cuts where it cuts, and that can be inside a character.
    /// Both halves are bytes of the same line and have to be decoded
    /// together: decoded chunk by chunk, each half becomes U+FFFD - in
    /// the text the operator reads, and in what a tool call writes.
    #[tokio::test]
    async fn a_character_cut_in_two_by_the_network_stays_whole() {
        // 2, 3 and 4 bytes a character.
        let said = "caf\u{e9} \u{2014} \u{6f22}\u{5b57} \u{1f600}";
        let args = serde_json::json!({"path": "notes.md", "content": said}).to_string();
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#.to_string(),
            serde_json::json!({"type":"content_block_delta","index":0,
                "delta":{"type":"thinking_delta","thinking": said}}).to_string(),
            r#"{"type":"content_block_stop","index":0}"#.to_string(),
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#.to_string(),
            serde_json::json!({"type":"content_block_delta","index":1,
                "delta":{"type":"text_delta","text": said}}).to_string(),
            r#"{"type":"content_block_stop","index":1}"#.to_string(),
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"file_write","input":{}}}"#.to_string(),
            serde_json::json!({"type":"content_block_delta","index":2,
                "delta":{"type":"input_json_delta","partial_json": args}}).to_string(),
            r#"{"type":"content_block_stop","index":2}"#.to_string(),
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#.to_string(),
            r#"{"type":"message_stop"}"#.to_string(),
        ];
        let events: Vec<&str> = events.iter().map(String::as_str).collect();
        let body = sse_body(&events);
        assert!(body.len() > body.iter().filter(|b| b.is_ascii()).count(), "raw UTF-8 on the wire");

        for cut in 1..body.len() {
            let out = run_chunks(vec![body[..cut].to_vec(), body[cut..].to_vec()]).await;
            assert_eq!(tokens(&out), [said], "text, cut at byte {cut}");
            let response = complete(&out).expect("Complete event");
            match &response.content[..] {
                [
                    MessageBlock::Thinking { thinking, .. },
                    MessageBlock::Text { text },
                    MessageBlock::ToolUse { input, .. },
                ] => {
                    assert_eq!(thinking, said, "thinking, cut at byte {cut}");
                    assert_eq!(text, said, "text block, cut at byte {cut}");
                    assert_eq!(input["content"], said, "tool input, cut at byte {cut}");
                    assert_eq!(input["path"], "notes.md", "cut at byte {cut}");
                }
                other => panic!("cut at byte {cut}: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_last_line_without_its_newline_is_not_processed() {
        let mut body = sse_body(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"whole"}}"#,
        ]);
        body.extend_from_slice(
            br#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" cut off"}}"#,
        );
        let out = run_chunks(vec![body]).await;
        assert_eq!(tokens(&out), ["whole"]);
        assert!(complete(&out).is_some());
    }

    /// The `/stop` contract. The consumer drops the stream; the next send
    /// fails; the parser returns, and with it goes the response body —
    /// the connection. It must not read on.
    #[tokio::test]
    async fn a_dropped_receiver_ends_the_parser_at_its_next_send() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let first = sse_body(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"one"}}"#,
        ]);
        let more = sse_body(&[
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"two"}}"#,
        ]);
        for checkpoint in ["text", "thinking", "block end"] {
            let first = match checkpoint {
                "text" => first.clone(),
                "thinking" => sse_body(&[
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}"#,
                ]),
                _ => sse_body(&[
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"time"}}"#,
                    r#"{"type":"content_block_stop","index":0}"#,
                ]),
            };
            let read = Arc::new(AtomicUsize::new(0));
            let counter = read.clone();
            let chunks = vec![first, more.clone(), more.clone(), more.clone()];
            let stream = futures_util::stream::iter(
                chunks.into_iter().map(Ok::<_, std::convert::Infallible>),
            )
            .inspect(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            });
            let (tx, rx) = mpsc::channel(8);
            drop(rx);
            pump(stream, tx).await.expect("a dropped receiver is not an error");
            assert_eq!(read.load(Ordering::SeqCst), 1, "read on after {checkpoint}");
        }
    }

    #[tokio::test]
    async fn a_transport_error_is_returned_to_the_caller() {
        let first = sse_body(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"one"}}"#,
        ]);
        let stream = futures_util::stream::iter(vec![
            Ok(first),
            Err(std::io::Error::other("connection reset")),
        ]);
        let (tx, mut rx) = mpsc::channel(8);
        let err = pump(stream, tx).await.expect_err("the read error");
        assert!(err.to_string().contains("connection reset"));
        // What arrived before it was delivered; nothing is made up after
        // it. The caller turns the error into the Error event.
        assert!(matches!(rx.recv().await, Some(AnthropicStreamEvent::Token(t)) if t == "one"));
        assert!(rx.recv().await.is_none());
    }
}
