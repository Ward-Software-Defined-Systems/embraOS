//! The status tools: `system_status`, `system_logs`, `uptime_report` and
//! `changelog`, with the status structs the gRPC surface serializes.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;

use super::sessions;
use crate::db::{WardsonDbClient, MEMORY_FETCH_WINDOW};
use crate::tools::registry::DispatchContext;

// ── Startup Time ──

static START_TIME: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

pub fn init_start_time() {
    START_TIME.get_or_init(std::time::Instant::now);
}

/// Seconds since this embra-brain process started. Not the same as session
/// age — sessions persist across process restarts, whereas this counter resets
/// on every launch. Used by uptime_report and SystemStatus.uptime_seconds.
fn process_uptime_secs() -> u64 {
    START_TIME.get().map(|t| t.elapsed().as_secs()).unwrap_or(0)
}

// ── Tool Dispatch ──
//
// Native tool-use dispatch lives in `tools/registry.rs` — the legacy
// `name args` string parser and match-block dispatcher were removed
// in Stage 3 of the NATIVE-TOOLS-01 migration. Each tool now declares a
// typed args struct annotated with `#[embra_tool(name, description)]`, and
// `registry::dispatch(name, input, ctx)` is the single entry point.

// ── Existing Tools ──

/// WardSONDB lifetime counters. All four are wardsondb-scoped (document
/// inserts/queries/deletes the DB itself routed) and explicitly NOT
/// global OS counters — filesystem ops via `file_delete` etc. don't tick
/// them. The nested placement under `wardsondb.lifetime` makes that scope
/// honest in the rendered JSON.
#[derive(Debug, Serialize)]
pub struct WardsondbLifetime {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inserts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queries: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletes: Option<u64>,
}

/// Per-collection search-window parity (FIX-6). `count` is the authoritative
/// server-side document count (`count_only`); `saturated` means the
/// collection has outgrown the tool fetch window and windowed search is no
/// longer covering every document.
#[derive(Debug, Serialize)]
pub struct MemoryCollectionStatus {
    pub name: String,
    /// None when the count query failed (serializes as null).
    pub count: Option<u64>,
    pub window: usize,
    pub saturated: bool,
}

#[derive(Debug, Serialize)]
pub struct WardsondbSection {
    pub healthy: bool,
    pub collections: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_poisoned: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifetime: Option<WardsondbLifetime>,
    pub memory_collections: Vec<MemoryCollectionStatus>,
}

/// The active LLM provider's last endpoint probe (`provider::health`):
/// reachability, model presence, key state, latency, age. Absent until
/// the health loop has run once (~30 s after boot).
#[derive(Debug, Serialize)]
pub struct ProviderStatus {
    #[serde(flatten)]
    pub probe: crate::provider::health::ProviderProbe,
    /// `up` | `down` | `unknown` (unknown = nothing configured yet).
    pub state: &'static str,
    pub checked_secs_ago: u64,
}

impl From<crate::provider::health::ProviderProbe> for ProviderStatus {
    fn from(probe: crate::provider::health::ProviderProbe) -> Self {
        Self {
            state: probe.state(),
            checked_secs_ago: probe.age_secs(),
            probe,
        }
    }
}

/// Embedding failures since boot (`embedding::cache::failures`). A failure
/// never fails a write — the node is saved without a vector and backfill
/// retries it — so this block, and `/embeddings`, are where a broken model
/// or missing weights show.
#[derive(Debug, Serialize)]
pub struct EmbeddingHealth {
    pub failures_write: u64,
    pub failures_query: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<crate::embedding::cache::EmbeddingFailure>,
}

#[derive(Debug, Serialize)]
pub struct SystemStatus {
    pub version: String,
    pub uptime_seconds: u64,
    pub memory_usage_mb: Option<u64>,
    pub soul_status: String,
    /// True iff any memory collection exceeds the search window (FIX-6) —
    /// the correct replacement for the confabulated "frozen FTS indexer"
    /// monitoring idea: it watches the thing that can actually fail.
    pub search_window_saturated: bool,
    pub wardsondb: WardsondbSection,
    /// See [`ProviderStatus`]; omitted before the first probe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderStatus>,
    pub embedding: EmbeddingHealth,
}

