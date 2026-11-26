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

/// The server refuses the call before any stream. A 4xx: a 5xx is tried
/// again after a second.
fn refuses(status: u16, body: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_string(body)
}

/// The model says `text`, then the server answers with an error inside the
/// stream and closes, as LM Studio does for a prompt over its context.
fn breaks_after(text: &str, error: &str) -> ResponseTemplate {
    stream(format!(
        "{}data: {}\n\n",
        chunk(json!({"content": text}), None),
        json!({"error": {"message": error}})
    ))
}

/// A model server that takes the call and has not answered an hour later.
/// It says when the call arrived.
#[derive(Clone, Default)]
struct Held(Arc<tokio::sync::Notify>);

impl Respond for Held {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.0.notify_one();
        ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(3600))
    }
}

/// The model server's answers, in the order its calls arrive. A call past
/// the script's end is refused with a 4xx, which no retry follows. An
/// answer that is held is one the server has not begun an hour later; the
/// script says when such a call arrived.
#[derive(Clone, Default)]
struct Script {
    answers: Arc<StdMutex<VecDeque<Option<ResponseTemplate>>>>,
    held: Arc<tokio::sync::Notify>,
}

impl Respond for Script {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        match self.answers.lock().unwrap().pop_front() {
            Some(Some(answer)) => answer,
            Some(None) => {
                self.held.notify_one();
                ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(3600))
            }
            None => ResponseTemplate::new(418).set_body_string("the script has no further answer"),
        }
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

/// A collection kept as the database keeps one: a write adds a document,
/// and a query answers the documents whose fields equal its filter's, in
/// the order of `started_at`. It counts its writes, for a test to wait on
/// one that the turn does not wait for.
#[derive(Clone)]
struct Kept {
    docs: Arc<StdMutex<Vec<Value>>>,
    written: Arc<watch::Sender<usize>>,
}

impl Kept {
    fn new() -> Self {
        Self { docs: Arc::default(), written: Arc::new(watch::channel(0).0) }
    }

    async fn mount(&self, server: &MockServer, collection: &str) {
        for route in ["docs", "query"] {
            Mock::given(method("POST"))
                .and(path(format!("/{collection}/{route}")))
                .respond_with(self.clone())
                .mount(server)
                .await;
        }
    }

    /// Returns once `n` documents have been written. The deadline is one no
    /// loaded machine reaches when the write comes.
    async fn holds(&self, n: usize) {
        let mut written = self.written.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(120), written.wait_for(|count| *count >= n))
            .await
            .expect("the document is written")
            .expect("the collection is there");
    }
}

impl Respond for Kept {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let mut docs = self.docs.lock().unwrap();
        if request.url.path().ends_with("/docs") {
            docs.push(body);
            // On record before the answer: a test that waits for the count
            // reads a collection that already holds the document.
            self.written.send_replace(docs.len());
            return data(json!({"_id": format!("d{}", docs.len())}));
        }
        let wanted = body["filter"].as_object().cloned().unwrap_or_default();
        let mut found: Vec<Value> =
            docs.iter().filter(|doc| wanted.iter().all(|(k, v)| doc.get(k) == Some(v))).cloned().collect();
        found.sort_by_key(|doc| doc["started_at"].as_str().unwrap_or_default().to_string());
        data(Value::Array(found))
    }
}

/// The stored configuration, read by its id; a test changes it in place.
#[derive(Clone)]
struct Config(Arc<StdMutex<Value>>);

impl Respond for Config {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        data(self.0.lock().unwrap().clone())
    }
}

/// A brain with one session, a database and a model server.
struct Rig {
    server: MockServer,
    script: Script,
    history: Stored,
    meta: Stored,
    /// `tools.turn_trace`: one document per finished tool call.
    traces: Kept,
    config: Config,
    ctx: ServiceCtx,
    /// The operator's stop: a turn in flight sees the counter move.
    stop_tx: watch::Sender<u64>,
    stop_rx: watch::Receiver<u64>,
}

