//! Neutral IR ↔ Anthropic wire converters.
//!
//! Round-trip invariants:
//! - A wire `Thinking` immediately followed by a `ToolUse` folds into a
//!   single `Block::ToolCall { provider_opaque: Some(<thinking JSON>) }`.
//! - A `Thinking` not followed by a `ToolUse` (e.g. ends a turn or
//!   precedes plain text) becomes a standalone `Block::ProviderOpaque`.
//! - Block order is preserved in both directions; loop-driver mutations
//!   on `Vec<Block>` survive round-trips because every IR variant maps
//!   1-to-1 (or 1-to-2 for the fold case) onto wire blocks.

use crate::provider::ir::{ApiMessage, Block, EarlyStopReason, ImageData, TurnOutcome};

use super::wire::{AnthropicWireMessage, ImageSource, MessageBlock, StopReason, ToolResultContent};

fn image_block(img: &ImageData) -> MessageBlock {
    MessageBlock::Image {
        source: ImageSource::Base64 {
            media_type: img.media_type.clone(),
            data: img.data_b64.to_string(),
        },
    }
}

/// Convert neutral IR messages into the Anthropic wire shape.
pub fn ir_messages_to_wire(messages: &[ApiMessage]) -> Vec<AnthropicWireMessage> {
    messages
        .iter()
        .map(|msg| match msg {
            ApiMessage::User { content } => AnthropicWireMessage::User {
                content: ir_blocks_to_wire(content),
            },
            ApiMessage::Assistant { content } => AnthropicWireMessage::Assistant {
                content: ir_blocks_to_wire(content),
            },
        })
        .collect()
}

/// Convert a neutral IR block list into the wire block list.
///
/// `Block::ToolCall.provider_opaque`, when present, expands to a
/// `MessageBlock::Thinking` emitted *before* the matching `ToolUse`,
/// preserving the Anthropic wire shape that the loop driver preserved
/// pre-refactor.
pub fn ir_blocks_to_wire(blocks: &[Block]) -> Vec<MessageBlock> {
    let mut out = Vec::with_capacity(blocks.len());
    for b in blocks {
        match b {
            Block::Text(text) => out.push(MessageBlock::Text { text: text.clone() }),
            Block::ToolCall { id, name, args, provider_opaque } => {
                if let Some(opaque) = provider_opaque {
                    if let Ok(thinking) = serde_json::from_value::<MessageBlock>(opaque.clone()) {
                        // Verbatim re-emit; signature MUST round-trip.
                        out.push(thinking);
                    }
                }
                out.push(MessageBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: args.clone(),
                });
            }
            Block::Image(img) => out.push(image_block(img)),
            Block::ToolResult { call_id, content, is_error, images } => {
                // Text-only results keep the bare-string form (byte-identical
                // to the pre-media wire); images switch to the array form —
                // text first, then each image block.
                let wire_content = if images.is_empty() {
                    ToolResultContent::Text(content.clone())
                } else {
                    let mut blocks = Vec::with_capacity(images.len() + 1);
                    if !content.is_empty() {
                        blocks.push(MessageBlock::Text { text: content.clone() });
                    }
                    blocks.extend(images.iter().map(image_block));
                    ToolResultContent::Blocks(blocks)
                };
                out.push(MessageBlock::ToolResult {
                    tool_use_id: call_id.clone(),
                    content: wire_content,
                    is_error: *is_error,
                });
            }
            Block::ProviderOpaque(json) => {
                if let Ok(block) = serde_json::from_value::<MessageBlock>(json.clone()) {
                    out.push(block);
                }
            }
        }
    }
    out
}

