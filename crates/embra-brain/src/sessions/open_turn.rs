//! A turn is on record from its start.
//!
//! A turn's message and reply are written together when the turn ends. A
//! turn that a failed model call cuts still ends, and is saved as far as
//! it got. A process that ends does not get there: a reboot, a restart of
//! the brain, the power. So a turn leaves a small record on its session's
//! meta document before its first model call (`SessionMeta.open_turns`):
//! the operator's message, and after each call that completes, the reply
//! so far. The write that saves the turn removes the record in the same
//! meta write.
//!
//! A record that is still there when the brain starts is a turn the
//! process did not live to finish: [`recover`] writes it into its session
//! as a turn cut by a restart, once, and removes it. It runs with the
//! migrations, before the brain listens, so no turn is in flight then and
//! every record it finds is a dead one.
//!
//! The record is on the meta document and not in a collection of its own:
//! a new collection is a step of the schema (`migrations`), and a schema
//! newer than a build makes the previous slot refuse to start. An optional
//! field is read with a default, an older build drops it at its next write
//! of the meta, and a session that is deleted, restored or reaped takes its
//! records with it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::brain::Message;
use crate::db::WardsonDbClient;

use super::model_view::{RESTART_MARKER, RESTART_SUFFIX};
use super::{CURRENT_SESSION_FORMAT, SessionHistory, SessionMeta, history_query_body};

/// One turn that has started and is not yet saved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenTurn {
    pub id: String,
    pub started_at: DateTime<Utc>,
    /// When the record was last written: the last the turn is known to have
    /// been alive.
    pub updated_at: DateTime<Utc>,
    /// The length of the history when the turn started. A history that has
    /// grown past it may hold this turn already: the process ended between
    /// the two writes that save one.
    pub base_len: usize,
    /// The operator's message, as the turn saves it.
    pub message: Message,
    /// The reply so far: the text of every model call that completed.
    #[serde(default)]
    pub text: String,
    /// Model calls of the turn that completed. With one or more the model
    /// was shown what the turn's first message carried.
    #[serde(default)]
    pub calls: u32,
}

impl OpenTurn {
    pub(super) fn new(message: Message, base_len: usize) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        // Unique among the turns of one boot; the records of an earlier
        // boot are gone before the first turn of this one.
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let now = Utc::now();
        Self {
            id: format!("{}-{}", now.timestamp_micros(), NEXT.fetch_add(1, Ordering::Relaxed)),
            started_at: now,
            updated_at: now,
            base_len,
            message,
            text: String::new(),
            calls: 0,
        }
    }
}

/// The reply a turn cut by a restart is saved with: what it had, and the
/// marker.
pub(crate) fn restart_reply(text: &str) -> String {
    if text.trim().is_empty() {
        RESTART_MARKER.to_string()
    } else {
        format!("{}\n\n{RESTART_SUFFIX}", text.trim_end())
    }
}

/// What a record left by a restart becomes.
#[derive(Debug)]
pub(crate) enum Recovered {
    /// The turn, as a cut one: the operator's message and the reply so far.
    Turn([Message; 2]),
    /// The history holds the message already: the turn was saved, and the
    /// process ended before the meta followed.
    AlreadySaved,
}

/// `record` against the history it was opened on.
pub(crate) fn recover_record(turns: &[Message], record: &OpenTurn) -> Recovered {
    let since = turns.get(record.base_len.min(turns.len())..).unwrap_or(&[]);
    let saved = since.iter().any(|m| m.role == "user" && m.content == record.message.content);
    if saved {
        Recovered::AlreadySaved
    } else {
        Recovered::Turn([record.message.clone(), Message::assistant(restart_reply(&record.text))])
    }
}

