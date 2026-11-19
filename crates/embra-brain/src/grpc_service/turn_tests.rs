//! Whole turns through `handle_request`, between a model server and a
//! database that are one mock: what a turn asks the model, what it writes
//! and what it tells the client. The provider is the LM Studio preset
//! pointed at the mock. The database answers 404 to what a test did not
//! mount, which is "no such document" and "no seal", so a turn here runs on
//! the placeholder prompt of an unsealed instance. The session's two
//! documents are kept as the database keeps them: a query answers what the
//! last write left, so a second turn reads what the first one saved.
//!
//! A message stays under `knowledge::enrichment`'s minimum length, so no
//! turn here asks the knowledge graph anything.

use super::*;
use embra_common::proto::brain::conversation_response::ResponseType;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// The session every turn here runs in.
const SESSION: &str = "main";
/// Where the LM Studio preset sends a model call.
const CHAT: &str = "/v1/chat/completions";

fn data(v: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"ok": true, "meta": {}, "data": v}))
}

/// One frame of the OpenAI-compatible stream.
fn chunk(delta: Value, finish: Option<&str>) -> String {
    let frame = json!({
        "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "scripted",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    format!("data: {frame}\n\n")
}

fn stream(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// The model answers `text` and ends its turn.
fn says(text: &str) -> ResponseTemplate {
    stream(format!(
        "{}{}data: [DONE]\n\n",
        chunk(json!({"content": text}), None),
        chunk(json!({}), Some("stop"))
    ))
}

/// The model says `text` and calls `calculate`, a tool that reads and
/// writes nothing, on `expression`.
fn calculates(text: &str, expression: &str) -> ResponseTemplate {
    let call = json!({"tool_calls": [{
        "index": 0, "id": "call_1", "type": "function",
        "function": {"name": "calculate", "arguments": json!({"expression": expression}).to_string()},
    }]});
    stream(format!(
        "{}{}{}data: [DONE]\n\n",
        chunk(json!({"content": text}), None),
        chunk(call, None),
        chunk(json!({}), Some("tool_calls"))
    ))
}

/// The model server's answers, in the order its calls arrive. A call past
/// the script's end is refused with a 4xx, which no retry follows.
#[derive(Clone, Default)]
struct Script(Arc<StdMutex<VecDeque<ResponseTemplate>>>);

impl Respond for Script {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.0.lock().unwrap().pop_front().unwrap_or_else(|| {
            ResponseTemplate::new(418).set_body_string("the script has no further answer")
        })
    }
}

/// One stored document: a query answers it and a PUT replaces it.
#[derive(Clone)]
struct Stored {
    id: &'static str,
    doc: Arc<StdMutex<Value>>,
}

impl Stored {
    fn new(id: &'static str, doc: Value) -> Self {
        Self { id, doc: Arc::new(StdMutex::new(doc)) }
    }

    fn now(&self) -> Value {
        self.doc.lock().unwrap().clone()
    }

    async fn mount(&self, server: &MockServer, collection: &str) {
        Mock::given(method("POST"))
            .and(path(format!("/{collection}/query")))
            .respond_with(self.clone())
            .mount(server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("/{collection}/docs/{}", self.id)))
            .respond_with(self.clone())
            .mount(server)
            .await;
    }
}

impl Respond for Stored {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut doc = self.doc.lock().unwrap();
        if request.method.as_str() != "PUT" {
            return data(json!([doc.clone()]));
        }
        // The typed documents carry no `_id`; the database keeps it.
        let mut written: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        written["_id"] = json!(self.id);
        *doc = written;
        data(json!({}))
    }
}

/// A brain with one session, a database and a model server.
struct Rig {
    server: MockServer,
    script: Script,
    history: Stored,
    meta: Stored,
    ctx: ServiceCtx,
    /// Held so that the turn's stop observer has a live sender.
    _stop_tx: watch::Sender<u64>,
    stop_rx: watch::Receiver<u64>,
}

impl Rig {
    async fn new() -> Self {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/config.system/docs/config"))
            .respond_with(data(json!({
                "name": "Embra", "api_key": "", "timezone": "UTC", "deployment_mode": "",
                "created_at": "", "version": "", "api_provider": "lm_studio",
                "openai_compat": {"lm_studio_endpoint": server.uri(), "lm_studio_model": "scripted"},
            })))
            .mount(&server)
            .await;
        let history = Stored::new(
            "h1",
            json!({"_id": "h1", "session_name": SESSION, "format_version": 2, "turns": []}),
        );
        history.mount(&server, &format!("sessions.{SESSION}.history")).await;
        let meta = Stored::new(
            "m1",
            json!({
                "_id": "m1", "id": "m1", "name": SESSION, "state": "Active",
                "created_at": "2026-11-01T00:00:00Z", "last_active": "2026-11-01T00:00:00Z",
            }),
        );
        meta.mount(&server, &format!("sessions.{SESSION}.meta")).await;
        let script = Script::default();
        Mock::given(method("POST")).and(path(CHAT)).respond_with(script.clone()).mount(&server).await;

        let mut sessions = SessionManager::new(WardsonDbClient::from_url(&server.uri()));
        sessions.active_session = Some(SESSION.to_string());
        let ctx = ServiceCtx {
            db: Arc::new(WardsonDbClient::from_url(&server.uri())),
            session_mgr: Arc::new(RwLock::new(sessions)),
            in_turn: Arc::new(AtomicBool::new(false)),
            pending_provider: Arc::new(Mutex::new(None)),
            pending_key_setup: Arc::new(Mutex::new(None)),
            pending_openai_compat_setup: Arc::new(Mutex::new(None)),
            pending_session_delete: Arc::new(Mutex::new(None)),
            pending_backup_restore: Arc::new(Mutex::new(None)),
        };
        let (stop_tx, stop_rx) = watch::channel(0u64);
        Self { server, script, history, meta, ctx, _stop_tx: stop_tx, stop_rx }
    }

