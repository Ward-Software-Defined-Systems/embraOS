//! The activity bus: what the brain is doing, as names and numbers.
//!
//! Every busy path emits one [`Event`] where it does its work: the turn
//! mark, the provider stream, the tool dispatcher, the database client,
//! retrieval, the embedding model, the notification delivery and the
//! session attach. [`emit`] bumps the process-wide totals and hands the
//! event to whoever watches. The `WatchActivity` RPC (`grpc_service.rs`)
//! folds events into frames for the web console's top-bar strip, which
//! reads them through apid and embra-web.
//!
//! Nothing here carries content: no message text, no tool input or result,
//! no memory content, no reasoning. The field set of a frame is pinned by
//! `activity_frames_carry_names_and_numbers_only`.
//!
//! The bus is a module static, like `provider::health::LATEST`, so an emit
//! site needs no handle. `emit` never awaits and never logs: it runs inside
//! the provider stream and the database client.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use embra_common::proto::brain::{
    ActivitySnapshot, ActivityTick, ActivityTotals, CollectionCount, DbLifetime, DbOpCount,
    OperatingMode, RetrievalEvent, ToolEvent,
};
use tokio::sync::{Notify, broadcast, watch};

use crate::db::WardsonDbClient;

/// Events a slow subscriber may fall behind by before it loses some. A loss
/// is reported in the next tick's `dropped`; the totals stay exact.
pub const CHANNEL_CAPACITY: usize = 1024;
/// How often a subscriber gets a tick while something happens.
pub const TICK_INTERVAL: Duration = Duration::from_millis(200);
/// How often a subscriber gets a fresh snapshot, and how often the database
/// is sampled while someone watches.
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(10);
/// How long the first snapshot of a subscription waits for a database
/// sample before it goes out without one.
pub const FIRST_SAMPLE_WAIT: Duration = Duration::from_millis(1500);
/// The ceiling on each request the sampler makes.
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(2);
/// The collections a snapshot counts: the knowledge graph, the identity
/// graph and the Guardian tools. Sessions and the rest stay out.
pub const SNAPSHOT_COLLECTION_PREFIXES: &[&str] = &["memory.", "identity.graph", "guardian.tools"];

/// What a database request did. Admin and health calls are not counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DbVerb {
    Read,
    Query,
    Write,
    Delete,
}

impl DbVerb {
    pub fn as_str(self) -> &'static str {
        match self {
            DbVerb::Read => "read",
            DbVerb::Query => "query",
            DbVerb::Write => "write",
            DbVerb::Delete => "delete",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbeddingKind {
    Query,
    Document,
}

/// Who asked for a tool: a turn of the model, or the cron loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolOrigin {
    Turn,
    Cron,
}

impl ToolOrigin {
    /// The cron loop dispatches under the session name `cron`
    /// (`tools/cron.rs`); every other dispatch belongs to a turn.
    pub fn from_session(session: &str) -> Self {
        if session == "cron" { ToolOrigin::Cron } else { ToolOrigin::Turn }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ToolOrigin::Turn => "turn",
            ToolOrigin::Cron => "cron",
        }
    }
}

/// One thing the brain did. Names and numbers only.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    TurnStarted { session: String },
    TurnEnded,
    ModelCallStarted,
    ModelCallEnded { error: bool },
    TextDelta { chars: u32 },
    ReasoningDelta { chars: u32 },
    ToolStarted { name: String, origin: ToolOrigin },
    ToolFinished { name: String, origin: ToolOrigin, elapsed_ms: u64, is_error: bool },
    DbOp { collection: String, verb: DbVerb, duration_ms: u32 },
    Retrieval { candidates: u32, results: u32, top_score: f32 },
    Embedding { kind: EmbeddingKind },
    Notification { priority: String },
    SessionAttached { name: String },
}

