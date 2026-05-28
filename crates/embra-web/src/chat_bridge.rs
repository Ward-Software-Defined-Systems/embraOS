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
    Msg { text: String },
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
            ClientMsg::Msg { text } => {
                RequestType::UserMessage(apid::UserMessage { content: text })
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
    })
}