impl Rig {
    async fn new() -> Self {
        let server = MockServer::start().await;
        let config = Config(Arc::new(StdMutex::new(json!({
            "name": "Embra", "api_key": "", "timezone": "UTC", "deployment_mode": "",
            "created_at": "", "version": "", "api_provider": "lm_studio",
            "openai_compat": {"lm_studio_endpoint": server.uri(), "lm_studio_model": "scripted"},
        }))));
        Mock::given(method("GET"))
            .and(path("/config.system/docs/config"))
            .respond_with(config.clone())
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
        let traces = Kept::new();
        traces.mount(&server, "tools.turn_trace").await;
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
            license: license::Gate::without_a_text(),
        };
        let (stop_tx, stop_rx) = watch::channel(0u64);
        Self { server, script, history, meta, traces, config, ctx, stop_tx, stop_rx }
    }

    /// The same brain on Anthropic or on Gemini. Both are reached through
    /// the provider gateway's base URL, which is this rig's server: their
    /// requests arrive under `/anthropic/` and `/gemini/` (`gateway::base_for`)
    /// and are answered from the same script.
    async fn on(provider: &str) -> Self {
        let rig = Self::new().await;
        {
            let mut config = rig.config.0.lock().unwrap();
            config["api_provider"] = json!(provider);
            config["api_key"] = json!("sk-scripted");
            config["gemini_api_key"] = json!("g-scripted");
            config["provider_gateway"] = json!({"url": rig.server.uri(), "enabled": true});
        }
        Mock::given(method("POST"))
            // A model call alone: Gemini's cache manager posts elsewhere
            // under the same prefix, and is not the script's to answer.
            .and(wiremock::matchers::path_regex(r"^/anthropic/v1/messages$|^/gemini/.*:streamGenerateContent$"))
            .respond_with(rig.script.clone())
            .mount(&rig.server)
            .await;
        rig
    }

    /// The model calls that arrived under `prefix` (`/anthropic/`, `/gemini/`).
    async fn asked_at(&self, prefix: &str) -> Vec<Value> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| {
                let path = r.url.path();
                r.method.as_str() == "POST"
                    && path.starts_with(prefix)
                    && (path.ends_with("/v1/messages") || path.ends_with(":streamGenerateContent"))
            })
            .map(|r| serde_json::from_slice(&r.body).expect("a JSON request"))
            .collect()
    }

    /// What the model answers to its next call that has no answer yet.
    fn then(&self, answer: ResponseTemplate) -> &Self {
        self.script.answers.lock().unwrap().push_back(Some(answer));
        self
    }

    /// The model server takes its next call and does not answer it.
    fn then_holds(&self) -> &Self {
        self.script.answers.lock().unwrap().push_back(None);
        self
    }

    /// Run `text` as a turn whose last scripted call is held; `during` is
    /// run once that call has arrived, and the operator's stop then ends
    /// the turn. The deadline is one no loaded machine reaches when the
    /// stop is heard.
    async fn say_and_stop_at_the_held_call(&self, text: &str, during: impl AsyncFnOnce()) -> Vec<ResponseType> {
        let stop = async {
            self.script.held.notified().await;
            during().await;
            self.stop_tx.send_modify(|v| *v += 1);
        };
        let (frames, ()) =
            tokio::time::timeout(std::time::Duration::from_secs(120), async { tokio::join!(self.say(text), stop) })
                .await
                .expect("the stop ends the held call");
        frames
    }

    /// The writes of one document the database saw, as their bodies.
    async fn writes_of(&self, document: &str) -> Vec<Value> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "PUT" && r.url.path() == document)
            .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
            .collect()
    }

    /// One message of the operator's, run to the end of its turn: the
    /// frames the client was sent, in order.
    async fn say(&self, text: &str) -> Vec<ResponseType> {
        self.send(conversation_request::RequestType::UserMessage(UserMessage {
            content: text.to_string(),
            ..Default::default()
        }))
        .await
    }

    /// A slash command, run to its end, with the turn it starts if it
    /// starts one.
    async fn command(&self, command: &str, args: &str) -> Vec<ResponseType> {
        self.send(conversation_request::RequestType::SlashCommand(SlashCommand {
            command: command.to_string(),
            args: args.to_string(),
        }))
        .await
    }

    async fn send(&self, request: conversation_request::RequestType) -> Vec<ResponseType> {
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
        let request = ConversationRequest { request_type: Some(request) };
        handle_request(request, &tx, &self.ctx, "UTC", "", &self.stop_rx)
            .await
            .expect("the turn returns without an error of its own");
        drop(tx);
        reader.await.expect("the reader ends with the turn")
    }

    /// A request as a console sends it in the operational loop, the
    /// licence's gate first (`console_request`): the frames it was sent.
    async fn console(&self, request: ConversationRequest) -> Vec<ResponseType> {
        let (tx, mut rx) = mpsc::channel::<Result<ConversationResponse, Status>>(64);
        let reader = tokio::spawn(async move {
            let mut frames = Vec::new();
            while let Some(frame) = rx.recv().await {
                frames.push(frame.expect("a frame").response_type.expect("a typed frame"));
            }
            frames
        });
        console_request(request, &tx, &self.ctx, "UTC", "", &self.stop_rx).await;
        drop(tx);
        reader.await.expect("the reader ends with the request")
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

/// What stands alone in the assistant's slot for a turn cut before any
/// text, and what follows the text of one cut later. Written out: these
/// are in stored histories.
const CUT_ALONE: &str = "(turn interrupted by a failed model call)";
const CUT_AFTER_TEXT: &str = "(response interrupted by a failed model call)";

fn row(role: &str, text: &str) -> (String, String) {
    (role.to_string(), text.to_string())
}

/// The first call is refused before any stream. The turn ended there with
/// nothing on record: the operator's message was written only with a reply,
/// and on this provider the failure left the handler as an error, with no
/// `Done` to end the client's wait.
#[tokio::test]
async fn a_first_call_refused_before_its_stream_saves_the_message_and_the_marker() {
    let rig = Rig::new().await;
    rig.then(refuses(400, "no such model"));

    let frames = rig.say("two plus two?").await;

    assert_eq!(dones(&frames), [""], "one Done ends the client's wait");
    let said = complaints(&frames);
    assert!(said.iter().any(|m| m.contains("HTTP 400") && m.contains("no such model")), "{said:?}");
    assert!(said.iter().any(|m| m.contains("saved")), "the operator is told what is on record: {said:?}");
    assert_eq!(rig.saved(), [row("user", "two plus two?"), row("assistant", CUT_ALONE)]);
    let meta = rig.meta.now();
    assert_eq!(meta["turn_count"], json!(2));
    assert!(
        meta["events_seen_at"].is_null(),
        "no call completed, so nothing that happened was shown to the model: {meta}"
    );
}

/// The first call breaks inside its stream. The text the operator watched
/// was dropped; it closes with its `Done` and is saved with the marker.
#[tokio::test]
async fn a_first_call_that_breaks_in_its_stream_closes_with_its_partial() {
    let rig = Rig::new().await;
    rig.then(breaks_after("Two plus two is", "the slot was lost"));

    let frames = rig.say("two plus two?").await;

    assert_eq!(dones(&frames), ["Two plus two is"]);
    let said = complaints(&frames);
    assert!(said.iter().any(|m| m.contains("the slot was lost")), "{said:?}");
    assert_eq!(
        rig.saved(),
        [row("user", "two plus two?"), row("assistant", &format!("Two plus two is\n\n{CUT_AFTER_TEXT}"))]
    );
}

/// A continuation is refused before its stream. On Gemini and the local
/// presets this left the handler as an error from any step of the loop,
/// and the whole turn went with it.
#[tokio::test]
async fn a_continuation_cut_before_its_stream_keeps_every_segment() {
    let rig = Rig::new().await;
    rig.then(calculates("Let me work it out.", "2 ** 10")).then(refuses(400, "context length exceeded"));

    let frames = rig.say("two to the ten?").await;

    assert_eq!(dones(&frames), ["Let me work it out.", ""]);
    assert_eq!(rig.asked().await.len(), 2);
    assert_eq!(
        rig.saved(),
        [
            row("user", "two to the ten?"),
            row("assistant", &format!("Let me work it out.\n\n{CUT_AFTER_TEXT}")),
        ]
    );
    assert!(
        rig.meta.now()["events_seen_at"].is_string(),
        "the first call completed: what it was shown is not shown again"
    );
}

#[tokio::test]
async fn a_continuation_cut_inside_its_stream_keeps_every_segment_and_its_partial() {
    let rig = Rig::new().await;
    rig.then(calculates("Let me work it out.", "2 ** 10")).then(breaks_after("It is 10", "the slot was lost"));

    let frames = rig.say("two to the ten?").await;

    assert_eq!(dones(&frames), ["Let me work it out.", "It is 10"]);
    assert_eq!(
        rig.saved(),
        [
            row("user", "two to the ten?"),
            row("assistant", &format!("Let me work it out.\n\nIt is 10\n\n{CUT_AFTER_TEXT}")),
        ]
    );
}

/// The turn after one that was cut before any text: the marker is never
/// the model's own reply in what it is sent.
#[tokio::test]
async fn the_turn_after_a_cut_one_reads_the_cut_as_a_note_in_the_operators_message() {
    let rig = Rig::new().await;
    rig.then(refuses(400, "no such model")).then(says("Four."));

    rig.say("two plus two?").await;
    rig.say("and now?").await;

    let asked = rig.asked().await;
    let sent: Vec<(&str, &str)> = asked[1]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["role"] != "system")
        .map(|m| (m["role"].as_str().unwrap(), m["content"].as_str().unwrap()))
        .collect();
    assert_eq!(sent.len(), 1, "one operator message, the cut turn folded into it: {sent:?}");
    assert_eq!(sent[0].0, "user");
    assert!(sent[0].1.starts_with("two plus two?\n\n[Your reply to the message above was cut by a failed model call"), "{}", sent[0].1);
    assert!(sent[0].1.ends_with("and now?"), "{}", sent[0].1);
    assert!(!sent[0].1.contains(CUT_ALONE));
    assert_eq!(rig.saved().len(), 4);
}

