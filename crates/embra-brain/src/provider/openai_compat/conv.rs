//! Neutral IR ↔ OpenAI Chat Completions wire translators.
//!
//! Maps `crate::provider::ir::Block` to and from `wire::OpenAIMessage`
//! and friends. Two-way mapping:
//! - **IR → Wire** (request body): `ir_messages_to_wire` flattens
//!   `Vec<ApiMessage>` into `Vec<OpenAIMessage>`. Tool results land
//!   as separate `role:"tool"` messages (one per `Block::ToolResult`),
//!   matching the OpenAI conversation shape.
//! - **Wire → IR** (response parsing): `assistant_message_to_blocks`
//!   produces `Vec<Block>` from a parsed `OpenAIMessageOut` with
//!   harmony token sanitization on tool-call names and reasoning
//!   content captured as `Block::ProviderOpaque(json!({"kind":
//!   "reasoning", "content": "..."}))`.
//!
//! Defensive multi-key reasoning parser per Step 0 C1: check
//! `reasoning` first (cookbook primary, Ollama), `reasoning_content`
//! fallback (LM Studio newer default).

use serde_json::Value as JsonValue;

use crate::provider::ir::{ApiMessage, Block};
use crate::provider::openai_compat::sanitize::sanitize_harmony_tokens;
use crate::provider::openai_compat::wire::{
    OpenAIMessage, OpenAIMessageOut, OpenAIToolCall, OpenAIToolCallFunction,
};

#[derive(Debug, thiserror::Error)]
pub enum ConvError {
    #[error("user message contains mixed Text and ToolResult blocks (IR invariant violated)")]
    MixedUserBlocks,
    #[error("assistant message contains a ToolResult block (IR invariant violated)")]
    AssistantHasToolResult,
}

/// JSON tag identifying a reasoning-content `ProviderOpaque` block.
/// The IR encodes reasoning as `{"kind":"reasoning","content":"..."}`
/// inside `Block::ProviderOpaque(JsonValue)`.
pub const REASONING_KIND: &str = "reasoning";

/// Convert IR conversation messages into the OpenAI Chat Completions
/// `messages[]` array. The system prompt is NOT included here — caller
/// prepends `OpenAIMessage::System { content }` when building the
/// request body.
pub fn ir_messages_to_wire(messages: &[ApiMessage]) -> Result<Vec<OpenAIMessage>, ConvError> {
    let mut out = Vec::with_capacity(messages.len() + 4);
    for msg in messages {
        match msg {
            ApiMessage::User { content } => {
                user_blocks_to_wire(content, &mut out)?;
            }
            ApiMessage::Assistant { content } => {
                out.push(assistant_blocks_to_wire(content)?);
            }
        }
    }
    Ok(out)
}

fn user_blocks_to_wire(blocks: &[Block], out: &mut Vec<OpenAIMessage>) -> Result<(), ConvError> {
    let has_text = blocks.iter().any(|b| matches!(b, Block::Text(_)));
    let has_tool_result = blocks.iter().any(|b| matches!(b, Block::ToolResult { .. }));
    if has_text && has_tool_result {
        return Err(ConvError::MixedUserBlocks);
    }
    if has_tool_result {
        // One role:"tool" message per ToolResult, correlated by tool_call_id.
        for block in blocks {
            if let Block::ToolResult {
                call_id, content, ..
            } = block
            {
                out.push(OpenAIMessage::Tool {
                    tool_call_id: call_id.clone(),
                    content: content.clone(),
                });
            }
        }
        return Ok(());
    }
    // Pure text path. Concatenate all Text blocks (typical: one block).
    let text: String = blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if !text.is_empty() {
        out.push(OpenAIMessage::User { content: text });
    }
    Ok(())
}

