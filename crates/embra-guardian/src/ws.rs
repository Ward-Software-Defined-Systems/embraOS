//! The `websocket` capability: a connection that stays open between calls.
//!
//! A guest is a fresh instance on every call and keeps nothing, so the
//! connections are the host's. The guest sends one operation as JSON
//! (`open`, `send`, `receive`, `close`, `list`) and the guard here applies
//! the policy before the broker is asked: at `open` the rules of
//! `http_request` (`wss://` alone, the address classes and the operator's
//! egress allowlist, the operator's secrets added to the handshake by host
//! and port, a credential header refused), and on every frame sent the
//! refusal of a stored secret. The broker behind the trait owns the
//! sockets ([`WebSocketBroker`]; the brain's is `guardian/ws.rs`), and the
//! parts that decide what a call is answered ([`WsBuffer`],
//! [`top_level_id`]) are here, so the crate's fake broker and the real one
//! run the same tested code.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};

use crate::caps::{
    carries_secret, check_destination, err_json, guest_headers, is_header_token, secret_is_for,
    Capabilities, HTTP_HEADERS_BYTES_MAX, HTTP_HEADERS_MAX,
};

/// The I/O of one call beside its wait: the handshake of an `open`, a
/// frame sent, a close.
pub const WS_IO_BUDGET: Duration = Duration::from_secs(10);
/// How long a call waits for a frame when it names no `wait_ms`.
pub const WS_WAIT_DEFAULT: Duration = Duration::from_secs(10);
/// The longest a call may wait (`caps::HTTP_TIMEOUT_MAX`'s figure).
pub const WS_WAIT_MAX: Duration = Duration::from_secs(60);
/// The largest frame a tool may send (`caps::HTTP_REQUEST_BODY_MAX`).
pub const WS_SEND_MAX: usize = 256 * 1024;
/// The largest message accepted from a server; a larger one ends the
/// connection (close code 1009).
pub const WS_MESSAGE_MAX: usize = 4 * 1024 * 1024;
/// How much of one message is kept; the rest is cut and the frame says so.
pub const WS_KEPT_MAX: usize = 1024 * 1024;
/// What a connection buffers while no call reads: frames and bytes. Past
/// either the oldest frames are dropped and counted.
pub const WS_BUFFER_FRAMES: usize = 1024;
pub const WS_BUFFER_BYTES: usize = 4 * 1024 * 1024;
/// What one call is answered with: frames, and bytes of JSON text.
pub const WS_TAKE_FRAMES_DEFAULT: usize = 20;
pub const WS_TAKE_FRAMES_MAX: usize = 100;
pub const WS_TAKE_BYTES_DEFAULT: usize = 64 * 1024;
pub const WS_TAKE_BYTES_MAX: usize = 1024 * 1024;
/// Connections open at once: for one tool, and in all.
pub const WS_PER_TOOL: usize = 4;
pub const WS_PER_PROCESS: usize = 8;
/// A connection no call touched for this long is closed.
pub const WS_IDLE: Duration = Duration::from_secs(10 * 60);
/// A connection is closed at this age, whatever uses it.
pub const WS_LIFETIME: Duration = Duration::from_secs(60 * 60);
/// How much of a refused handshake's body is answered.
pub const WS_HANDSHAKE_BODY_MAX: usize = 512;
/// Subprotocols one `open` may offer.
const WS_PROTOCOLS_MAX: usize = 8;

/// Headers the handshake owns; a guest's value is dropped without a word.
/// A subprotocol is offered with `protocols`, and no extension is offered.
const HANDSHAKE_HEADERS: &[&str] = &[
    "host", "connection", "upgrade", "content-length", "transfer-encoding", "expect", "te", "keep-alive",
    "sec-websocket-key", "sec-websocket-version", "sec-websocket-accept", "sec-websocket-extensions",
    "sec-websocket-protocol",
];

/// The budget of one call of a tool with the capability: the I/O of its
/// operations and its wait, which the input names at its top level
/// (`wait_ms`, clamped as the guard clamps it; the default when it names
/// none). The broker gives every operation of the call what is left of it,
/// and the sandbox's deadline is `host::deadline_for_budget` of it. A
/// deadline of the wait alone would trap a guest whose `open` took nine
/// seconds.
pub fn call_budget(input: &Value) -> Duration {
    let wait = input
        .get("wait_ms")
        .and_then(Value::as_u64)
        .map_or(WS_WAIT_DEFAULT, Duration::from_millis)
        .min(WS_WAIT_MAX);
    WS_IO_BUDGET + wait
}

/// An `open` the guard passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsOpen {
    /// The URL as checked, its query included.
    pub url: String,
    pub host: String,
    pub port: u16,
    /// The addresses the guard classified: the broker dials these alone.
    pub addrs: Vec<SocketAddr>,
    /// Lowercase names; the operator's secret for this host among them.
    pub headers: Vec<(String, String)>,
    pub protocols: Vec<String>,
}

/// A connection the broker opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsOpened {
    pub id: String,
    /// The subprotocol the server chose.
    pub protocol: Option<String>,
    /// The handshake answer's headers.
    pub headers: Vec<(String, String)>,
}

/// Why an `open` gave no connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsOpenError {
    /// No handshake: the reason.
    Failed(String),
    /// The server answered the handshake with something other than 101.
    Http { status: u16, location: Option<String>, body: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsPayload {
    Text(String),
    Binary(Vec<u8>),
}

impl WsPayload {
    fn len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Binary(bytes) => bytes.len(),
        }
    }

    /// How many bytes of JSON text the payload takes in an answer: a text
    /// frame as a JSON string (a quote or a backslash is two bytes, a
    /// control character six, as the guest's serializer writes them), a
    /// binary frame as base64.
    fn answered_len(&self) -> usize {
        match self {
            Self::Text(text) => text.bytes().map(escaped_len).sum(),
            Self::Binary(bytes) => bytes.len().div_ceil(3) * 4,
        }
    }
}

fn escaped_len(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | b'\n' | b'\r' | b'\t' => 2,
        0x00..=0x1F => 6,
        _ => 1,
    }
}

/// One frame as a call is answered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsFrame {
    pub payload: WsPayload,
    /// The length of the message as it arrived; more than the payload's
    /// when the frame was cut.
    pub full_len: usize,
    /// The top-level `id` of a text frame that is a JSON object, read when
    /// the frame arrived and before any cut.
    pub top_id: Option<String>,
}

