//! What a turn is told when the turn before it was cut.
//!
//! A turn whose model call failed is saved with the operator's message, the
//! text it had and a marker (`model_view`). The text is all a history
//! keeps: the tool calls of the turn, their results and the model's own
//! steps were in memory, and are gone. So the first turn after a cut one
//! carries a block at the head of its model-facing text: that the turn was
//! cut, and which of its tool calls had finished, read from
//! `tools.turn_trace`. Nothing is persisted, and the block is said once:
//! it follows from the last entry of the stored history, which the turn
//! that carries it replaces as the last.
//!
//! `/continue` goes without it: there the model is sent the cut call
//! itself, with everything the turn held.

use chrono::{DateTime, Utc};

use crate::brain::Message;
use crate::db::WardsonDbClient;
use crate::tools::sessions::truncate_str;

use super::model_view::{
    CALL_FAILED_MARKER, CALL_FAILED_SUFFIX, RESTART_MARKER, RESTART_SUFFIX, STREAM_FAILED_MARKER,
};

/// Tool calls read of the cut turn. A full window makes the count a lower
/// bound.
const CALLS_WINDOW: usize = 100;

/// Tool calls listed; the rest are counted.
const CALLS_SHOWN: usize = 20;

/// Byte cap on one call's input in its line.
const INPUT_SHOWN: usize = 120;

/// How the last turn of a history ended, when it was cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cut {
    /// A model call failed: the request was refused or lost, the server
    /// sent nothing for the whole wait, or the stream broke.
    FailedCall,
    /// The process ended while the turn was running, and the turn was
    /// written into the session at the next boot (`open_turn::recover`).
    Restart,
}

/// One tool call of the cut turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CutCall {
    pub name: String,
    pub input: String,
    /// `ok`, `ERR` or `stopped`.
    pub state: &'static str,
}

/// Whether the last turn of `history` was cut: its reply is a cut's marker
/// alone, or ends with one after the text it had.
pub(crate) fn cut_of(history: &[Message]) -> Option<Cut> {
    let last = history.last()?;
    if last.role != "assistant" {
        return None;
    }
    let reply = last.content.trim_end();
    let ends_with = |marker: &str| {
        reply == marker || reply.strip_suffix(marker).is_some_and(|before| before.ends_with("\n\n"))
    };
    if [RESTART_MARKER, RESTART_SUFFIX].into_iter().any(ends_with) {
        return Some(Cut::Restart);
    }
    [CALL_FAILED_MARKER, CALL_FAILED_SUFFIX, STREAM_FAILED_MARKER]
        .into_iter()
        .any(ends_with)
        .then_some(Cut::FailedCall)
}

/// The tool calls of turn `turn_index` of `session` that are on record, in
/// the order they started, and whether there may be more than were read. A
/// call that started before `since` is another turn's: one session's name
/// can have been another session's before, and an index is a count. A read
/// that fails gives none: the turn goes ahead without them.
pub(crate) async fn gather(
    db: &WardsonDbClient,
    session: &str,
    turn_index: usize,
    since: DateTime<Utc>,
) -> (Vec<CutCall>, bool) {
    let body = crate::tool_calls::turn_query_body(session, turn_index, CALLS_WINDOW);
    let docs = db.query("tools.turn_trace", &body).await.unwrap_or_default();
    let window_full = docs.len() >= CALLS_WINDOW;
    (calls_since(&docs, since), window_full)
}

/// The calls among `docs` that started at or after `since`.
fn calls_since(docs: &[serde_json::Value], since: DateTime<Utc>) -> Vec<CutCall> {
    docs.iter()
        .filter_map(crate::tool_calls::record_from_trace)
        .filter(|call| call.started_at_ms >= since.timestamp_millis())
        .map(|call| CutCall {
            state: if call.stopped {
                "stopped"
            } else if call.is_error {
                "ERR"
            } else {
                "ok"
            },
            input: shown(&call.input_json),
            name: call.name,
        })
        .collect()
}