/// Write every turn a process did not live to finish into its session, as
/// a turn cut by a restart, and remove its record. Every boot, with the
/// migrations; a session that cannot be read or written keeps its records
/// for the next boot, and nothing here fails a boot.
pub async fn recover(db: &WardsonDbClient) {
    let collections = match db.list_collections().await {
        Ok(c) => c,
        Err(e) => {
            warn!("Open turns not recovered (list_collections failed): {e}");
            return;
        }
    };
    for meta_collection in collections {
        let Some(name) = meta_collection.strip_prefix("sessions.").and_then(|rest| rest.strip_suffix(".meta")) else {
            continue;
        };
        let Ok(docs) = db.query(&meta_collection, &history_query_body()).await else {
            continue;
        };
        let Some(doc) = docs.into_iter().next() else {
            continue;
        };
        let Some(meta_id) = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()).map(str::to_string) else {
            continue;
        };
        let Ok(mut meta) = serde_json::from_value::<SessionMeta>(doc) else {
            continue;
        };
        if meta.open_turns.is_empty() {
            continue;
        }
        let records = std::mem::take(&mut meta.open_turns);
        let found = records.len();
        recover_session(db, name, &mut meta, records).await;
        match serde_json::to_value(&meta) {
            Ok(value) => match db.update(&meta_collection, &meta_id, &value).await {
                Ok(_) => info!(
                    session = %name,
                    found,
                    kept = meta.open_turns.len(),
                    "open turns of a process that ended were recovered"
                ),
                Err(e) => warn!(session = %name, "the meta of a recovered session was not written: {e}"),
            },
            Err(e) => warn!(session = %name, "the meta of a recovered session did not serialize: {e}"),
        }
    }
}

/// One session's records, oldest first. A record that could not be written
/// goes back into `meta`, for the next boot.
async fn recover_session(db: &WardsonDbClient, name: &str, meta: &mut SessionMeta, mut records: Vec<OpenTurn>) {
    if meta.deleted_at.is_some() {
        // A write of the history would drop the deletion's stamp, and the
        // session is on its way out with everything it holds.
        info!(session = %name, dropped = records.len(), "a deleted session's open turns were dropped");
        return;
    }
    let history_collection = format!("sessions.{name}.history");
    let doc = match db.query(&history_collection, &history_query_body()).await {
        Ok(docs) => docs.into_iter().next(),
        Err(e) => {
            warn!(session = %name, "the history could not be read; its open turns wait for the next boot: {e}");
            meta.open_turns = records;
            return;
        }
    };
    let Some(doc) = doc else {
        info!(session = %name, dropped = records.len(), "a session without a history: its open turns were dropped");
        return;
    };
    let history_id = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()).map(str::to_string);
    let (Some(history_id), Ok(mut history)) = (history_id, serde_json::from_value::<SessionHistory>(doc)) else {
        warn!(session = %name, "the history could not be parsed; its open turns wait for the next boot");
        meta.open_turns = records;
        return;
    };
    if history.format_version < CURRENT_SESSION_FORMAT {
        info!(session = %name, dropped = records.len(), "a legacy session is read-only: its open turns were dropped");
        return;
    }

    records.sort_by_key(|record| record.started_at);
    let mut appended: Vec<&OpenTurn> = Vec::new();
    for record in &records {
        if let Recovered::Turn(turn) = recover_record(&history.turns, record) {
            history.turns.extend(turn);
            appended.push(record);
        }
    }
    if appended.is_empty() {
        return;
    }
    let written = match serde_json::to_value(&history) {
        Ok(value) => db.update(&history_collection, &history_id, &value).await.map(|_| ()),
        Err(e) => Err(e.into()),
    };
    if let Err(e) = written {
        warn!(session = %name, "the recovered turns were not written; they wait for the next boot: {e}");
        meta.open_turns = records;
        return;
    }
    meta.turn_count = Some(u32::try_from(history.turns.len()).unwrap_or(u32::MAX));
    for record in appended {
        // The last the turn was known alive, never the boot's own time:
        // the attach's idle gate measures the operator's absence from it.
        meta.last_active = meta.last_active.max(record.updated_at);
        // A call had completed: the model was shown what the turn's first
        // message carried, and the turn's calls are on record from its start.
        if record.calls > 0 {
            meta.events_seen_at = Some(meta.events_seen_at.map_or(record.started_at, |at| at.max(record.started_at)));
        }
    }
}