/// A turn is cut after a tool call had finished. The history keeps its
/// text; the call and its result were in memory. The next turn is told, at
/// the head of its message: that the turn was cut, and the call on record.
/// The turn after that is told nothing: the block is said once.
#[tokio::test]
async fn the_turn_after_a_cut_one_carries_the_interrupted_block_once() {
    let rig = Rig::new().await;
    rig.then(calculates("Let me work it out.", "2 ** 10"))
        .then(refuses(400, "context length exceeded"))
        .then(says("It was 1024."))
        .then(says("Yes."));

    rig.say("two to the ten?").await;
    // The call's record is written beside the turn, not within it.
    rig.traces.holds(1).await;
    rig.say("where were we?").await;
    rig.say("and then?").await;

    let asked = rig.asked().await;
    assert_eq!(asked.len(), 4);
    let told = last_of(&asked[2], "user");
    let block = "<interrupted_turn>\n\
        Your last turn in this session was cut before it finished: a model call failed. The message it was answering is above, with what you had written of the reply. The tool results of that turn are no longer in your context.\n\
        Tool calls of that turn, in order:\n\
        - calculate {\"expression\":\"2 ** 10\"}: ok\n\
        turn_trace with turn_index_back=1 shows a preview of each result; run a tool again when you need the whole of one.\n\
        Take the work up from there if the operator's message below asks for it.\n\
        </interrupted_turn>";
    assert_eq!(told, format!("{block}\nwhere were we?"));
    // What it follows in the history: the operator's message, and the
    // reply as far as it got.
    let before: Vec<(&str, &str)> = asked[2]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["role"] != "system")
        .map(|m| (m["role"].as_str().unwrap(), m["content"].as_str().unwrap()))
        .take(2)
        .collect();
    assert_eq!(
        before,
        [("user", "two to the ten?"), ("assistant", format!("Let me work it out.\n\n{CUT_AFTER_TEXT}").as_str())]
    );

    assert_eq!(last_of(&asked[3], "user"), "and then?", "said once");
    // Nothing of the block is kept: the history holds the operator's words.
    assert_eq!(rig.saved()[2], row("user", "where were we?"));
}

/// A turn cut at its first call ran no tool: the next turn is told the cut
/// and that nothing is on record, inside the one message the cut folded
/// into.
#[tokio::test]
async fn a_turn_cut_before_any_tool_call_is_told_without_a_list() {
    let rig = Rig::new().await;
    rig.then(refuses(400, "no such model")).then(says("Four."));

    rig.say("two plus two?").await;
    rig.say("and now?").await;

    let told = last_of(&rig.asked().await[1], "user");
    assert!(told.contains("<interrupted_turn>") && told.contains("No tool call of that turn is on record."), "{told}");
    assert!(told.ends_with("</interrupted_turn>\nand now?"), "{told}");
}

/// The operator's stop while the model server has the request and has not
/// begun to answer. The request was awaited outside every stop checkpoint
/// on Gemini and the local presets: Esc did nothing until the first byte.
/// The deadline is one no loaded machine reaches when the stop is heard.
#[tokio::test]
async fn a_stop_ends_the_wait_for_an_answer_to_begin() {
    let rig = Rig::new().await;
    let held = Held::default();
    Mock::given(method("POST"))
        .and(path(CHAT))
        .respond_with(held.clone())
        .with_priority(1)
        .mount(&rig.server)
        .await;

    let stop = async {
        held.0.notified().await;
        rig.stop_tx.send_modify(|v| *v += 1);
    };
    let (frames, ()) = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        tokio::join!(rig.say("wait for it"), stop)
    })
    .await
    .expect("the stop ends the wait for the answer");

    assert_eq!(dones(&frames), [""]);
    let said = complaints(&frames);
    assert!(said.iter().any(|m| m.contains("stopped by operator")), "{said:?}");
    assert_eq!(
        rig.saved(),
        [row("user", "wait for it"), row("assistant", crate::sessions::model_view::OPERATOR_STOP_MARKER)]
    );
}

/// A resume briefing is a turn of the brain's own making. Cut, it saves
/// nothing: the attach's cooldown counts on a failed briefing leaving the
/// session as it was.
#[tokio::test]
async fn a_briefing_that_is_cut_saves_nothing() {
    let rig = Rig::new().await;
    rig.then(refuses(400, "no such model"));
    rig.ctx.session_mgr.write().await.pending_resume_briefing =
        Some(crate::sessions::ResumeBriefing { away_since: None });

    let frames = rig.say("[Session resumed]").await;

    assert_eq!(dones(&frames), [""]);
    assert_eq!(rig.saved(), [] as [(String, String); 0]);
    assert!(rig.meta.now()["turn_count"].is_null(), "the meta was not written");
    let said = complaints(&frames);
    assert!(!said.iter().any(|m| m.contains("/continue")), "nothing is kept of a briefing: {said:?}");
    let after = rig.command("/continue", "").await;
    assert!(self::said(&after).iter().any(|m| m.starts_with("Nothing to continue.")));
}

/// The memorize turn of a guided delete, cut after the model wrote the
/// marker that executes the deletion. A reply that was cut decides nothing.
#[tokio::test]
async fn a_cut_reply_that_carries_the_delete_marker_deletes_nothing() {
    let rig = Rig::new().await;
    rig.then(breaks_after("Kept what matters. [SESSION_DELETE_READY]", "the slot was lost"));
    *rig.ctx.pending_session_delete.lock().await = Some(PendingSessionDelete {
        name: SESSION.to_string(),
        phase: DeleteFlowPhase::AwaitingReason,
    });

    let frames = rig.say("it is finished").await;

    let meta = rig.meta.now();
    assert!(meta["deleted_at"].is_null(), "the session stands: {meta}");
    let said = complaints(&frames);
    assert!(said.iter().any(|m| m.contains("not executed")), "{said:?}");
}

/// A provider whose request never comes back: it says when it was asked
/// and when its request was dropped.
struct Waits {
    asked: Arc<tokio::sync::Notify>,
    dropped: Arc<AtomicBool>,
}

struct SaysWhenDropped(Arc<AtomicBool>);