/// The totals since the brain started. Atomics, so an emit never blocks;
/// the per-collection map takes a lock for the length of one map update.
#[derive(Default)]
struct Totals {
    turns: AtomicU64,
    model_calls: AtomicU64,
    model_errors: AtomicU64,
    text_chars: AtomicU64,
    reasoning_chars: AtomicU64,
    tool_calls: AtomicU64,
    tool_errors: AtomicU64,
    db_ops: AtomicU64,
    retrievals: AtomicU64,
    embeddings: AtomicU64,
    notifications: AtomicU64,
    /// `(collection, verb)` to `(count, total milliseconds)`.
    db_by_collection: Mutex<BTreeMap<(String, DbVerb), (u64, u64)>>,
}

impl Totals {
    fn bump(&self, ev: &Event) {
        let add = |counter: &AtomicU64, n: u64| {
            counter.fetch_add(n, Ordering::Relaxed);
        };
        match ev {
            Event::TurnStarted { .. } => add(&self.turns, 1),
            Event::TurnEnded | Event::ModelCallStarted | Event::SessionAttached { .. } => {}
            Event::ModelCallEnded { error } => {
                add(&self.model_calls, 1);
                if *error {
                    add(&self.model_errors, 1);
                }
            }
            Event::TextDelta { chars } => add(&self.text_chars, u64::from(*chars)),
            Event::ReasoningDelta { chars } => add(&self.reasoning_chars, u64::from(*chars)),
            Event::ToolStarted { .. } => add(&self.tool_calls, 1),
            Event::ToolFinished { is_error, .. } => {
                if *is_error {
                    add(&self.tool_errors, 1);
                }
            }
            Event::DbOp { collection, verb, duration_ms } => {
                add(&self.db_ops, 1);
                if let Ok(mut map) = self.db_by_collection.lock() {
                    let entry = map.entry((collection.clone(), *verb)).or_insert((0, 0));
                    entry.0 += 1;
                    entry.1 += u64::from(*duration_ms);
                }
            }
            Event::Retrieval { .. } => add(&self.retrievals, 1),
            Event::Embedding { .. } => add(&self.embeddings, 1),
            Event::Notification { .. } => add(&self.notifications, 1),
        }
    }

    fn snapshot(&self) -> ActivityTotals {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let db_by_collection = self
            .db_by_collection
            .lock()
            .map(|map| {
                map.iter()
                    .map(|((collection, verb), (count, total_ms))| DbOpCount {
                        collection: collection.clone(),
                        verb: verb.as_str().to_string(),
                        count: *count,
                        total_ms: *total_ms,
                    })
                    .collect()
            })
            .unwrap_or_default();
        ActivityTotals {
            turns: get(&self.turns),
            model_calls: get(&self.model_calls),
            model_errors: get(&self.model_errors),
            text_chars: get(&self.text_chars),
            reasoning_chars: get(&self.reasoning_chars),
            tool_calls: get(&self.tool_calls),
            tool_errors: get(&self.tool_errors),
            db_ops: get(&self.db_ops),
            retrievals: get(&self.retrievals),
            embeddings: get(&self.embeddings),
            notifications: get(&self.notifications),
            db_by_collection,
        }
    }
}

struct Bus {
    tx: broadcast::Sender<Event>,
    totals: Totals,
}

static BUS: OnceLock<Bus> = OnceLock::new();

fn bus() -> &'static Bus {
    BUS.get_or_init(|| {
        let (tx, _idle) = broadcast::channel(CHANNEL_CAPACITY);
        Bus { tx, totals: Totals::default() }
    })
}

/// Count the event and hand it to every subscriber. Never awaits, never
/// logs.
pub fn emit(ev: Event) {
    let bus = bus();
    bus.totals.bump(&ev);
    // Fan-out to whoever watches. With nobody subscribed the event has
    // nowhere to go, and the totals already hold it. This is not a provider
    // pump, which must stop on a dropped receiver: here a send with no
    // receiver is the normal case on an instance nobody watches.
    if bus.tx.receiver_count() > 0 {
        let _ = bus.tx.send(ev);
    }
}

pub fn subscribe() -> broadcast::Receiver<Event> {
    bus().tx.subscribe()
}