/// Convert wire blocks (an assistant turn's `content`) into neutral IR.
///
/// A wire `Thinking` followed by a `ToolUse` folds into one
/// `Block::ToolCall` with the thinking JSON in `provider_opaque`.
/// Standalone thinking becomes `Block::ProviderOpaque`.
pub fn wire_blocks_to_ir(blocks: Vec<MessageBlock>) -> Vec<Block> {
    let mut out = Vec::with_capacity(blocks.len());
    let mut iter = blocks.into_iter().peekable();
    while let Some(block) = iter.next() {
        match block {
            MessageBlock::Text { text } => out.push(Block::Text(text)),
            MessageBlock::Thinking { .. } => {
                let opaque = serde_json::to_value(&block).unwrap_or(serde_json::Value::Null);
                if matches!(iter.peek(), Some(MessageBlock::ToolUse { .. })) {
                    let Some(MessageBlock::ToolUse { id, name, input }) = iter.next() else {
                        unreachable!("peek matched ToolUse");
                    };
                    out.push(Block::ToolCall {
                        id,
                        name,
                        args: input,
                        provider_opaque: Some(opaque),
                    });
                } else {
                    out.push(Block::ProviderOpaque(opaque));
                }
            }
            MessageBlock::ToolUse { id, name, input } => out.push(Block::ToolCall {
                id,
                name,
                args: input,
                provider_opaque: None,
            }),
            MessageBlock::ToolResult { tool_use_id, content, is_error } => {
                // Wire → IR only ever runs on assistant output, which never
                // carries tool_result images; flatten defensively.
                out.push(Block::ToolResult {
                    call_id: tool_use_id,
                    content: content.text(),
                    is_error,
                    images: Vec::new(),
                });
            }
            MessageBlock::Image { .. } => {
                // The API never emits image blocks in assistant content;
                // if one ever arrives, dropping it is safer than replaying
                // it on an assistant turn (which the API rejects).
                tracing::warn!(target: "provider::anthropic", "dropped unexpected image block in assistant content");
            }
        }
    }
    out
}

/// Translate Anthropic's `stop_reason` into the neutral `TurnOutcome`.
pub fn stop_reason_to_outcome(reason: StopReason) -> TurnOutcome {
    match reason {
        StopReason::EndTurn => TurnOutcome::EndTurn,
        StopReason::ToolUse => TurnOutcome::ToolUse,
        StopReason::MaxTokens => TurnOutcome::MaxTokens,
        StopReason::PauseTurn => TurnOutcome::Pause,
        StopReason::StopSequence => TurnOutcome::EarlyStop(EarlyStopReason::StopSequence),
        StopReason::Refusal => TurnOutcome::EarlyStop(EarlyStopReason::Refusal),
    }
}

