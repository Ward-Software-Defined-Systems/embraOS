//! JSON for `/ws/tools` and `/api/tools`: the brain's `ToolCallRecord`,
//! arriving through apid's opaque payload, as the tagged messages the
//! sidebar's Tools tab reads. Unlike `/ws/activity`, this wire carries the
//! call's content — the brain cut each side at 64 KiB and marked it — and
//! the shape is held by `tool_call_json_shape_is_pinned`. A field that
//! moves here moves in `embra-web-ui/src/tools.rs` with it.

use embra_common::proto::brain;
use serde::Serialize;

/// What `/ws/tools` sends. Tagged with `t`, like `/ws/chat`.
#[derive(Debug, Serialize, PartialEq, Clone)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum ToolMsg {
    /// The calls embra-web still holds, oldest first; first on every
    /// socket, empty on a fresh process.
    Backlog { calls: Vec<ToolCall> },
    /// One finished call. Boxed: the variant is many strings wide and the
    /// message is built once per call.
    Call(Box<ToolCall>),
    /// The feed to apid opened.
    Online,
    /// The feed to apid is down. Sent once per outage.
    Offline,
}

/// One tool call, live from the brain or read back from its record.
#[derive(Debug, Serialize, PartialEq, Clone, Default)]
pub struct ToolCall {
    /// The brain's per-boot call number; 0 on a record without one.
    pub seq: u64,
    /// The provider's `tool_use_id`; empty for a cron-fired call.
    pub id: String,
    pub name: String,
    pub session: String,
    /// `turn` | `cron`.
    pub origin: String,
    /// The cron job that fired the call; empty otherwise.
    pub job_id: String,
    pub started_at_ms: i64,
    /// The stored form of the start, the paging cursor for `/api/tools`.
    pub started_at_rfc: String,
    pub elapsed_ms: u32,
    pub is_error: bool,
    /// The dispatch was abandoned on an operator stop.
    pub stopped: bool,
    pub input: String,
    /// The full length; 0 when read back from the record.
    pub input_bytes: u32,
    pub result: String,
    /// The full length; 0 when read back from the record.
    pub result_bytes: u32,
    /// Records a lagging watcher lost before this one.
    pub dropped: u32,
    /// Read back from `tools.turn_trace`: 2 KiB previews, no byte totals.
    pub from_record: bool,
}

impl ToolCall {
    /// What the call costs the backlog.
    pub fn bytes(&self) -> usize {
        self.input.len() + self.result.len()
    }
}

pub fn record_to_call(r: brain::ToolCallRecord) -> ToolCall {
    ToolCall {
        seq: r.seq,
        id: r.tool_use_id,
        name: r.name,
        session: r.session,
        origin: r.origin,
        job_id: r.job_id,
        started_at_ms: r.started_at_ms,
        started_at_rfc: r.started_at_rfc,
        elapsed_ms: r.elapsed_ms,
        is_error: r.is_error,
        stopped: r.stopped,
        input: r.input_json,
        input_bytes: r.input_bytes,
        result: r.result,
        result_bytes: r.result_bytes,
        dropped: r.dropped,
        from_record: r.from_record,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn full_record() -> brain::ToolCallRecord {
        brain::ToolCallRecord {
            seq: 17,
            tool_use_id: "toolu_01AB".into(),
            name: "file_read".into(),
            session: "main".into(),
            origin: "turn".into(),
            started_at_ms: 1_792_072_931_250,
            elapsed_ms: 812,
            is_error: false,
            stopped: false,
            input_json: r#"{"path":"/embra/workspace/notes.md"}"#.into(),
            input_bytes: 36,
            result: "# Notes\nline two".into(),
            result_bytes: 16,
            dropped: 0,
            job_id: String::new(),
            from_record: false,
            started_at_rfc: "2026-10-15T14:02:11.250+00:00".into(),
        }
    }

    #[test]
    fn a_record_becomes_a_call_message_with_its_content() {
        let call = record_to_call(full_record());
        assert_eq!(call.id, "toolu_01AB");
        assert_eq!(call.input, r#"{"path":"/embra/workspace/notes.md"}"#);
        assert_eq!(call.result, "# Notes\nline two");
        assert_eq!(call.bytes(), 36 + 16);
        assert_eq!(call.started_at_rfc, "2026-10-15T14:02:11.250+00:00");
        assert!(!call.from_record);
    }

    /// The wire shape the Tools tab parses. A field that moves here moves
    /// in `embra-web-ui/src/tools.rs` with it.
    #[test]
    fn tool_call_json_shape_is_pinned() {
        let call = record_to_call(full_record());
        let one = serde_json::to_string(&ToolMsg::Call(Box::new(call.clone()))).unwrap();
        assert_eq!(
            one,
            r##"{"t":"call","seq":17,"id":"toolu_01AB","name":"file_read","session":"main","origin":"turn","job_id":"","started_at_ms":1792072931250,"started_at_rfc":"2026-10-15T14:02:11.250+00:00","elapsed_ms":812,"is_error":false,"stopped":false,"input":"{\"path\":\"/embra/workspace/notes.md\"}","input_bytes":36,"result":"# Notes\nline two","result_bytes":16,"dropped":0,"from_record":false}"##
        );
        let backlog = serde_json::to_string(&ToolMsg::Backlog { calls: vec![call] }).unwrap();
        assert!(backlog.starts_with(r#"{"t":"backlog","calls":[{"seq":17,"id":"toolu_01AB","#), "{backlog}");
        assert_eq!(serde_json::to_string(&ToolMsg::Online).unwrap(), r#"{"t":"online"}"#);
        assert_eq!(serde_json::to_string(&ToolMsg::Offline).unwrap(), r#"{"t":"offline"}"#);
    }
}
