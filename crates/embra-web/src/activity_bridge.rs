//! JSON for `/ws/activity`: the brain's `ActivityFrame`, arriving through
//! apid's opaque payload, as the tagged messages the top-bar strip reads.
//! Names and numbers only, like the frames themselves; the wire shape is
//! held by `activity_json_shape_is_pinned`.

use embra_common::proto::brain::{self, activity_frame};
use serde::Serialize;

use crate::chat_bridge::operating_mode;

/// What `/ws/activity` sends. Tagged with `t`, like `/ws/chat`.
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum ActivityMsg {
    /// The brain's state and totals; first on every socket, then every
    /// ten seconds.
    Snapshot(Snapshot),
    /// What happened since the previous message.
    Tick(Tick),
    /// The feed to apid is down. Sent once per outage; the next snapshot
    /// ends it.
    Offline,
}

#[derive(Debug, Serialize, PartialEq, Default)]
pub struct Snapshot {
    pub provider: String,
    pub model: String,
    /// `setup` | `learning` | `operational`.
    pub mode: String,
    pub in_turn: bool,
    pub session: String,
    pub uptime_seconds: u64,
    pub totals: Totals,
    pub collections: Vec<Collection>,
    /// WardSONDB's lifetime counters; absent until the brain sampled them.
    pub db_lifetime: Option<DbLifetime>,
    pub probe: Probe,
}

#[derive(Debug, Serialize, PartialEq, Default)]
pub struct Totals {
    pub turns: u64,
    pub model_calls: u64,
    pub model_errors: u64,
    pub text_chars: u64,
    pub reasoning_chars: u64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub db_ops: u64,
    pub retrievals: u64,
    pub embeddings: u64,
    pub notifications: u64,
    pub db: Vec<DbOps>,
}

#[derive(Debug, Serialize, PartialEq, Default)]
pub struct DbOps {
    pub collection: String,
    /// `read` | `query` | `write` | `delete`.
    pub verb: String,
    pub count: u64,
    pub total_ms: u64,
}

#[derive(Debug, Serialize, PartialEq, Default)]
pub struct Collection {
    pub name: String,
    pub docs: u64,
}

#[derive(Debug, Serialize, PartialEq, Default)]
pub struct DbLifetime {
    pub requests: u64,
    pub inserts: u64,
    pub queries: u64,
    pub deletes: u64,
}

#[derive(Debug, Serialize, PartialEq, Default)]
pub struct Probe {
    /// `up` | `down` | `unknown`.
    pub state: String,
    pub latency_ms: u64,
}

#[derive(Debug, Serialize, PartialEq, Default)]
pub struct Tick {
    pub span_ms: u32,
    pub text_chars: u32,
    pub reasoning_chars: u32,
    pub model_calls: u32,
    pub model_errors: u32,
    pub tools: Vec<Tool>,
    pub db: Vec<DbOps>,
    pub retrievals: Vec<Retrieval>,
    pub embeddings: u32,
    pub notifications: u32,
    pub turns_started: u32,
    pub turns_ended: u32,
    pub in_turn: bool,
    /// Events the brain's subscriber lost; the totals in the next snapshot
    /// are exact regardless.
    pub dropped: u32,
    /// Set when a session was attached or a turn began during the tick.
    pub session: String,
}

#[derive(Debug, Serialize, PartialEq, Default)]
pub struct Tool {
    pub name: String,
    /// `turn` | `cron`.
    pub origin: String,
    pub finished: bool,
    pub elapsed_ms: u32,
    pub is_error: bool,
}

#[derive(Debug, Serialize, PartialEq, Default)]
pub struct Retrieval {
    pub candidates: u32,
    pub results: u32,
    pub top_score: f32,
}

/// A decoded brain frame as the message the browser gets. `None` for a
/// frame without a kind, which the brain never sends.
pub fn frame_to_msg(frame: brain::ActivityFrame) -> Option<ActivityMsg> {
    Some(match frame.frame? {
        activity_frame::Frame::Snapshot(s) => ActivityMsg::Snapshot(Snapshot {
            provider: s.provider,
            model: s.model,
            mode: operating_mode(s.mode).to_string(),
            in_turn: s.in_turn,
            session: s.active_session,
            uptime_seconds: s.uptime_seconds,
            totals: s.totals.map(totals).unwrap_or_default(),
            collections: s
                .collections
                .into_iter()
                .map(|c| Collection { name: c.name, docs: c.doc_count })
                .collect(),
            db_lifetime: s.db.map(|d| DbLifetime {
                requests: d.requests,
                inserts: d.inserts,
                queries: d.queries,
                deletes: d.deletes,
            }),
            probe: Probe { state: s.probe_state, latency_ms: s.probe_latency_ms },
        }),
        activity_frame::Frame::Tick(t) => ActivityMsg::Tick(Tick {
            span_ms: t.span_ms,
            text_chars: t.text_chars,
            reasoning_chars: t.reasoning_chars,
            model_calls: t.model_calls,
            model_errors: t.model_errors,
            tools: t
                .tools
                .into_iter()
                .map(|e| Tool {
                    name: e.name,
                    origin: e.origin,
                    finished: e.finished,
                    elapsed_ms: e.elapsed_ms,
                    is_error: e.is_error,
                })
                .collect(),
            db: t.db_ops.into_iter().map(db_ops).collect(),
            retrievals: t
                .retrievals
                .into_iter()
                .map(|r| Retrieval { candidates: r.candidates, results: r.results, top_score: r.top_score })
                .collect(),
            embeddings: t.embeddings,
            notifications: t.notifications,
            turns_started: t.turns_started,
            turns_ended: t.turns_ended,
            in_turn: t.in_turn,
            dropped: t.dropped,
            session: t.session,
        }),
    })
}