/// Which frames a call wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsPick {
    /// The frames in arrival order.
    Next,
    /// The frames in arrival order through the first text frame that
    /// contains the text.
    Through(String),
    /// The one text frame whose JSON has this top-level `id`. The frames
    /// around it stay buffered.
    Reply(String),
}

/// What a `receive` asks of the broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsTake {
    pub wait: Duration,
    pub max_frames: usize,
    pub max_bytes: usize,
    pub pick: WsPick,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsClosed {
    pub code: Option<u16>,
    pub reason: String,
}

/// What a `receive` is answered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WsBatch {
    pub frames: Vec<WsFrame>,
    /// Whether the frame a `reply_id` or an `until` asked for is among
    /// them; nothing for a plain read.
    pub matched: Option<bool>,
    /// The wait ended before what was asked for arrived.
    pub timed_out: bool,
    /// Frames still buffered.
    pub buffered: usize,
    /// Frames dropped since the connection opened.
    pub dropped: u64,
    pub closed: Option<WsClosed>,
}

/// One open connection, as `list` and the operator's command show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsConnInfo {
    pub id: String,
    pub host: String,
    pub port: u16,
    /// The URL's path, without its query.
    pub path: String,
    pub open_secs: u64,
    pub idle_secs: u64,
    pub buffered: usize,
    pub dropped: u64,
    pub closed: bool,
}

/// The connections of one tool. The guard has applied the policy before a
/// method is called; the broker dials, buffers and bounds. Every method is
/// called from the sandbox's blocking thread and may block up to the
/// call's budget.
pub trait WebSocketBroker: Send + Sync {
    fn open(&self, request: &WsOpen) -> Result<WsOpened, WsOpenError>;
    fn send(&self, id: &str, payload: WsPayload) -> Result<(), String>;
    fn receive(&self, id: &str, take: &WsTake) -> Result<WsBatch, String>;
    fn close(&self, id: &str, code: u16, reason: &str) -> Result<WsBatch, String>;
    fn list(&self) -> Vec<WsConnInfo>;
}

