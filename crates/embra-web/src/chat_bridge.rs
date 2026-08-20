//! JSON ↔ proto translation for `/ws/chat` — the mobile chat UI's
//! conversation channel.
//!
//! The browser sends JSON [`ClientMsg`]s; the WS handler converts them to
//! [`apid::ConversationRequest`]s and forwards them through the
//! `Converse` gRPC stream to embra-apid. apid returns
//! [`apid::ConversationResponse`]s whose `payload` field is an opaque
//! serialized [`brain::ConversationResponse`] (pass-through); the handler
//! decodes that and re-encodes the variants as [`ServerMsg`] JSON for the
//! browser.
//!
//! Slash command parsing is *server-side* — the brain owns the slash
//! dispatcher. The client just sends [`ClientMsg::Slash`] (or types it
//! into a chat bubble as `/cmd args`); there is intentionally no
//! client-side slash parser here.
//!
//! [`ServerMsg::Reasoning`] is forwarded verbatim per the
//! REASONING-STREAM-01 contract: display-only, never persisted or
//! accumulated. Mirrors the TUI's expression panel.

use embra_common::proto::apid::{self, conversation_request};
use embra_common::proto::brain;
use serde::{Deserialize, Serialize};

/// What the browser sends over `/ws/chat`. Tagged with `t`.
#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum ClientMsg {
    /// User text turn — becomes a `UserMessage` on the Converse stream.
    /// `attachment_ids` are media ids from `PUT /api/media` (default
    /// empty, so pre-media clients keep parsing).
    Msg {
        text: String,
        #[serde(default)]
        attachment_ids: Vec<String>,
    },
    /// Slash command — becomes a `SlashCommand`. The brain parses the
    /// command name + args server-side; `args` may be empty.
    Slash {
        command: String,
        #[serde(default)]
        args: String,
    },
    /// Session attach (sent once on initial connect or after reconnect).
    /// Empty `session` = restore the most recent active session, matching
    /// the embra-console default.
    Attach {
        #[serde(default)]
        session: String,
    },
}

/// What the bridge sends back to the browser over `/ws/chat`.
#[derive(Debug, Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum ServerMsg {
    /// Streaming text chunk (one chunk per provider delta).
    Token { text: String },
    /// Assistant response complete — `text` is the assembled response.
    Done { text: String },
    /// System notification (info / warning / error / notification /
    /// reconnection). `kind` lower-cased from the proto enum so the
    /// frontend can switch on the string directly.
    System { content: String, kind: String },
    /// Tool execution record (after dispatch, with result).
    Tool {
        tool_use_id: String,
        name: String,
        input_json: String,
        result: String,
        is_error: bool,
    },
    /// Thinking-state indicator. `current_tool` is empty when no tool
    /// is in flight, populated while one is dispatching.
    Thinking {
        is_thinking: bool,
        name: String,
        current_tool: String,
    },
    /// Mode transition (setup → learning → operational). `from` / `to`
    /// are lower-cased mode names.
    Mode {
        from: String,
        to: String,
        message: String,
    },
    /// First-run wizard prompt. `field_type` is one of "text" /
    /// "selector" / "confirm".
    Setup {
        field_type: String,
        prompt: String,
        options: Vec<String>,
        default_value: String,
    },
    /// Live reasoning shard — **display-only, never persist** per
    /// REASONING-STREAM-01.
    Reasoning { text: String },
    /// An image became visible — attached by the operator, produced or
    /// viewed by a tool, or replayed from history on attach. `url` is the
    /// embra-web route that serves the bytes (`/api/media/<id>`).
    Media {
        id: String,
        media_type: String,
        width: u32,
        height: u32,
        byte_size: u64,
        name: String,
        origin: String,
        path: String,
        caption: String,
        tool_use_id: String,
        replay: bool,
        url: String,
    },
    /// Transport / decode error originating from this bridge.
    Error { message: String },
}

impl ClientMsg {
    /// Convert to the apid wire type. Note `apid::UserMessage` only has
    /// `content` (timestamp is dropped at apid; the brain stamps its own
    /// on receipt).
    pub fn into_proto(self) -> apid::ConversationRequest {
        use conversation_request::RequestType;
        let request_type = match self {
            ClientMsg::Msg { text, attachment_ids } => {
                RequestType::UserMessage(apid::UserMessage {
                    content: text,
                    attachment_ids,
                })
            }
            ClientMsg::Slash { command, args } => {
                RequestType::SlashCommand(apid::SlashCommand { command, args })
            }
            ClientMsg::Attach { session } => RequestType::SessionAttach(apid::SessionAttach {
                session_name: session,
            }),
        };
        apid::ConversationRequest {
            request_type: Some(request_type),
        }
    }
}

/// Map `brain::SystemMessageType` enum → lowercase string for JSON.
fn system_msg_kind(t: i32) -> &'static str {
    match t {
        1 => "info",
        2 => "warning",
        3 => "error",
        4 => "notification",
        5 => "reconnection",
        _ => "unspecified",
    }
}

