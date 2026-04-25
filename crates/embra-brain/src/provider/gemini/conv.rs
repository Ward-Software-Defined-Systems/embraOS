//! Neutral IR → Gemini wire converter.
//!
//! The reverse direction (wire → IR) lives in [`super::streaming`]
//! because chunk processing folds adjacent deltas before emitting.
//! This module handles the IR → wire path used by `stream_turn` to
//! build the request body.
//!
//! Round-trip rule: every IR construct emitted from streaming.rs
//! `part_to_blocks` must come back to a structurally-equivalent part
//! through `ir_messages_to_wire`. The IR's `Block::ToolCall.provider_opaque`
//! and `Block::ProviderOpaque(_)` carry JSON payloads minted by the
//! parser — they shape directly into `GeminiPart` fields here.

use std::collections::HashMap;

use crate::provider::ir::{ApiMessage, Block};

use super::wire::{GeminiContent, GeminiFunctionCall, GeminiFunctionResponse, GeminiPart};

/// Convert neutral IR messages into the Gemini wire shape.
///
/// `name` resolution for `Block::ToolResult`: walks all prior
/// messages, builds an `id → name` map from `ToolCall` blocks, and
/// uses that to fill `GeminiFunctionResponse.name` (which Gemini
/// requires but the neutral IR's `ToolResult` doesn't carry).
pub fn ir_messages_to_wire(messages: &[ApiMessage]) -> Vec<GeminiContent> {
    // Build call_id → tool_name lookup from every ToolCall in the
    // history. Walking the full slice (not just messages[..i]) is
    // fine — ids are unique per-call and the lookup just resolves a
    // name; later iteration won't introduce ambiguity.
    let mut call_names: HashMap<String, String> = HashMap::new();
    for msg in messages {
        for block in msg.content() {
            if let Block::ToolCall { id, name, .. } = block {
                call_names.insert(id.clone(), name.clone());
            }
        }
    }

    messages
        .iter()
        .map(|msg| {
            let (role, blocks) = match msg {
                ApiMessage::User { content } => ("user", content.as_slice()),
                ApiMessage::Assistant { content } => ("model", content.as_slice()),
            };
            GeminiContent {
                role: role.to_string(),
                parts: ir_blocks_to_parts(blocks, &call_names),
            }
        })
        .collect()
}