impl Drop for SaysWhenDropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A provider that answers what it is scripted with.
struct Answers(std::result::Result<Vec<StreamEvent>, &'static str>);

macro_rules! scripted_provider {
    ($provider:ty, |$this:ident| $stream_turn:expr) => {
        #[tonic::async_trait]
        impl LlmProvider for $provider {
            fn display_name(&self) -> &str {
                "scripted"
            }

            fn kind(&self) -> ProviderKind {
                ProviderKind::LmStudio
            }

            fn base_url(&self) -> &str {
                "http://scripted.invalid"
            }

            async fn validate_key(&self, _key: &str) -> crate::provider::ValidationResult {
                crate::provider::ValidationResult::Valid
            }

            async fn stream_turn(
                &self,
                _messages: &[ApiMessage],
                _system: &SystemPromptBundle,
                _tools: &ToolManifest,
                _options: LlmRequestOptions,
            ) -> std::result::Result<BoxStream<'static, StreamEvent>, crate::provider::ProviderError> {
                let $this = self;
                $stream_turn
            }

            fn build_tool_manifest(
                &self,
                _descriptors: &[&'static tools::registry::ToolDescriptor],
            ) -> ToolManifest {
                ToolManifest { wire_json: json!([]) }
            }
        }
    };
}

scripted_provider!(Waits, |this| {
    let _request = SaysWhenDropped(this.dropped.clone());
    this.asked.notify_one();
    std::future::pending().await
});

scripted_provider!(Answers, |this| {
    use futures::StreamExt;
    match &this.0 {
        Ok(events) => Ok(futures::stream::iter(events.clone()).boxed()),
        Err(refusal) => Err(crate::provider::ProviderError::Http { status: 400, body: refusal.to_string() }),
    }
});

/// One model call on `provider`, with a stop observer: the turn it comes
/// back as, and the frames it sent.
async fn one_call(
    provider: &dyn LlmProvider,
    stop_rx: watch::Receiver<u64>,
) -> (crate::provider::AssistantTurn, Vec<ResponseType>) {
    let system = SystemPromptBundle { text: "s".into(), session_name: SESSION.into() };
    let tools = ToolManifest { wire_json: json!([]) };
    let (tx, mut rx) = mpsc::channel(64);
    let call = ModelCall {
        provider,
        system: &system,
        tools: &tools,
        options: LlmRequestOptions::default(),
        tx: &tx,
        config_name: "Embra",
    };
    let mut stop = StopCheck::new(stop_rx);
    let turn = call.run(&[ApiMessage::user_text("hi")], &mut stop).await;
    drop(tx);
    let mut frames = Vec::new();
    while let Some(Ok(frame)) = rx.recv().await {
        frames.push(frame.response_type.expect("a typed frame"));
    }
    (turn, frames)
}

/// The stop while the request waits: the call ends as a stop with nothing
/// written, and the request is dropped, which is what closes its
/// connection and stops a local model server.
#[tokio::test(start_paused = true)]
async fn a_stop_during_the_wait_for_an_answer_ends_the_call_and_drops_the_request() {
    let provider = Waits { asked: Arc::default(), dropped: Arc::default() };
    let (stop_tx, stop_rx) = watch::channel(0u64);
    let asked = provider.asked.clone();
    let stop = async {
        asked.notified().await;
        stop_tx.send_modify(|v| *v += 1);
    };
    let ((turn, frames), ()) = tokio::time::timeout(std::time::Duration::from_secs(3600), async {
        tokio::join!(one_call(&provider, stop_rx), stop)
    })
    .await
    .expect("the stop ends the call");

    assert_eq!(turn.outcome, TurnOutcome::EarlyStop(EarlyStopReason::OperatorStop));
    assert!(turn.content.is_empty());
    assert!(provider.dropped.load(Ordering::SeqCst), "the request was dropped");
    assert_eq!(complaints(&frames), [] as [&str; 0], "the loop says the stop, once, at the turn's end");
}

/// A request that fails before its stream comes back as a turn, with the
/// provider's words shown to the operator; it was an error that left the
/// turn's function.
#[tokio::test]
async fn a_request_that_fails_before_its_stream_is_a_cut_call_with_no_text() {
    let provider = Answers(Err("no such model"));
    let (_stop_tx, stop_rx) = watch::channel(0u64);

    let (turn, frames) = one_call(&provider, stop_rx).await;

    assert!(is_cut(&turn));
    assert!(turn.content.is_empty());
    assert_eq!(complaints(&frames), ["Brain error: HTTP 400: no such model"]);
    assert_eq!(dones(&frames), [] as [&str; 0], "the turn's closing frames send the Done");
}

/// A stream that ends without its result: the text that had arrived comes
/// back with the turn, and no `Done` has closed it yet.
#[tokio::test]
async fn a_stream_that_ends_without_a_result_keeps_what_had_arrived() {
    let provider = Answers(Ok(vec![
        StreamEvent::TextDelta("Two plus ".into()),
        StreamEvent::TextDelta("two is".into()),
        StreamEvent::Error("the slot was lost".into()),
    ]));
    let (_stop_tx, stop_rx) = watch::channel(0u64);

    let (turn, frames) = one_call(&provider, stop_rx).await;

    assert!(is_cut(&turn));
    assert_eq!(turn_text(&turn), "Two plus two is");
    assert_eq!(undelivered_after(&turn).as_deref(), Some("Two plus two is"));
    assert_eq!(complaints(&frames), ["Brain error: the slot was lost"]);
    assert_eq!(dones(&frames), [] as [&str; 0]);
}

/// What the brain says outside a turn: a command's answer.
fn said(frames: &[ResponseType]) -> Vec<&str> {
    frames
        .iter()
        .filter_map(|f| match f {
            ResponseType::System(m) => Some(m.content.as_str()),
            _ => None,
        })
        .collect()
}

/// The messages of a request that are not the system prompt, as JSON.
fn conversation(request: &Value) -> Vec<Value> {
    request["messages"].as_array().expect("messages").iter().filter(|m| m["role"] != "system").cloned().collect()
}

/// A turn is cut after a tool call. `/continue` sends the call that failed
/// once more as it was: the operator's message, the model's own step with
/// its tool call, and the tool's result, none of which a history keeps. The
/// reply is saved after the cut turn, under the brain's own row.
#[tokio::test]
async fn continue_sends_the_cut_calls_messages_again() {
    let rig = Rig::new().await;
    rig.then(calculates("Let me work it out.", "2 ** 10"))
        .then(refuses(400, "context length exceeded"))
        .then(says("It is 1024."));

    let cut = rig.say("two to the ten?").await;
    assert!(
        complaints(&cut).iter().any(|m| m.contains("/continue")),
        "the cut's notice names the way on: {:?}",
        complaints(&cut)
    );
    let frames = rig.command("/continue", "").await;

    assert_eq!(dones(&frames), ["It is 1024."]);
    assert_eq!(complaints(&frames), [] as [&str; 0]);
    let asked = rig.asked().await;
    assert_eq!(asked.len(), 3);
    assert_eq!(asked[2]["messages"], asked[1]["messages"], "the same call, to the byte of its messages");
    let sent = conversation(&asked[2]);
    assert_eq!(sent.last().expect("messages")["role"], "tool", "it ends on the tool's result: {sent:?}");
    assert_eq!(
        rig.saved(),
        [
            row("user", "two to the ten?"),
            row("assistant", &format!("Let me work it out.\n\n{CUT_AFTER_TEXT}")),
            row("user", "[continue the turn that was cut]"),
            row("assistant", "It is 1024."),
        ]
    );
}

/// A turn that is cut again is kept again, as it then stands.
#[tokio::test]
async fn a_continued_turn_that_is_cut_can_be_continued_again() {
    let rig = Rig::new().await;
    rig.then(refuses(400, "busy")).then(refuses(400, "busy")).then(says("Four."));

    rig.say("two plus two?").await;
    let again = rig.command("/continue", "").await;
    assert!(complaints(&again).iter().any(|m| m.contains("/continue")), "{:?}", complaints(&again));
    let frames = rig.command("/continue", "").await;

    assert_eq!(dones(&frames), ["Four."]);
    let asked = rig.asked().await;
    assert_eq!(asked[1]["messages"], asked[0]["messages"]);
    assert_eq!(asked[2]["messages"], asked[0]["messages"]);
}

/// A message of the operator's after a cut goes on from where the session
/// is; the cut turn is no longer one to continue, and the model was told
/// of it by that message's turn.
#[tokio::test]
async fn continue_is_refused_once_the_operator_has_gone_on() {
    let rig = Rig::new().await;
    rig.then(refuses(400, "no such model")).then(says("Never mind then."));

    rig.say("two plus two?").await;
    rig.say("never mind").await;
    let frames = rig.command("/continue", "").await;

    assert_eq!(rig.asked().await.len(), 2, "no call for a turn that is not kept");
    assert!(said(&frames).iter().any(|m| m.starts_with("Nothing to continue.")), "{:?}", said(&frames));
    assert_eq!(rig.saved().len(), 4);
}

#[tokio::test]
async fn continue_with_no_turn_cut_says_so_and_calls_no_model() {
    let rig = Rig::new().await;
    let frames = rig.command("/continue", "").await;
    assert!(said(&frames).iter().any(|m| m.starts_with("Nothing to continue.")), "{:?}", said(&frames));
    assert_eq!(rig.asked().await.len(), 0);
    assert_eq!(rig.saved().len(), 0);
}

/// The call is the same call only on the model it was made for. After a
/// switch `/continue` is refused with both names, and the turn stays kept:
/// back on its model it runs.
#[tokio::test]
async fn continue_is_refused_on_another_model_and_runs_back_on_its_own() {
    let rig = Rig::new().await;
    rig.then(refuses(400, "busy")).then(says("Four."));

    rig.say("two plus two?").await;
    rig.config.0.lock().unwrap()["openai_compat"]["lm_studio_model"] = json!("another");
    let refused = rig.command("/continue", "").await;
    assert!(
        said(&refused).iter().any(|m| m.contains("ran on LM Studio scripted") && m.contains("LM Studio another")),
        "{:?}",
        said(&refused)
    );
    assert_eq!(rig.asked().await.len(), 1);

    rig.config.0.lock().unwrap()["openai_compat"]["lm_studio_model"] = json!("scripted");
    let frames = rig.command("/continue", "").await;
    assert_eq!(dones(&frames), ["Four."]);
}

/// An armed key step takes the operator's next message as an API key. The
/// row a continued turn is saved under is the brain's own and is never
/// read as one: the turn runs, and the step stays armed.
#[tokio::test]
async fn the_row_of_a_continued_turn_is_never_read_as_a_key() {
    let rig = Rig::new().await;
    rig.then(refuses(400, "busy")).then(says("Four."));

    rig.say("two plus two?").await;
    *rig.ctx.pending_key_setup.lock().await = Some(KeySetupStep::Key(ProviderKind::Anthropic));
    let frames = rig.command("/continue", "").await;

    assert_eq!(dones(&frames), ["Four."]);
    assert_eq!(*rig.ctx.pending_key_setup.lock().await, Some(KeySetupStep::Key(ProviderKind::Anthropic)));
}

const META: &str = "/sessions.main.meta/docs/m1";
const HISTORY: &str = "/sessions.main.history/docs/h1";

/// A turn's message and reply are written when the turn ends, and a process
/// that ends first left nothing. The message is on the session's meta
/// document before the first model call, and off it once the turn is saved.
#[tokio::test]
async fn a_turn_is_on_record_before_its_first_call_and_off_it_once_saved() {
    let rig = Rig::new().await;
    rig.then_holds();

    rig.say_and_stop_at_the_held_call("two plus two?", async || {
        let open = rig.meta.now()["open_turns"].clone();
        let records = open.as_array().expect("a record while the turn runs");
        assert_eq!(records.len(), 1, "{open}");
        assert_eq!(records[0]["message"]["content"], json!("two plus two?"));
        assert_eq!(records[0]["message"]["role"], json!("user"));
        assert_eq!(records[0]["base_len"], json!(0));
        assert_eq!(records[0]["calls"], json!(0), "no call has completed");
        assert_eq!(rig.saved().len(), 0, "the history holds nothing of the turn yet");
    })
    .await;

    let meta = rig.meta.now();
    assert!(meta.get("open_turns").is_none(), "saved, and no longer open: {meta}");
    assert_eq!(rig.saved().len(), 2);
}

/// As the turn goes on, its record takes the reply so far: what a restart
/// in the middle of the tool loop leaves behind.
#[tokio::test]
async fn the_record_of_a_turn_that_goes_on_takes_the_reply_so_far() {
    let rig = Rig::new().await;
    rig.then(calculates("Let me work it out.", "2 ** 10")).then_holds();

    rig.say_and_stop_at_the_held_call("two to the ten?", async || {
        let open = rig.meta.now()["open_turns"].clone();
        assert_eq!(open[0]["text"], json!("Let me work it out."), "{open}");
        assert_eq!(open[0]["calls"], json!(1), "{open}");
    })
    .await;

    assert!(rig.meta.now().get("open_turns").is_none());
}

/// What the record costs an ordinary turn: one write of the meta at the
/// start. The write that saves the turn removes it.
#[tokio::test]
async fn an_ordinary_turn_writes_its_record_once_and_the_save_removes_it() {
    let rig = Rig::new().await;
    rig.then(says("Four."));

    rig.say("two plus two?").await;

    let meta_writes = rig.writes_of(META).await;
    assert_eq!(meta_writes.len(), 2, "the record, then the save");
    assert_eq!(meta_writes[0]["open_turns"].as_array().map(Vec::len), Some(1));
    assert!(meta_writes[0].get("turn_count").is_none(), "the record's write saves nothing of the turn");
    assert!(meta_writes[1].get("open_turns").is_none());
    assert_eq!(meta_writes[1]["turn_count"], json!(2));
    assert_eq!(rig.writes_of(HISTORY).await.len(), 1, "the history is written once, as before");
}

/// The record is a precaution. A database that refuses its write costs the
/// turn nothing: it runs, and is saved at its end as it always was.
#[tokio::test]
async fn a_record_that_cannot_be_written_does_not_fail_the_turn() {
    let rig = Rig::new().await;
    Mock::given(method("PUT"))
        .and(path(META))
        .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&rig.server)
        .await;
    rig.then(says("Four."));

    let frames = rig.say("two plus two?").await;

    assert_eq!(dones(&frames), ["Four."]);
    assert_eq!(complaints(&frames), [] as [&str; 0]);
    assert_eq!(rig.saved(), [row("user", "two plus two?"), row("assistant", "Four.")]);
}