pub fn receiver_count() -> usize {
    bus().tx.receiver_count()
}

/// The totals since the brain started.
pub fn totals() -> ActivityTotals {
    bus().totals.snapshot()
}

/// One database request, from the client's methods.
pub fn db_op(collection: &str, verb: DbVerb, started: Instant) {
    emit(Event::DbOp {
        collection: collection.to_string(),
        verb,
        duration_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    });
}

/// A tool dispatch, from `ToolStarted` to `ToolFinished`. Dropped without
/// [`ToolSpan::finish`], it reports an error: the turn loop abandons a
/// dispatch future on an operator stop, and the strip must not show that
/// tool running for good.
pub struct ToolSpan {
    name: String,
    origin: ToolOrigin,
    started: Instant,
    finished: bool,
}

impl ToolSpan {
    pub fn start(name: &str, origin: ToolOrigin) -> Self {
        emit(Event::ToolStarted { name: name.to_string(), origin });
        Self { name: name.to_string(), origin, started: Instant::now(), finished: false }
    }

    pub fn finish(mut self, is_error: bool) {
        self.finished = true;
        self.report(is_error);
    }

    fn report(&mut self, is_error: bool) {
        emit(Event::ToolFinished {
            name: std::mem::take(&mut self.name),
            origin: self.origin,
            elapsed_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            is_error,
        });
    }
}

impl Drop for ToolSpan {
    fn drop(&mut self) {
        if !self.finished {
            self.finished = true;
            self.report(true);
        }
    }
}

/// Folds events into the next tick. One per subscriber.
#[derive(Default)]
pub(crate) struct TickBuilder {
    touched: bool,
    text_chars: u32,
    reasoning_chars: u32,
    model_calls: u32,
    model_errors: u32,
    tools: Vec<ToolEvent>,
    db: BTreeMap<(String, DbVerb), (u32, u32)>,
    retrievals: Vec<RetrievalEvent>,
    embeddings: u32,
    notifications: u32,
    turns_started: u32,
    turns_ended: u32,
    session: String,
    dropped: u32,
}

impl TickBuilder {
    pub(crate) fn fold(&mut self, ev: Event) {
        self.touched = true;
        match ev {
            Event::TurnStarted { session } => {
                self.turns_started = self.turns_started.saturating_add(1);
                self.session = session;
            }
            Event::TurnEnded => self.turns_ended = self.turns_ended.saturating_add(1),
            Event::ModelCallStarted => {}
            Event::ModelCallEnded { error } => {
                self.model_calls = self.model_calls.saturating_add(1);
                if error {
                    self.model_errors = self.model_errors.saturating_add(1);
                }
            }
            Event::TextDelta { chars } => self.text_chars = self.text_chars.saturating_add(chars),
            Event::ReasoningDelta { chars } => {
                self.reasoning_chars = self.reasoning_chars.saturating_add(chars);
            }
            Event::ToolStarted { name, origin } => self.tools.push(ToolEvent {
                name,
                origin: origin.as_str().to_string(),
                finished: false,
                elapsed_ms: 0,
                is_error: false,
            }),
            Event::ToolFinished { name, origin, elapsed_ms, is_error } => self.tools.push(ToolEvent {
                name,
                origin: origin.as_str().to_string(),
                finished: true,
                elapsed_ms: u32::try_from(elapsed_ms).unwrap_or(u32::MAX),
                is_error,
            }),
            Event::DbOp { collection, verb, duration_ms } => {
                let entry = self.db.entry((collection, verb)).or_insert((0, 0));
                entry.0 = entry.0.saturating_add(1);
                entry.1 = entry.1.saturating_add(duration_ms);
            }
            Event::Retrieval { candidates, results, top_score } => {
                self.retrievals.push(RetrievalEvent { candidates, results, top_score });
            }
            Event::Embedding { .. } => self.embeddings = self.embeddings.saturating_add(1),
            Event::Notification { .. } => self.notifications = self.notifications.saturating_add(1),
            Event::SessionAttached { name } => self.session = name,
        }
    }

