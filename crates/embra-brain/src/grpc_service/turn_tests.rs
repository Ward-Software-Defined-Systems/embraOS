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

/// A brain with one session, a database and a model server.
struct Rig {
    server: MockServer,
    script: Script,
    history: Stored,
    meta: Stored,
    /// `tools.turn_trace`: one document per finished tool call.
    traces: Kept,
    ctx: ServiceCtx,
    /// The operator's stop: a turn in flight sees the counter move.
    stop_tx: watch::Sender<u64>,
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
        };
        let (stop_tx, stop_rx) = watch::channel(0u64);
        Self { server, script, history, meta, traces, ctx, stop_tx, stop_rx }
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