/// A call's input on one line, cut.
fn shown(input: &str) -> String {
    let one_line: String = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.len() > INPUT_SHOWN {
        format!("{}…", truncate_str(&one_line, INPUT_SHOWN))
    } else {
        one_line
    }
}

/// The block as the turn carries it.
pub(crate) fn render(cut: Cut, calls: &[CutCall], may_be_more: bool) -> String {
    let how = match cut {
        Cut::FailedCall => "a model call failed",
        Cut::Restart => "embraOS restarted while it was running",
    };
    let mut out = format!(
        "<interrupted_turn>\nYour last turn in this session was cut before it finished: {how}. The message it was answering is above, with what you had written of the reply. The tool results of that turn are no longer in your context.\n"
    );
    if calls.is_empty() {
        out.push_str("No tool call of that turn is on record.\n");
    } else {
        out.push_str("Tool calls of that turn, in order:\n");
        for call in calls.iter().take(CALLS_SHOWN) {
            if call.input.is_empty() {
                out.push_str(&format!("- {}: {}\n", call.name, call.state));
            } else {
                out.push_str(&format!("- {} {}: {}\n", call.name, call.input, call.state));
            }
        }
        if calls.len() > CALLS_SHOWN {
            let rest = calls.len() - CALLS_SHOWN;
            let at_least = if may_be_more { "at least " } else { "" };
            out.push_str(&format!("- and {at_least}{rest} more\n"));
        }
        out.push_str("turn_trace with turn_index_back=1 shows a preview of each result; run a tool again when you need the whole of one.\n");
    }
    if cut == Cut::Restart {
        out.push_str("A tool call that was running when embraOS restarted is not on record, and may have done part of its work.\n");
    }
    out.push_str("Take the work up from there if the operator's message below asks for it.\n</interrupted_turn>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn history(reply: &str) -> Vec<Message> {
        vec![Message::user("Read the three files"), Message::assistant(reply)]
    }

    fn call(name: &str, input: &str, state: &'static str) -> CutCall {
        CutCall { name: name.into(), input: input.into(), state }
    }

    #[test]
    fn a_reply_that_is_a_cuts_marker_or_ends_with_one_is_a_cut_turn() {
        assert_eq!(cut_of(&history(CALL_FAILED_MARKER)), Some(Cut::FailedCall));
        assert_eq!(
            cut_of(&history(&format!("I have read two.\n\n{CALL_FAILED_SUFFIX}"))),
            Some(Cut::FailedCall)
        );
        // The marker the turn loop wrote before, alone and after text.
        assert_eq!(cut_of(&history(STREAM_FAILED_MARKER)), Some(Cut::FailedCall));
        assert_eq!(
            cut_of(&history(&format!("I have read two.\n\n{STREAM_FAILED_MARKER}"))),
            Some(Cut::FailedCall)
        );
    }

    #[test]
    fn a_reply_written_at_boot_for_a_turn_the_process_did_not_finish_is_a_restart() {
        assert_eq!(cut_of(&history(RESTART_MARKER)), Some(Cut::Restart));
        assert_eq!(cut_of(&history(&format!("Two are read.\n\n{RESTART_SUFFIX}"))), Some(Cut::Restart));
        let block = render(Cut::Restart, &[call("file_read", "{}", "ok")], false);
        assert!(block.contains("was cut before it finished: embraOS restarted while it was running."), "{block}");
        assert!(
            block.contains("A tool call that was running when embraOS restarted is not on record"),
            "{block}"
        );
        assert!(!render(Cut::FailedCall, &[], false).contains("restarted"));
    }

    #[test]
    fn any_other_last_entry_is_no_cut() {
        assert_eq!(cut_of(&[]), None);
        assert_eq!(cut_of(&history("All three read.")), None);
        // The operator stopped it: a stop is no cut, and is said by its own note.
        assert_eq!(cut_of(&history(super::super::model_view::OPERATOR_STOP_MARKER)), None);
        assert_eq!(cut_of(&history("Done.\n\n(response interrupted by operator stop)")), None);
        // A reply that quotes the marker inside a sentence did not end with it.
        assert_eq!(cut_of(&history(&format!("The log says {CALL_FAILED_SUFFIX}"))), None);
        assert_eq!(cut_of(&history(&format!("{CALL_FAILED_MARKER} is what the history shows."))), None);
        // The last entry is the operator's: no reply to judge.
        assert_eq!(cut_of(&[Message::user("hello")]), None);
    }

    #[test]
    fn the_calls_are_the_turns_own_in_order_with_how_each_ended() {
        let since: DateTime<Utc> = "2026-11-19T10:00:00Z".parse().unwrap();
        let doc = |name: &str, at: &str, extra: serde_json::Value| {
            let mut d = json!({
                "session": "main", "turn_index": 4, "seq": 1, "tool_use_id": "t",
                "tool_name": name, "input_preview": "{\"path\": \"a.md\"}",
                "started_at": at, "elapsed_ms": 5, "is_error": false, "result_preview": "x",
            });
            for (k, v) in extra.as_object().unwrap() {
                d[k] = v.clone();
            }
            d
        };
        let docs = [
            // Another session of this name ran a turn 4 a month before.
            doc("git_log", "2026-10-19T08:00:00Z", json!({})),
            doc("file_read", "2026-11-19T10:00:01Z", json!({})),
            doc("shell", "2026-11-19T10:00:02Z", json!({"is_error": true})),
            doc("ssh_exec", "2026-11-19T10:00:03Z", json!({"is_error": true, "stopped": true})),
            // No readable start: skipped.
            doc("file_write", "soon", json!({})),
        ];
        assert_eq!(
            calls_since(&docs, since),
            [
                call("file_read", "{\"path\": \"a.md\"}", "ok"),
                call("shell", "{\"path\": \"a.md\"}", "ERR"),
                call("ssh_exec", "{\"path\": \"a.md\"}", "stopped"),
            ]
        );
    }

    #[test]
    fn an_input_is_one_line_and_cut() {
        assert_eq!(shown("{\n  \"path\": \"a.md\"\n}"), "{ \"path\": \"a.md\" }");
        let long = format!("{{\"content\": \"{}\"}}", "é".repeat(200));
        let cut = shown(&long);
        assert!(cut.ends_with('…') && cut.len() <= INPUT_SHOWN + '…'.len_utf8(), "{}", cut.len());
    }

    #[test]
    fn the_block_says_the_cut_and_lists_the_calls() {
        let block = render(
            Cut::FailedCall,
            &[call("file_read", "{\"path\": \"a.md\"}", "ok"), call("time", "", "ERR")],
            false,
        );
        assert_eq!(
            block,
            "<interrupted_turn>\n\
             Your last turn in this session was cut before it finished: a model call failed. The message it was answering is above, with what you had written of the reply. The tool results of that turn are no longer in your context.\n\
             Tool calls of that turn, in order:\n\
             - file_read {\"path\": \"a.md\"}: ok\n\
             - time: ERR\n\
             turn_trace with turn_index_back=1 shows a preview of each result; run a tool again when you need the whole of one.\n\
             Take the work up from there if the operator's message below asks for it.\n\
             </interrupted_turn>"
        );
    }

    #[test]
    fn the_block_lists_twenty_calls_and_counts_the_rest() {
        let calls: Vec<CutCall> = (0..23).map(|i| call(&format!("tool_{i}"), "", "ok")).collect();
        let block = render(Cut::FailedCall, &calls, false);
        assert!(block.contains("- tool_19: ok\n- and 3 more\n"), "{block}");
        assert!(!block.contains("tool_20"), "{block}");
        // A full window: there may be more than were read.
        assert!(render(Cut::FailedCall, &calls, true).contains("- and at least 3 more\n"));
    }

    #[test]
    fn a_turn_cut_before_any_tool_call_says_so() {
        let block = render(Cut::FailedCall, &[], false);
        assert!(block.contains("No tool call of that turn is on record.\n"), "{block}");
        assert!(!block.contains("turn_trace"), "{block}");
    }
}