/// A cut turn is saved, so its record goes with the save like any other's.
#[tokio::test]
async fn a_cut_turn_leaves_no_record_behind() {
    let rig = Rig::new().await;
    rig.then(refuses(400, "no such model"));

    rig.say("two plus two?").await;

    assert!(rig.meta.now().get("open_turns").is_none(), "{}", rig.meta.now());
    assert_eq!(rig.saved().len(), 2);
}

/// The two halves together. What the database holds while a turn runs is
/// what a power cut leaves; the step that runs at the next boot is run on
/// it here. The session then holds the turn: the operator's message, the
/// reply as far as it got, and the restart's marker.
#[tokio::test]
async fn a_turn_in_flight_when_the_process_ends_is_in_its_session_at_the_next_boot() {
    let rig = Rig::new().await;
    Mock::given(method("GET"))
        .and(path("/_collections"))
        .respond_with(data(json!([
            {"name": format!("sessions.{SESSION}.meta"), "doc_count": 1},
            {"name": format!("sessions.{SESSION}.history"), "doc_count": 1},
        ])))
        .mount(&rig.server)
        .await;
    rig.then(calculates("Let me work it out.", "2 ** 10")).then_holds();

    rig.say_and_stop_at_the_held_call("two to the ten?", async || {
        crate::sessions::open_turn::recover(&rig.ctx.db).await;

        assert_eq!(
            rig.saved(),
            [
                row("user", "two to the ten?"),
                row("assistant", "Let me work it out.\n\n(response interrupted by an embraOS restart)"),
            ]
        );
        let meta = rig.meta.now();
        assert!(meta.get("open_turns").is_none(), "{meta}");
        assert_eq!(meta["turn_count"], json!(2));
        assert!(meta["events_seen_at"].is_string(), "a call had completed: {meta}");
    })
    .await;
}