/// Collections covered by the FIX-6 parity check — the three windowed-search
/// targets. Deliberately local to system_status.
const MEMORY_STATUS_COLLECTIONS: [&str; 3] =
    ["memory.entries", "memory.semantic", "memory.procedural"];

/// Saturation predicate: a known count strictly above the window means
/// windowed fetches are pruning; an unknown count is not reported saturated.
fn count_exceeds_window(count: Option<u64>, window: usize) -> bool {
    matches!(count, Some(c) if c as usize > window)
}

pub async fn system_status(db: &WardsonDbClient) -> SystemStatus {
    let healthy = db.health().await.unwrap_or(false);
    let collections = db.list_collections().await.unwrap_or_default();
    let soul_status = if db.collection_exists("soul.invariant").await.unwrap_or(false) {
        "sealed"
    } else {
        "unsealed"
    };

    let stats = db.stats().await.ok();
    let storage_poisoned = stats
        .as_ref()
        .and_then(|s| s.get("storage_poisoned"))
        .and_then(|v| v.as_bool());
    let lifetime_block = stats
        .as_ref()
        .and_then(|s| s.get("lifetime"))
        .map(|l| WardsondbLifetime {
            requests: l.get("requests").and_then(|v| v.as_u64()),
            inserts: l.get("inserts").and_then(|v| v.as_u64()),
            queries: l.get("queries").and_then(|v| v.as_u64()),
            deletes: l.get("deletes").and_then(|v| v.as_u64()),
        });

    // FIX-6: parity check — authoritative counts vs the tool fetch window.
    let mut memory_collections = Vec::with_capacity(MEMORY_STATUS_COLLECTIONS.len());
    for coll in MEMORY_STATUS_COLLECTIONS {
        let count = db.count(coll).await.ok();
        let saturated = count_exceeds_window(count, MEMORY_FETCH_WINDOW);
        if saturated {
            tracing::warn!(
                target: "wardsondb::window",
                collection = coll,
                count = count.unwrap_or(0),
                window = MEMORY_FETCH_WINDOW,
                "memory collection exceeds search window — SEARCH_WINDOW_SATURATED"
            );
        }
        memory_collections.push(MemoryCollectionStatus {
            name: coll.to_string(),
            count,
            window: MEMORY_FETCH_WINDOW,
            saturated,
        });
    }
    let search_window_saturated = memory_collections.iter().any(|m| m.saturated);

    let failures = crate::embedding::cache::failures().await;
    SystemStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: process_uptime_secs(),
        memory_usage_mb: get_memory_usage_mb(),
        soul_status: soul_status.into(),
        search_window_saturated,
        wardsondb: WardsondbSection {
            healthy,
            collections,
            storage_poisoned,
            lifetime: lifetime_block,
            memory_collections,
        },
        provider: crate::provider::health::latest().map(ProviderStatus::from),
        embedding: EmbeddingHealth {
            failures_write: failures.write,
            failures_query: failures.query,
            last_failure: failures.last,
        },
    }
}

#[cfg(test)]
mod status_window_tests {
    //! FIX-6 saturation predicate.
    use super::count_exceeds_window;

    #[test]
    fn saturated_only_when_count_exceeds_window() {
        assert!(!count_exceeds_window(Some(10_000), 10_000)); // at window: covered
        assert!(count_exceeds_window(Some(10_001), 10_000)); // past window: pruning
        assert!(!count_exceeds_window(Some(0), 10_000));
    }

    #[test]
    fn unknown_count_is_not_saturated() {
        assert!(!count_exceeds_window(None, 10_000));
    }
}

// ── Self-Awareness Tools ──