/// The top-level `id` of a JSON object, as text: a number as it is written,
/// a string as it reads. A CDP or JSON-RPC reply carries the `id` of its
/// command there; an `id` deeper in the object is an event's own.
pub fn top_level_id(text: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Top {
        id: Option<Value>,
    }
    if !text.trim_start().starts_with('{') {
        return None;
    }
    match serde_json::from_str::<Top>(text).ok()?.id? {
        Value::String(id) => Some(id),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

/// The frames of one connection that no call has taken yet.
#[derive(Debug)]
pub struct WsBuffer {
    frames: VecDeque<WsFrame>,
    bytes: usize,
    dropped: u64,
    max_frames: usize,
    max_bytes: usize,
}

impl Default for WsBuffer {
    fn default() -> Self {
        Self::new(WS_BUFFER_FRAMES, WS_BUFFER_BYTES)
    }
}

impl WsBuffer {
    pub fn new(max_frames: usize, max_bytes: usize) -> Self {
        Self { frames: VecDeque::new(), bytes: 0, dropped: 0, max_frames, max_bytes }
    }

    /// A message as it arrived. Its top-level id is read first, then the
    /// payload is kept up to [`WS_KEPT_MAX`]; the oldest frames make room.
    pub fn push(&mut self, payload: WsPayload) {
        let full_len = payload.len();
        let top_id = match &payload {
            WsPayload::Text(text) => top_level_id(text),
            WsPayload::Binary(_) => None,
        };
        let payload = cut_to(payload, WS_KEPT_MAX, false);
        self.bytes += payload.len();
        self.frames.push_back(WsFrame { payload, full_len, top_id });
        while self.frames.len() > self.max_frames || (self.bytes > self.max_bytes && self.frames.len() > 1) {
            if let Some(oldest) = self.frames.pop_front() {
                self.bytes -= oldest.payload.len();
                self.dropped += 1;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Whether what `pick` asks for has arrived: a wait ends when it has.
    pub fn has(&self, pick: &WsPick) -> bool {
        match pick {
            WsPick::Next => !self.frames.is_empty(),
            WsPick::Through(text) => self.frames.iter().any(|f| carries(f, text)),
            WsPick::Reply(id) => self.frames.iter().any(|f| f.top_id.as_deref() == Some(id)),
        }
    }

    /// The frames `pick` asks for, removed from the buffer, within
    /// `max_frames` and `max_bytes` of JSON text; and whether the frame a
    /// `reply_id` or an `until` named is among them. A first frame that
    /// does not fit is answered cut: it would otherwise hold the queue.
    pub fn take(&mut self, pick: &WsPick, max_frames: usize, max_bytes: usize) -> (Vec<WsFrame>, Option<bool>) {
        if let WsPick::Reply(id) = pick {
            let Some(at) = self.frames.iter().position(|f| f.top_id.as_deref() == Some(id)) else {
                return (Vec::new(), Some(false));
            };
            let Some(mut frame) = self.frames.remove(at) else { return (Vec::new(), Some(false)) };
            self.bytes -= frame.payload.len();
            frame.payload = cut_to(frame.payload, max_bytes, true);
            return (vec![frame], Some(true));
        }
        // An `until` that has not arrived answers what there is.
        let stop_at = match pick {
            WsPick::Through(text) => self.frames.iter().position(|f| carries(f, text)),
            _ => None,
        };
        let mut taken = Vec::new();
        let mut room = max_bytes;
        let mut matched = false;
        while taken.len() < max_frames {
            let Some(front) = self.frames.front() else { break };
            let needs = front.payload.answered_len();
            if needs > room && !taken.is_empty() {
                break;
            }
            let Some(mut frame) = self.frames.pop_front() else { break };
            self.bytes -= frame.payload.len();
            if needs > room {
                frame.payload = cut_to(frame.payload, room, true);
            }
            room = room.saturating_sub(frame.payload.answered_len());
            let was_the_match = stop_at == Some(taken.len());
            taken.push(frame);
            if was_the_match {
                matched = true;
                break;
            }
        }
        let matched = matches!(pick, WsPick::Through(_)).then_some(matched);
        (taken, matched)
    }
}

fn carries(frame: &WsFrame, text: &str) -> bool {
    matches!(&frame.payload, WsPayload::Text(payload) if payload.contains(text))
}

/// A payload cut to `max` bytes: of its own length, or, with `answered`,
/// of the JSON text it takes in an answer. Text is cut on a character.
fn cut_to(payload: WsPayload, max: usize, answered: bool) -> WsPayload {
    match payload {
        WsPayload::Text(mut text) => {
            let mut used = 0;
            let mut end = text.len();
            for (at, c) in text.char_indices() {
                let mut utf8 = [0u8; 4];
                let costs: usize = if answered {
                    c.encode_utf8(&mut utf8).bytes().map(escaped_len).sum()
                } else {
                    c.len_utf8()
                };
                if used + costs > max {
                    end = at;
                    break;
                }
                used += costs;
            }
            text.truncate(end);
            WsPayload::Text(text)
        }
        WsPayload::Binary(mut bytes) => {
            bytes.truncate(if answered { max / 4 * 3 } else { max });
            WsPayload::Binary(bytes)
        }
    }
}

/// The guard: one operation of the guest's, as JSON, to the JSON it is
/// answered. Always well-formed JSON, an error among it.
pub fn guarded_websocket(caps: &Capabilities, input: &str) -> String {
    let Some(broker) = caps.websocket.as_ref() else {
        return err_json("capability 'websocket' not granted to this tool");
    };
    let Ok(Value::Object(op)) = serde_json::from_str::<Value>(input) else {
        return err_json("the operation must be a JSON object with an \"op\"");
    };
    let answer = match op.get("op").and_then(Value::as_str) {
        Some("open") => open(caps, broker.as_ref(), &op),
        Some("send") => send(caps, broker.as_ref(), &op),
        Some("receive") => receive(broker.as_ref(), &op, None),
        Some("close") => close(broker.as_ref(), &op),
        Some("list") => Ok(json!({
            "ok": true,
            "connections": broker.list().iter().map(connection_json).collect::<Vec<_>>(),
        })),
        _ => Err("\"op\" must be one of open, send, receive, close, list".to_string()),
    };
    match answer {
        Ok(value) => value.to_string(),
        Err(why) => err_json(&why),
    }
}

type Op = serde_json::Map<String, Value>;

fn connection_json(c: &WsConnInfo) -> Value {
    json!({
        "id": c.id, "host": c.host, "port": c.port, "path": c.path,
        "open_seconds": c.open_secs, "idle_seconds": c.idle_secs,
        "buffered": c.buffered, "dropped": c.dropped, "closed": c.closed,
    })
}

fn id_of(op: &Op) -> Result<&str, String> {
    op.get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "\"id\" names the connection, as open answered it".to_string())
}

fn open(caps: &Capabilities, broker: &dyn WebSocketBroker, op: &Op) -> Result<Value, String> {
    let url = op.get("url").and_then(Value::as_str).ok_or("open needs a \"url\" (wss://…)")?;
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(given) = op.get("headers") {
        let given = given.as_object().ok_or("\"headers\" must be an object of strings")?;
        for (name, value) in given {
            let value = value.as_str().ok_or_else(|| format!("header '{name}' must be a string"))?;
            let lname = name.trim().to_ascii_lowercase();
            if !is_header_token(&lname) {
                return Err(format!("header '{name}' is not a valid header name"));
            }
            if value.contains(['\r', '\n']) {
                return Err(format!("header '{name}' must not contain a line break"));
            }
            if embra_tools_core::CREDENTIAL_HEADERS.contains(&lname.as_str()) {
                return Err(format!(
                    "header '{name}' is a credential: set it with /guardian secret <host> {name} <value>, \
                     never in a tool's input"
                ));
            }
            if HANDSHAKE_HEADERS.contains(&lname.as_str()) {
                continue;
            }
            headers.retain(|(n, _)| n != &lname);
            headers.push((lname, value.to_string()));
        }
    }
    let mut protocols = Vec::new();
    if let Some(given) = op.get("protocols") {
        for protocol in given.as_array().ok_or("\"protocols\" must be an array of strings")? {
            let protocol = protocol.as_str().filter(|p| is_header_token(p)).ok_or("a subprotocol must be a token")?;
            protocols.push(protocol.to_string());
        }
        if protocols.len() > WS_PROTOCOLS_MAX {
            return Err(format!("at most {WS_PROTOCOLS_MAX} subprotocols"));
        }
    }
    let mut sent: Vec<&str> = vec![url];
    sent.extend(headers.iter().map(|(_, value)| value.as_str()));
    sent.extend(protocols.iter().map(String::as_str));
    carries_secret(caps, &sent)?;

    let destination = check_destination(caps, url, "wss")?;
    let host = destination.url.host_str().unwrap_or_default().to_string();
    let port = destination.url.port_or_known_default().unwrap_or(443);
    // The operator's secret for this host and port rides the handshake, as
    // it rides a request. One stored under a header the handshake owns
    // cannot be sent.
    for secret in &caps.secrets {
        if secret_is_for(&secret.host, &destination.url) && !HANDSHAKE_HEADERS.contains(&secret.name.as_str()) {
            headers.retain(|(n, _)| n != &secret.name);
            headers.push((secret.name.clone(), secret.value.clone()));
        }
    }
    if !headers.iter().any(|(name, _)| name == "user-agent") {
        headers.push(("user-agent".to_string(), embra_tools_core::USER_AGENT.to_string()));
    }
    let header_bytes: usize = headers.iter().map(|(n, v)| n.len() + v.len()).sum();
    if headers.len() > HTTP_HEADERS_MAX || header_bytes > HTTP_HEADERS_BYTES_MAX {
        return Err(format!(
            "too many headers: at most {HTTP_HEADERS_MAX}, {HTTP_HEADERS_BYTES_MAX} bytes in all"
        ));
    }
    let request = WsOpen { url: destination.url.to_string(), host, port, addrs: destination.addrs, headers, protocols };
    match broker.open(&request) {
        Ok(opened) => Ok(json!({
            "ok": true,
            "id": opened.id,
            "protocol": opened.protocol,
            "headers": guest_headers(&opened.headers),
        })),
        Err(WsOpenError::Failed(why)) => Err(why),
        Err(WsOpenError::Http { status, location, body }) => {
            let body = &body[..body.len().min(WS_HANDSHAKE_BODY_MAX)];
            Ok(json!({
                "ok": false,
                "error": format!("the server answered the handshake with HTTP {status}, not a WebSocket"),
                "status": status,
                "redirect": location,
                "body": String::from_utf8_lossy(body),
            }))
        }
    }
}

fn send(caps: &Capabilities, broker: &dyn WebSocketBroker, op: &Op) -> Result<Value, String> {
    let id = id_of(op)?;
    let given = ["text", "json", "binary"].iter().filter(|key| op.contains_key(**key)).count();
    if given != 1 {
        return Err("send takes one of \"text\", \"json\" or \"binary\"".to_string());
    }
    let payload = if let Some(text) = op.get("text") {
        WsPayload::Text(text.as_str().ok_or("\"text\" must be a string")?.to_string())
    } else if let Some(value) = op.get("json") {
        // Serialized here: the guest's numbers are floating point, and an
        // integer above 2^53 would not survive it.
        WsPayload::Text(value.to_string())
    } else {
        let encoded = op.get("binary").and_then(Value::as_str).ok_or("\"binary\" must be base64 text")?;
        WsPayload::Binary(
            base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map_err(|_| "\"binary\" is not base64".to_string())?,
        )
    };
    if payload.len() > WS_SEND_MAX {
        return Err(format!("a frame is at most {WS_SEND_MAX} bytes; this one is {}", payload.len()));
    }
    match &payload {
        WsPayload::Text(text) => carries_secret(caps, &[text])?,
        WsPayload::Binary(bytes) => carries_secret(caps, &[&String::from_utf8_lossy(bytes)])?,
    }
    broker.send(id, payload)?;
    let waits = ["reply_id", "until", "wait_ms"].iter().any(|key| op.contains_key(*key));
    if waits {
        receive(broker, op, Some(true))
    } else {
        Ok(json!({ "ok": true, "sent": true }))
    }
}

/// A number or a string as the text an id is matched by.
fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::String(id) => Some(id.clone()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

fn clamped(op: &Op, key: &str, default: usize, max: usize) -> usize {
    op.get(key)
        .and_then(Value::as_u64)
        .map_or(default, |n| usize::try_from(n).unwrap_or(max))
        .clamp(1, max)
}

fn receive(broker: &dyn WebSocketBroker, op: &Op, sent: Option<bool>) -> Result<Value, String> {
    let id = id_of(op)?;
    let pick = match (op.get("reply_id"), op.get("until")) {
        (Some(_), Some(_)) => return Err("give \"reply_id\" or \"until\", not both".to_string()),
        (Some(reply), None) => WsPick::Reply(id_text(reply).ok_or("\"reply_id\" must be a number or a string")?),
        (None, Some(until)) => {
            WsPick::Through(until.as_str().filter(|u| !u.is_empty()).ok_or("\"until\" must be text")?.to_string())
        }
        (None, None) => WsPick::Next,
    };
    let wait = op
        .get("wait_ms")
        .and_then(Value::as_u64)
        .map_or(WS_WAIT_DEFAULT, Duration::from_millis)
        .min(WS_WAIT_MAX);
    let take = WsTake {
        wait,
        max_frames: clamped(op, "max_frames", WS_TAKE_FRAMES_DEFAULT, WS_TAKE_FRAMES_MAX),
        max_bytes: clamped(op, "max_bytes", WS_TAKE_BYTES_DEFAULT, WS_TAKE_BYTES_MAX),
        pick,
    };
    let batch = broker.receive(id, &take)?;
    let frames: Vec<Value> = batch
        .frames
        .iter()
        .map(|frame| {
            let mut out = match &frame.payload {
                WsPayload::Text(text) => json!({ "text": text }),
                WsPayload::Binary(bytes) => json!({
                    "binary": base64::engine::general_purpose::STANDARD.encode(bytes),
                    "bytes": bytes.len(),
                }),
            };
            if frame.payload.len() < frame.full_len {
                out["truncated"] = json!({ "bytes": frame.full_len });
            }
            out
        })
        .collect();
    let mut answer = json!({
        "ok": true,
        "frames": frames,
        "timed_out": batch.timed_out,
        "buffered": batch.buffered,
        "dropped": batch.dropped,
    });
    if let Some(sent) = sent {
        answer["sent"] = json!(sent);
    }
    if let Some(matched) = batch.matched {
        answer["matched"] = json!(matched);
    }
    if let Some(closed) = &batch.closed {
        answer["closed"] = json!({ "code": closed.code, "reason": closed.reason });
    }
    Ok(answer)
}

fn close(broker: &dyn WebSocketBroker, op: &Op) -> Result<Value, String> {
    let id = id_of(op)?;
    let code = op.get("code").and_then(Value::as_u64).map_or(1000, |c| u16::try_from(c).unwrap_or(1000));
    let reason: String = op.get("reason").and_then(Value::as_str).unwrap_or("").chars().take(123).collect();
    // A close reason is at most 123 bytes on the wire.
    let reason = cut_reason(&reason);
    let batch = broker.close(id, code, reason)?;
    Ok(json!({
        "ok": true,
        "closed": batch.closed.as_ref().map(|c| json!({ "code": c.code, "reason": c.reason })).unwrap_or(json!(true)),
        "dropped": batch.dropped,
    }))
}

fn cut_reason(reason: &str) -> &str {
    let mut end = reason.len().min(123);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    &reason[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{EgressPolicy, SecretHeader};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// A broker without a socket: every `open` is recorded and answered,
    /// what a test pushes arrives, and what is sent is kept.
    #[derive(Default)]
    struct Fake {
        opens: Mutex<Vec<WsOpen>>,
        sent: Mutex<Vec<(String, WsPayload)>>,
        buffers: Mutex<HashMap<String, WsBuffer>>,
        refuse: Mutex<Option<WsOpenError>>,
    }

    impl Fake {
        fn arrive(&self, id: &str, text: &str) {
            self.buffers.lock().unwrap().entry(id.to_string()).or_default().push(WsPayload::Text(text.to_string()));
        }
    }

    impl WebSocketBroker for Fake {
        fn open(&self, request: &WsOpen) -> Result<WsOpened, WsOpenError> {
            if let Some(refusal) = self.refuse.lock().unwrap().clone() {
                return Err(refusal);
            }
            let mut opens = self.opens.lock().unwrap();
            opens.push(request.clone());
            let id = format!("ws-{:08x}", opens.len());
            self.buffers.lock().unwrap().insert(id.clone(), WsBuffer::default());
            Ok(WsOpened {
                id,
                protocol: request.protocols.first().cloned(),
                headers: vec![("Set-Cookie".into(), "s=1".into()), ("Server".into(), "fake".into())],
            })
        }

        fn send(&self, id: &str, payload: WsPayload) -> Result<(), String> {
            if !self.buffers.lock().unwrap().contains_key(id) {
                return Err(format!("no connection {id}"));
            }
            self.sent.lock().unwrap().push((id.to_string(), payload));
            Ok(())
        }

        fn receive(&self, id: &str, take: &WsTake) -> Result<WsBatch, String> {
            let mut buffers = self.buffers.lock().unwrap();
            let buffer = buffers.get_mut(id).ok_or_else(|| format!("no connection {id}"))?;
            let arrived = buffer.has(&take.pick);
            let (frames, matched) = buffer.take(&take.pick, take.max_frames, take.max_bytes);
            Ok(WsBatch { frames, matched, timed_out: !arrived, buffered: buffer.len(), dropped: buffer.dropped(), closed: None })
        }

        fn close(&self, id: &str, code: u16, reason: &str) -> Result<WsBatch, String> {
            self.buffers.lock().unwrap().remove(id).ok_or_else(|| format!("no connection {id}"))?;
            Ok(WsBatch { closed: Some(WsClosed { code: Some(code), reason: reason.to_string() }), ..WsBatch::default() })
        }

        fn list(&self) -> Vec<WsConnInfo> {
            let mut ids: Vec<String> = self.buffers.lock().unwrap().keys().cloned().collect();
            ids.sort();
            ids.into_iter()
                .map(|id| WsConnInfo {
                    id,
                    host: "1.1.1.1".into(),
                    port: 443,
                    path: "/".into(),
                    open_secs: 1,
                    idle_secs: 0,
                    buffered: 0,
                    dropped: 0,
                    closed: false,
                })
                .collect()
        }
    }

    fn caps_with(fake: &Arc<Fake>) -> Capabilities {
        Capabilities { websocket: Some(fake.clone()), ..Capabilities::none() }
    }

    fn ask(caps: &Capabilities, op: Value) -> Value {
        serde_json::from_str(&guarded_websocket(caps, &op.to_string())).expect("the guard answers JSON")
    }

    fn text(frame: &str) -> WsPayload {
        WsPayload::Text(frame.to_string())
    }

    #[test]
    fn a_ws_or_http_url_is_refused_and_wss_passes() {
        let fake = Arc::new(Fake::default());
        let caps = caps_with(&fake);
        for url in ["ws://1.1.1.1/", "https://1.1.1.1/", "http://1.1.1.1/"] {
            let answer = ask(&caps, json!({"op": "open", "url": url}));
            assert_eq!(answer["ok"], false, "{url}");
            assert!(answer["error"].as_str().unwrap().starts_with("only wss:// destinations are allowed"), "{answer}");
        }
        let answer = ask(&caps, json!({"op": "open", "url": "wss://u:p@1.1.1.1/"}));
        assert!(answer["error"].as_str().unwrap().contains("userinfo"), "{answer}");
        let answer = ask(&caps, json!({"op": "open", "url": "wss://1.1.1.1/chromium?timeout=300000"}));
        assert_eq!(answer["ok"], true, "{answer}");
        assert_eq!(answer["id"], "ws-00000001");
        assert_eq!(fake.opens.lock().unwrap().len(), 1, "the refused opens never reached the broker");
        let none = ask(&Capabilities::none(), json!({"op": "open", "url": "wss://1.1.1.1/"}));
        assert!(none["error"].as_str().unwrap().contains("not granted"), "{none}");
        for bad in ["[]", "\"open\"", "{\"op\":\"dial\"}", "{}", "not json"] {
            let answer: Value = serde_json::from_str(&guarded_websocket(&caps, bad)).unwrap();
            assert_eq!(answer["ok"], false, "{bad}");
        }
    }

    /// An open is checked as a request is, and the broker is handed the
    /// addresses the check classified, the URL with its query, and the
    /// shared user agent.
    #[test]
    fn an_open_is_checked_like_a_request_and_dials_only_checked_addresses() {
        let fake = Arc::new(Fake::default());
        let answer = ask(&caps_with(&fake), json!({"op": "open", "url": "wss://1.1.1.1:8443/devtools?x=1", "protocols": ["cdp"]}));
        assert_eq!(answer["ok"], true, "{answer}");
        assert_eq!(answer["protocol"], "cdp");
        assert_eq!(answer["headers"]["server"], "fake");
        assert!(answer["headers"].get("set-cookie").is_none(), "a cookie never reaches the guest: {answer}");
        let opens = fake.opens.lock().unwrap();
        assert_eq!(opens[0].addrs, vec!["1.1.1.1:8443".parse::<SocketAddr>().unwrap()]);
        assert_eq!((opens[0].host.as_str(), opens[0].port), ("1.1.1.1", 8443));
        assert_eq!(opens[0].url, "wss://1.1.1.1:8443/devtools?x=1");
        assert_eq!(opens[0].headers, vec![("user-agent".to_string(), embra_tools_core::USER_AGENT.to_string())]);
        assert_eq!(opens[0].protocols, vec!["cdp".to_string()]);
    }

    #[test]
    fn a_private_host_needs_the_allowlist_and_loopback_never_opens() {
        let fake = Arc::new(Fake::default());
        let mut caps = caps_with(&fake);
        let private = ask(&caps, json!({"op": "open", "url": "wss://10.0.0.5/"}));
        assert!(private["error"].as_str().unwrap().contains("/guardian egress allow"), "{private}");
        caps.http_policy = EgressPolicy { private_hosts: vec!["10.0.0.5".into()], ..EgressPolicy::default() };
        assert_eq!(ask(&caps, json!({"op": "open", "url": "wss://10.0.0.5/"}))["ok"], true);
        caps.http_policy = EgressPolicy { private_hosts: vec!["127.0.0.1".into()], ..EgressPolicy::default() };
        let loopback = ask(&caps, json!({"op": "open", "url": "wss://127.0.0.1/"}));
        assert!(loopback["error"].as_str().unwrap().contains("loopback"), "{loopback}");
        assert_eq!(fake.opens.lock().unwrap().len(), 1);
    }

    /// The operator's secret rides the handshake of its host and port and
    /// of no other, and a secret stored under a header the handshake owns
    /// is not sent.
    #[test]
    fn a_secret_rides_the_handshake_of_its_host_and_port_and_no_other() {
        let fake = Arc::new(Fake::default());
        let mut caps = caps_with(&fake);
        caps.secrets = vec![
            SecretHeader { host: "1.1.1.1".into(), name: "authorization".into(), value: "Bearer tok".into() },
            SecretHeader { host: "1.1.1.1".into(), name: "sec-websocket-protocol".into(), value: "smuggled".into() },
            SecretHeader { host: "9.9.9.9".into(), name: "x-other".into(), value: "no".into() },
        ];
        let header = |open: &WsOpen, name: &str| open.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
        ask(&caps, json!({"op": "open", "url": "wss://1.1.1.1/chromium"}));
        ask(&caps, json!({"op": "open", "url": "wss://1.1.1.1:8443/chromium"}));
        ask(&caps, json!({"op": "open", "url": "wss://8.8.8.8/chromium"}));
        let opens = fake.opens.lock().unwrap();
        assert_eq!(header(&opens[0], "authorization").as_deref(), Some("Bearer tok"));
        assert_eq!(header(&opens[0], "sec-websocket-protocol"), None);
        assert_eq!(header(&opens[1], "authorization"), None, "another port is another place");
        assert_eq!(header(&opens[2], "authorization"), None);
        assert_eq!(header(&opens[2], "x-other"), None);
    }

    #[test]
    fn a_guest_may_not_set_a_credential_or_a_handshake_header() {
        let fake = Arc::new(Fake::default());
        let caps = caps_with(&fake);
        for name in ["Authorization", "cookie", "Proxy-Authorization"] {
            let answer = ask(&caps, json!({"op": "open", "url": "wss://1.1.1.1/", "headers": {name: "x"}}));
            assert!(answer["error"].as_str().unwrap().contains("/guardian secret"), "{name}: {answer}");
        }
        let answer = ask(&caps, json!({"op": "open", "url": "wss://1.1.1.1/", "headers": {
            "Sec-WebSocket-Extensions": "permessage-deflate", "Sec-WebSocket-Protocol": "x", "Upgrade": "h2c",
            "Host": "evil", "Origin": "https://app.example", "User-Agent": "tool/1"
        }}));
        assert_eq!(answer["ok"], true, "{answer}");
        let opens = fake.opens.lock().unwrap();
        let mut names: Vec<&str> = opens[0].headers.iter().map(|(n, _)| n.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["origin", "user-agent"], "the handshake's own headers are dropped");
        assert_eq!(opens[0].headers.iter().find(|(n, _)| n == "user-agent").unwrap().1, "tool/1");
        drop(opens);
        let broken = ask(&caps, json!({"op": "open", "url": "wss://1.1.1.1/", "headers": {"x": "a\r\nb"}}));
        assert!(broken["error"].as_str().unwrap().contains("line break"), "{broken}");
    }

    /// A stored secret in the URL, a header, a subprotocol or a frame is
    /// refused before the broker hears of it.
    #[test]
    fn an_open_or_a_frame_that_carries_a_stored_secret_is_refused_before_the_broker() {
        let fake = Arc::new(Fake::default());
        let mut caps = caps_with(&fake);
        let mut secrets = embra_tools_core::SecretSet::new();
        secrets.add("the browser service's token", "tok-0123456789abcdef");
        caps.outbound_secrets = Arc::new(secrets);
        for op in [
            json!({"op": "open", "url": "wss://1.1.1.1/?token=tok-0123456789abcdef"}),
            json!({"op": "open", "url": "wss://1.1.1.1/", "headers": {"x-token": "tok-0123456789abcdef"}}),
            json!({"op": "open", "url": "wss://1.1.1.1/", "protocols": ["tok-0123456789abcdef"]}),
        ] {
            let answer = ask(&caps, op);
            assert!(answer["error"].as_str().unwrap().contains("carries the browser service's token"), "{answer}");
        }
        assert!(fake.opens.lock().unwrap().is_empty());
        let id = ask(&caps, json!({"op": "open", "url": "wss://1.1.1.1/"}))["id"].as_str().unwrap().to_string();
        let encoded = base64::engine::general_purpose::STANDARD.encode("x tok-0123456789abcdef y");
        for op in [
            json!({"op": "send", "id": id, "text": "say tok-0123456789abcdef"}),
            json!({"op": "send", "id": id, "json": {"params": {"value": "tok-0123456789abcdef"}}}),
            json!({"op": "send", "id": id, "binary": encoded}),
        ] {
            let answer = ask(&caps, op);
            assert!(answer["error"].as_str().unwrap().contains("carries the browser service's token"), "{answer}");
        }
        assert!(fake.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn a_refused_handshake_answers_its_status_and_a_redirect_is_not_followed() {
        let fake = Arc::new(Fake::default());
        let caps = caps_with(&fake);
        *fake.refuse.lock().unwrap() = Some(WsOpenError::Http {
            status: 401,
            location: None,
            body: format!("Bad or missing authentication.{}", "x".repeat(2000)).into_bytes(),
        });
        let answer = ask(&caps, json!({"op": "open", "url": "wss://1.1.1.1/chromium"}));
        assert_eq!((answer["ok"].as_bool(), answer["status"].as_u64()), (Some(false), Some(401)), "{answer}");
        let body = answer["body"].as_str().unwrap();
        assert!(body.starts_with("Bad or missing authentication.") && body.len() == WS_HANDSHAKE_BODY_MAX, "{}", body.len());
        *fake.refuse.lock().unwrap() =
            Some(WsOpenError::Http { status: 302, location: Some("wss://127.0.0.1/".into()), body: Vec::new() });
        let answer = ask(&caps, json!({"op": "open", "url": "wss://1.1.1.1/chromium"}));
        assert_eq!(answer["redirect"], "wss://127.0.0.1/", "named, and not followed: {answer}");
        *fake.refuse.lock().unwrap() = Some(WsOpenError::Failed("the handshake took longer than 10 s".into()));
        let answer = ask(&caps, json!({"op": "open", "url": "wss://1.1.1.1/chromium"}));
        assert_eq!(answer["error"], "the handshake took longer than 10 s");
    }

    /// A command and its reply are one call: the frame with the command's
    /// top-level id is answered alone, whatever arrived around it, and the
    /// rest stays for the next read.
    #[test]
    fn a_reply_is_picked_by_its_top_level_id_and_the_rest_stay() {
        let fake = Arc::new(Fake::default());
        let caps = caps_with(&fake);
        let id = ask(&caps, json!({"op": "open", "url": "wss://1.1.1.1/"}))["id"].as_str().unwrap().to_string();
        fake.arrive(&id, r#"{"method":"Target.targetCreated","params":{"targetInfo":{"targetId":"T"}}}"#);
        fake.arrive(&id, r#"{"id":10,"result":{}}"#);
        fake.arrive(&id, r#"{"method":"Runtime.executionContextCreated","params":{"context":{"id":1}}}"#);
        fake.arrive(&id, r#"{"id":1,"result":{"targetId":"T"}}"#);
        let answer = ask(&caps, json!({
            "op": "send", "id": id, "reply_id": 1,
            "json": {"id": 1, "method": "Target.createTarget", "params": {"url": "about:blank", "big": 9007199254740993u64}},
        }));
        assert_eq!(answer["sent"], true, "{answer}");
        assert_eq!(answer["matched"], true);
        assert_eq!(answer["frames"].as_array().unwrap().len(), 1);
        assert_eq!(answer["frames"][0]["text"], r#"{"id":1,"result":{"targetId":"T"}}"#);
        assert_eq!(answer["buffered"], 3, "the events and the other reply stay");
        let sent = fake.sent.lock().unwrap();
        assert_eq!(
            sent[0].1,
            text(r#"{"id":1,"method":"Target.createTarget","params":{"big":9007199254740993,"url":"about:blank"}}"#),
            "the host serializes the command: a large integer survives"
        );
        drop(sent);
        let again = ask(&caps, json!({"op": "receive", "id": id, "reply_id": "10"}));
        assert_eq!(again["frames"][0]["text"], r#"{"id":10,"result":{}}"#, "a string names the same id");
        let none = ask(&caps, json!({"op": "receive", "id": id, "reply_id": 7, "wait_ms": 0}));
        assert_eq!((none["matched"].as_bool(), none["timed_out"].as_bool(), none["buffered"].as_u64()), (Some(false), Some(true), Some(2)));
        assert!(none["frames"].as_array().unwrap().is_empty());
    }

    #[test]
    fn an_id_inside_an_event_is_not_a_reply() {
        assert_eq!(top_level_id(r#"{"id":1,"result":{}}"#).as_deref(), Some("1"));
        assert_eq!(top_level_id(r#" {"result":{},"id":"abc"}"#).as_deref(), Some("abc"));
        assert_eq!(top_level_id(r#"{"method":"x","params":{"context":{"id":1}}}"#), None);
        assert_eq!(top_level_id(r#"{"id":10,"result":{}}"#).as_deref(), Some("10"), "10 is not 1");
        assert_eq!(top_level_id(r#"[{"id":1}]"#), None);
        assert_eq!(top_level_id(r#"{"id":null}"#), None);
        assert_eq!(top_level_id(r#"{"id":{"a":1}}"#), None);
        assert_eq!(top_level_id("id: 1"), None);
        assert_eq!(top_level_id(r#"{"id":1"#), None, "what does not parse has no id");
    }

    /// `until` answers the frames in order through the first that carries
    /// the text; when it has not arrived, what there is.
    #[test]
    fn until_answers_the_frames_through_the_first_that_carries_it() {
        let mut buffer = WsBuffer::default();
        for frame in ["one", "two Page.loadEventFired", "three", "four Page.loadEventFired"] {
            buffer.push(text(frame));
        }
        let pick = WsPick::Through("Page.loadEventFired".into());
        assert!(buffer.has(&pick));
        let (frames, matched) = buffer.take(&pick, 20, WS_TAKE_BYTES_DEFAULT);
        assert_eq!(frames.iter().map(|f| f.payload.clone()).collect::<Vec<_>>(), vec![text("one"), text("two Page.loadEventFired")]);
        assert_eq!(matched, Some(true));
        assert_eq!(buffer.len(), 2);
        let absent = WsPick::Through("Network.loadingFinished".into());
        assert!(!buffer.has(&absent));
        let (frames, matched) = buffer.take(&absent, 20, WS_TAKE_BYTES_DEFAULT);
        assert_eq!((frames.len(), matched), (2, Some(false)));
        let (frames, matched) = buffer.take(&WsPick::Next, 20, WS_TAKE_BYTES_DEFAULT);
        assert_eq!((frames.len(), matched), (0, None));
    }

    #[test]
    fn a_full_buffer_drops_its_oldest_frames_and_counts_them() {
        let mut buffer = WsBuffer::new(3, 1000);
        for n in 0..5 {
            buffer.push(text(&format!("frame {n}")));
        }
        assert_eq!((buffer.len(), buffer.dropped()), (3, 2));
        let (frames, _) = buffer.take(&WsPick::Next, 10, 1000);
        assert_eq!(frames[0].payload, text("frame 2"), "the oldest went");
        let mut buffer = WsBuffer::new(100, 1000);
        buffer.push(text(&"a".repeat(600)));
        buffer.push(text(&"b".repeat(600)));
        assert_eq!((buffer.len(), buffer.dropped()), (1, 1), "over the bytes: the oldest goes");
        buffer.push(text(&"c".repeat(5000)));
        assert_eq!((buffer.len(), buffer.dropped()), (1, 2), "one frame over the bytes by itself is kept");
        assert!(!buffer.is_empty());
    }

    /// The id of a frame is read as it arrives, before the frame is cut to
    /// what is kept.
    #[test]
    fn the_id_of_a_cut_frame_is_read_before_the_cut() {
        let mut buffer = WsBuffer::default();
        let big = format!(r#"{{"id":5,"result":{{"data":"{}"}}}}"#, "A".repeat(WS_KEPT_MAX + 100));
        buffer.push(text(&big));
        let (frames, matched) = buffer.take(&WsPick::Reply("5".into()), 1, WS_TAKE_BYTES_MAX);
        assert_eq!(matched, Some(true));
        assert_eq!(frames[0].top_id.as_deref(), Some("5"));
        assert_eq!(frames[0].full_len, big.len());
        assert!(frames[0].payload.len() <= WS_KEPT_MAX);
    }

    /// A frame costs what it takes as JSON text in the answer: a control
    /// character six bytes, a quote two. A budget counted in raw bytes
    /// would overrun the sandbox's output on such a frame.
    #[test]
    fn frames_are_budgeted_by_their_escaped_length() {
        assert_eq!(text("ab").answered_len(), 2);
        assert_eq!(text("a\"b\\").answered_len(), 6);
        assert_eq!(text("\u{1}\n").answered_len(), 8);
        assert_eq!(text("é").answered_len(), 2, "what is not ASCII is not escaped");
        assert_eq!(WsPayload::Binary(vec![0; 5]).answered_len(), 8);
        let mut buffer = WsBuffer::default();
        for _ in 0..4 {
            buffer.push(text(&"\u{1}".repeat(100)));
        }
        let (frames, _) = buffer.take(&WsPick::Next, 20, 1300);
        assert_eq!(frames.len(), 2, "600 bytes each as JSON text: two fit in 1300");
        assert_eq!(buffer.len(), 2);
    }

    #[test]
    fn a_first_frame_over_the_budget_is_cut_and_marked() {
        let fake = Arc::new(Fake::default());
        let caps = caps_with(&fake);
        let id = ask(&caps, json!({"op": "open", "url": "wss://1.1.1.1/"}))["id"].as_str().unwrap().to_string();
        fake.arrive(&id, &"é".repeat(1000));
        fake.arrive(&id, "next");
        let answer = ask(&caps, json!({"op": "receive", "id": id, "max_bytes": 101}));
        let frames = answer["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 1, "{answer}");
        assert_eq!(frames[0]["text"].as_str().unwrap(), "é".repeat(50), "cut on a character");
        assert_eq!(frames[0]["truncated"]["bytes"], 2000);
        assert_eq!(answer["buffered"], 1);
        let rest = ask(&caps, json!({"op": "receive", "id": id}));
        assert_eq!(rest["frames"][0]["text"], "next");
        assert!(rest["frames"][0].get("truncated").is_none());
    }

    #[test]
    fn a_frame_is_text_json_or_binary_and_one_of_them() {
        let fake = Arc::new(Fake::default());
        let caps = caps_with(&fake);
        let id = ask(&caps, json!({"op": "open", "url": "wss://1.1.1.1/"}))["id"].as_str().unwrap().to_string();
        assert_eq!(ask(&caps, json!({"op": "send", "id": id, "text": "hello"})), json!({"ok": true, "sent": true}));
        assert_eq!(ask(&caps, json!({"op": "send", "id": id, "binary": "AAEC"}))["sent"], true);
        let sent = fake.sent.lock().unwrap().clone();
        assert_eq!(sent, vec![(id.clone(), text("hello")), (id.clone(), WsPayload::Binary(vec![0, 1, 2]))]);
        for op in [
            json!({"op": "send", "id": id}),
            json!({"op": "send", "id": id, "text": "a", "json": 1}),
            json!({"op": "send", "id": id, "binary": "not base64!"}),
            json!({"op": "send", "id": id, "text": "x".repeat(WS_SEND_MAX + 1)}),
            json!({"op": "send", "text": "a"}),
            json!({"op": "send", "id": "ws-missing", "text": "a"}),
            json!({"op": "receive", "id": id, "reply_id": 1, "until": "x"}),
        ] {
            assert_eq!(ask(&caps, op.clone())["ok"], false, "{op}");
        }
        assert_eq!(fake.sent.lock().unwrap().len(), 2);
    }

    #[test]
    fn the_wait_and_the_sizes_are_clamped() {
        struct Asked(Mutex<Vec<WsTake>>);
        impl WebSocketBroker for Asked {
            fn open(&self, _: &WsOpen) -> Result<WsOpened, WsOpenError> {
                Err(WsOpenError::Failed("unused".into()))
            }
            fn send(&self, _: &str, _: WsPayload) -> Result<(), String> {
                Ok(())
            }
            fn receive(&self, _: &str, take: &WsTake) -> Result<WsBatch, String> {
                self.0.lock().unwrap().push(take.clone());
                Ok(WsBatch::default())
            }
            fn close(&self, _: &str, _: u16, _: &str) -> Result<WsBatch, String> {
                Ok(WsBatch::default())
            }
            fn list(&self) -> Vec<WsConnInfo> {
                Vec::new()
            }
        }
        let asked = Arc::new(Asked(Mutex::new(Vec::new())));
        let caps = Capabilities { websocket: Some(asked.clone()), ..Capabilities::none() };
        ask(&caps, json!({"op": "receive", "id": "ws-1"}));
        ask(&caps, json!({"op": "receive", "id": "ws-1", "wait_ms": 3_600_000, "max_frames": 5000, "max_bytes": 99_999_999}));
        ask(&caps, json!({"op": "receive", "id": "ws-1", "wait_ms": 0, "max_frames": 0, "max_bytes": 0}));
        let takes = asked.0.lock().unwrap();
        assert_eq!((takes[0].wait, takes[0].max_frames, takes[0].max_bytes), (WS_WAIT_DEFAULT, WS_TAKE_FRAMES_DEFAULT, WS_TAKE_BYTES_DEFAULT));
        assert_eq!((takes[1].wait, takes[1].max_frames, takes[1].max_bytes), (WS_WAIT_MAX, WS_TAKE_FRAMES_MAX, WS_TAKE_BYTES_MAX));
        assert_eq!((takes[2].wait, takes[2].max_frames, takes[2].max_bytes), (Duration::ZERO, 1, 1));
        assert_eq!(takes[0].pick, WsPick::Next);
    }

    /// The call's budget is the I/O and the wait, and the wait is the one
    /// the guard gives the broker: the deadline that follows from it is at
    /// most 75 s, far under the dispatcher's ceiling.
    #[test]
    fn the_call_budget_is_the_io_budget_and_the_clamped_wait() {
        assert_eq!(call_budget(&json!({"op": "open"})), WS_IO_BUDGET + WS_WAIT_DEFAULT);
        assert_eq!(call_budget(&json!({"op": "receive", "wait_ms": 0})), WS_IO_BUDGET);
        assert_eq!(call_budget(&json!({"op": "receive", "wait_ms": 30_000})), Duration::from_secs(40));
        assert_eq!(call_budget(&json!({"op": "receive", "wait_ms": 9_999_999})), WS_IO_BUDGET + WS_WAIT_MAX);
        assert_eq!(call_budget(&Value::Null), WS_IO_BUDGET + WS_WAIT_DEFAULT);
        assert_eq!(crate::host::deadline_for_budget(call_budget(&json!({"wait_ms": 9_999_999}))), Duration::from_secs(75));
    }

    #[test]
    fn a_close_and_a_list_reach_the_broker() {
        let fake = Arc::new(Fake::default());
        let caps = caps_with(&fake);
        let id = ask(&caps, json!({"op": "open", "url": "wss://1.1.1.1/"}))["id"].as_str().unwrap().to_string();
        let listed = ask(&caps, json!({"op": "list"}));
        assert_eq!(listed["connections"][0]["id"], id.as_str(), "{listed}");
        let closed = ask(&caps, json!({"op": "close", "id": id, "reason": "é".repeat(100)}));
        assert_eq!(closed["closed"]["code"], 1000, "{closed}");
        assert_eq!(closed["closed"]["reason"].as_str().unwrap(), "é".repeat(61), "123 bytes at most, cut on a character");
        assert!(ask(&caps, json!({"op": "list"}))["connections"].as_array().unwrap().is_empty());
        assert_eq!(ask(&caps, json!({"op": "close", "id": id}))["ok"], false);
    }
}