/// Translate wire `stop_details` (refusal detail) into the neutral IR
/// shape — a straight field map; both sides keep everything optional.
pub fn wire_stop_details_to_ir(
    w: super::wire::StopDetails,
) -> crate::provider::ir::StopDetails {
    crate::provider::ir::StopDetails {
        category: w.category,
        explanation: w.explanation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn thinking_followed_by_tool_use_folds_into_tool_call() {
        let wire = vec![
            MessageBlock::Thinking {
                thinking: "reason".into(),
                signature: "sig".into(),
            },
            MessageBlock::ToolUse {
                id: "t1".into(),
                name: "time".into(),
                input: json!({}),
            },
        ];
        let ir = wire_blocks_to_ir(wire);
        assert_eq!(ir.len(), 1);
        match &ir[0] {
            Block::ToolCall { id, name, provider_opaque, .. } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "time");
                let opaque = provider_opaque.as_ref().unwrap();
                assert_eq!(opaque["type"], "thinking");
                assert_eq!(opaque["signature"], "sig");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn thinking_with_text_in_between_does_not_fold() {
        let wire = vec![
            MessageBlock::Thinking {
                thinking: "reason".into(),
                signature: "sig".into(),
            },
            MessageBlock::Text { text: "hi".into() },
            MessageBlock::ToolUse {
                id: "t1".into(),
                name: "time".into(),
                input: json!({}),
            },
        ];
        let ir = wire_blocks_to_ir(wire);
        assert_eq!(ir.len(), 3);
        assert!(matches!(ir[0], Block::ProviderOpaque(_)));
        assert!(matches!(ir[1], Block::Text(_)));
        match &ir[2] {
            Block::ToolCall { provider_opaque, .. } => assert!(provider_opaque.is_none()),
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_preserves_order_for_thinking_text_tool_use() {
        let wire_in = vec![
            MessageBlock::Thinking {
                thinking: String::new(),
                signature: "sig".into(),
            },
            MessageBlock::Text { text: "I'll check.".into() },
            MessageBlock::ToolUse {
                id: "t1".into(),
                name: "time".into(),
                input: json!({}),
            },
        ];
        let ir = wire_blocks_to_ir(wire_in.clone());
        let wire_out = ir_blocks_to_wire(&ir);
        // Same length, same per-position kind, same payloads.
        assert_eq!(wire_out.len(), wire_in.len());
        for (i, (a, b)) in wire_in.iter().zip(wire_out.iter()).enumerate() {
            let ja = serde_json::to_value(a).unwrap();
            let jb = serde_json::to_value(b).unwrap();
            assert_eq!(ja, jb, "block {i} differs after round-trip");
        }
    }

    #[test]
    fn round_trip_preserves_order_for_folded_thinking_tool_use() {
        let wire_in = vec![
            MessageBlock::Thinking {
                thinking: String::new(),
                signature: "sig".into(),
            },
            MessageBlock::ToolUse {
                id: "t1".into(),
                name: "time".into(),
                input: json!({"q": 1}),
            },
        ];
        let ir = wire_blocks_to_ir(wire_in.clone());
        assert_eq!(ir.len(), 1);
        let wire_out = ir_blocks_to_wire(&ir);
        assert_eq!(wire_out.len(), 2);
        let ja: Vec<_> = wire_in.iter().map(|b| serde_json::to_value(b).unwrap()).collect();
        let jb: Vec<_> = wire_out.iter().map(|b| serde_json::to_value(b).unwrap()).collect();
        assert_eq!(ja, jb);
    }

    #[test]
    fn tool_result_round_trips() {
        let ir = vec![Block::ToolResult {
            call_id: "t1".into(),
            content: "ok".into(),
            is_error: false,
            images: Vec::new(),
        }];
        let wire = ir_blocks_to_wire(&ir);
        match &wire[0] {
            MessageBlock::ToolResult { tool_use_id, content, is_error } => {
                assert_eq!(tool_use_id, "t1");
                assert_eq!(content.text(), "ok");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    fn img(name: &str) -> ImageData {
        ImageData {
            media_type: "image/png".into(),
            data_b64: std::sync::Arc::from("iVBOR"),
            width: 2,
            height: 2,
            name: name.into(),
        }
    }

    #[test]
    fn tool_result_without_images_serializes_as_bare_string() {
        // Byte lock: the pre-media wire form. A text-only tool result must
        // never pick up the array form (prompt-cache + API parity).
        let wire = ir_blocks_to_wire(&[Block::ToolResult {
            call_id: "t1".into(),
            content: "ok".into(),
            is_error: false,
            images: Vec::new(),
        }]);
        assert_eq!(
            serde_json::to_string(&wire[0]).unwrap(),
            r#"{"type":"tool_result","tool_use_id":"t1","content":"ok"}"#
        );
    }

    #[test]
    fn tool_result_with_images_serializes_content_array_text_first() {
        let wire = ir_blocks_to_wire(&[Block::ToolResult {
            call_id: "t1".into(),
            content: "=== image a.png ===".into(),
            is_error: false,
            images: vec![img("a.png")],
        }]);
        let v = serde_json::to_value(&wire[0]).unwrap();
        assert_eq!(v["type"], "tool_result");
        let content = v["content"].as_array().expect("array form with images");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "=== image a.png ===");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "iVBOR");
    }

    #[test]
    fn image_block_serializes_base64_source() {
        let wire = ir_blocks_to_wire(&[Block::Image(img("a.png")), Block::Text("what is it".into())]);
        assert_eq!(
            serde_json::to_string(&wire[0]).unwrap(),
            r#"{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBOR"}}"#
        );
        assert!(matches!(&wire[1], MessageBlock::Text { text } if text == "what is it"));
    }

    #[test]
    fn assistant_image_block_is_dropped_on_wire_to_ir() {
        let ir = wire_blocks_to_ir(vec![
            MessageBlock::Image {
                source: ImageSource::Base64 { media_type: "image/png".into(), data: "x".into() },
            },
            MessageBlock::Text { text: "hi".into() },
        ]);
        assert_eq!(ir.len(), 1);
        assert!(matches!(&ir[0], Block::Text(t) if t == "hi"));
    }

    #[test]
    fn stop_reason_maps_outcomes() {
        assert_eq!(stop_reason_to_outcome(StopReason::EndTurn), TurnOutcome::EndTurn);
        assert_eq!(stop_reason_to_outcome(StopReason::ToolUse), TurnOutcome::ToolUse);
        assert_eq!(stop_reason_to_outcome(StopReason::MaxTokens), TurnOutcome::MaxTokens);
        assert_eq!(stop_reason_to_outcome(StopReason::PauseTurn), TurnOutcome::Pause);
        assert_eq!(
            stop_reason_to_outcome(StopReason::StopSequence),
            TurnOutcome::EarlyStop(EarlyStopReason::StopSequence)
        );
        assert_eq!(
            stop_reason_to_outcome(StopReason::Refusal),
            TurnOutcome::EarlyStop(EarlyStopReason::Refusal)
        );
    }

    #[test]
    fn wire_stop_details_maps_to_ir() {
        let ir = wire_stop_details_to_ir(super::super::wire::StopDetails {
            category: Some("cyber".into()),
            explanation: None,
        });
        assert_eq!(ir.category.as_deref(), Some("cyber"));
        assert_eq!(ir.explanation, None);
    }
}
