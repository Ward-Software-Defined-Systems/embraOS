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

use crate::provider::ir::{ApiMessage, Block, EarlyStopReason, TurnOutcome};

use super::wire::{AnthropicWireMessage, MessageBlock, StopReason};

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
            Block::ToolResult { call_id, content, is_error } => {
                out.push(MessageBlock::ToolResult {
                    tool_use_id: call_id.clone(),
                    content: content.clone(),
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
                out.push(Block::ToolResult {
                    call_id: tool_use_id,
                    content,
                    is_error,
                });
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
        }];
        let wire = ir_blocks_to_wire(&ir);
        match &wire[0] {
            MessageBlock::ToolResult { tool_use_id, content, is_error } => {
                assert_eq!(tool_use_id, "t1");
                assert_eq!(content, "ok");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
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
}