fn assistant_blocks_to_wire(blocks: &[Block]) -> Result<OpenAIMessage, ConvError> {
    let mut text = String::new();
    let mut tool_calls: Vec<OpenAIToolCall> = Vec::new();
    let mut reasoning: Option<String> = None;
    for block in blocks {
        match block {
            Block::Text(s) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(s);
            }
            Block::ToolCall {
                id, name, args, ..
            } => {
                tool_calls.push(OpenAIToolCall {
                    id: id.clone(),
                    call_type: "function".to_string(),
                    function: OpenAIToolCallFunction {
                        name: name.clone(),
                        arguments: serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string()),
                    },
                });
            }
            Block::ToolResult { .. } => {
                return Err(ConvError::AssistantHasToolResult);
            }
            Block::ProviderOpaque(v) => {
                // Extract reasoning content if this opaque is our
                // {"kind":"reasoning","content":"..."} tagged shape.
                if let Some(content) = extract_reasoning_content(v) {
                    if reasoning.is_none() {
                        reasoning = Some(content);
                    } else if let Some(existing) = reasoning.as_mut() {
                        existing.push('\n');
                        existing.push_str(&content);
                    }
                }
            }
        }
    }
    Ok(OpenAIMessage::Assistant {
        content: if text.is_empty() { None } else { Some(text) },
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        reasoning,
        // Always emit `reasoning` (cookbook primary), never the alias.
        reasoning_content: None,
    })
}

/// Convert a parsed assistant message (non-streaming response) to IR
/// `Vec<Block>`. Streaming responses go through `streaming.rs` and
/// emit blocks block-by-block; this is for the assembled-message path
/// used by tests and any future non-streaming caller.
///
/// Tool-call names pass through `sanitize_harmony_tokens` with
/// telemetry (always-on per Locked Decision #11).
pub fn assistant_message_to_blocks(message: &OpenAIMessageOut, model_id: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();

    // Reasoning first (cookbook primary, then LM Studio alias).
    if let Some(content) = message
        .reasoning
        .as_deref()
        .or(message.reasoning_content.as_deref())
    {
        if !content.is_empty() {
            out.push(reasoning_block(content));
        }
    }

    // Text content.
    if let Some(text) = message.content.as_deref() {
        if !text.is_empty() {
            out.push(Block::Text(text.to_string()));
        }
    }

    // Tool calls.
    if let Some(calls) = &message.tool_calls {
        for call in calls {
            let sanitized_name = sanitize_harmony_tokens(&call.function.name, model_id);
            let args = parse_tool_args(&call.function.arguments);
            out.push(Block::ToolCall {
                id: call.id.clone(),
                name: sanitized_name.into_owned(),
                args,
                provider_opaque: None,
            });
        }
    }

    out
}

/// Build a `Block::ProviderOpaque` wrapping reasoning content.
pub fn reasoning_block(content: &str) -> Block {
    Block::ProviderOpaque(serde_json::json!({
        "kind": REASONING_KIND,
        "content": content,
    }))
}

/// Extract reasoning content from a `Block::ProviderOpaque` payload.
/// Returns None if the payload is not a reasoning-tagged opaque.
fn extract_reasoning_content(v: &JsonValue) -> Option<String> {
    let kind = v.get("kind").and_then(|k| k.as_str())?;
    if kind != REASONING_KIND {
        return None;
    }
    v.get("content")
        .and_then(|c| c.as_str())
        .map(String::from)
        .filter(|s| !s.is_empty())
}