fn ir_blocks_to_parts(
    blocks: &[Block],
    call_names: &HashMap<String, String>,
) -> Vec<GeminiPart> {
    let mut parts = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            Block::Text(s) => {
                parts.push(GeminiPart {
                    text: Some(s.clone()),
                    ..GeminiPart::default()
                });
            }
            Block::ToolCall { id, name, args, provider_opaque } => {
                let signature = provider_opaque
                    .as_ref()
                    .and_then(|v| v.get("thought_signature"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                parts.push(GeminiPart {
                    function_call: Some(GeminiFunctionCall {
                        id: id.clone(),
                        name: name.clone(),
                        args: args.clone(),
                    }),
                    thought_signature: signature,
                    ..GeminiPart::default()
                });
            }
            Block::ToolResult { call_id, content, is_error } => {
                let resolved_name = call_names.get(call_id).cloned().unwrap_or_default();
                let response = if *is_error {
                    serde_json::json!({"error": content})
                } else {
                    serde_json::json!({"result": content})
                };
                parts.push(GeminiPart {
                    function_response: Some(GeminiFunctionResponse {
                        id: call_id.clone(),
                        name: resolved_name,
                        response,
                    }),
                    ..GeminiPart::default()
                });
            }
            Block::ProviderOpaque(json) => {
                // Mint a part from whatever the parser stashed.
                // Recognized payload keys: thought_signature (string),
                // thought (bool), text (string).
                let thought_signature = json
                    .get("thought_signature")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let thought = json.get("thought").and_then(|v| v.as_bool());
                let text = json.get("text").and_then(|v| v.as_str()).map(str::to_string);
                // Skip empty opaque blocks (no fields recognized).
                if thought_signature.is_none() && thought.is_none() && text.is_none() {
                    continue;
                }
                parts.push(GeminiPart {
                    text,
                    thought_signature,
                    thought,
                    ..GeminiPart::default()
                });
            }
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_text_translates_to_user_role_with_text_part() {
        let msgs = vec![ApiMessage::user_text("hi")];
        let wire = ir_messages_to_wire(&msgs);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].role, "user");
        assert_eq!(wire[0].parts.len(), 1);
        assert_eq!(wire[0].parts[0].text.as_deref(), Some("hi"));
    }

    #[test]
    fn assistant_tool_call_with_signature_serializes_to_function_call_with_signature() {
        let msgs = vec![ApiMessage::Assistant {
            content: vec![Block::ToolCall {
                id: "fc1".into(),
                name: "system_status".into(),
                args: json!({}),
                provider_opaque: Some(json!({"thought_signature": "sig-abc"})),
            }],
        }];
        let wire = ir_messages_to_wire(&msgs);
        assert_eq!(wire[0].role, "model");
        let part = &wire[0].parts[0];
        let fc = part.function_call.as_ref().expect("function_call set");
        assert_eq!(fc.id, "fc1");
        assert_eq!(fc.name, "system_status");
        assert_eq!(part.thought_signature.as_deref(), Some("sig-abc"));
    }

    #[test]
    fn parallel_tool_calls_no_signature_synthesized_for_subsequent() {
        let msgs = vec![ApiMessage::Assistant {
            content: vec![
                Block::ToolCall {
                    id: "fc1".into(),
                    name: "a".into(),
                    args: json!({}),
                    provider_opaque: Some(json!({"thought_signature": "only-on-first"})),
                },
                Block::ToolCall {
                    id: "fc2".into(),
                    name: "b".into(),
                    args: json!({}),
                    provider_opaque: None,
                },
            ],
        }];
        let wire = ir_messages_to_wire(&msgs);
        assert_eq!(wire[0].parts.len(), 2);
        assert_eq!(wire[0].parts[0].thought_signature.as_deref(), Some("only-on-first"));
        assert_eq!(wire[0].parts[1].thought_signature, None);
    }

    #[test]
    fn tool_result_resolves_name_from_prior_tool_call() {
        let msgs = vec![
            ApiMessage::Assistant {
                content: vec![Block::ToolCall {
                    id: "fc1".into(),
                    name: "system_status".into(),
                    args: json!({}),
                    provider_opaque: None,
                }],
            },
            ApiMessage::user_tool_results(vec![Block::ToolResult {
                call_id: "fc1".into(),
                content: "{\"healthy\": true}".into(),
                is_error: false,
            }]),
        ];
        let wire = ir_messages_to_wire(&msgs);
        // Second message is the user-side tool_results.
        assert_eq!(wire[1].role, "user");
        let part = &wire[1].parts[0];
        let fr = part.function_response.as_ref().expect("function_response set");
        assert_eq!(fr.id, "fc1");
        assert_eq!(fr.name, "system_status");
        assert_eq!(fr.response["result"], "{\"healthy\": true}");
    }

    #[test]
    fn tool_result_error_uses_error_key() {
        let msgs = vec![
            ApiMessage::Assistant {
                content: vec![Block::ToolCall {
                    id: "fc1".into(),
                    name: "broken".into(),
                    args: json!({}),
                    provider_opaque: None,
                }],
            },
            ApiMessage::user_tool_results(vec![Block::ToolResult {
                call_id: "fc1".into(),
                content: "boom".into(),
                is_error: true,
            }]),
        ];
        let wire = ir_messages_to_wire(&msgs);
        let fr = wire[1].parts[0].function_response.as_ref().unwrap();
        assert_eq!(fr.response["error"], "boom");
    }

    #[test]
    fn standalone_provider_opaque_with_signature_emits_signature_only_part() {
        let msgs = vec![ApiMessage::Assistant {
            content: vec![Block::ProviderOpaque(json!({
                "thought_signature": "late-sig"
            }))],
        }];
        let wire = ir_messages_to_wire(&msgs);
        assert_eq!(wire[0].parts.len(), 1);
        let part = &wire[0].parts[0];
        assert_eq!(part.thought_signature.as_deref(), Some("late-sig"));
        assert!(part.text.is_none());
        assert!(part.function_call.is_none());
    }
}