// ── A turn on the disk ──────────────────────────────────────────────

/// The database answers a write that the kernel still holds, and a power
/// cut takes what the kernel holds (`durable`). A turn has DATA written out
/// when it goes on record and when it is saved: two requests for a turn of
/// one call.
#[tokio::test]
async fn a_turn_has_data_written_out_at_its_start_and_at_its_save() {
    let rig = Rig::new().await;
    rig.then(says("Four."));
    let before = crate::durable::asked();

    rig.say("two plus two?").await;

    assert_eq!(crate::durable::asked() - before, 2);
}

/// And twice for each call that goes on: with the record of the reply so
/// far, before its tools run, and with what the tools did, before the model
/// is asked again. A long turn cut by the power is in its session up to its
/// last finished call, and the turn after it is told which tool calls had
/// finished.
#[tokio::test]
async fn a_turn_that_goes_on_has_data_written_out_after_each_call() {
    let rig = Rig::new().await;
    rig.then(calculates("Let me work it out.", "2 ** 10")).then(says("It is 1024."));
    let before = crate::durable::asked();

    rig.say("two to the ten?").await;

    assert_eq!(
        crate::durable::asked() - before,
        4,
        "the record, the record after the first call, the tool call's trace before the second call, the save"
    );
    // The trace's write is not left running beside the turn: it was waited
    // for before the model was asked again, and is there when the turn ends.
    assert_eq!(rig.traces.docs.lock().unwrap().len(), 1);
}

/// A cut turn is saved like any other, and as durably.
#[tokio::test]
async fn a_cut_turn_has_data_written_out_at_its_save_too() {
    let rig = Rig::new().await;
    rig.then(refuses(400, "no such model"));
    let before = crate::durable::asked();

    rig.say("hello?").await;

    assert_eq!(crate::durable::asked() - before, 2);
    assert_eq!(rig.saved().len(), 2, "the message and the marker");
}

// ── The same turn loop on Anthropic and on Gemini ───────────────────