async fn uptime_report(db: &WardsonDbClient, session_name: &str) -> String {
    let uptime = process_uptime_secs();
    let hours = uptime / 3600;
    let mins = (uptime % 3600) / 60;

    // Session age — queried from sessions.{name}.meta.created_at if available.
    // This is independent of process uptime: sessions outlive restarts.
    let session_age_line = {
        let meta_col = format!("sessions.{}.meta", session_name);
        let created_at = db
            .query(&meta_col, &crate::sessions::history_query_body())
            .await
            .ok()
            .and_then(|docs| docs.into_iter().next())
            .and_then(|doc| doc.get("created_at").and_then(|v| v.as_str()).map(String::from));
        match created_at {
            Some(ts) => {
                if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&ts) {
                    let age = Utc::now().signed_duration_since(dt.with_timezone(&Utc));
                    let age_secs = age.num_seconds().max(0) as u64;
                    let ah = age_secs / 3600;
                    let am = (age_secs % 3600) / 60;
                    format!("Session age: {}h {}m (session '{}' since {})\n", ah, am, session_name, ts)
                } else {
                    format!("Session age: unknown (session '{}', unparseable timestamp)\n", session_name)
                }
            }
            None => format!("Session age: unknown (no meta doc for session '{}')\n", session_name),
        }
    };

    let collections = db.list_collections().await.unwrap_or_default();

    // Count sessions
    let session_count = collections
        .iter()
        .filter(|c| c.starts_with("sessions.") && c.ends_with(".meta"))
        .count();

    // Count memory entries using count_only
    let memory_count = db
        .query_with_options("memory.entries", &serde_json::json!({"count_only": true}))
        .await
        .ok()
        .and_then(|v| v.get("count").and_then(|c| c.as_u64()))
        .unwrap_or(0) as usize;

    // Count total messages across all session histories
    let mut total_messages = 0u64;
    for col in &collections {
        if col.starts_with("sessions.")
            && col.ends_with(".history")
            && let Ok(docs) = db.query(col, &crate::sessions::history_query_body()).await
        {
            for doc in &docs {
                if let Some(turns) = doc.get("turns").and_then(|v| v.as_array()) {
                    total_messages += turns.len() as u64;
                }
            }
        }
    }

    let healthy = db.health().await.unwrap_or(false);
    let soul_sealed = db.collection_exists("soul.invariant").await.unwrap_or(false);

    format!(
        "Uptime Report:\n\
         Process uptime: {}h {}m\n\
         {}\
         WardSONDB: {}\n\
         Collections: {}\n\
         Sessions created: {}\n\
         Total messages exchanged: {}\n\
         Memory entries stored: {}\n\
         Soul: {}",
        hours,
        mins,
        session_age_line,
        if healthy { "healthy" } else { "unhealthy" },
        collections.len(),
        session_count,
        total_messages,
        memory_count,
        if soul_sealed { "sealed" } else { "unsealed" }
    )
}

/// The entries `changelog` reports, from a newest-first window: those
/// created after the session started, or the whole window when the session
/// has no start on record. The order is kept.
fn entries_since<'a>(
    newest_first: &'a [serde_json::Value],
    session_start: Option<&str>,
) -> Vec<&'a serde_json::Value> {
    match session_start {
        Some(start) => newest_first
            .iter()
            .filter(|doc| {
                doc.get("created_at")
                    .and_then(|v| v.as_str())
                    .is_some_and(|ts| ts > start)
            })
            .collect(),
        None => newest_first.iter().collect(),
    }
}

#[cfg(test)]
mod changelog_tests {
    use super::entries_since;
    use serde_json::json;

    #[test]
    fn entries_after_the_session_start_are_reported_newest_first() {
        // As `fetch_recent_with_fields` returns them: newest first.
        let window = vec![
            json!({"content": "c", "created_at": "2026-09-28T12:00:00Z"}),
            json!({"content": "b", "created_at": "2026-09-28T11:00:00Z"}),
            json!({"content": "a", "created_at": "2026-09-27T09:00:00Z"}),
            json!({"content": "no timestamp"}),
        ];
        let since = entries_since(&window, Some("2026-09-28T00:00:00Z"));
        let contents: Vec<_> = since.iter().map(|d| d["content"].as_str().unwrap()).collect();
        assert_eq!(contents, ["c", "b"]);

        // Without a start on record the whole window is reported, in its order.
        let all = entries_since(&window, None);
        assert_eq!(all.len(), 4);
        assert_eq!(all[0]["content"], "c");
    }
}