    /// The subscriber fell `n` events behind and lost them.
    pub(crate) fn lagged(&mut self, n: u64) {
        self.touched = true;
        self.dropped = self.dropped.saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.touched
    }

    /// The tick so far, leaving the builder empty.
    pub(crate) fn take(&mut self, span: Duration, in_turn: bool) -> ActivityTick {
        let b = std::mem::take(self);
        ActivityTick {
            span_ms: u32::try_from(span.as_millis()).unwrap_or(u32::MAX),
            text_chars: b.text_chars,
            reasoning_chars: b.reasoning_chars,
            model_calls: b.model_calls,
            model_errors: b.model_errors,
            tools: b.tools,
            db_ops: b
                .db
                .into_iter()
                .map(|((collection, verb), (count, total_ms))| DbOpCount {
                    collection,
                    verb: verb.as_str().to_string(),
                    count: u64::from(count),
                    total_ms: u64::from(total_ms),
                })
                .collect(),
            retrievals: b.retrievals,
            embeddings: b.embeddings,
            notifications: b.notifications,
            turns_started: b.turns_started,
            turns_ended: b.turns_ended,
            in_turn,
            dropped: b.dropped,
            session: b.session,
        }
    }
}

/// The mode a snapshot reports. The onboarding watch is seeded `Setup` on
/// every boot and only the first-run loops advance it (`grpc_service.rs`),
/// so on a sealed boot it still says `Setup`: the seal decides, as `GetMode`
/// does. Before the seal the watch is the truth: the wizard, then learning.
pub(crate) fn mode_for(stage: i32, sealed: bool) -> OperatingMode {
    if sealed {
        return OperatingMode::Operational;
    }
    match OperatingMode::try_from(stage) {
        Ok(OperatingMode::Unspecified) | Err(_) => OperatingMode::Setup,
        Ok(mode) => mode,
    }
}

/// What the sampler read from the database.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct DbSample {
    pub collections: Vec<CollectionCount>,
    pub lifetime: DbLifetime,
}

/// Set once the soul has been seen sealed; the sampler stops asking then.
static SEALED: AtomicBool = AtomicBool::new(false);

struct Sampler {
    rx: watch::Receiver<Option<DbSample>>,
    wake: Arc<Notify>,
}

static SAMPLER: OnceLock<Sampler> = OnceLock::new();

/// The database sample every snapshot reads. One task samples: every
/// `SNAPSHOT_INTERVAL` while someone watches, and right away when a watcher
/// arrives. GETs only, no query body; the seal is read until seen once.
pub(crate) fn db_sample(db: &WardsonDbClient) -> watch::Receiver<Option<DbSample>> {
    let sampler = SAMPLER.get_or_init(|| {
        let (tx, rx) = watch::channel(None);
        let wake = Arc::new(Notify::new());
        let db = db.clone();
        let waker = wake.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = waker.notified() => {}
                    _ = tokio::time::sleep(SNAPSHOT_INTERVAL) => {}
                }
                if receiver_count() == 0 {
                    continue;
                }
                if let Some(sample) = take_sample(&db).await {
                    tx.send_replace(Some(sample));
                }
            }
        });
        Sampler { rx, wake }
    });
    sampler.wake.notify_one();
    sampler.rx.clone()
}

async fn take_sample(db: &WardsonDbClient) -> Option<DbSample> {
    let collections = tokio::time::timeout(SAMPLE_TIMEOUT, db.list_collections_with_counts())
        .await
        .ok()?
        .ok()?;
    let stats = tokio::time::timeout(SAMPLE_TIMEOUT, db.stats()).await.ok()?.ok()?;
    if !SEALED.load(Ordering::Relaxed)
        && let Ok(Ok(true)) =
            tokio::time::timeout(SAMPLE_TIMEOUT, db.collection_exists("soul.invariant")).await
    {
        SEALED.store(true, Ordering::Relaxed);
    }
    Some(DbSample { collections: counted_collections(collections), lifetime: lifetime_from(&stats) })
}