fn totals(t: brain::ActivityTotals) -> Totals {
    Totals {
        turns: t.turns,
        model_calls: t.model_calls,
        model_errors: t.model_errors,
        text_chars: t.text_chars,
        reasoning_chars: t.reasoning_chars,
        tool_calls: t.tool_calls,
        tool_errors: t.tool_errors,
        db_ops: t.db_ops,
        retrievals: t.retrievals,
        embeddings: t.embeddings,
        notifications: t.notifications,
        db: t.db_by_collection.into_iter().map(db_ops).collect(),
    }
}

fn db_ops(d: brain::DbOpCount) -> DbOps {
    DbOps { collection: d.collection, verb: d.verb, count: d.count, total_ms: d.total_ms }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_snapshot() -> brain::ActivityFrame {
        brain::ActivityFrame {
            frame: Some(activity_frame::Frame::Snapshot(brain::ActivitySnapshot {
                provider: "anthropic".into(),
                model: "claude-opus-5".into(),
                mode: brain::OperatingMode::Operational as i32,
                in_turn: true,
                active_session: "main".into(),
                uptime_seconds: 42,
                totals: Some(brain::ActivityTotals {
                    turns: 1,
                    model_calls: 2,
                    model_errors: 0,
                    text_chars: 300,
                    reasoning_chars: 40,
                    tool_calls: 3,
                    tool_errors: 1,
                    db_ops: 9,
                    retrievals: 1,
                    embeddings: 2,
                    notifications: 1,
                    db_by_collection: vec![brain::DbOpCount {
                        collection: "memory.entries".into(),
                        verb: "write".into(),
                        count: 4,
                        total_ms: 12,
                    }],
                }),
                collections: vec![brain::CollectionCount { name: "memory.entries".into(), doc_count: 1284 }],
                db: Some(brain::DbLifetime { requests: 100, inserts: 10, queries: 50, deletes: 1 }),
                probe_state: "up".into(),
                probe_latency_ms: 181,
            })),
        }
    }

    fn full_tick() -> brain::ActivityFrame {
        brain::ActivityFrame {
            frame: Some(activity_frame::Frame::Tick(brain::ActivityTick {
                span_ms: 200,
                text_chars: 57,
                reasoning_chars: 0,
                model_calls: 1,
                model_errors: 0,
                tools: vec![brain::ToolEvent {
                    name: "remember".into(),
                    origin: "turn".into(),
                    finished: true,
                    elapsed_ms: 812,
                    is_error: false,
                }],
                db_ops: vec![brain::DbOpCount {
                    collection: "memory.edges".into(),
                    verb: "query".into(),
                    count: 12,
                    total_ms: 34,
                }],
                retrievals: vec![brain::RetrievalEvent { candidates: 112, results: 5, top_score: 0.5 }],
                embeddings: 1,
                notifications: 0,
                turns_started: 1,
                turns_ended: 0,
                in_turn: true,
                dropped: 0,
                session: "main".into(),
            })),
        }
    }

    #[test]
    fn a_snapshot_frame_becomes_a_snapshot_message_with_a_lowercase_mode() {
        let Some(ActivityMsg::Snapshot(s)) = frame_to_msg(full_snapshot()) else {
            panic!("a snapshot frame is a snapshot message");
        };
        assert_eq!(s.mode, "operational");
        assert_eq!(s.session, "main");
        assert_eq!(s.totals.db[0].collection, "memory.entries");
        assert_eq!(s.collections[0].docs, 1284);
        assert_eq!(s.db_lifetime.as_ref().map(|d| d.queries), Some(50));
        assert!(frame_to_msg(brain::ActivityFrame { frame: None }).is_none());
    }

    /// The wire shape the strip parses. A field that moves here moves in
    /// `embra-web-ui/src/activity.rs` with it.
    #[test]
    fn activity_json_shape_is_pinned() {
        let snapshot = serde_json::to_string(&frame_to_msg(full_snapshot()).unwrap()).unwrap();
        let tick = serde_json::to_string(&frame_to_msg(full_tick()).unwrap()).unwrap();
        let offline = serde_json::to_string(&ActivityMsg::Offline).unwrap();
        assert_eq!(
            snapshot,
            r#"{"t":"snapshot","provider":"anthropic","model":"claude-opus-5","mode":"operational","in_turn":true,"session":"main","uptime_seconds":42,"totals":{"turns":1,"model_calls":2,"model_errors":0,"text_chars":300,"reasoning_chars":40,"tool_calls":3,"tool_errors":1,"db_ops":9,"retrievals":1,"embeddings":2,"notifications":1,"db":[{"collection":"memory.entries","verb":"write","count":4,"total_ms":12}]},"collections":[{"name":"memory.entries","docs":1284}],"db_lifetime":{"requests":100,"inserts":10,"queries":50,"deletes":1},"probe":{"state":"up","latency_ms":181}}"#
        );
        assert_eq!(
            tick,
            r#"{"t":"tick","span_ms":200,"text_chars":57,"reasoning_chars":0,"model_calls":1,"model_errors":0,"tools":[{"name":"remember","origin":"turn","finished":true,"elapsed_ms":812,"is_error":false}],"db":[{"collection":"memory.edges","verb":"query","count":12,"total_ms":34}],"retrievals":[{"candidates":112,"results":5,"top_score":0.5}],"embeddings":1,"notifications":0,"turns_started":1,"turns_ended":0,"in_turn":true,"dropped":0,"session":"main"}"#
        );
        assert_eq!(offline, r#"{"t":"offline"}"#);
    }
}