/// How long ago, in the words of a notice: seconds under a minute, minutes
/// under an hour, hours and minutes after.
fn ago(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - since).num_seconds().max(0);
    match secs {
        0..=59 => format!("{secs} s"),
        60..=3599 => format!("{} min", secs / 60),
        _ => format!("{} h {} min", secs / 3600, (secs % 3600) / 60),
    }
}

/// Bytes of the operator's message quoted in the notice.
const MESSAGE_SHOWN: usize = 160;

/// What a console is told when it attaches to a session in which a turn is
/// running. The turn's frames go to the console that started it, and that
/// console may be gone (a phone that lost its connection, a browser tab
/// that was reloaded): the one that attaches now saw neither the running
/// turn nor, later, its end. The record of the open turn says what runs.
pub(crate) fn running_notice(open: &OpenTurn, now: DateTime<Utc>) -> String {
    let message: String = open.message.content.split_whitespace().collect::<Vec<_>>().join(" ");
    let quoted = if message.len() > MESSAGE_SHOWN {
        format!("{}…", crate::tools::sessions::truncate_str(&message, MESSAGE_SHOWN))
    } else {
        message
    };
    let progress = match open.calls {
        0 => "The model has not answered yet".to_string(),
        1 => "One model call of it is done".to_string(),
        n => format!("{n} model calls of it are done"),
    };
    format!(
        "A turn is running in this session. It began {} ago with: {quoted}\n{progress}. This console was not attached when it began: the reply appears here when the turn ends, and a stop (Esc, /stop) ends it from here too.",
        ago(open.started_at, now)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// What a console that attaches during a turn is told: when the turn
    /// began, with which message, how far it is, and what the console can
    /// do.
    #[test]
    fn a_running_turns_notice_says_when_it_began_with_what_and_how_far_it_is() {
        let at = |t: &str| -> DateTime<Utc> { t.parse().unwrap() };
        let open = record("Read the three files\n  and compare them", 4, "", 0);
        let said = running_notice(&open, at("2026-11-19T10:00:42Z"));
        assert_eq!(
            said,
            "A turn is running in this session. It began 42 s ago with: Read the three files and compare them\nThe model has not answered yet. This console was not attached when it began: the reply appears here when the turn ends, and a stop (Esc, /stop) ends it from here too."
        );
        let said = running_notice(&record("hello", 4, "Two are read.", 1), at("2026-11-19T10:12:30Z"));
        assert!(said.contains("It began 12 min ago with: hello\nOne model call of it is done."), "{said}");
        let said = running_notice(&record("hello", 4, "Two are read.", 7), at("2026-11-19T11:20:00Z"));
        assert!(said.contains("It began 1 h 20 min ago") && said.contains("7 model calls of it are done."), "{said}");
        // A long message is cut, on a character's boundary.
        let long = "é".repeat(400);
        let said = running_notice(&record(&long, 4, "", 0), at("2026-11-19T10:00:01Z"));
        let quoted = said.split("with: ").nth(1).unwrap().split('\n').next().unwrap();
        assert!(quoted.ends_with('…') && quoted.len() <= MESSAGE_SHOWN + '…'.len_utf8(), "{}", quoted.len());
        // A clock that reads earlier than the start counts no negative time.
        assert!(running_notice(&open, at("2026-11-19T09:59:00Z")).contains("It began 0 s ago"));
    }

    fn record(message: &str, base_len: usize, text: &str, calls: u32) -> OpenTurn {
        OpenTurn {
            id: "1".into(),
            started_at: "2026-11-19T10:00:00Z".parse().unwrap(),
            updated_at: "2026-11-19T10:05:00Z".parse().unwrap(),
            base_len,
            message: Message::user(message),
            text: text.into(),
            calls,
        }
    }

    #[test]
    fn a_record_left_by_a_restart_becomes_a_cut_turn() {
        let earlier = [Message::user("hello"), Message::assistant("Hello.")];
        let Recovered::Turn([asked, reply]) = recover_record(&earlier, &record("read the files", 2, "", 0)) else {
            panic!("the turn is not in the history yet")
        };
        assert_eq!((asked.role.as_str(), asked.content.as_str()), ("user", "read the files"));
        assert_eq!(reply.content, "(turn interrupted by an embraOS restart)");

        let Recovered::Turn([_, reply]) = recover_record(&earlier, &record("read the files", 2, "Two are read.\n", 3))
        else {
            panic!("the turn is not in the history yet")
        };
        assert_eq!(reply.content, "Two are read.\n\n(response interrupted by an embraOS restart)");
    }

    /// The process ended between the two writes that save a turn: the
    /// history has it, the meta still held the record.
    #[test]
    fn a_turn_the_history_already_holds_is_not_written_twice() {
        let saved = [
            Message::user("hello"),
            Message::assistant("Hello."),
            Message::user("read the files"),
            Message::assistant("All three read."),
        ];
        assert!(matches!(recover_record(&saved, &record("read the files", 2, "", 1)), Recovered::AlreadySaved));
        // The same words in an earlier turn are not this turn.
        let Recovered::Turn(_) = recover_record(&saved, &record("hello", 4, "", 0)) else {
            panic!("the earlier hello is before the record's base")
        };
        // A base past the history's end (a history that was restored
        // shorter): the turn is written.
        assert!(matches!(recover_record(&saved, &record("again", 40, "", 0)), Recovered::Turn(_)));
    }

    fn data(v: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"ok": true, "meta": {}, "data": v}))
    }

    /// A database with the sessions given: each its meta document and its
    /// history's turns. Every write is answered.
    async fn database(sessions: &[(&str, Value, Value)]) -> MockServer {
        let server = MockServer::start().await;
        let mut names = Vec::new();
        for (name, meta, turns) in sessions {
            names.push(json!({"name": format!("sessions.{name}.meta"), "doc_count": 1}));
            names.push(json!({"name": format!("sessions.{name}.history"), "doc_count": 1}));
            Mock::given(method("POST"))
                .and(path(format!("/sessions.{name}.meta/query")))
                .respond_with(data(json!([meta])))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path(format!("/sessions.{name}.history/query")))
                .respond_with(data(json!([{
                    "_id": "h1", "session_name": name, "format_version": 2, "turns": turns,
                }])))
                .mount(&server)
                .await;
        }
        names.push(json!({"name": "memory.entries", "doc_count": 3}));
        Mock::given(method("GET")).and(path("/_collections")).respond_with(data(json!(names))).mount(&server).await;
        Mock::given(method("PUT")).respond_with(data(json!({}))).mount(&server).await;
        server
    }

    fn meta(name: &str, extra: Value) -> Value {
        let mut doc = json!({
            "_id": "m1", "id": "m1", "name": name, "state": "Active",
            "created_at": "2026-11-01T00:00:00Z", "last_active": "2026-11-19T09:00:00Z",
        });
        for (k, v) in extra.as_object().unwrap() {
            doc[k] = v.clone();
        }
        doc
    }

    fn open(message: &str, base_len: usize, text: &str, calls: u32) -> Value {
        serde_json::to_value(record(message, base_len, text, calls)).unwrap()
    }

    async fn puts(server: &MockServer) -> Vec<(String, Value)> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "PUT")
            .map(|r| (r.url.path().to_string(), serde_json::from_slice(&r.body).unwrap_or(Value::Null)))
            .collect()
    }

    /// A turn was running when the process ended. At the next boot the
    /// session holds it: the operator's message, the reply as far as it
    /// got, and the marker; the record is gone, and the session's times are
    /// the turn's own, never the boot's.
    #[tokio::test]
    async fn a_record_left_by_a_restart_is_written_into_its_session_once() {
        let server = database(&[
            (
                "main",
                meta("main", json!({"open_turns": [open("read the files", 2, "Two are read.", 2)]})),
                json!([{"role": "user", "content": "hello"}, {"role": "assistant", "content": "Hello."}]),
            ),
            // A session with nothing open is read and left alone.
            ("notes", meta("notes", json!({})), json!([])),
        ])
        .await;

        recover(&WardsonDbClient::from_url(&server.uri())).await;

        let writes = puts(&server).await;
        let paths: Vec<&str> = writes.iter().map(|w| w.0.as_str()).collect();
        assert_eq!(paths, ["/sessions.main.history/docs/h1", "/sessions.main.meta/docs/m1"]);
        let turns: Vec<(&str, &str)> = writes[0].1["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| (t["role"].as_str().unwrap(), t["content"].as_str().unwrap()))
            .collect();
        assert_eq!(
            turns,
            [
                ("user", "hello"),
                ("assistant", "Hello."),
                ("user", "read the files"),
                ("assistant", "Two are read.\n\n(response interrupted by an embraOS restart)"),
            ]
        );
        let meta = &writes[1].1;
        assert!(meta.get("open_turns").is_none(), "the record is gone: {meta}");
        assert_eq!(meta["turn_count"], json!(4));
        assert_eq!(meta["last_active"], json!("2026-11-19T10:05:00Z"), "the last the turn was alive");
        assert_eq!(meta["events_seen_at"], json!("2026-11-19T10:00:00Z"), "a call had completed");
    }

    #[tokio::test]
    async fn a_record_of_a_deleted_or_legacy_session_is_dropped_without_a_history_write() {
        let server = MockServer::start().await;
        let names = json!([
            {"name": "sessions.gone.meta", "doc_count": 1}, {"name": "sessions.gone.history", "doc_count": 1},
            {"name": "sessions.old.meta", "doc_count": 1}, {"name": "sessions.old.history", "doc_count": 1},
        ]);
        Mock::given(method("GET")).and(path("/_collections")).respond_with(data(names)).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/sessions.gone.meta/query"))
            .respond_with(data(json!([meta(
                "gone",
                json!({"deleted_at": "2026-11-18T00:00:00Z", "open_turns": [open("one last thing", 0, "", 0)]})
            )])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/sessions.old.meta/query"))
            .respond_with(data(json!([meta("old", json!({"open_turns": [open("still there?", 0, "", 0)]}))])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/sessions.old.history/query"))
            .respond_with(data(json!([{"_id": "h1", "session_name": "old", "format_version": 1, "turns": []}])))
            .mount(&server)
            .await;
        Mock::given(method("PUT")).respond_with(data(json!({}))).mount(&server).await;

        recover(&WardsonDbClient::from_url(&server.uri())).await;

        let writes = puts(&server).await;
        let paths: Vec<&str> = writes.iter().map(|w| w.0.as_str()).collect();
        assert_eq!(paths, ["/sessions.gone.meta/docs/m1", "/sessions.old.meta/docs/m1"], "no history is written");
        assert!(writes.iter().all(|w| w.1.get("open_turns").is_none()), "{writes:?}");
        assert_eq!(writes[0].1["deleted_at"], json!("2026-11-18T00:00:00Z"), "the deletion's stamp stands");
    }

    /// A history that cannot be written keeps the record: the next boot
    /// tries again, and nothing is lost meanwhile.
    #[tokio::test]
    async fn a_turn_that_cannot_be_written_keeps_its_record_for_the_next_boot() {
        let server = database(&[(
            "main",
            meta("main", json!({"open_turns": [open("read the files", 0, "", 0)]})),
            json!([]),
        )])
        .await;
        Mock::given(method("PUT"))
            .and(path("/sessions.main.history/docs/h1"))
            .respond_with(ResponseTemplate::new(500))
            .with_priority(1)
            .mount(&server)
            .await;

        recover(&WardsonDbClient::from_url(&server.uri())).await;

        let writes = puts(&server).await;
        let kept = writes.iter().find(|w| w.0 == "/sessions.main.meta/docs/m1").expect("the meta is written");
        assert_eq!(kept.1["open_turns"].as_array().map(Vec::len), Some(1), "{}", kept.1);
        assert!(kept.1.get("turn_count").is_none());
    }
}
