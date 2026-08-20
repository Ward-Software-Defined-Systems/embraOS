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

use crate::provider::ir::{ApiMessage, Block, ImageData};

use super::wire::{
    GeminiBlob, GeminiContent, GeminiFunctionCall, GeminiFunctionResponse,
    GeminiFunctionResponsePart, GeminiPart,
};

/// Where a media tool's images go on the Gemini wire. The three candidate
/// placements are all implemented; the const selects one so the live
/// probe against a real Gemini 3 model is a one-line switch. Unit test
/// `tool_result_images_placement_is_locked` pins whichever is selected.
///
/// - `FunctionResponseParts`: `functionResponse.parts[].inlineData` — the
///   documented multimodal-function-response form (default).
/// - `SiblingInlineData`: `inlineData` parts in the same user content,
///   right after the `functionResponse` part.
/// - `TrailingUserContent`: a separate trailing `user` content with a
///   text label + `inlineData` parts (handled in `ir_messages_to_wire`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // the non-selected placements are the probe's alternatives, kept compiled
pub(crate) enum ToolResultImagePlacement {
    FunctionResponseParts,
    SiblingInlineData,
    TrailingUserContent,
}

pub(crate) const TOOL_RESULT_IMAGE_PLACEMENT: ToolResultImagePlacement =
    ToolResultImagePlacement::FunctionResponseParts;

fn blob(img: &ImageData) -> GeminiBlob {
    GeminiBlob {
        mime_type: img.media_type.clone(),
        data: img.data_b64.to_string(),
    }
}

fn inline_part(img: &ImageData) -> GeminiPart {
    GeminiPart {
        inline_data: Some(blob(img)),
        ..GeminiPart::default()
    }
}

/// Build the part(s) for one tool result. Text-only results produce the
/// single historical `functionResponse` part, byte-identical. With
/// images, the placement const decides; `TrailingUserContent` returns
/// the extra parts separately so the caller can mint a new content.
pub(crate) fn tool_result_parts(
    call_id: &str,
    name: &str,
    content: &str,
    is_error: bool,
    images: &[ImageData],
) -> (GeminiPart, Vec<GeminiPart>) {
    let response = if is_error {
        serde_json::json!({"error": content})
    } else {
        serde_json::json!({"result": content})
    };
    let mut func = GeminiFunctionResponse {
        id: call_id.to_string(),
        name: name.to_string(),
        response,
        parts: None,
    };
    if images.is_empty() {
        return (
            GeminiPart {
                function_response: Some(func),
                ..GeminiPart::default()
            },
            Vec::new(),
        );
    }
    match TOOL_RESULT_IMAGE_PLACEMENT {
        ToolResultImagePlacement::FunctionResponseParts => {
            func.parts = Some(
                images
                    .iter()
                    .map(|img| GeminiFunctionResponsePart { inline_data: blob(img) })
                    .collect(),
            );
            (
                GeminiPart {
                    function_response: Some(func),
                    ..GeminiPart::default()
                },
                Vec::new(),
            )
        }
        ToolResultImagePlacement::SiblingInlineData
        | ToolResultImagePlacement::TrailingUserContent => (
            GeminiPart {
                function_response: Some(func),
                ..GeminiPart::default()
            },
            {
                let mut extra = Vec::with_capacity(images.len() + 1);
                if TOOL_RESULT_IMAGE_PLACEMENT == ToolResultImagePlacement::TrailingUserContent {
                    extra.push(GeminiPart {
                        text: Some(format!("Image output of tool call {call_id}:")),
                        ..GeminiPart::default()
                    });
                }
                extra.extend(images.iter().map(inline_part));
                extra
            },
        ),
    }
}

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

    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        let (role, blocks) = match msg {
            ApiMessage::User { content } => ("user", content.as_slice()),
            ApiMessage::Assistant { content } => ("model", content.as_slice()),
        };
        let (parts, trailing) = ir_blocks_to_parts(blocks, &call_names);
        out.push(GeminiContent {
            role: role.to_string(),
            parts,
        });
        if !trailing.is_empty() {
            // `TrailingUserContent` placement: the media tool's images
            // ride a separate user content after the function responses.
            out.push(GeminiContent {
                role: "user".to_string(),
                parts: trailing,
            });
        }
    }
    out
}