async fn changelog(db: &WardsonDbClient, current_session: &str) -> String {
    // Find the current session's creation time
    let meta_col = format!("sessions.{}.meta", current_session);
    let session_start = db
        .query(&meta_col, &crate::sessions::history_query_body())
        .await
        .ok()
        .and_then(|docs| docs.into_iter().next())
        .and_then(|doc| doc.get("created_at").and_then(|v| v.as_str()).map(|s| s.to_string()));

    let mut output = String::from("Changes since last session:\n");

    // Recent memory entries, newest first. The window and its order are
    // explicit: a body that carries a projection and nothing else is
    // answered with the server's default window, the OLDEST 100 documents,
    // and an instance with more entries than that would never see a new one.
    let entries = db
        .fetch_recent_with_fields(
            "memory.entries",
            MEMORY_FETCH_WINDOW,
            Some(&["content", "tags", "created_at"]),
        )
        .await
        .unwrap_or_default();

    const DISPLAY_CAP: usize = 5;

    let recent_entries = entries_since(&entries, session_start.as_deref());

    let total_recent = recent_entries.len();
    if total_recent == 0 {
        output.push_str("  No new memory entries.\n");
    } else if total_recent <= DISPLAY_CAP {
        output.push_str(&format!("  {} new memory entries:\n", total_recent));
        for entry in recent_entries.iter() {
            let content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("?");
            output.push_str(&format!("    - {}\n", content));
        }
    } else {
        output.push_str(&format!(
            "  {} new memory entries (showing latest {}):\n",
            total_recent, DISPLAY_CAP
        ));
        for entry in recent_entries.iter().take(DISPLAY_CAP) {
            let content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("?");
            output.push_str(&format!("    - {}\n", content));
        }
    }

    // Count the sessions `session_list` shows: every session the operator
    // has not deleted. Learning sessions are a one-time setup artifact; they
    // are left out of the "operational" count and named beside it, so that
    // the total agrees with the list.
    let live_sessions = sessions::live_session_names(db).await;
    let learning_count = live_sessions.iter().filter(|name| name.contains("learning")).count();
    let operational_count = live_sessions.len() - learning_count;
    if learning_count > 0 {
        output.push_str(&format!(
            "  Total sessions: {} operational + {} learning (use `session_list` to see all)\n",
            operational_count, learning_count
        ));
    } else {
        output.push_str(&format!("  Total sessions: {}\n", operational_count));
    }

    output
}

fn get_memory_usage_mb() -> Option<u64> {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("VmRSS:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(kb) = parts.get(1).and_then(|v| v.parse::<u64>().ok()) {
                    return Some(kb / 1024);
                }
            }
        }
    }
    None
}

// -- No-arg tools --------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "system_status",
    description = "Report system status: version, uptime, soul status, memory usage, and a `wardsondb` section nesting health, collections, storage_poisoned, and lifetime counters (requests/inserts/queries/deletes — all wardsondb-scoped, NOT global OS counters)."
)]
pub struct SystemStatusArgs {}

impl SystemStatusArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let status = system_status(ctx.db).await;
        Ok(serde_json::to_string_pretty(&status).unwrap_or_default())
    }
}

// -- system_logs — read-only service-log tail (self-diagnostics) ----------

/// The services whose logs embrad captures on the ephemeral tmpfs
/// (`supervisor.rs` redirects every child's stdout/stderr to
/// `/embra/ephemeral/<name>.log`; embrad's own post-UI redirect writes
/// `embrad.log`). The tool validates against this list and NEVER takes a
/// path, so the carve-out below cannot traverse.
const SYSTEM_LOG_SERVICES: [&str; 7] = [
    "embra-brain",
    "embrad",
    "wardsondb",
    "embra-trustd",
    "embra-apid",
    "embra-web",
    "embra-console",
];

/// Logs live OUTSIDE the workspace jail — a deliberate READ-ONLY carve-out
/// for self-diagnostics, bounded to fixed filenames on the boot-wiped
/// tmpfs. Do not generalize this into a path-taking file reader.
const SYSTEM_LOG_DIR: &str = "/embra/ephemeral";

