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
use futures_util::StreamExt;
use std::collections::BTreeMap;
use tokio::sync::mpsc;

use super::wire::{AnthropicStreamEvent, AssistantResponse, MessageBlock, StopReason};

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
            MessageBlock::ToolResult { .. } => {
                // Tool results are client-originated; should never
                // appear in an assistant stream. Fall back to Text.
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
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut full_text = String::new();
    let mut blocks: BTreeMap<usize, BlockAccumulator> = BTreeMap::new();
    let mut stop_reason: Option<StopReason> = None;

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
                emit_complete(&tx, &mut blocks, stop_reason, &full_text).await;
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
                        if let Some(input) = cb.get("input") {
                            if let Ok(s) = serde_json::to_string(input) {
                                if s != "{}" {
                                    acc.input_json = s;
                                }
                            }
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
                                full_text.push_str(t);
                                let _ = tx.send(AnthropicStreamEvent::Token(t.to_string())).await;
                            }
                        }
                        "thinking_delta" => {
                            if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) {
                                acc.thinking.push_str(t);
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
                        let _ = tx
                            .send(AnthropicStreamEvent::BlockComplete {
                                block_index: index,
                                block: block.clone(),
                            })
                            .await;
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
                    }
                }
                "message_stop" => {
                    emit_complete(&tx, &mut blocks, stop_reason, &full_text).await;
                    return Ok(());
                }
                "error" => {
                    let msg = event
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("Unknown stream error");
                    let _ = tx.send(AnthropicStreamEvent::Error(msg.to_string())).await;
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    // Stream ended without message_stop — emit Complete anyway so
    // consumers don't hang.
    emit_complete(&tx, &mut blocks, stop_reason, &full_text).await;
    Ok(())
}

async fn emit_complete(
    tx: &mpsc::Sender<AnthropicStreamEvent>,
    blocks: &mut BTreeMap<usize, BlockAccumulator>,
    stop_reason: Option<StopReason>,
    full_text: &str,
) {
    let content: Vec<MessageBlock> = std::mem::take(blocks)
        .into_iter()
        .map(|(_, acc)| acc.finalize())
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
        id: None,
        content,
        stop_reason: effective_stop,
        stop_sequence: None,
    };
    let _ = tx
        .send(AnthropicStreamEvent::Complete {
            response: response.clone(),
        })
        .await;
    let _ = tx
        .send(AnthropicStreamEvent::Done(full_text.to_string()))
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
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run_stream(events: &[&str]) -> Vec<AnthropicStreamEvent> {
        // Build a synthetic SSE response body and drive the parser by
        // bypassing reqwest::Response (which is opaque).
        let mut body = String::new();
        for e in events {
            body.push_str("data: ");
            body.push_str(e);
            body.push_str("\n\n");
        }
        let (tx, mut rx) = mpsc::channel(128);
        let body_arc = body.clone();
        tokio::spawn(async move {
            let _ = drive_fake(body_arc, tx).await;
        });
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev);
        }
        out
    }

    // Drive the SSE parser against an in-memory body string by reusing
    // the state-machine logic manually (the prod fn takes a reqwest
    // body).
    async fn drive_fake(body: String, tx: mpsc::Sender<AnthropicStreamEvent>) -> Result<()> {
        let mut buffer = String::new();
        let mut full_text = String::new();
        let mut blocks: BTreeMap<usize, BlockAccumulator> = BTreeMap::new();
        let mut stop_reason: Option<StopReason> = None;
        buffer.push_str(&body);

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
                emit_complete(&tx, &mut blocks, stop_reason, &full_text).await;
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
                        if let Some(sig) = cb.get("signature").and_then(|v| v.as_str()) {
                            acc.signature = Some(sig.to_string());
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
                                full_text.push_str(t);
                                let _ = tx.send(AnthropicStreamEvent::Token(t.to_string())).await;
                            }
                        }
                        "thinking_delta" => {
                            if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) {
                                acc.thinking.push_str(t);
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
                        let _ = tx
                            .send(AnthropicStreamEvent::BlockComplete {
                                block_index: index,
                                block: block.clone(),
                            })
                            .await;
                        blocks.insert(index, BlockAccumulator::from_finalized(block));
                    }
                }
                "message_delta" => {
                    if let Some(delta) = event.get("delta") {
                        if let Some(sr) = delta.get("stop_reason").and_then(|v| v.as_str()) {
                            stop_reason = parse_stop_reason(sr);
                        }
                    }
                }
                "message_stop" => {
                    emit_complete(&tx, &mut blocks, stop_reason, &full_text).await;
                    return Ok(());
                }
                _ => {}
            }
        }
        emit_complete(&tx, &mut blocks, stop_reason, &full_text).await;
        Ok(())
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
}
