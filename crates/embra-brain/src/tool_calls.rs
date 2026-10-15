//! The tool-call feed: one record per tool call, with its input and result,
//! for the web console's Tools tab. Brain bus → `WatchToolCalls` → apid →
//! embra-web `/ws/tools`. Unlike the activity feed (`activity.rs`), this
//! feed carries content by design, cut per side at `FEED_INPUT_MAX` and
//! `FEED_RESULT_MAX` and marked when cut. `ListToolCalls` reads the
//! `tools.turn_trace` record back, newest first, for the browser's backfill
//! after a restart and its paging.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use embra_common::proto::brain::ToolCallRecord;
use tokio::sync::{broadcast, mpsc};
use tonic::Status;

use crate::activity::ToolOrigin;
use crate::tools::sessions::truncate_str;

/// Records a lagging watcher may fall behind by; `dropped` on the next
/// record it receives says how many it lost.
pub const CHANNEL_CAPACITY: usize = 256;
/// Per-side caps on the wire. The model gets the whole text; the feed
/// shows this much and names the full length (`input_bytes`,
/// `result_bytes`).
pub const FEED_INPUT_MAX: usize = 64 * 1024;
pub const FEED_RESULT_MAX: usize = 64 * 1024;
/// `ListToolCalls`: the most records in one page, and the page when none is
/// asked for.
pub const LIST_MAX: u32 = 200;
pub const LIST_DEFAULT: u32 = 100;
/// The text a stopped call carries, in its record and in its trace
/// document.
pub const STOPPED_RESULT: &str = "(stopped by operator)";

static BUS: OnceLock<broadcast::Sender<Arc<ToolCallRecord>>> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(0);

fn bus() -> &'static broadcast::Sender<Arc<ToolCallRecord>> {
    BUS.get_or_init(|| broadcast::channel(CHANNEL_CAPACITY).0)
}

/// The next call number, per brain process. Taken once per call, before the
/// dispatch, so the live record, a stopped record, the trace document and
/// the `ToolExecution` frame share it; the console prints it on the call's
/// marker line and the browser opens the call from it.
pub fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed) + 1
}

/// Whether anyone watches. The emit sites build their record behind this,
/// so a call copies nothing for nobody.
pub fn has_watchers() -> bool {
    bus().receiver_count() > 0
}

/// Hand a record to every watcher. Never awaits, never logs. With nobody
/// watching it goes nowhere, which is the normal case on an instance nobody
/// looks at; this is not a provider pump.
pub fn emit(rec: ToolCallRecord) {
    let tx = bus();
    if tx.receiver_count() > 0 {
        let _ = tx.send(Arc::new(rec));
    }
}

pub fn subscribe() -> broadcast::Receiver<Arc<ToolCallRecord>> {
    bus().subscribe()
}

/// One finished call, as the dispatch sites know it.
pub struct Call<'a> {
    pub seq: u64,
    pub tool_use_id: &'a str,
    pub name: &'a str,
    pub session: &'a str,
    /// The cron job that fired the call; empty for a turn's call.
    pub job_id: &'a str,
    pub started_at: DateTime<Utc>,
    pub elapsed_ms: u64,
    pub is_error: bool,
    pub input_json: &'a str,
    pub result: &'a str,
}

/// Cut `s` at a character boundary under `max`, say so in the text, and
/// keep the full length for the record.
fn cap(s: &str, max: usize) -> (String, u32) {
    let total = s.len();
    let shown = if total <= max {
        s.to_string()
    } else {
        let kept = truncate_str(s, max);
        format!("{kept}\n…[cut: {} of {total} bytes]", kept.len())
    };
    (shown, u32::try_from(total).unwrap_or(u32::MAX))
}

pub fn record(c: Call<'_>) -> ToolCallRecord {
    let (input_json, input_bytes) = cap(c.input_json, FEED_INPUT_MAX);
    let (result, result_bytes) = cap(c.result, FEED_RESULT_MAX);
    ToolCallRecord {
        seq: c.seq,
        tool_use_id: c.tool_use_id.to_string(),
        name: c.name.to_string(),
        session: c.session.to_string(),
        origin: ToolOrigin::from_session(c.session).as_str().to_string(),
        started_at_ms: c.started_at.timestamp_millis(),
        elapsed_ms: u32::try_from(c.elapsed_ms).unwrap_or(u32::MAX),
        is_error: c.is_error,
        stopped: false,
        input_json,
        input_bytes,
        result,
        result_bytes,
        dropped: 0,
        job_id: c.job_id.to_string(),
        from_record: false,
        started_at_rfc: c.started_at.to_rfc3339(),
    }
}