/// Window read from the end of a log before line filtering — bounds memory
/// on very large files while covering any sane tail request.
const SYSTEM_LOG_TAIL_BYTES: u64 = 512 * 1024;

const SYSTEM_LOG_DEFAULT_LINES: usize = 200;

const SYSTEM_LOG_MAX_LINES: usize = 2000;

/// Default service is the brain's own log — the one carrying the retrieval
/// funnel, saturation, seed, and slow-query lines.
fn resolve_log_service(service: Option<&str>) -> Result<&'static str, String> {
    let requested = service
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("embra-brain");
    SYSTEM_LOG_SERVICES
        .iter()
        .copied()
        .find(|s| s.eq_ignore_ascii_case(requested))
        .ok_or_else(|| {
            format!(
                "unknown service '{}' — pick from: {}",
                requested,
                SYSTEM_LOG_SERVICES.join(", ")
            )
        })
}

/// Pure tail-and-filter over a text window. `window_truncated` = the read
/// started mid-file, so the first (partial) line is dropped. Returns the
/// selected lines plus the total matching-line count in the window.
fn tail_filter_lines<'a>(
    buf: &'a str,
    filter: Option<&str>,
    lines: usize,
    window_truncated: bool,
) -> (Vec<&'a str>, usize) {
    let mut all: Vec<&'a str> = buf.lines().collect();
    if window_truncated && !all.is_empty() {
        all.remove(0);
    }
    let filter_lower = filter.map(|f| f.to_lowercase()).filter(|f| !f.is_empty());
    let matching: Vec<&'a str> = match &filter_lower {
        Some(f) => all
            .into_iter()
            .filter(|l| l.to_lowercase().contains(f.as_str()))
            .collect(),
        None => all,
    };
    let total = matching.len();
    let start = total.saturating_sub(lines);
    (matching[start..].to_vec(), total)
}

async fn system_logs(service: Option<&str>, lines: Option<u32>, filter: Option<&str>) -> String {
    let service = match resolve_log_service(service) {
        Ok(s) => s,
        Err(e) => return format!("Error: {}", e),
    };
    let lines = (lines.unwrap_or(SYSTEM_LOG_DEFAULT_LINES as u32) as usize)
        .clamp(1, SYSTEM_LOG_MAX_LINES);
    let path = format!("{}/{}.log", SYSTEM_LOG_DIR, service);

    let path_for_read = path.clone();
    let read = tokio::task::spawn_blocking(move || -> std::io::Result<(String, u64, bool)> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&path_for_read)?;
        let size = f.metadata()?.len();
        let start = size.saturating_sub(SYSTEM_LOG_TAIL_BYTES);
        if start > 0 {
            f.seek(SeekFrom::Start(start))?;
        }
        let mut raw = Vec::with_capacity((size - start) as usize);
        f.read_to_end(&mut raw)?;
        Ok((String::from_utf8_lossy(&raw).into_owned(), size, start > 0))
    })
    .await;

    let (buf, size, truncated) = match read {
        Ok(Ok(v)) => v,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return format!(
                "No log at {} — the service has not started on this boot (or this is dev mode without the ephemeral tmpfs). Logs reset at boot and at service restarts.",
                path
            );
        }
        Ok(Err(e)) => return format!("Error: reading {} failed: {}", path, e),
        Err(e) => return format!("Error: log read task failed: {}", e),
    };

    let (selected, matching_total) = tail_filter_lines(&buf, filter, lines, truncated);
    let mut out = format!(
        "system_logs {} ({} bytes total{}): last {} of {} matching line(s){}\n\n",
        service,
        size,
        if truncated { ", scanning the final 512 KiB window" } else { "" },
        selected.len(),
        matching_total,
        filter
            .map(|f| format!(" for filter \"{}\"", f))
            .unwrap_or_default()
    );
    if selected.is_empty() {
        out.push_str("(no matching lines)\n");
    } else {
        for l in selected {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "system_logs",
    description = "Read the tail of a service's log from the ephemeral tmpfs (/embra/ephemeral/<service>.log) — the OS's own journals, for self-diagnostics. service: embra-brain (default) | embrad | wardsondb | embra-trustd | embra-apid | embra-web | embra-console. lines: tail count, default 200, max 2000. filter: case-insensitive substring applied per line before the tail cut. The brain log carries the auto-enrichment funnel lines (candidates_*), kg::traversal saturation lines, knowledge_seed heals, and wardsondb slow-query warns. Logs reset at boot and at service restarts; on very large files only the final 512 KiB window is scanned."
)]
pub struct SystemLogsArgs {
    /// Service log to read. Default embra-brain.
    #[serde(default)]
    pub service: Option<String>,
    /// Tail line count (default 200, clamped to [1, 2000]).
    #[serde(default)]
    pub lines: Option<u32>,
    /// Case-insensitive substring filter applied per line before the tail.
    #[serde(default)]
    pub filter: Option<String>,
}