/// The collections a snapshot reports, by name.
pub(crate) fn counted_collections(all: Vec<(String, u64)>) -> Vec<CollectionCount> {
    let mut counted: Vec<CollectionCount> = all
        .into_iter()
        .filter(|(name, _)| SNAPSHOT_COLLECTION_PREFIXES.iter().any(|p| name.starts_with(p)))
        .map(|(name, doc_count)| CollectionCount { name, doc_count })
        .collect();
    counted.sort_by(|a, b| a.name.cmp(&b.name));
    counted
}

/// WardSONDB's lifetime counters out of its `/_stats` document.
pub(crate) fn lifetime_from(stats: &serde_json::Value) -> DbLifetime {
    let lifetime = &stats["lifetime"];
    let read = |key: &str| lifetime[key].as_u64().unwrap_or(0);
    DbLifetime {
        requests: read("requests"),
        inserts: read("inserts"),
        queries: read("queries"),
        deletes: read("deletes"),
    }
}

/// What a snapshot takes from the service; the handler gathers it.
pub(crate) struct SnapshotInput<'a> {
    pub stage: i32,
    pub in_turn: bool,
    pub active_session: Option<String>,
    pub uptime_seconds: u64,
    pub sample: Option<&'a DbSample>,
}

/// The snapshot frame: the provider from the last probe, the mode, the turn
/// mark, the totals and the last database sample.
pub(crate) fn snapshot(input: SnapshotInput<'_>) -> ActivitySnapshot {
    let probe = crate::provider::health::latest();
    let (provider, model, probe_state, probe_latency_ms) = match &probe {
        Some(p) => (p.kind.clone(), p.model.clone(), p.state().to_string(), p.latency_ms.unwrap_or(0)),
        None => (String::new(), String::new(), "unknown".to_string(), 0),
    };
    ActivitySnapshot {
        provider,
        model,
        mode: mode_for(input.stage, SEALED.load(Ordering::Relaxed)) as i32,
        in_turn: input.in_turn,
        active_session: input.active_session.unwrap_or_default(),
        uptime_seconds: input.uptime_seconds,
        totals: Some(totals()),
        collections: input.sample.map(|s| s.collections.clone()).unwrap_or_default(),
        db: input.sample.map(|s| s.lifetime),
        probe_state,
        probe_latency_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Events from other tests of this process reach every subscriber;
    /// a test reads what it emitted by a marker of its own.
    fn drain(rx: &mut broadcast::Receiver<Event>) -> Vec<Event> {
        let mut got = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            got.push(ev);
        }
        got
    }

    #[test]
    fn an_emit_with_no_receiver_bumps_totals_and_blocks_nothing() {
        let before = totals().reasoning_chars;
        emit(Event::ReasoningDelta { chars: 7 });
        assert_eq!(totals().reasoning_chars, before + 7);
    }

    #[test]
    fn a_tick_folds_deltas_and_db_ops_and_keeps_tool_events_in_order() {
        let mut b = TickBuilder::default();
        assert!(b.is_empty());
        b.fold(Event::TurnStarted { session: "main".into() });
        b.fold(Event::TextDelta { chars: 10 });
        b.fold(Event::TextDelta { chars: 5 });
        b.fold(Event::ToolStarted { name: "remember".into(), origin: ToolOrigin::Turn });
        b.fold(Event::DbOp { collection: "memory.entries".into(), verb: DbVerb::Write, duration_ms: 3 });
        b.fold(Event::DbOp { collection: "memory.entries".into(), verb: DbVerb::Write, duration_ms: 4 });
        b.fold(Event::DbOp { collection: "memory.edges".into(), verb: DbVerb::Query, duration_ms: 1 });
        b.fold(Event::ToolFinished {
            name: "remember".into(),
            origin: ToolOrigin::Turn,
            elapsed_ms: 812,
            is_error: false,
        });
        b.fold(Event::ModelCallEnded { error: false });
        b.fold(Event::TurnEnded);
        assert!(!b.is_empty());

        let tick = b.take(Duration::from_millis(200), false);
        assert!(b.is_empty(), "take leaves the builder empty");
        assert_eq!(tick.span_ms, 200);
        assert_eq!(tick.text_chars, 15);
        assert_eq!(tick.model_calls, 1);
        assert_eq!((tick.turns_started, tick.turns_ended), (1, 1));
        assert_eq!(tick.session, "main");
        let tools: Vec<(&str, bool, u32)> =
            tick.tools.iter().map(|t| (t.name.as_str(), t.finished, t.elapsed_ms)).collect();
        assert_eq!(tools, vec![("remember", false, 0), ("remember", true, 812)]);
        let db: Vec<(&str, &str, u64, u64)> = tick
            .db_ops
            .iter()
            .map(|d| (d.collection.as_str(), d.verb.as_str(), d.count, d.total_ms))
            .collect();
        assert_eq!(db, vec![("memory.edges", "query", 1, 1), ("memory.entries", "write", 2, 7)]);
    }

    #[test]
    fn a_lagged_receiver_reports_the_dropped_count_in_the_next_tick() {
        let mut b = TickBuilder::default();
        b.lagged(3);
        assert!(!b.is_empty(), "a loss is worth a tick of its own");
        let tick = b.take(Duration::from_millis(200), true);
        assert_eq!(tick.dropped, 3);
        assert!(tick.in_turn);
    }

    #[test]
    fn an_abandoned_tool_span_reports_an_error_on_drop() {
        let mut rx = subscribe();
        let marker = "activity-span-test";
        let span = ToolSpan::start(marker, ToolOrigin::Cron);
        drop(span);
        let finished = ToolSpan::start(marker, ToolOrigin::Turn);
        finished.finish(false);
        let mine: Vec<Event> = drain(&mut rx)
            .into_iter()
            .filter(|e| matches!(e, Event::ToolStarted { name, .. } | Event::ToolFinished { name, .. } if name == marker))
            .collect();
        assert_eq!(mine.len(), 4, "{mine:?}");
        assert!(matches!(mine[0], Event::ToolStarted { origin: ToolOrigin::Cron, .. }));
        assert!(matches!(mine[1], Event::ToolFinished { is_error: true, origin: ToolOrigin::Cron, .. }));
        assert!(matches!(mine[2], Event::ToolStarted { origin: ToolOrigin::Turn, .. }));
        assert!(matches!(mine[3], Event::ToolFinished { is_error: false, origin: ToolOrigin::Turn, .. }));
    }

    #[test]
    fn the_mode_follows_the_seal_when_the_watch_still_says_setup() {
        assert_eq!(mode_for(OperatingMode::Setup as i32, true), OperatingMode::Operational);
        assert_eq!(mode_for(OperatingMode::Setup as i32, false), OperatingMode::Setup);
        assert_eq!(mode_for(OperatingMode::Learning as i32, false), OperatingMode::Learning);
        assert_eq!(mode_for(OperatingMode::Learning as i32, true), OperatingMode::Operational);
        assert_eq!(mode_for(99, false), OperatingMode::Setup);
    }

    #[test]
    fn a_snapshot_without_a_sample_carries_totals_and_no_collections() {
        let snap = snapshot(SnapshotInput {
            stage: OperatingMode::Setup as i32,
            in_turn: true,
            active_session: Some("main".into()),
            uptime_seconds: 5,
            sample: None,
        });
        assert!(snap.totals.is_some());
        assert!(snap.collections.is_empty());
        assert!(snap.db.is_none());
        assert!(snap.in_turn);
        assert_eq!(snap.active_session, "main");
        assert_eq!(snap.uptime_seconds, 5);
    }

    #[test]
    fn the_sample_keeps_the_counted_collections_in_name_order() {
        let counted = counted_collections(vec![
            ("sessions.main.history".into(), 9),
            ("memory.entries".into(), 3),
            ("identity.graph".into(), 40),
            ("guardian.tools".into(), 2),
            ("memory.edges".into(), 7),
        ]);
        let names: Vec<&str> = counted.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["guardian.tools", "identity.graph", "memory.edges", "memory.entries"]);
        let lifetime = lifetime_from(&serde_json::json!({
            "lifetime": {"requests": 10, "inserts": 2, "queries": 5, "deletes": 1}
        }));
        assert_eq!((lifetime.requests, lifetime.inserts, lifetime.queries, lifetime.deletes), (10, 2, 5, 1));
        assert_eq!(lifetime_from(&serde_json::json!({"error": "unavailable"})).requests, 0);
    }

    /// The field names of a Debug rendering: every `ident:` token.
    fn field_names(debug: &str) -> BTreeSet<String> {
        debug
            .split(|c: char| c.is_whitespace() || c == '{' || c == '}' || c == '[' || c == ']' || c == ',')
            .filter_map(|tok| tok.strip_suffix(':'))
            .filter(|name| name.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
            .map(str::to_string)
            .collect()
    }

    /// The wire carries names and numbers. The field set is pinned here so a
    /// field that could carry content cannot be added without this test
    /// moving with it.
    #[test]
    fn activity_frames_carry_names_and_numbers_only() {
        let mut b = TickBuilder::default();
        b.fold(Event::TurnStarted { session: "main".into() });
        b.fold(Event::TurnEnded);
        b.fold(Event::ModelCallStarted);
        b.fold(Event::ModelCallEnded { error: true });
        b.fold(Event::TextDelta { chars: 1 });
        b.fold(Event::ReasoningDelta { chars: 1 });
        b.fold(Event::ToolStarted { name: "remember".into(), origin: ToolOrigin::Turn });
        b.fold(Event::ToolFinished { name: "remember".into(), origin: ToolOrigin::Turn, elapsed_ms: 1, is_error: false });
        b.fold(Event::DbOp { collection: "memory.entries".into(), verb: DbVerb::Write, duration_ms: 1 });
        b.fold(Event::Retrieval { candidates: 2, results: 1, top_score: 0.5 });
        b.fold(Event::Embedding { kind: EmbeddingKind::Query });
        b.fold(Event::Notification { priority: "NOTICE".into() });
        b.fold(Event::SessionAttached { name: "main".into() });
        b.lagged(1);
        let tick = b.take(Duration::from_millis(200), true);
        let sample = DbSample {
            collections: vec![CollectionCount { name: "memory.entries".into(), doc_count: 1 }],
            lifetime: DbLifetime { requests: 1, inserts: 1, queries: 1, deletes: 1 },
        };
        let snap = snapshot(SnapshotInput {
            stage: OperatingMode::Operational as i32,
            in_turn: true,
            active_session: Some("main".into()),
            uptime_seconds: 1,
            sample: Some(&sample),
        });
        emit(Event::DbOp { collection: "memory.entries".into(), verb: DbVerb::Write, duration_ms: 1 });
        let snap = ActivitySnapshot { totals: Some(totals()), ..snap };

        let rendered = format!("{tick:?} {snap:?}");
        let names = field_names(&rendered);
        for forbidden in ["content", "text", "input", "input_json", "result", "args", "message", "full_response"] {
            assert!(!names.contains(forbidden), "a frame carries `{forbidden}`");
        }
        let expected: BTreeSet<String> = [
            "active_session", "candidates", "collection", "collections", "count", "db", "db_by_collection",
            "db_ops", "deletes", "doc_count", "dropped", "elapsed_ms", "embeddings", "finished", "in_turn",
            "inserts", "is_error", "mode", "model", "model_calls", "model_errors", "name", "notifications",
            "origin", "probe_latency_ms", "probe_state", "provider", "queries", "reasoning_chars", "requests",
            "results", "retrievals", "session", "span_ms", "text_chars", "tool_calls", "tool_errors", "tools",
            "top_score", "total_ms", "totals", "turns", "turns_ended", "turns_started", "uptime_seconds", "verb",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        assert_eq!(names, expected, "the frame's field set moved; re-pin it on purpose");
    }
}