/// Returns the content's parts plus any trailing parts that must become a
/// separate user content (only non-empty under the
/// `TrailingUserContent` placement).
fn ir_blocks_to_parts(
    blocks: &[Block],
    call_names: &HashMap<String, String>,
) -> (Vec<GeminiPart>, Vec<GeminiPart>) {
    let mut parts = Vec::with_capacity(blocks.len());
    let mut trailing = Vec::new();
    for block in blocks {
        match block {
            Block::Text(s) => {
                parts.push(GeminiPart {
                    text: Some(s.clone()),
                    ..GeminiPart::default()
                });
            }
            Block::Image(img) => parts.push(inline_part(img)),
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
            Block::ToolResult { call_id, content, is_error, images } => {
                let resolved_name = call_names.get(call_id).cloned().unwrap_or_default();
                let (part, extra) =
                    tool_result_parts(call_id, &resolved_name, content, *is_error, images);
                parts.push(part);
                match TOOL_RESULT_IMAGE_PLACEMENT {
                    ToolResultImagePlacement::SiblingInlineData => parts.extend(extra),
                    _ => trailing.extend(extra),
                }
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
    (parts, trailing)
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
                images: Vec::new(),
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
                images: Vec::new(),
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

    fn img(name: &str) -> ImageData {
        ImageData {
            media_type: "image/jpeg".into(),
            data_b64: std::sync::Arc::from("/9j/"),
            width: 4,
            height: 4,
            name: name.into(),
        }
    }

    #[test]
    fn image_block_becomes_inline_data_part() {
        let msgs = vec![ApiMessage::user_with_images(vec![img("a.jpg")], "what is this")];
        let wire = ir_messages_to_wire(&msgs);
        assert_eq!(wire.len(), 1);
        let v = serde_json::to_value(&wire[0]).unwrap();
        assert_eq!(v["parts"][0]["inlineData"]["mimeType"], "image/jpeg");
        assert_eq!(v["parts"][0]["inlineData"]["data"], "/9j/");
        assert_eq!(v["parts"][1]["text"], "what is this");
    }

    #[test]
    fn tool_result_without_images_is_byte_identical() {
        let (part, extra) = tool_result_parts("fc1", "system_status", "ok", false, &[]);
        assert!(extra.is_empty());
        assert_eq!(
            serde_json::to_string(&part).unwrap(),
            r#"{"functionResponse":{"id":"fc1","name":"system_status","response":{"result":"ok"}}}"#
        );
    }

    #[test]
    fn tool_result_images_placement_is_locked() {
        // Pins the SELECTED placement (see TOOL_RESULT_IMAGE_PLACEMENT).
        let msgs = vec![
            ApiMessage::Assistant {
                content: vec![Block::ToolCall {
                    id: "fc1".into(),
                    name: "image_view".into(),
                    args: json!({}),
                    provider_opaque: None,
                }],
            },
            ApiMessage::user_tool_results(vec![Block::ToolResult {
                call_id: "fc1".into(),
                content: "=== image a.jpg ===".into(),
                is_error: false,
                images: vec![img("a.jpg")],
            }]),
        ];
        let wire = ir_messages_to_wire(&msgs);
        match TOOL_RESULT_IMAGE_PLACEMENT {
            ToolResultImagePlacement::FunctionResponseParts => {
                assert_eq!(wire.len(), 2);
                let v = serde_json::to_value(&wire[1]).unwrap();
                assert_eq!(v["parts"].as_array().unwrap().len(), 1);
                let fr = &v["parts"][0]["functionResponse"];
                assert_eq!(fr["response"]["result"], "=== image a.jpg ===");
                assert_eq!(fr["parts"][0]["inlineData"]["mimeType"], "image/jpeg");
                assert_eq!(fr["parts"][0]["inlineData"]["data"], "/9j/");
            }
            ToolResultImagePlacement::SiblingInlineData => {
                assert_eq!(wire.len(), 2);
                let v = serde_json::to_value(&wire[1]).unwrap();
                assert_eq!(v["parts"].as_array().unwrap().len(), 2);
                assert!(v["parts"][0].get("functionResponse").is_some());
                assert_eq!(v["parts"][1]["inlineData"]["mimeType"], "image/jpeg");
            }
            ToolResultImagePlacement::TrailingUserContent => {
                assert_eq!(wire.len(), 3);
                assert_eq!(wire[2].role, "user");
                let v = serde_json::to_value(&wire[2]).unwrap();
                assert_eq!(v["parts"][0]["text"], "Image output of tool call fc1:");
                assert_eq!(v["parts"][1]["inlineData"]["mimeType"], "image/jpeg");
            }
        }
    }
}