impl SystemLogsArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(system_logs(self.service.as_deref(), self.lines, self.filter.as_deref()).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "uptime_report",
    description = "Detailed system report with uptime, memory usage, session age, and lifetime counters."
)]
pub struct UptimeReportArgs {}

impl UptimeReportArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(uptime_report(ctx.db, ctx.session_name).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "changelog",
    description = "Report what changed in embraOS since the previous session: new memory entries, new sessions, key activity."
)]
pub struct ChangelogArgs {}

impl ChangelogArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(changelog(ctx.db, ctx.session_name).await)
    }
}

#[cfg(test)]
mod native_args_tests {
    use super::*;

    /// The embedding failure counters ride `system_status` under
    /// `embedding`, with the last failure only when there is one.
    #[test]
    fn system_status_carries_the_embedding_failure_counters() {
        let health = EmbeddingHealth {
            failures_write: 2,
            failures_query: 1,
            last_failure: Some(crate::embedding::cache::EmbeddingFailure {
                at: "2026-10-02T00:00:00Z".into(),
                subject: "memory.semantic:n1".into(),
                reason: "model not loaded".into(),
            }),
        };
        let v = serde_json::to_value(&health).unwrap();
        assert_eq!(v["failures_write"], 2);
        assert_eq!(v["failures_query"], 1);
        assert_eq!(v["last_failure"]["subject"], "memory.semantic:n1");
        assert_eq!(v["last_failure"]["reason"], "model not loaded");
    }

    #[test]
    fn system_status_nests_lifetime_under_wardsondb() {
        // Closes #42: lifetime_* fields no longer appear at top level — they
        // sit under wardsondb.lifetime so the wardsondb scope is honest.
        let s = SystemStatus {
            version: "test".into(),
            uptime_seconds: 0,
            memory_usage_mb: None,
            soul_status: "sealed".into(),
            search_window_saturated: false,
            wardsondb: WardsondbSection {
                healthy: true,
                collections: vec!["memory.entries".into()],
                storage_poisoned: None,
                lifetime: Some(WardsondbLifetime {
                    requests: Some(7),
                    inserts: Some(3),
                    queries: Some(4),
                    deletes: Some(0),
                }),
                memory_collections: vec![MemoryCollectionStatus {
                    name: "memory.entries".into(),
                    count: Some(42),
                    window: MEMORY_FETCH_WINDOW,
                    saturated: false,
                }],
            },
            provider: None,
            embedding: EmbeddingHealth { failures_write: 0, failures_query: 0, last_failure: None },
        };
        let v = serde_json::to_value(&s).unwrap();
        assert!(v.get("provider").is_none(), "no probe yet → no provider block");
        assert_eq!(v["embedding"]["failures_write"], 0);
        assert_eq!(v["embedding"]["failures_query"], 0);
        assert!(v["embedding"].get("last_failure").is_none(), "no failure → no last_failure");
        assert!(v.get("lifetime_requests").is_none(), "flat field leaked");
        assert!(v.get("lifetime_inserts").is_none(), "flat field leaked");
        assert!(v.get("lifetime_queries").is_none(), "flat field leaked");
        assert!(v.get("lifetime_deletes").is_none(), "flat field leaked");
        assert!(v.get("wardsondb_healthy").is_none(), "flat field leaked");
        assert_eq!(v["wardsondb"]["healthy"], true);
        assert_eq!(v["wardsondb"]["collections"][0], "memory.entries");
        assert_eq!(v["wardsondb"]["lifetime"]["requests"], 7);
        assert_eq!(v["wardsondb"]["lifetime"]["inserts"], 3);
        assert_eq!(v["wardsondb"]["lifetime"]["queries"], 4);
        assert_eq!(v["wardsondb"]["lifetime"]["deletes"], 0);
        // FIX-6 additions: parity section nests under wardsondb; the
        // saturation headline is top-level.
        assert_eq!(v["search_window_saturated"], false);
        assert_eq!(v["wardsondb"]["memory_collections"][0]["name"], "memory.entries");
        assert_eq!(v["wardsondb"]["memory_collections"][0]["count"], 42);
        assert_eq!(v["wardsondb"]["memory_collections"][0]["saturated"], false);
    }
}