/// The record of a dispatch the operator stopped: an error that says so.
pub fn stopped_record(c: Call<'_>) -> ToolCallRecord {
    let mut rec = record(Call {
        is_error: true,
        result: STOPPED_RESULT,
        ..c
    });
    rec.stopped = true;
    rec
}

/// One watcher's forwarding loop: every record it receives goes to `tx`,
/// with the count a lag lost on the next one. Ends when the bus closes or
/// the watcher is gone.
pub(crate) async fn forward(
    mut rx: broadcast::Receiver<Arc<ToolCallRecord>>,
    tx: mpsc::Sender<Result<ToolCallRecord, Status>>,
) {
    let mut pending: u32 = 0;
    loop {
        match rx.recv().await {
            Ok(rec) => {
                let out = ToolCallRecord {
                    dropped: pending,
                    ..(*rec).clone()
                };
                pending = 0;
                if tx.send(Ok(out)).await.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                pending = pending.saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// The `tools.turn_trace` page the browser asks for: newest first under a
/// limit; before an exact stored `started_at` when paging (the string the
/// record carries, so two calls inside one millisecond are never skipped);
/// within one session when asked. The filter is left out when it would be
/// empty.
pub fn list_query_body(limit: u32, before: &str, session: &str) -> serde_json::Value {
    let mut filter = serde_json::Map::new();
    if !before.is_empty() {
        filter.insert("started_at".into(), serde_json::json!({ "$lt": before }));
    }
    if !session.is_empty() {
        filter.insert("session".into(), serde_json::Value::String(session.to_string()));
    }
    let mut body = serde_json::json!({
        "sort": [{"started_at": "desc"}, {"_id": "desc"}],
        "limit": limit,
    });
    if !filter.is_empty() {
        body["filter"] = serde_json::Value::Object(filter);
    }
    body
}

/// A `tools.turn_trace` document read back as a record: the previews are
/// the content, the byte totals are unknown (zero), `from_record` is set.
/// A document without a readable `started_at` is skipped.
pub fn record_from_trace(doc: &serde_json::Value) -> Option<ToolCallRecord> {
    let text = |k: &str| doc.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let number = |k: &str| doc.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let flag = |k: &str| doc.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let started_at_rfc = text("started_at");
    let started_at_ms = DateTime::parse_from_rfc3339(&started_at_rfc).ok()?.timestamp_millis();
    let session = text("session");
    Some(ToolCallRecord {
        seq: number("seq"),
        tool_use_id: text("tool_use_id"),
        name: text("tool_name"),
        origin: ToolOrigin::from_session(&session).as_str().to_string(),
        session,
        started_at_ms,
        elapsed_ms: u32::try_from(number("elapsed_ms")).unwrap_or(u32::MAX),
        is_error: flag("is_error"),
        stopped: flag("stopped"),
        input_json: text("input_preview"),
        input_bytes: 0,
        result: text("result_preview"),
        result_bytes: 0,
        dropped: 0,
        job_id: String::new(),
        from_record: true,
        started_at_rfc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call<'a>(input: &'a str, result: &'a str) -> Call<'a> {
        Call {
            seq: 7,
            tool_use_id: "toolu_01",
            name: "file_read",
            session: "main",
            job_id: "",
            started_at: DateTime::parse_from_rfc3339("2026-10-15T14:02:11.250+00:00")
                .unwrap()
                .with_timezone(&Utc),
            elapsed_ms: 812,
            is_error: false,
            input_json: input,
            result,
        }
    }

    #[test]
    fn a_record_carries_the_input_and_the_result() {
        let rec = record(call(r#"{"path":"/tmp/a"}"#, "line one\nline two"));
        assert_eq!(rec.input_json, r#"{"path":"/tmp/a"}"#);
        assert_eq!(rec.input_bytes, 17);
        assert_eq!(rec.result, "line one\nline two");
        assert_eq!(rec.result_bytes, 17);
        assert_eq!(rec.origin, "turn");
        assert_eq!(rec.started_at_ms, 1_792_072_931_250);
        assert_eq!(rec.started_at_rfc, "2026-10-15T14:02:11.250+00:00");
        assert_eq!(rec.elapsed_ms, 812);
        assert!(!rec.stopped && !rec.is_error && !rec.from_record);
    }

    #[test]
    fn the_caps_cut_on_a_char_boundary_and_name_the_full_length() {
        let wide = "日".repeat(FEED_INPUT_MAX / 3 + 100);
        let rec = record(call(&wide, &wide));
        assert_eq!(rec.input_bytes as usize, wide.len());
        assert!(rec.input_json.contains("…[cut: "));
        assert!(rec.input_json.ends_with(&format!(" of {} bytes]", wide.len())));
        let shown = rec.input_json.split("\n…[cut").next().unwrap();
        assert!(shown.len() <= FEED_INPUT_MAX);
        assert!(shown.len() > FEED_INPUT_MAX - 3, "cut at the boundary, not far under it");
        assert!(shown.chars().all(|c| c == '日'), "no character was split");
        assert_eq!(rec.result_bytes as usize, wide.len());
    }

    #[test]
    fn a_stopped_record_is_an_error_that_says_so() {
        let rec = stopped_record(call(r#"{"cmd":"sleep"}"#, "ignored"));
        assert!(rec.stopped);
        assert!(rec.is_error);
        assert_eq!(rec.result, STOPPED_RESULT);
        assert_eq!(rec.input_json, r#"{"cmd":"sleep"}"#);
    }

    #[test]
    fn a_cron_call_reads_as_cron_origin_with_its_job() {
        let mut c = call("{}", "ok");
        c.session = "cron";
        c.job_id = "0199a-job";
        c.tool_use_id = "";
        let rec = record(c);
        assert_eq!(rec.origin, "cron");
        assert_eq!(rec.job_id, "0199a-job");
        assert_eq!(rec.tool_use_id, "");
    }

    #[test]
    fn seq_is_taken_once_per_call_and_only_grows() {
        let a = next_seq();
        let b = next_seq();
        assert!(b > a);
    }

    #[test]
    fn an_emit_without_a_watcher_sends_nothing_and_blocks_nothing() {
        // No subscriber on the bus: the record has nowhere to go and the
        // call returns at once.
        emit(record(call("{}", "ok")));
    }

    #[tokio::test]
    async fn a_lagged_watcher_sees_the_count_on_the_next_record() {
        let (bus_tx, bus_rx) = broadcast::channel::<Arc<ToolCallRecord>>(2);
        for seq in 1..=4u64 {
            let mut rec = record(call("{}", "ok"));
            rec.seq = seq;
            bus_tx.send(Arc::new(rec)).unwrap();
        }
        let (tx, mut rx) = mpsc::channel(8);
        let task = tokio::spawn(forward(bus_rx, tx));
        let first = rx.recv().await.unwrap().unwrap();
        assert_eq!(first.dropped, 2, "two records fell out of a channel of two");
        assert_eq!(first.seq, 3);
        let second = rx.recv().await.unwrap().unwrap();
        assert_eq!(second.dropped, 0);
        assert_eq!(second.seq, 4);
        drop(bus_tx);
        task.await.unwrap();
    }

    #[test]
    fn the_list_query_pages_on_the_stored_started_at_newest_first() {
        let newest = list_query_body(100, "", "");
        assert!(newest.get("filter").is_none(), "{newest}");
        assert_eq!(newest["limit"], 100);
        assert_eq!(newest["sort"], serde_json::json!([{"started_at": "desc"}, {"_id": "desc"}]));

        let older = list_query_body(50, "2026-10-15T14:02:11.250+00:00", "");
        assert_eq!(older["filter"]["started_at"]["$lt"], "2026-10-15T14:02:11.250+00:00");
        assert!(older["filter"].get("session").is_none());

        let one = list_query_body(50, "", "main");
        assert_eq!(one["filter"]["session"], "main");
        assert!(one["filter"].get("started_at").is_none());
    }

    #[test]
    fn a_trace_document_reads_back_as_a_record_marked_from_record() {
        let doc = serde_json::json!({
            "session": "main",
            "turn_index": 3,
            "seq": 17,
            "tool_use_id": "toolu_01",
            "tool_name": "file_read",
            "input_preview": "{\"path\":\"/tmp/a\"}",
            "started_at": "2026-10-15T14:02:11.250+00:00",
            "elapsed_ms": 812,
            "is_error": false,
            "stopped": true,
            "result_preview": "(stopped by operator)",
        });
        let rec = record_from_trace(&doc).expect("a readable document");
        assert!(rec.from_record);
        assert_eq!(rec.seq, 17);
        assert_eq!(rec.name, "file_read");
        assert_eq!(rec.input_json, "{\"path\":\"/tmp/a\"}");
        assert_eq!(rec.input_bytes, 0);
        assert_eq!(rec.result, "(stopped by operator)");
        assert!(rec.stopped);
        assert_eq!(rec.started_at_ms, 1_792_072_931_250);
        assert_eq!(rec.started_at_rfc, "2026-10-15T14:02:11.250+00:00");
        assert_eq!(rec.origin, "turn");

        let unreadable = serde_json::json!({"tool_name": "x", "started_at": "yesterday"});
        assert!(record_from_trace(&unreadable).is_none());
    }
}