    /// What the model answers to its next call that has no answer yet.
    fn then(&self, answer: ResponseTemplate) -> &Self {
        self.script.0.lock().unwrap().push_back(answer);
        self
    }

    /// One message of the operator's, run to the end of its turn: the
    /// frames the client was sent, in order.
    async fn say(&self, text: &str) -> Vec<ResponseType> {
        let (tx, mut rx) = mpsc::channel::<Result<ConversationResponse, Status>>(64);
        // Read while the turn runs: a turn that sends more frames than the
        // channel holds would wait for a reader.
        let reader = tokio::spawn(async move {
            let mut frames = Vec::new();
            while let Some(frame) = rx.recv().await {
                frames.push(frame.expect("a frame").response_type.expect("a typed frame"));
            }
            frames
        });
        let request = ConversationRequest {
            request_type: Some(conversation_request::RequestType::UserMessage(UserMessage {
                content: text.to_string(),
                ..Default::default()
            })),
        };
        handle_request(request, &tx, &self.ctx, "UTC", "", &self.stop_rx)
            .await
            .expect("the turn returns without an error of its own");
        drop(tx);
        reader.await.expect("the reader ends with the turn")
    }

    /// The model calls the server received, as their JSON bodies.
    async fn asked(&self) -> Vec<Value> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == CHAT)
            .map(|r| serde_json::from_slice(&r.body).expect("a JSON request"))
            .collect()
    }

    /// The session's history as the database holds it: role and text.
    fn saved(&self) -> Vec<(String, String)> {
        self.history.now()["turns"]
            .as_array()
            .expect("turns")
            .iter()
            .map(|t| (t["role"].as_str().unwrap().to_string(), t["content"].as_str().unwrap().to_string()))
            .collect()
    }
}

/// The text of every `Done` frame, in order.
fn dones(frames: &[ResponseType]) -> Vec<&str> {
    frames
        .iter()
        .filter_map(|f| match f {
            ResponseType::Done(d) => Some(d.full_response.as_str()),
            _ => None,
        })
        .collect()
}

/// Every system frame that is an error or a warning.
fn complaints(frames: &[ResponseType]) -> Vec<&str> {
    frames
        .iter()
        .filter_map(|f| match f {
            ResponseType::System(m)
                if m.msg_type == SystemMessageType::Error as i32 || m.msg_type == SystemMessageType::Warning as i32 =>
            {
                Some(m.content.as_str())
            }
            _ => None,
        })
        .collect()
}

/// What a request's last message of `role` says.
fn last_of(request: &Value, role: &str) -> String {
    request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|m| m["role"] == role)
        .map(|m| m["content"].as_str().unwrap_or_default().to_string())
        .unwrap_or_default()
}

#[tokio::test]
async fn an_ordinary_turn_asks_the_model_once_and_saves_the_message_with_the_reply() {
    let rig = Rig::new().await;
    rig.then(says("Four."));

    let frames = rig.say("two plus two?").await;

    assert_eq!(dones(&frames), ["Four."]);
    assert_eq!(complaints(&frames), [] as [&str; 0]);
    let asked = rig.asked().await;
    assert_eq!(asked.len(), 1, "one model call");
    assert_eq!(last_of(&asked[0], "user"), "two plus two?");
    assert_eq!(
        rig.saved(),
        [("user".to_string(), "two plus two?".to_string()), ("assistant".to_string(), "Four.".to_string())]
    );
    let meta = rig.meta.now();
    assert_eq!(meta["turn_count"], json!(2));
    assert!(meta["events_seen_at"].is_string(), "the turn's start is the events watermark: {meta}");
}

#[tokio::test]
async fn a_turn_with_a_tool_call_asks_again_with_the_result_and_saves_every_segment() {
    let rig = Rig::new().await;
    rig.then(calculates("Let me work it out.", "2 ** 10")).then(says("It is 1024."));

    let frames = rig.say("two to the ten?").await;

    assert_eq!(dones(&frames), ["Let me work it out.", "It is 1024."]);
    assert_eq!(complaints(&frames), [] as [&str; 0]);
    let ran: Vec<(&str, &str)> = frames
        .iter()
        .filter_map(|f| match f {
            ResponseType::Tool(t) => Some((t.tool_name.as_str(), t.result.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(ran, [("calculate", "2 ** 10 = 1024")]);
    let asked = rig.asked().await;
    assert_eq!(asked.len(), 2, "the call, and the call that carries the result");
    assert_eq!(last_of(&asked[1], "tool"), "2 ** 10 = 1024");
    assert_eq!(
        rig.saved(),
        [
            ("user".to_string(), "two to the ten?".to_string()),
            ("assistant".to_string(), "Let me work it out.\n\nIt is 1024.".to_string()),
        ]
    );
}

#[tokio::test]
async fn a_second_turn_reads_what_the_first_one_saved() {
    let rig = Rig::new().await;
    rig.then(says("Four.")).then(says("Six."));

    rig.say("two plus two?").await;
    rig.say("and plus two?").await;

    let asked = rig.asked().await;
    let second: Vec<(&str, &str)> = asked[1]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["role"] != "system")
        .map(|m| (m["role"].as_str().unwrap(), m["content"].as_str().unwrap()))
        .collect();
    assert_eq!(second, [("user", "two plus two?"), ("assistant", "Four."), ("user", "and plus two?")]);
    assert_eq!(rig.saved().len(), 4);
}