#[cfg(test)]
mod system_logs_tests {
    use super::{
        resolve_log_service, tail_filter_lines, SystemLogsArgs, SYSTEM_LOG_DEFAULT_LINES,
        SYSTEM_LOG_MAX_LINES, SYSTEM_LOG_SERVICES,
    };

    #[test]
    fn resolve_defaults_to_brain_case_insensitive_rejects_unknown() {
        assert_eq!(resolve_log_service(None).unwrap(), "embra-brain");
        assert_eq!(resolve_log_service(Some("  ")).unwrap(), "embra-brain");
        assert_eq!(resolve_log_service(Some("WardsonDB")).unwrap(), "wardsondb");
        assert_eq!(resolve_log_service(Some("embrad")).unwrap(), "embrad");
        let err = resolve_log_service(Some("kernel")).unwrap_err();
        assert!(err.contains("unknown service 'kernel'"));
        assert!(err.contains("embra-brain"), "error lists the allowlist: {err}");
        // The allowlist is names, never paths — the read carve-out depends
        // on this staying enum-shaped.
        assert!(SYSTEM_LOG_SERVICES.iter().all(|s| !s.contains('/')));
    }

    #[test]
    fn tail_filter_cuts_tail_counts_matches_case_insensitive() {
        let buf = "alpha one\nBETA two\nalpha three\nbeta FOUR\nalpha five";
        let (sel, total) = tail_filter_lines(buf, Some("beta"), 1, false);
        assert_eq!(total, 2, "case-insensitive matches counted pre-tail");
        assert_eq!(sel, vec!["beta FOUR"], "tail keeps the newest match");
        let (sel, total) = tail_filter_lines(buf, None, 3, false);
        assert_eq!(total, 5);
        assert_eq!(sel, vec!["alpha three", "beta FOUR", "alpha five"]);
        // Empty filter behaves like no filter.
        let (_, total) = tail_filter_lines(buf, Some(""), 10, false);
        assert_eq!(total, 5);
    }

    #[test]
    fn tail_filter_drops_partial_first_line_when_window_truncated() {
        let buf = "rtial line\nfull one\nfull two";
        let (sel, total) = tail_filter_lines(buf, None, 10, true);
        assert_eq!(total, 2, "the seek-split first line is dropped");
        assert_eq!(sel, vec!["full one", "full two"]);
        // Untruncated windows keep every line.
        let (_, total) = tail_filter_lines(buf, None, 10, false);
        assert_eq!(total, 3);
    }

    #[test]
    fn line_caps_pinned() {
        assert_eq!(SYSTEM_LOG_DEFAULT_LINES, 200);
        assert_eq!(SYSTEM_LOG_MAX_LINES, 2000);
    }

    #[test]
    fn system_logs_registered_with_plain_object_schema() {
        let names: Vec<&'static str> = inventory::iter::<crate::tools::registry::ToolDescriptor>()
            .map(|d| d.name)
            .collect();
        assert!(names.contains(&"system_logs"), "system_logs registered");

        // Anthropic rejects top-level oneOf/allOf/anyOf in input_schema.
        let schema = schemars::schema_for!(SystemLogsArgs);
        let v = serde_json::to_value(&schema).unwrap();
        assert!(v.get("oneOf").is_none());
        assert!(v.get("allOf").is_none());
        assert!(v.get("anyOf").is_none());
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("object"));
    }
}