/// An Anthropic stream that says `text`. A finished one closes with the
/// model's reason to stop and `message_stop`; an unfinished one is a body
/// that just ends.
fn anthropic_says(text: &str, finished: bool) -> ResponseTemplate {
    let mut events = vec![
        json!({"type": "message_start", "message": {"id": "m1", "type": "message", "role": "assistant",
            "content": [], "model": "scripted", "usage": {"input_tokens": 1, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
    ];
    if finished {
        events.push(json!({"type": "content_block_stop", "index": 0}));
        events.push(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}));
        events.push(json!({"type": "message_stop"}));
    }
    stream(events.iter().map(|e| format!("data: {e}\n\n")).collect())
}

/// A Gemini stream that says `text`: its one chunk carries `finishReason`
/// when the reply is finished, and none when the body just ends.
fn gemini_says(text: &str, finished: bool) -> ResponseTemplate {
    let mut candidate = json!({"content": {"role": "model", "parts": [{"text": text}]}, "index": 0});
    if finished {
        candidate["finishReason"] = json!("STOP");
    }
    stream(format!("data: {}\n\n", json!({"candidates": [candidate]})))
}

fn rows(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(role, text)| (role.to_string(), text.to_string())).collect()
}

/// William asked whether the cut turn and `/continue` hold on Anthropic and
/// on Gemini as on the local presets. The turn loop is one for every
/// provider; this runs it on each of the two, end to end: a call that is
/// refused is a cut turn, saved with its marker, and `/continue` sends the
/// same messages again.
#[tokio::test]
async fn on_anthropic_and_on_gemini_a_refused_call_is_a_cut_turn_that_continue_sends_again() {
    type Says = fn(&str, bool) -> ResponseTemplate;
    let cases: [(&str, &str, Says, &str); 2] = [
        ("anthropic", "/anthropic/", anthropic_says, "messages"),
        ("gemini", "/gemini/", gemini_says, "contents"),
    ];
    for (provider, prefix, says, conversation) in cases {
        let rig = Rig::on(provider).await;
        rig.then(refuses(400, r#"{"error": {"code": 400, "type": "invalid_request_error", "message": "scripted refusal", "status": "INVALID_ARGUMENT"}}"#))
            .then(says("Four.", true));

        let frames = rig.say("two plus two?").await;

        assert_eq!(dones(&frames), [""], "{provider}: one Done closes the cut turn");
        let said = complaints(&frames);
        assert_eq!(said.len(), 2, "{provider}: the error, then the notice: {said:?}");
        assert!(said[0].starts_with("Brain error: ") && said[0].contains("scripted refusal"), "{provider}: {}", said[0]);
        assert_eq!(said[1], format!("{CUT_NOTICE} {CONTINUE_HINT}"), "{provider}");
        assert_eq!(
            rig.saved(),
            rows(&[("user", "two plus two?"), ("assistant", crate::sessions::model_view::CALL_FAILED_MARKER)]),
            "{provider}"
        );

        let frames = rig.command("/continue", "").await;

        assert_eq!(dones(&frames).last().copied(), Some("Four."), "{provider}: {frames:?}");
        assert_eq!(complaints(&frames), [] as [&str; 0], "{provider}");
        let asked = rig.asked_at(prefix).await;
        assert_eq!(asked.len(), 2, "{provider}: the refused call and the continued one");
        assert!(asked[0][conversation].is_array(), "{provider}: {}", asked[0]);
        assert_eq!(asked[0][conversation], asked[1][conversation], "{provider}: the call that failed, sent again as it was");
        assert_eq!(
            rig.saved()[2..],
            rows(&[("user", CONTINUE_ROW), ("assistant", "Four.")]),
            "{provider}"
        );
    }
}

/// The gap on these two providers: a body that ended without the event
/// that closes a reply (`message_delta` with the stop reason; the chunk
/// with `finishReason`) was read as a finished turn. It is a cut turn like
/// the refused call: the text that arrived is kept and marked, the notice
/// says what happened, and `/continue` sends the call again.
#[tokio::test]
async fn on_anthropic_and_on_gemini_a_reply_whose_body_just_ends_is_a_cut_turn() {
    type Says = fn(&str, bool) -> ResponseTemplate;
    let cases: [(&str, &str, Says); 2] = [("anthropic", "/anthropic/", anthropic_says), ("gemini", "/gemini/", gemini_says)];
    for (provider, prefix, says) in cases {
        let rig = Rig::on(provider).await;
        rig.then(says("The answer is", false)).then(says("The answer is four.", true));

        let frames = rig.say("two plus two?").await;

        assert_eq!(dones(&frames), ["The answer is"], "{provider}: the text that arrived, closed once");
        assert_eq!(complaints(&frames), [format!("{STREAM_ENDED_NOTICE} {CONTINUE_HINT}").as_str()], "{provider}");
        assert_eq!(
            rig.saved(),
            rows(&[
                ("user", "two plus two?"),
                ("assistant", &format!("The answer is\n\n{}", crate::sessions::model_view::CALL_FAILED_SUFFIX)),
            ]),
            "{provider}"
        );

        let frames = rig.command("/continue", "").await;

        assert_eq!(dones(&frames).last().copied(), Some("The answer is four."), "{provider}: {frames:?}");
        assert_eq!(rig.asked_at(prefix).await.len(), 2, "{provider}");
    }
}

/// The local presets ended such a stream as `StreamEnded` already, with a
/// warning and the half reply saved as if it were whole. It is the same
/// cut turn now.
#[tokio::test]
async fn on_a_local_preset_a_reply_whose_stream_ends_without_its_finish_is_a_cut_turn() {
    let rig = Rig::new().await;
    // Text, then the body ends: no finish reason and no [DONE].
    rig.then(stream(chunk(json!({"content": "The answer is"}), None))).then(says("The answer is four."));

    let frames = rig.say("two plus two?").await;

    assert_eq!(dones(&frames), ["The answer is"]);
    assert_eq!(complaints(&frames), [format!("{STREAM_ENDED_NOTICE} {CONTINUE_HINT}").as_str()]);
    assert_eq!(
        rig.saved(),
        rows(&[
            ("user", "two plus two?"),
            ("assistant", &format!("The answer is\n\n{}", crate::sessions::model_view::CALL_FAILED_SUFFIX)),
        ])
    );

    let frames = rig.command("/continue", "").await;

    assert_eq!(dones(&frames).last().copied(), Some("The answer is four."));
    let asked = rig.asked().await;
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0]["messages"], asked[1]["messages"]);
}

/// A prompt Gemini blocked: no candidate comes back. It was saved as
/// `(no response)` with nothing said. It is said with its reason, saved as
/// a request the provider declined, and is nothing `/continue` could send
/// again with another result.
#[tokio::test]
async fn a_prompt_gemini_blocked_is_said_with_its_reason_and_is_not_kept_for_continue() {
    let rig = Rig::on("gemini").await;
    rig.then(stream(format!("data: {}\n\n", json!({"promptFeedback": {"blockReason": "PROHIBITED_CONTENT"}}))));

    let frames = rig.say("tell me that").await;

    let said = complaints(&frames);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].starts_with("Gemini blocked this prompt and generated nothing (block reason: PROHIBITED_CONTENT)."), "{}", said[0]);
    assert_eq!(rig.saved(), rows(&[("user", "tell me that"), ("assistant", "(request declined by provider: refusal)")]));

    let frames = rig.command("/continue", "").await;
    assert!(
        frames.iter().any(|f| matches!(f, ResponseType::System(m) if m.content.starts_with("Nothing to continue."))),
        "{frames:?}"
    );
    assert_eq!(rig.asked_at("/gemini/").await.len(), 1);
}

// ── A Guardian command that waits for the model ─────────────────────

/// `/guardian approve` and `/guardian define` ask the model for the
/// replicant check, and ran outside a turn: no console was told that
/// something ran, so Esc sent no stop, and a stop would have found no turn.
/// They run as a turn. The others, which ask the model nothing, do not.
#[tokio::test]
async fn a_guardian_command_that_asks_for_the_check_runs_as_a_turn_and_the_others_do_not() {
    let rig = Rig::new().await;
    let thinking = |frames: &[ResponseType]| -> Vec<(bool, String)> {
        frames
            .iter()
            .filter_map(|f| match f {
                ResponseType::Thinking(t) => Some((t.is_thinking, t.current_tool.clone())),
                _ => None,
            })
            .collect()
    };

    let frames = rig.command("/guardian", "approve no_such_tool").await;

    assert_eq!(thinking(&frames), [(true, GUARDIAN_CHECK_LABEL.to_string()), (false, String::new())]);
    assert!(
        matches!(frames.last(), Some(ResponseType::System(m)) if m.content.starts_with("guardian:")),
        "the answer follows the frame that ends the wait: {frames:?}"
    );
    assert!(!rig.ctx.in_turn.load(std::sync::atomic::Ordering::SeqCst), "the mark is gone with the command");

    let frames = rig.command("/guardian", "list").await;
    assert_eq!(thinking(&frames), []);

    // One tool's rebuild waits for its check too; the batch checks in the
    // background, where no console waits.
    let frames = rig.command("/guardian", "rebuild no_such_tool").await;
    assert_eq!(thinking(&frames), [(true, GUARDIAN_CHECK_LABEL.to_string()), (false, String::new())]);
    assert!(
        matches!(frames.last(), Some(ResponseType::System(m)) if m.content == "guardian: no such tool 'no_such_tool'."),
        "{frames:?}"
    );

    assert!(guardian_runs_the_check("approve temp_delta"));
    assert!(guardian_runs_the_check("define\n// guardian-tool: temp_delta\nfn run() {}"));
    assert!(guardian_runs_the_check("rebuild temp_delta"));
    for other in ["", "list", "status temp_delta", "reject temp_delta", "approved", "egress add host", "rebuild", "rebuild --all"] {
        assert!(!guardian_runs_the_check(other), "{other:?}");
    }
}

// ── A console that attaches while a turn runs ───────────────────────

/// The rig's session answers the probe an attach starts with, and is
/// recorded under the provider the rig runs on (an attach refuses a session
/// of another provider's).
async fn session_is_there(rig: &Rig) {
    rig.meta.doc.lock().unwrap()["provider"] = json!("lm_studio");
    Mock::given(method("GET"))
        .and(path(format!("/sessions.{SESSION}.meta")))
        .respond_with(ResponseTemplate::new(200))
        .mount(&rig.server)
        .await;
}

fn attach_to(session: &str) -> ConversationRequest {
    ConversationRequest {
        request_type: Some(conversation_request::RequestType::SessionAttach(SessionAttach {
            session_name: session.to_string(),
        })),
    }
}