/// Map `brain::OperatingMode` enum → lowercase string for JSON.
fn operating_mode(m: i32) -> &'static str {
    match m {
        1 => "setup",
        2 => "learning",
        3 => "operational",
        _ => "unspecified",
    }
}

/// Map `brain::SetupFieldType` enum → lowercase string for JSON.
/// Defaults to "text" for unspecified — matches embra-console's
/// fallback in `grpc_client.rs`.
fn setup_field_type(t: i32) -> &'static str {
    match t {
        2 => "selector",
        3 => "confirm",
        _ => "text",
    }
}

/// Translate a decoded `brain::ConversationResponse` into a `ServerMsg`.
/// Returns `None` when the response has no `response_type` set (shouldn't
/// happen in practice — the brain always emits a variant).
pub fn brain_to_server_msg(resp: brain::ConversationResponse) -> Option<ServerMsg> {
    use brain::conversation_response::ResponseType;
    let rt = resp.response_type?;
    Some(match rt {
        ResponseType::Token(t) => ServerMsg::Token { text: t.text },
        ResponseType::Done(d) => ServerMsg::Done {
            text: d.full_response,
        },
        ResponseType::System(s) => ServerMsg::System {
            content: s.content,
            kind: system_msg_kind(s.msg_type).to_string(),
        },
        ResponseType::Tool(t) => ServerMsg::Tool {
            tool_use_id: t.tool_use_id,
            name: t.tool_name,
            input_json: t.input_json,
            result: t.result,
            is_error: t.is_error,
        },
        ResponseType::Thinking(t) => ServerMsg::Thinking {
            is_thinking: t.is_thinking,
            name: t.name,
            current_tool: t.current_tool,
        },
        ResponseType::ModeChange(m) => ServerMsg::Mode {
            from: operating_mode(m.from_mode).to_string(),
            to: operating_mode(m.to_mode).to_string(),
            message: m.message,
        },
        ResponseType::Setup(s) => ServerMsg::Setup {
            field_type: setup_field_type(s.field_type).to_string(),
            prompt: s.prompt,
            options: s.options,
            default_value: s.default_value,
        },
        ResponseType::ReasoningDelta(r) => ServerMsg::Reasoning { text: r.text },
        ResponseType::Media(m) => media_server_msg(m),
    })
}

/// `MediaRef` → `ServerMsg::Media` with the serving URL filled in.
pub fn media_server_msg(m: brain::MediaRef) -> ServerMsg {
    ServerMsg::Media {
        url: format!("/api/media/{}", m.id),
        id: m.id,
        media_type: m.media_type,
        width: m.width,
        height: m.height,
        byte_size: m.byte_size,
        name: m.name,
        origin: m.origin,
        path: m.path,
        caption: m.caption,
        tool_use_id: m.tool_use_id,
        replay: m.replay,
    }
}

#[cfg(test)]
mod media_bridge_tests {
    use super::*;

    #[test]
    fn client_msg_msg_without_attachment_ids_still_parses() {
        // Pre-media clients send `{"t":"msg","text":...}` — must keep parsing.
        let m: ClientMsg = serde_json::from_str(r#"{"t":"msg","text":"hi"}"#).unwrap();
        match m {
            ClientMsg::Msg { text, attachment_ids } => {
                assert_eq!(text, "hi");
                assert!(attachment_ids.is_empty());
            }
            other => panic!("expected Msg, got {other:?}"),
        }
        let m: ClientMsg = serde_json::from_str(
            r#"{"t":"msg","text":"","attachment_ids":["att-20260820T153012Z-1a2b3c4d"]}"#,
        )
        .unwrap();
        let req = m.into_proto();
        match req.request_type {
            Some(conversation_request::RequestType::UserMessage(um)) => {
                assert_eq!(um.attachment_ids, vec!["att-20260820T153012Z-1a2b3c4d"]);
            }
            other => panic!("expected UserMessage, got {other:?}"),
        }
    }

    #[test]
    fn media_ref_maps_to_server_msg_with_url() {
        let resp = brain::ConversationResponse {
            response_type: Some(brain::conversation_response::ResponseType::Media(brain::MediaRef {
                id: "gen-20260820T153012Z-deadbeef".into(),
                media_type: "image/png".into(),
                width: 1024,
                height: 768,
                byte_size: 4096,
                name: "poster.png".into(),
                origin: "generated".into(),
                path: "/embra/workspace/MEDIA/gen-20260820T153012Z-deadbeef.png".into(),
                caption: String::new(),
                tool_use_id: "toolu_1".into(),
                replay: false,
            })),
        };
        let msg = brain_to_server_msg(resp).unwrap();
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["t"], "media");
        assert_eq!(v["url"], "/api/media/gen-20260820T153012Z-deadbeef");
        assert_eq!(v["origin"], "generated");
        assert_eq!(v["width"], 1024);
        assert_eq!(v["replay"], false);
    }
}