/// Parse the wire's string-encoded `arguments` field into a JsonValue.
/// On parse failure (malformed JSON from the model), returns `{}` and
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
                target: "provider::openai_compat::conv",
                error = %e,
                raw = %raw,
                "tool_call.function.arguments is not valid JSON; using empty object"
            );
            JsonValue::Object(serde_json::Map::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::openai_compat::wire::OpenAIToolCall;
    use serde_json::json;

    #[test]
    fn user_text_block_round_trips() {
        let messages = vec![ApiMessage::user_text("hello")];
        let wire = ir_messages_to_wire(&messages).unwrap();
        assert_eq!(wire.len(), 1);
        let OpenAIMessage::User { content } = &wire[0] else {
            panic!("expected User variant");
        };
        assert_eq!(content, "hello");
    }

    #[test]
    fn user_tool_results_become_separate_tool_messages() {
        let messages = vec![ApiMessage::user_tool_results(vec![
            Block::ToolResult {
                call_id: "call_1".to_string(),
                content: "result one".to_string(),
                is_error: false,
            },
            Block::ToolResult {
                call_id: "call_2".to_string(),
                content: "result two".to_string(),
                is_error: false,
            },
        ])];
        let wire = ir_messages_to_wire(&messages).unwrap();
        assert_eq!(wire.len(), 2, "two tool results → two tool messages");
        let OpenAIMessage::Tool {
            tool_call_id,
            content,
        } = &wire[0]
        else {
            panic!("expected Tool variant");
        };
        assert_eq!(tool_call_id, "call_1");
        assert_eq!(content, "result one");
    }

    #[test]
    fn assistant_blocks_with_text_and_tool_call_produce_one_message() {
        let messages = vec![ApiMessage::Assistant {
            content: vec![
                Block::Text("Calling git_status".to_string()),
                Block::ToolCall {
                    id: "call_x".to_string(),
                    name: "git_status".to_string(),
                    args: json!({"path": "."}),
                    provider_opaque: None,
                },
            ],
        }];
        let wire = ir_messages_to_wire(&messages).unwrap();
        assert_eq!(wire.len(), 1);
        let OpenAIMessage::Assistant {
            content,
            tool_calls,
            reasoning,
            ..
        } = &wire[0]
        else {
            panic!("expected Assistant");
        };
        assert_eq!(content.as_deref(), Some("Calling git_status"));
        let calls = tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_x");
        assert_eq!(calls[0].function.name, "git_status");
        // arguments must be a STRING (encoded JSON) on the wire.
        assert_eq!(calls[0].function.arguments, "{\"path\":\".\"}");
        assert!(reasoning.is_none());
    }

    #[test]
    fn reasoning_provider_opaque_round_trips_through_wire() {
        // IR → wire: ProviderOpaque{kind:"reasoning"} → reasoning field.
        let blocks = vec![
            reasoning_block("internal thought process"),
            Block::Text("public answer".to_string()),
        ];
        let messages = vec![ApiMessage::Assistant { content: blocks }];
        let wire = ir_messages_to_wire(&messages).unwrap();
        let OpenAIMessage::Assistant {
            content, reasoning, ..
        } = &wire[0]
        else {
            panic!("expected Assistant");
        };
        assert_eq!(reasoning.as_deref(), Some("internal thought process"));
        assert_eq!(content.as_deref(), Some("public answer"));
    }

    #[test]
    fn non_reasoning_provider_opaque_is_ignored_in_wire_translate() {
        // A foreign-shape ProviderOpaque (not our reasoning tag) just
        // doesn't contribute to the wire — no panic, no leak.
        let blocks = vec![
            Block::ProviderOpaque(json!({"kind": "anthropic_thinking", "signature": "sig"})),
            Block::Text("answer".to_string()),
        ];
        let messages = vec![ApiMessage::Assistant { content: blocks }];
        let wire = ir_messages_to_wire(&messages).unwrap();
        let OpenAIMessage::Assistant {
            content, reasoning, ..
        } = &wire[0]
        else {
            panic!("expected Assistant");
        };
        assert_eq!(reasoning, &None);
        assert_eq!(content.as_deref(), Some("answer"));
    }

    #[test]
    fn wire_assistant_with_reasoning_primary_round_trips_to_ir() {
        // Wire → IR: reasoning field on assistant message produces
        // a Block::ProviderOpaque with our tagged shape.
        let msg = OpenAIMessageOut {
            role: "assistant".to_string(),
            content: Some("answer".to_string()),
            tool_calls: None,
            reasoning: Some("step 1, step 2".to_string()),
            reasoning_content: None,
        };
        let blocks = assistant_message_to_blocks(&msg, "test-model");
        // Reasoning before text in our convention.
        assert_eq!(blocks.len(), 2);
        let Block::ProviderOpaque(opaque) = &blocks[0] else {
            panic!("expected ProviderOpaque first");
        };
        assert_eq!(opaque["kind"], "reasoning");
        assert_eq!(opaque["content"], "step 1, step 2");
        let Block::Text(text) = &blocks[1] else {
            panic!("expected Text second");
        };
        assert_eq!(text, "answer");
    }

    #[test]
    fn wire_assistant_with_reasoning_content_alias_round_trips_to_ir() {
        // C1 fallback path: reasoning_content (LM Studio newer default).
        let msg = OpenAIMessageOut {
            role: "assistant".to_string(),
            content: Some("answer".to_string()),
            tool_calls: None,
            reasoning: None,
            reasoning_content: Some("alias content".to_string()),
        };
        let blocks = assistant_message_to_blocks(&msg, "test-model");
        let Block::ProviderOpaque(opaque) = &blocks[0] else {
            panic!("expected ProviderOpaque");
        };
        assert_eq!(opaque["content"], "alias content");
    }

    #[test]
    fn wire_assistant_with_both_reasoning_keys_prefers_primary() {
        // Defensive: if a server emits both, primary wins per C1 ordering.
        let msg = OpenAIMessageOut {
            role: "assistant".to_string(),
            content: Some("answer".to_string()),
            tool_calls: None,
            reasoning: Some("primary".to_string()),
            reasoning_content: Some("alias".to_string()),
        };
        let blocks = assistant_message_to_blocks(&msg, "test-model");
        let Block::ProviderOpaque(opaque) = &blocks[0] else {
            panic!("expected ProviderOpaque");
        };
        assert_eq!(opaque["content"], "primary");
    }

    #[test]
    fn empty_reasoning_strings_skipped() {
        let msg = OpenAIMessageOut {
            role: "assistant".to_string(),
            content: Some("answer".to_string()),
            tool_calls: None,
            reasoning: Some("".to_string()),
            reasoning_content: None,
        };
        let blocks = assistant_message_to_blocks(&msg, "test-model");
        assert!(matches!(blocks[0], Block::Text(_)));
    }

    #[test]
    fn tool_call_name_harmony_leak_sanitized() {
        // Wire → IR sanitizes the name field per Locked Decision #11.
        let msg = OpenAIMessageOut {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(vec![OpenAIToolCall {
                id: "call_x".to_string(),
                call_type: "function".to_string(),
                function: OpenAIToolCallFunction {
                    name: "git_commit<|channel|>analysis".to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
            reasoning: None,
            reasoning_content: None,
        };
        let blocks = assistant_message_to_blocks(&msg, "gpt-oss:120b");
        let Block::ToolCall { name, .. } = &blocks[0] else {
            panic!("expected ToolCall");
        };
        assert_eq!(name, "git_commit");
    }

    #[test]
    fn tool_call_args_string_round_trip_through_wire() {
        // Round-trip args object → wire string → IR object preserves content.
        let original_args = json!({
            "path": "/embra/workspace",
            "depth": 3,
            "flags": ["--verbose", "--json"]
        });
        let assistant = ApiMessage::Assistant {
            content: vec![Block::ToolCall {
                id: "call_y".to_string(),
                name: "fs_query".to_string(),
                args: original_args.clone(),
                provider_opaque: None,
            }],
        };
        let wire = ir_messages_to_wire(&[assistant]).unwrap();
        let OpenAIMessage::Assistant { tool_calls, .. } = &wire[0] else {
            panic!("expected Assistant");
        };
        let args_str = &tool_calls.as_ref().unwrap()[0].function.arguments;
        // Wire side: arguments is a string-encoded JSON.
        assert!(args_str.starts_with('{'));
        // Round-trip back via assistant_message_to_blocks.
        let received = OpenAIMessageOut {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(vec![OpenAIToolCall {
                id: "call_y".to_string(),
                call_type: "function".to_string(),
                function: OpenAIToolCallFunction {
                    name: "fs_query".to_string(),
                    arguments: args_str.clone(),
                },
            }]),
            reasoning: None,
            reasoning_content: None,
        };
        let blocks = assistant_message_to_blocks(&received, "test-model");
        let Block::ToolCall { args, .. } = &blocks[0] else {
            panic!("expected ToolCall");
        };
        assert_eq!(args, &original_args);
    }

    #[test]
    fn malformed_args_yield_empty_object() {
        // Per LM Studio docs, malformed tool calls fall back to content;
        // but if a malformed args string DOES reach the parser, fail
        // gracefully into {} rather than panic.
        let msg = OpenAIMessageOut {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(vec![OpenAIToolCall {
                id: "call_bad".to_string(),
                call_type: "function".to_string(),
                function: OpenAIToolCallFunction {
                    name: "git_status".to_string(),
                    arguments: "{not valid json".to_string(),
                },
            }]),
            reasoning: None,
            reasoning_content: None,
        };
        let blocks = assistant_message_to_blocks(&msg, "test-model");
        let Block::ToolCall { args, .. } = &blocks[0] else {
            panic!("expected ToolCall");
        };
        assert_eq!(args, &json!({}));
    }

    #[test]
    fn mixed_user_blocks_error() {
        let msg = ApiMessage::User {
            content: vec![
                Block::Text("hi".to_string()),
                Block::ToolResult {
                    call_id: "call_x".to_string(),
                    content: "r".to_string(),
                    is_error: false,
                },
            ],
        };
        let err = ir_messages_to_wire(&[msg]).unwrap_err();
        assert!(matches!(err, ConvError::MixedUserBlocks));
    }
}