/// A turn's frames go to the console that started it. One that attaches
/// while the turn runs (a phone that reconnects, a reloaded tab) saw
/// neither the running turn nor its end. It is told what runs, shown as
/// busy so that its stop reaches the turn, given no briefing beside the
/// turn, and sent the reply when the turn has ended.
#[tokio::test]
async fn a_console_that_attaches_while_a_turn_runs_is_told_and_is_sent_the_reply_when_it_ends() {
    let rig = Rig::new().await;
    session_is_there(&rig).await;
    rig.then_holds();
    let (tx2, mut rx2) = mpsc::channel::<Result<ConversationResponse, Status>>(256);

    // The first console's turn waits at its model call; the second attaches; the turn is stopped.
    rig.say_and_stop_at_the_held_call("a question that takes long", async || {
        handle_request(attach_to(SESSION), &tx2, &rig.ctx, "UTC", "", &rig.stop_rx).await.expect("the attach");
    })
    .await;

    let mut second = Vec::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(20), rx2.recv())
            .await
            .unwrap_or_else(|_| panic!("the second console is sent the turn's end; it got {second:?}"))
            .expect("its stream is open")
            .expect("a frame")
            .response_type
            .expect("a typed frame");
        let done = matches!(frame, ResponseType::Done(_));
        second.push(frame);
        if done {
            break;
        }
    }
    let told = second
        .iter()
        .position(|f| matches!(f, ResponseType::System(m) if m.content.starts_with("A turn is running in this session. It began ")))
        .unwrap_or_else(|| panic!("the second console is told that a turn runs: {second:?}"));
    let ResponseType::System(notice) = &second[told] else { unreachable!() };
    assert!(notice.content.contains("with: a question that takes long\n"), "{}", notice.content);
    assert!(notice.content.contains("The model has not answered yet."), "{}", notice.content);
    assert!(
        matches!(&second[told + 1], ResponseType::Thinking(t) if t.is_thinking && t.current_tool.is_empty()),
        "it is shown as busy, which is when a console sends a stop: {second:?}"
    );
    let n = second.len();
    assert!(
        matches!(&second[n - 2], ResponseType::System(m) if m.content == "The turn that was running has ended. Its reply:"),
        "{second:?}"
    );
    assert!(
        matches!(&second[n - 1], ResponseType::Done(d) if d.full_response == crate::sessions::model_view::OPERATOR_STOP_MARKER),
        "the turn's saved reply, which ends the busy state: {second:?}"
    );
    assert_eq!(rig.asked().await.len(), 1, "the attach asked the model nothing beside the running turn");
}

/// The licence's question on a configured machine, through the loop's own
/// step for a request a console sent. While the text this release carries
/// is not accepted there, a message reaches no model and is not saved, a
/// command waits, and an attach attaches the session, starts no briefing
/// and is followed by the question. Once it is accepted an attach briefs as
/// it did, and a message is a turn.
#[tokio::test]
async fn a_console_reaches_no_model_until_the_license_is_accepted() {
    let mut rig = Rig::new().await;
    session_is_there(&rig).await;
    // A sealed instance with a conversation from before: an attach of one briefs.
    let sealed = Mock::given(method("GET")).and(path("/soul.invariant")).respond_with(ResponseTemplate::new(200)).mount_as_scoped(&rig.server).await;
    let earlier = json!([{"role": "user", "content": "earlier"}, {"role": "assistant", "content": "an earlier reply"}]);
    rig.history.doc.lock().unwrap()["turns"] = earlier.clone();
    let (root, the_license) = crate::license::test_support::machine("console-request", true);
    rig.ctx.license = license::Gate::of(Arc::new(the_license));
    let message = |text: &str| ConversationRequest {
        request_type: Some(conversation_request::RequestType::UserMessage(UserMessage { content: text.to_string(), ..Default::default() })),
    };
    let command = |command: &str| ConversationRequest {
        request_type: Some(conversation_request::RequestType::SlashCommand(SlashCommand { command: command.to_string(), args: String::new() })),
    };
    let asks = |frames: &[ResponseType]| {
        matches!(frames.last(), Some(ResponseType::Setup(p)) if p.options == ["I do not accept", "Show the full text", "I accept the license"] && p.default_value.is_empty())
    };
    let briefing_begun = async || rig.ctx.session_mgr.read().await.briefing_attempt_recent(SESSION, std::time::Duration::from_secs(3600));

    // A message, a command and an attach while the licence is open.
    let frames = rig.console(message("what did I miss?")).await;
    assert_eq!(said(&frames), [license::NOT_SENT_ON]);
    assert!(asks(&frames), "{frames:?}");
    let frames = rig.console(command("/status")).await;
    assert_eq!(said(&frames), [crate::license::command_waits("/status")]);
    let frames = rig.console(attach_to(SESSION)).await;
    assert!(said(&frames).iter().any(|m| m.starts_with("Session 'main' attached")), "the attach went through: {frames:?}");
    assert!(asks(&frames), "the question follows the attach: {frames:?}");
    assert!(!briefing_begun().await, "an attach starts no briefing while the licence is open");
    let frames = rig.console(message("I do not accept")).await;
    assert_eq!(said(&frames), [crate::license::NOT_ACCEPTED]);
    assert!(rig.asked().await.is_empty(), "no model was asked anything");
    assert_eq!(rig.history.now()["turns"], earlier, "nothing was saved");

    // The accepting answer: recorded, and no turn of its own.
    let frames = rig.console(message("I accept the license")).await;
    assert_eq!(said(&frames).len(), 1, "{frames:?}");
    assert!(said(&frames)[0].starts_with("Accepted on this machine and recorded: "), "{frames:?}");
    assert!(rig.asked().await.is_empty());

    // An attach briefs as it did. (The briefing itself has no soul to read
    // on this rig and ends there; that it was begun is the attach's part.)
    let frames = rig.console(attach_to(SESSION)).await;
    assert!(!frames.iter().any(|f| matches!(f, ResponseType::Setup(_))), "asked once: {frames:?}");
    assert!(briefing_begun().await, "the attach after the answer briefs");
    assert!(rig.asked().await.is_empty());

    // And a message is a turn: on this rig's unsealed instance again, where a turn reaches the model.
    drop(sealed);
    rig.then(says("Nothing happened while you were away."));
    let frames = rig.console(message("what did I miss?")).await;
    assert_eq!(dones(&frames), ["Nothing happened while you were away."], "{frames:?}");
    assert_eq!(rig.asked().await.len(), 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_console_that_attaches_to_a_quiet_session_is_told_of_no_turn() {
    let rig = Rig::new().await;
    session_is_there(&rig).await;

    let frames = rig.send(attach_to(SESSION).request_type.expect("an attach")).await;

    assert!(
        frames.iter().any(|f| matches!(f, ResponseType::System(m) if m.content.starts_with("Session 'main' attached"))),
        "the attach went through: {frames:?}"
    );

    assert!(
        !frames.iter().any(|f| matches!(f, ResponseType::System(m) if m.content.contains("A turn is running"))),
        "{frames:?}"
    );
    assert!(!frames.iter().any(|f| matches!(f, ResponseType::Thinking(t) if t.is_thinking)), "{frames:?}");
}
