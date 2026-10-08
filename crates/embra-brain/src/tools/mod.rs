use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::config::SystemConfig;
use crate::db::{WardsonDbClient, MEMORY_FETCH_WINDOW};
use crate::knowledge;

mod calc;
pub mod cron;
pub mod engineering;
pub mod express;
pub mod file_copy;
pub mod file_offer;
pub mod file_patch;
pub mod guardian;
pub mod media;
pub mod registry;
pub mod security;
pub mod sessions;

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

// ── Memory & Knowledge Tools ──

async fn ensure_collection(db: &WardsonDbClient, name: &str) {
    if !db.collection_exists(name).await.unwrap_or(true) {
        let _ = db.create_collection(name).await;
    }
}

/// Canonical is-promoted predicate: `promoted_to` present and non-null.
/// `promoted_to` is the maintained pointer — set by promotion (`remember`
/// at creation, `knowledge_promote` afterwards), cleared by
/// `knowledge_unlink_node`'s cascade and the dangling-pointer repair in
/// `knowledge/promotion.rs`. (The `derived_from` edge also exists
/// per promotion but can drift via unlink_edge/sweep_orphans, so it is NOT
/// used as the promotion signal.) Missing and null are both "unpromoted" —
/// entries predating the field lack the key entirely.
fn entry_is_promoted(doc: &serde_json::Value) -> bool {
    doc.get("promoted_to").map(|v| !v.is_null()).unwrap_or(false)
}

/// Display caps: recall shows the newest matches (fetches are recency-
/// sorted). The unpromoted worklist mode shows more because its purpose is
/// enumerating the entries that have no node, not answering a lookup.
const RECALL_DISPLAY_CAP: usize = 10;
const RECALL_UNPROMOTED_DISPLAY_CAP: usize = 200;

async fn recall(db: &WardsonDbClient, query: &str, unpromoted_only: bool) -> String {
    ensure_collection(db, "memory.entries").await;

    // FIX-2: explicit recency windows. The old empty bodies fell into the
    // server default (limit:100, UUIDv7 key order = oldest first), freezing
    // recall over the oldest ~100 docs per collection forever. Newest-first
    // also means the display cap below shows the most recent matches.
    let entries = db.fetch_recent("memory.entries", MEMORY_FETCH_WINDOW).await.unwrap_or_default();
    // Unpromoted-worklist mode only concerns memory.entries — semantic and
    // procedural docs are promoted by definition.
    let (semantic, procedural) = if unpromoted_only {
        (Vec::new(), Vec::new())
    } else {
        (
            db.fetch_recent("memory.semantic", MEMORY_FETCH_WINDOW).await.unwrap_or_default(),
            db.fetch_recent("memory.procedural", MEMORY_FETCH_WINDOW).await.unwrap_or_default(),
        )
    };

    if entries.is_empty() && semantic.is_empty() && procedural.is_empty() {
        return "No memory entries found.".into();
    }

    let query_lower = query.trim_start_matches('#').to_lowercase();
    let query_tokens: Vec<String> = query_lower
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    let mut output_lines = recall_lines(&entries, &semantic, &procedural, &query_tokens, unpromoted_only);

    if output_lines.is_empty() {
        if unpromoted_only {
            return if query_tokens.is_empty() {
                "No unpromoted memory entries — everything has been promoted.".into()
            } else {
                format!("No unpromoted memory entries matching '{}'.", query)
            };
        }
        info!(target: "recall", query = %query, token_count = query_tokens.len(), "zero-result recall");
        let mut msg = format!("No memory entries matching '{}'.", query);
        if query_tokens.len() > 1 {
            msg.push_str(" Multi-token queries require ALL tokens to appear; try a single word, or omit the query to list recent entries.");
        }
        return msg;
    }

    let total = output_lines.len();
    if unpromoted_only {
        output_lines.truncate(RECALL_UNPROMOTED_DISPLAY_CAP);
        format!("Found {} unpromoted entries:\n{}", total, output_lines.join("\n"))
    } else {
        output_lines.truncate(RECALL_DISPLAY_CAP);
        format!("Found {} matching entries:\n{}", total, output_lines.join("\n"))
    }
}

/// The lines `recall` prints, before its display cap: the matching nodes
/// first, then the matching entries. A memory is listed once: an entry whose
/// node is already in the listing is left out, because the node line carries
/// the same memory; an entry whose node did not match (its text was changed
/// since) keeps its line and its `[promoted → …]` marker. In the worklist
/// mode the nodes are not passed in, and a promoted entry is skipped.
fn recall_lines(
    entries: &[serde_json::Value],
    semantic: &[serde_json::Value],
    procedural: &[serde_json::Value],
    query_tokens: &[String],
    unpromoted_only: bool,
) -> Vec<String> {
    fn tags_to_str(doc: &serde_json::Value) -> String {
        match doc.get("tags") {
            Some(v) if v.is_array() => v.as_array().unwrap().iter()
                .filter_map(|t| t.as_str()).collect::<Vec<_>>().join(", "),
            Some(v) if v.is_string() => v.as_str().unwrap_or("").to_string(),
            _ => String::new(),
        }
    }

    let matches_query = |content: &str, tags: &str| -> bool {
        if query_tokens.is_empty() { return true; }
        let hay = format!("{} {}", content.to_lowercase(), tags.to_lowercase());
        tokens_all_match(&hay, query_tokens)
    };

    let mut output_lines: Vec<String> = Vec::new();
    let mut listed_nodes: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();

    // Promoted collections first (ranked higher)
    for (collection, docs) in [("memory.semantic", semantic), ("memory.procedural", procedural)] {
        for doc in docs {
            let id = doc.get("_id").and_then(|v| v.as_str()).unwrap_or("?");
            let content = doc.get("content").and_then(|v| v.as_str())
                .or_else(|| doc.get("description").and_then(|v| v.as_str()))
                .unwrap_or("");
            let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let tags = tags_to_str(doc);
            let searchable = format!("{} {} {}", title, content, tags);
            if !matches_query(&searchable, &tags) { continue; }
            let ts = doc.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
            let display = if !title.is_empty() { format!("{}: {}", title, content) } else { content.to_string() };
            output_lines.push(format!("  [{}] [{}] {} (tags: {}) — {}", collection, id, display, tags, ts));
            listed_nodes.insert((collection, id));
        }
    }

    // Episodic entries
    for doc in entries.iter() {
        if unpromoted_only && entry_is_promoted(doc) { continue; }
        let pointer = doc.get("promoted_to").filter(|v| !v.is_null());
        let promoted_collection = pointer.and_then(|v| v.get("collection")).and_then(|v| v.as_str());
        let promoted_id = pointer.and_then(|v| v.get("id")).and_then(|v| v.as_str());
        if let (Some(c), Some(i)) = (promoted_collection, promoted_id)
            && listed_nodes.contains(&(c, i))
        {
            continue;
        }
        let content = doc.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let tags = tags_to_str(doc);
        if !matches_query(content, &tags) { continue; }
        let id = doc.get("_id").and_then(|v| v.as_str()).unwrap_or("?");
        let ts = doc.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let promoted_marker = promoted_collection
            .map(|c| format!(" [promoted → {}]", c))
            .unwrap_or_default();
        output_lines.push(format!("  [memory.entries] [{}] {}{} (tags: {}) — {}", id, content, promoted_marker, tags, ts));
    }

    output_lines
}

#[cfg(test)]
mod recall_listing_tests {
    use super::recall_lines;
    use serde_json::json;

    fn tokens(q: &str) -> Vec<String> {
        q.split_whitespace().map(str::to_string).collect()
    }

    fn promoted_entry() -> serde_json::Value {
        json!({
            "_id": "e1", "content": "the cert refresh works after manual generation", "tags": ["certs"],
            "promoted_to": {"collection": "memory.semantic", "id": "n1"}, "created_at": "2026-10-01T00:00:00Z",
        })
    }

    #[test]
    fn a_promoted_memory_is_listed_once_as_its_node() {
        let node = json!({
            "_id": "n1", "content": "the cert refresh works after manual generation", "category": "fact",
            "tags": ["certs"], "created_at": "2026-10-01T00:00:01Z",
        });
        let lines = recall_lines(&[promoted_entry()], &[node], &[], &tokens("refresh"), false);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("[memory.semantic] [n1]"), "{lines:?}");
    }

    #[test]
    fn an_entry_whose_node_did_not_match_keeps_its_marker() {
        // The node's text was changed since the promotion; the query only
        // finds the entry's original wording.
        let node = json!({
            "_id": "n1", "content": "certificates rotate by hand", "category": "fact",
            "tags": ["certs"], "created_at": "2026-10-01T00:00:01Z",
        });
        let lines = recall_lines(&[promoted_entry()], &[node], &[], &tokens("refresh"), false);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("[memory.entries] [e1]"), "{lines:?}");
        assert!(lines[0].contains("[promoted → memory.semantic]"), "{lines:?}");
    }

    #[test]
    fn a_procedure_keeps_its_entry_out_of_the_listing_too() {
        let entry = json!({
            "_id": "e2", "content": "how to rotate the cert", "tags": [],
            "promoted_to": {"collection": "memory.procedural", "id": "p1"}, "created_at": "2026-10-01T00:00:00Z",
        });
        let procedure = json!({
            "_id": "p1", "title": "Rotate the cert", "description": "how to rotate the cert by hand",
            "tags": [], "created_at": "2026-10-01T00:00:01Z",
        });
        let lines = recall_lines(&[entry], &[], &[procedure], &tokens("rotate"), false);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("[memory.procedural] [p1] Rotate the cert: "), "{lines:?}");
    }

    #[test]
    fn the_worklist_lists_unpromoted_entries_only() {
        let unpromoted = json!({
            "_id": "e3", "content": "refresh the docs", "tags": [], "promoted_to": null,
            "created_at": "2026-10-01T00:00:00Z",
        });
        let lines = recall_lines(&[promoted_entry(), unpromoted], &[], &[], &tokens("refresh"), true);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("[memory.entries] [e3]"), "{lines:?}");
    }
}

/// Return true iff every token appears as a substring of `hay` (already lowercased).
fn tokens_all_match(hay: &str, tokens: &[String]) -> bool {
    tokens.iter().all(|t| hay.contains(t.as_str()))
}

#[cfg(test)]
mod is_tag_token_tests {
    use super::is_tag_token;

    #[test]
    fn alpha_start_is_tag() {
        assert!(is_tag_token("#soul"));
        assert!(is_tag_token("#architecture"));
        assert!(is_tag_token("#issue-tracking"));
        assert!(is_tag_token("#A"));
    }

    #[test]
    fn numeric_start_is_not_tag() {
        // GitHub-style issue refs stay in content
        assert!(!is_tag_token("#5"));
        assert!(!is_tag_token("#42"));
        assert!(!is_tag_token("#5issues"));
    }

    #[test]
    fn non_alpha_start_is_not_tag() {
        assert!(!is_tag_token("#-leading-hyphen"));
        assert!(!is_tag_token("#_underscore"));
    }

    #[test]
    fn lone_hash_is_not_tag() {
        assert!(!is_tag_token("#"));
    }

    #[test]
    fn no_hash_is_not_tag() {
        assert!(!is_tag_token("soul"));
        assert!(!is_tag_token(""));
        assert!(!is_tag_token("hello#world"));
    }
}

#[cfg(test)]
mod tokens_all_match_tests {
    use super::tokens_all_match;

    fn toks(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn empty_tokens_any_hay_matches() {
        assert!(tokens_all_match("anything", &toks(&[])));
    }

    #[test]
    fn all_tokens_present_matches() {
        assert!(tokens_all_match("express tool caveats noted", &toks(&["express", "tool", "caveats"])));
    }

    #[test]
    fn missing_one_token_rejects() {
        assert!(!tokens_all_match("express tool only", &toks(&["express", "tool", "caveats"])));
    }

    #[test]
    fn tokens_can_appear_out_of_order() {
        assert!(tokens_all_match("caveats about the express tool", &toks(&["express", "caveats"])));
    }

    #[test]
    fn single_token_still_works() {
        assert!(tokens_all_match("express panel", &toks(&["express"])));
        assert!(!tokens_all_match("panel only", &toks(&["express"])));
    }
}

#[cfg(test)]
mod entry_is_promoted_tests {
    //! `promoted_to` is the maintained promotion pointer; missing and null
    //! must both read as unpromoted (entries predating the field lack the
    //! key; unlink_node's cascade PATCHes it to null).
    use super::entry_is_promoted;
    use serde_json::json;

    #[test]
    fn promoted_pointer_object_is_promoted() {
        let doc = json!({"content": "x", "promoted_to": {"collection": "memory.semantic", "id": "abc"}});
        assert!(entry_is_promoted(&doc));
    }

    #[test]
    fn null_pointer_is_unpromoted() {
        let doc = json!({"content": "x", "promoted_to": null});
        assert!(!entry_is_promoted(&doc));
    }

    #[test]
    fn missing_field_is_unpromoted() {
        let doc = json!({"content": "x"});
        assert!(!entry_is_promoted(&doc));
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

/// Is `word` a tag token (`#<letter>[letters/digits/_/-]*`)?
///
/// Hashtag-prefixed tokens are stripped from content and pushed into the
/// `tags` array. The previous rule (anything starting with `#`) also captured
/// GitHub-style issue references (`#5`, `#42`) and turned them into tag
/// entries, which drops the reference from the remembered prose (Issue #14).
///
/// The letter-start rule is cheap and correct for the common cases:
///   #soul, #architecture, #issue-tracking  → tags
///   #5, #42, #-hyphen-start, #             → stay in text
/// Commit SHAs prefixed with `#` (rare) that happen to start with a letter
/// would be classified as tags; operators typically reference SHAs without
/// a leading `#`, so the ambiguity is acceptable.
fn is_tag_token(word: &str) -> bool {
    let mut chars = word.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some('#'), Some(c)) if c.is_alphabetic()
    )
}

/// Split the `#tags` from the text: "content text #tag1 #tag2". GitHub-style
/// issue references like `#5` stay in the text (`is_tag_token` requires a
/// letter start).
fn split_tags(content: &str) -> (String, Vec<String>) {
    let mut tags: Vec<String> = Vec::new();
    let mut text_parts = Vec::new();
    for word in content.split_whitespace() {
        if is_tag_token(word) {
            tags.push(word.trim_start_matches('#').to_string());
        } else {
            text_parts.push(word);
        }
    }
    (text_parts.join(" "), tags)
}

/// The episodic entry of a memory. `promoted_to` is written as null, not
/// left out: the promotion that follows sets it, and the server's `$ne`
/// does not match a document that lacks the field.
fn entry_doc(text: &str, tags: &[String], session: &str, created_at: &str) -> serde_json::Value {
    serde_json::json!({
        "content": text,
        "tags": tags,
        "session": session,
        "promoted_to": serde_json::Value::Null,
        "created_at": created_at,
    })
}

/// The node `remember` gives a memory.
#[derive(Debug)]
enum Promotion {
    Semantic(knowledge::types::SemanticCategory),
    Procedural(knowledge::promotion::Procedure),
}

/// The promotion the arguments ask for, checked before anything is written.
/// A procedure argument that is empty, `null` or `{}` is none: a model that
/// fills every optional argument by reflex still saves a semantic memory.
fn remember_plan(
    category: knowledge::types::SemanticCategory,
    procedure: Option<&str>,
) -> Result<Promotion, String> {
    match procedure.map(str::trim).filter(|p| !matches!(*p, "" | "null" | "{}")) {
        None => Ok(Promotion::Semantic(category)),
        Some(json) => knowledge::promotion::parse_procedure(json)
            .map(Promotion::Procedural)
            .map_err(|e| format!("Nothing saved: {}", e)),
    }
}

fn remember_reply(entry_id: &str, node: &knowledge::promotion::NewNode, candidates: &str) -> String {
    let what = if node.collection == "memory.procedural" { "procedure" } else { "category" };
    format!(
        "Remembered as {}:{} ({}: {}). Entry ID: {}\n{}",
        node.collection, node.id, what, node.label, entry_id, candidates
    )
}

/// The entry is saved and the node is not. The answer has to keep the model
/// from saving the memory a second time.
fn remember_unpromoted_reply(entry_id: &str, error: &str) -> String {
    format!(
        "Remembered as an entry only: writing the node failed ({}). Entry ID: {}. The entry is saved; do not call remember again. Give it its node with knowledge_promote.",
        error, entry_id
    )
}

/// Save a memory: the episodic entry, and the node it is promoted to in the
/// same call. The entry is written first and stays when the node cannot be
/// written; `recall` with `unpromoted_only` lists such an entry.
async fn remember(
    db: &WardsonDbClient,
    content: &str,
    promotion: Promotion,
    session: &str,
    config: &SystemConfig,
) -> String {
    if content.is_empty() {
        return "Nothing to remember. Provide content after remember ....".into();
    }
    let (text, tags) = split_tags(content);
    if text.is_empty() {
        return "Nothing to remember: give content besides the tags.".into();
    }

    ensure_collection(db, "memory.entries").await;

    let created_at = Utc::now().to_rfc3339();
    let entry_id = match db.write("memory.entries", &entry_doc(&text, &tags, session, &created_at)).await {
        Ok(id) => id,
        Err(e) => return format!("Failed to save memory: {}", e),
    };

    let written = match &promotion {
        Promotion::Semantic(category) => {
            knowledge::promotion::write_semantic_node(db, &entry_id, category, config).await
        }
        Promotion::Procedural(procedure) => {
            knowledge::promotion::write_procedural_node(db, &entry_id, procedure, config).await
        }
    };
    let (reply, node) = match written {
        Ok(node) => {
            let candidates = knowledge::neighbors::candidates_block(
                &knowledge::neighbors::link_candidates(db, config, node.collection, &node.id).await,
            );
            (remember_reply(&entry_id, &node, &candidates), Some(node))
        }
        Err(e) => (remember_unpromoted_reply(&entry_id, &e.to_string()), None),
    };

    // Background edge derivation (spec §4.8), for the entry and its node in
    // one task. Both gather their candidates at once, as the entry's always
    // did; the entry's leaves its own node out, and the node's plans the
    // pair (`derive_edges_except`).
    let db = db.clone();
    let config = config.clone();
    let session = session.to_string();
    tokio::spawn(async move {
        let entry = ("memory.entries", entry_id.as_str());
        match &node {
            Some(node) => {
                let _ = tokio::join!(
                    knowledge::edges::derive_edges_except(
                        &db,
                        entry,
                        &session,
                        &tags,
                        &created_at,
                        &config,
                        Some((node.collection, node.id.as_str())),
                    ),
                    knowledge::edges::derive_edges(
                        &db,
                        &node.id,
                        node.collection,
                        &node.session,
                        &node.tags,
                        &node.created_at,
                        &config,
                    ),
                );
            }
            None => {
                let _ = knowledge::edges::derive_edges_except(
                    &db, entry, &session, &tags, &created_at, &config, None,
                )
                .await;
            }
        }
    });

    reply
}

#[cfg(test)]
mod remember_tests {
    use super::*;
    use knowledge::types::SemanticCategory;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PROCEDURE: &str = r#"{"title": "Rotate the cert", "description": "when trustd complains",
        "steps": [{"order": 1, "action": "stop embra-web"}],
        "outcomes": {"success": "a new cert", "failure": "the old one stays"}}"#;

    #[test]
    fn tags_are_split_from_the_content_and_issue_numbers_stay() {
        let (text, tags) = split_tags("fixed  in #5 by the  retry #networking #embra-web");
        assert_eq!(text, "fixed in #5 by the retry");
        assert_eq!(tags, ["networking", "embra-web"]);
        assert_eq!(split_tags("#only #tags"), (String::new(), vec!["only".to_string(), "tags".to_string()]));
    }

    #[test]
    fn the_entry_document_has_five_fields_and_a_null_pointer() {
        let doc = entry_doc("the text", &["t".to_string()], "ops", "2026-10-03T00:00:00Z");
        assert_eq!(
            doc,
            json!({
                "content": "the text", "tags": ["t"], "session": "ops",
                "promoted_to": null, "created_at": "2026-10-03T00:00:00Z",
            })
        );
        assert!(doc.as_object().unwrap().contains_key("promoted_to"), "null, not absent");
    }

    #[test]
    fn remember_requires_content_alone_and_names_the_five_categories() {
        let d = registry::all_descriptors().find(|d| d.name == "remember").expect("registered");
        let schema = (d.input_schema)();
        assert_eq!(schema["required"], json!(["content"]), "a stored cron job gives content alone");
        let category_doc = schema["properties"]["category"]["description"].as_str().unwrap_or_default().to_string();
        for category in SemanticCategory::ALL {
            assert!(category_doc.contains(&format!("{}:", category.as_str())), "{}", category.as_str());
        }
        assert!(d.description.contains("knowledge_link"), "linking is part of saving");
        assert!(d.description.contains("No knowledge_promote call follows"));
    }

    #[test]
    fn the_category_defaults_to_observation_and_an_unknown_one_is_refused() {
        let args: RememberArgs = serde_json::from_value(json!({"content": "x"})).expect("content alone");
        assert_eq!(args.category, SemanticCategory::Observation);
        assert!(args.procedure.is_none());
        let named: RememberArgs =
            serde_json::from_value(json!({"content": "x", "category": "preference"})).expect("a category");
        assert_eq!(named.category, SemanticCategory::Preference);
        // The categories the feedback-loop spec used to promote with.
        for unknown in ["evaluation", "practice", "Fact"] {
            assert!(
                serde_json::from_value::<RememberArgs>(json!({"content": "x", "category": unknown})).is_err(),
                "{unknown}"
            );
        }
    }

    #[test]
    fn an_empty_procedure_argument_means_a_semantic_node() {
        for empty in [None, Some(""), Some("  "), Some("null"), Some("{}")] {
            let plan = remember_plan(SemanticCategory::Decision, empty).expect("a plan");
            assert!(matches!(plan, Promotion::Semantic(SemanticCategory::Decision)), "{empty:?}: {plan:?}");
        }
        let plan = remember_plan(SemanticCategory::Observation, Some(PROCEDURE)).expect("a plan");
        assert!(matches!(&plan, Promotion::Procedural(p) if p.title == "Rotate the cert"), "{plan:?}");
    }

    #[test]
    fn an_invalid_procedure_is_refused_before_anything_is_written() {
        let why = remember_plan(SemanticCategory::Observation, Some(r#"{"title": "t"}"#)).expect_err("refused");
        assert!(why.starts_with("Nothing saved: "), "{why}");
        assert!(why.contains("missing field 'description'"), "{why}");
        assert!(why.contains("Expected schema"), "{why}");
    }

    fn new_node(collection: &'static str, id: &str, label: &str) -> knowledge::promotion::NewNode {
        knowledge::promotion::NewNode {
            collection,
            id: id.into(),
            label: label.into(),
            session: "ops".into(),
            tags: Vec::new(),
            created_at: String::new(),
        }
    }

    #[test]
    fn the_reply_names_the_node_then_the_entry() {
        let semantic = remember_reply("e1", &new_node("memory.semantic", "n1", "decision"), "CANDIDATES");
        assert_eq!(semantic, "Remembered as memory.semantic:n1 (category: decision). Entry ID: e1\nCANDIDATES");
        let procedure = remember_reply("e2", &new_node("memory.procedural", "p1", "Rotate the cert"), "CANDIDATES");
        assert_eq!(
            procedure,
            "Remembered as memory.procedural:p1 (procedure: Rotate the cert). Entry ID: e2\nCANDIDATES"
        );
    }

    #[test]
    fn a_failed_promotion_says_the_entry_is_saved_and_names_knowledge_promote() {
        let reply = remember_unpromoted_reply("e1", "WardSONDB returned error 500: ");
        assert!(reply.starts_with("Remembered as an entry only"), "{reply}");
        assert!(reply.contains("Entry ID: e1"), "{reply}");
        assert!(reply.contains("do not call remember again"), "{reply}");
        assert!(reply.contains("knowledge_promote"), "{reply}");
    }

    // ── against a stub server ────────────────────────────────────────────

    fn test_config() -> SystemConfig {
        serde_json::from_value(json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
            "created_at": "", "version": "test", "embedding_enabled": false
        }))
        .expect("minimal config deserializes")
    }

    fn data(v: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": v, "meta": {}}))
    }

    /// A server that takes an entry (`e1`) and answers the node write with
    /// `node_write`.
    async fn server(node_collection: &str, node_write: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/memory.entries/docs")).respond_with(data(json!({"_id": "e1"}))).mount(&server).await;
        let entry = json!({
            "_id": "e1", "content": "the cert refresh works", "tags": ["certs"], "session": "ops", "promoted_to": null,
        });
        Mock::given(method("GET")).and(path("/memory.entries/docs/e1")).respond_with(data(entry)).mount(&server).await;
        Mock::given(method("POST"))
            .and(path(format!("/{node_collection}/docs")))
            .respond_with(node_write)
            .mount(&server)
            .await;
        Mock::given(method("PATCH")).and(path("/memory.entries/docs/e1")).respond_with(data(json!({}))).mount(&server).await;
        Mock::given(method("POST")).and(path("/memory.edges/docs")).respond_with(data(json!({"_id": "edge1"}))).mount(&server).await;
        server
    }

    /// The document writes the call made, in order, as "METHOD /path" with
    /// the body. The background derivation's queries are left out.
    async fn writes_seen(server: &MockServer) -> Vec<(String, serde_json::Value)> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "PATCH" || (r.method.as_str() == "POST" && r.url.path().ends_with("/docs")))
            .map(|r| {
                (
                    format!("{} {}", r.method, r.url.path()),
                    serde_json::from_slice(&r.body).unwrap_or(serde_json::Value::Null),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn remember_writes_the_entry_the_node_the_pointer_and_the_provenance_edge() {
        let server = server("memory.semantic", data(json!({"_id": "n1"}))).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let reply = remember(
            &db,
            "the cert refresh works #certs",
            Promotion::Semantic(SemanticCategory::Decision),
            "ops",
            &test_config(),
        )
        .await;
        assert!(
            reply.starts_with("Remembered as memory.semantic:n1 (category: decision). Entry ID: e1\n"),
            "{reply}"
        );
        // No model in a test: the node has no vector, and the reply says so
        // instead of claiming that nothing is near.
        assert!(reply.ends_with("No link candidates: the node has no embedding (see /embeddings)."), "{reply}");

        let writes = writes_seen(&server).await;
        let order: Vec<&str> = writes.iter().map(|w| w.0.as_str()).collect();
        assert_eq!(
            order,
            [
                "POST /memory.entries/docs",
                "POST /memory.semantic/docs",
                "PATCH /memory.entries/docs/e1",
                "POST /memory.edges/docs",
            ]
        );
        assert_eq!(writes[0].1["content"], "the cert refresh works");
        assert_eq!(writes[0].1["tags"], json!(["certs"]));
        assert_eq!(writes[0].1["session"], "ops");
        assert_eq!(writes[1].1["category"], "decision");
        assert_eq!(writes[1].1["source_entry_id"], "e1");
        assert_eq!(writes[2].1, json!({"promoted_to": {"collection": "memory.semantic", "id": "n1"}}));
        assert_eq!(writes[3].1["edge_type"], "derived_from");
    }

    #[tokio::test]
    async fn remember_with_a_procedure_writes_a_procedural_node() {
        let server = server("memory.procedural", data(json!({"_id": "p1"}))).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let plan = remember_plan(SemanticCategory::Observation, Some(PROCEDURE)).expect("a plan");
        let reply = remember(&db, "how to rotate the cert #certs", plan, "ops", &test_config()).await;
        assert!(
            reply.starts_with("Remembered as memory.procedural:p1 (procedure: Rotate the cert). Entry ID: e1\n"),
            "{reply}"
        );
        let writes = writes_seen(&server).await;
        assert_eq!(writes[1].0, "POST /memory.procedural/docs");
        assert_eq!(writes[1].1["title"], "Rotate the cert");
        assert!(writes[1].1.get("category").is_none());
        assert_eq!(writes[2].1, json!({"promoted_to": {"collection": "memory.procedural", "id": "p1"}}));
    }

    #[tokio::test]
    async fn a_node_that_cannot_be_written_leaves_the_entry_and_says_so() {
        let server = server("memory.semantic", ResponseTemplate::new(500)).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let reply = remember(
            &db,
            "the cert refresh works",
            Promotion::Semantic(SemanticCategory::Observation),
            "ops",
            &test_config(),
        )
        .await;
        assert!(reply.starts_with("Remembered as an entry only: writing the node failed ("), "{reply}");
        assert!(reply.contains("Entry ID: e1."), "{reply}");
        let order: Vec<String> = writes_seen(&server).await.into_iter().map(|w| w.0).collect();
        assert_eq!(order, ["POST /memory.entries/docs", "POST /memory.semantic/docs"], "no pointer, no edge");
    }

    #[tokio::test]
    async fn content_that_is_only_tags_saves_nothing() {
        let server = server("memory.semantic", data(json!({"_id": "n1"}))).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let reply = remember(
            &db,
            "#certs #ops",
            Promotion::Semantic(SemanticCategory::Observation),
            "ops",
            &test_config(),
        )
        .await;
        assert_eq!(reply, "Nothing to remember: give content besides the tags.");
        assert!(writes_seen(&server).await.is_empty());
    }
}

/// The node an entry was promoted to, as `(collection, id)`. Only the two
/// node collections count; anything else in the pointer is not a node
/// `forget` may remove.
fn promoted_pointer(entry: &serde_json::Value) -> Option<(&'static str, String)> {
    let pointer = entry.get("promoted_to").filter(|v| !v.is_null())?;
    let collection = match pointer.get("collection").and_then(|v| v.as_str())? {
        "memory.semantic" => "memory.semantic",
        "memory.procedural" => "memory.procedural",
        _ => return None,
    };
    Some((collection, pointer.get("id").and_then(|v| v.as_str())?.to_string()))
}

/// The entries whose `promoted_to` points at a node, newest first. The
/// pointer is the maintained record of a promotion (`entry_is_promoted`);
/// `derived_from` edges drift. No index serves the nested key, so this
/// scans `memory.entries`: a cold path, behind an operator's confirmation.
/// Eleven is ten ids to show and one to know there are more.
fn entries_of_node_query_body(collection: &str, node_id: &str) -> serde_json::Value {
    serde_json::json!({
        "filter": { "promoted_to.collection": collection, "promoted_to.id": node_id },
        "sort": [{ "created_at": "desc" }],
        "limit": 11,
    })
}

/// What `forget` does with the node its entry was promoted to.
#[derive(Debug, PartialEq)]
enum NodeFate {
    /// The entry has no live node.
    None,
    /// The node goes with its only entry.
    Remove,
    /// Other entries point at it too: a merge re-pointed them.
    KeepShared(Vec<String>),
    /// A seed-pack node comes back at the next boot; removing it is noise.
    KeepSeed,
    /// Whether another entry points at it could not be read. It stays.
    KeepUnread(String),
}

/// `entries` is what `entries_of_node_query_body` returned: the ids of
/// every entry that points at the node, the forgotten one included.
fn node_fate(entry_id: &str, node: Option<&serde_json::Value>, entries: Result<&[String], &str>) -> NodeFate {
    let Some(node) = node else { return NodeFate::None };
    if node.get("origin").and_then(|v| v.as_str()) == Some("knowledge_seed") {
        return NodeFate::KeepSeed;
    }
    match entries {
        Err(e) => NodeFate::KeepUnread(e.to_string()),
        Ok(ids) => {
            let others: Vec<String> = ids.iter().filter(|i| i.as_str() != entry_id).cloned().collect();
            if others.is_empty() { NodeFate::Remove } else { NodeFate::KeepShared(others) }
        }
    }
}

/// The cascade over `memory.edges` for an entry, and for its node when the
/// node goes too: one scan for both documents.
///
/// Cold path: `$or` forces a WardSONDB full scan — acceptable for an
/// operator-invoked one-off. NEVER copy this shape into a `query()` hot
/// path; hot paths arm-split for the indexes (knowledge/traversal.rs,
/// 2026-07-04).
fn forget_edge_filter(entry_id: &str, node: Option<(&str, &str)>) -> serde_json::Value {
    let mut arms = vec![
        serde_json::json!({"source_id": entry_id, "source_collection": "memory.entries"}),
        serde_json::json!({"target_id": entry_id, "target_collection": "memory.entries"}),
    ];
    if let Some((collection, node_id)) = node {
        arms.push(serde_json::json!({"source_id": node_id, "source_collection": collection}));
        arms.push(serde_json::json!({"target_id": node_id, "target_collection": collection}));
    }
    serde_json::json!({ "$or": arms })
}

/// Why a node outlived its entry, appended to the entry's removal line.
fn kept_node_note(collection: &str, node_id: &str, fate: &NodeFate) -> String {
    match fate {
        NodeFate::None | NodeFate::Remove => String::new(),
        NodeFate::KeepShared(others) => format!(
            " Its node {}:{} stays: {} other {} point at it ({}). knowledge_unlink_node removes the node.",
            collection,
            node_id,
            others.len(),
            if others.len() == 1 { "entry" } else { "entries" },
            others.join(", ")
        ),
        NodeFate::KeepSeed => format!(" Its node {}:{} stays: it is a seed-pack node.", collection, node_id),
        NodeFate::KeepUnread(e) => format!(
            " Its node {}:{} stays: whether another entry points at it could not be read ({}).",
            collection, node_id, e
        ),
    }
}

fn doc_ids(docs: &[serde_json::Value]) -> Vec<String> {
    docs.iter()
        .filter_map(|d| d.get("_id").and_then(|v| v.as_str()).map(str::to_string))
        .collect()
}

/// Remove a memory: the entry, the node it was promoted to, and every edge
/// that touches either. The inverse of `remember`. `id` is the entry's id,
/// or the node's when exactly one entry stands behind it.
async fn forget(db: &WardsonDbClient, id: &str) -> String {
    if id.is_empty() {
        return "Provide the entry ID to forget: forget <id>".into();
    }
    let id = id.trim();

    match db.read("memory.entries", id).await {
        Ok(entry) => forget_entry(db, id, &entry).await,
        Err(e) if crate::db::error::is_not_found(&e) => forget_by_node(db, id).await,
        Err(e) => format!("Failed to remove entry: {}", e),
    }
}

/// `id` is no entry. When it is a node with one entry behind it, that
/// memory is forgotten; otherwise the answer says what the id is.
async fn forget_by_node(db: &WardsonDbClient, id: &str) -> String {
    for collection in ["memory.semantic", "memory.procedural"] {
        match db.read(collection, id).await {
            Ok(_) => {}
            Err(e) if crate::db::error::is_not_found(&e) => continue,
            Err(e) => return format!("Failed to remove entry: {}", e),
        }
        let entries = match db.query("memory.entries", &entries_of_node_query_body(collection, id)).await {
            Ok(docs) => docs,
            Err(e) => {
                return format!(
                    "Error: {} is a node ({}), and its entries could not be read ({}); nothing was removed. Run forget again.",
                    id, collection, e
                )
            }
        };
        let ids = doc_ids(&entries);
        return match ids.as_slice() {
            [] => format!(
                "{} is a node ({}) with no entry behind it. knowledge_unlink_node removes it.",
                id, collection
            ),
            [entry_id] => forget_entry(db, entry_id, &entries[0]).await,
            _ => format!(
                "{} is a node ({}) that {} entries point at ({}). forget one of them to drop that record, or knowledge_unlink_node to remove the node.",
                id,
                collection,
                ids.len(),
                ids.join(", ")
            ),
        };
    }
    format!("No memory entry or node with id {}.", id)
}

async fn forget_entry(db: &WardsonDbClient, id: &str, entry: &serde_json::Value) -> String {
    // The node the entry was promoted to, when it is still there. A read
    // that fails for another reason than "not there" stops the call: what
    // cannot be read is neither removed nor orphaned.
    let pointer = promoted_pointer(entry);
    let node = match &pointer {
        Some((collection, node_id)) => match db.read(collection, node_id).await {
            Ok(doc) => Some(doc),
            Err(e) if crate::db::error::is_not_found(&e) => None,
            Err(e) => {
                return format!(
                    "Error: the node {}:{} could not be read ({}); nothing was removed. Run forget again.",
                    collection, node_id, e
                )
            }
        },
        None => None,
    };
    let fate = match (&pointer, &node) {
        (Some((collection, node_id)), Some(doc)) => {
            let entries = db
                .query("memory.entries", &entries_of_node_query_body(collection, node_id))
                .await
                .map(|docs| doc_ids(&docs))
                .map_err(|e| e.to_string());
            node_fate(id, Some(doc), entries.as_deref().map_err(|e| e.as_str()))
        }
        _ => NodeFate::None,
    };

    if let (NodeFate::Remove, Some((collection, node_id)), Some(doc)) = (&fate, &pointer, &node) {
        // Edges first, the node next, the entry last: whatever fails, the
        // entry is still there for a second `forget` to finish from.
        let edge_count = db
            .delete_by_query("memory.edges", &forget_edge_filter(id, Some((collection, node_id.as_str()))))
            .await
            .unwrap_or(0);
        if let Err(e) = db.delete(collection, node_id).await
            && !crate::db::error::is_not_found(&e)
        {
            return format!(
                "Error: the node {}:{} could not be removed ({}); entry {} was left in place. Run forget again.",
                collection, node_id, e, id
            );
        }
        // The node is gone — drop its vector, as `knowledge_unlink_node` does.
        crate::embedding::write::forget_node(collection, node_id).await;
        if let Err(e) = db.delete("memory.entries", id).await {
            return format!(
                "Error: the node {}:{} was removed, and entry {} could not be ({}). Run forget again.",
                collection, node_id, id, e
            );
        }
        let preview_src = doc
            .get("content")
            .or_else(|| doc.get("title"))
            .and_then(|v| v.as_str())
            .unwrap_or("(no preview)");
        return format!(
            "Memory removed: entry {} and its node {}:{} (\"{}\"). {} edge(s) cascaded.",
            id,
            collection,
            node_id,
            knowledge::types::content_preview(preview_src, 80),
            edge_count
        );
    }

    // The entry alone: it has no live node, or the node stays.
    if let Err(e) = db.delete("memory.entries", id).await {
        return format!("Failed to remove entry: {}", e);
    }
    let edge_count = db
        .delete_by_query("memory.edges", &forget_edge_filter(id, None))
        .await
        .unwrap_or(0);
    let note = match &pointer {
        Some((collection, node_id)) => kept_node_note(collection, node_id, &fate),
        None => String::new(),
    };
    format!(
        "Memory entry {} removed; {} referencing edge(s) cascaded.{}",
        id, edge_count, note
    )
}

#[cfg(test)]
mod forget_tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_node_goes_with_its_only_entry() {
        let node = json!({"_id": "n1", "content": "x", "category": "fact"});
        assert_eq!(node_fate("e1", Some(&node), Ok(&ids(&["e1"]))), NodeFate::Remove);
        // A pointer query that came back empty (the entry's own pointer is
        // read before it): nothing else points there either.
        assert_eq!(node_fate("e1", Some(&node), Ok(&[])), NodeFate::Remove);
        assert_eq!(node_fate("e1", None, Ok(&[])), NodeFate::None);
    }

    #[test]
    fn a_node_other_entries_point_at_stays() {
        let node = json!({"_id": "n1", "content": "x", "category": "fact"});
        assert_eq!(
            node_fate("e1", Some(&node), Ok(&ids(&["e2", "e1", "e3"]))),
            NodeFate::KeepShared(ids(&["e2", "e3"]))
        );
    }

    #[test]
    fn a_seed_node_stays() {
        let node = json!({"_id": "seed_kg_overview", "content": "x", "origin": "knowledge_seed"});
        assert_eq!(node_fate("e1", Some(&node), Ok(&ids(&["e1"]))), NodeFate::KeepSeed);
    }

    #[test]
    fn an_unread_pointer_scan_keeps_the_node() {
        let node = json!({"_id": "n1", "content": "x"});
        assert_eq!(
            node_fate("e1", Some(&node), Err("timeout")),
            NodeFate::KeepUnread("timeout".into())
        );
    }

    #[test]
    fn the_entries_of_a_node_are_read_from_the_pointer_under_a_window() {
        let body = entries_of_node_query_body("memory.semantic", "n1");
        assert_eq!(
            body["filter"],
            json!({"promoted_to.collection": "memory.semantic", "promoted_to.id": "n1"})
        );
        assert_eq!(body["sort"], json!([{"created_at": "desc"}]));
        assert_eq!(body["limit"], json!(11));
    }

    #[test]
    fn the_forget_cascade_names_the_entry_and_the_node_on_both_sides() {
        let entry_only = forget_edge_filter("e1", None);
        assert_eq!(entry_only["$or"].as_array().unwrap().len(), 2);
        let both = forget_edge_filter("e1", Some(("memory.semantic", "n1")));
        assert_eq!(
            both["$or"],
            json!([
                {"source_id": "e1", "source_collection": "memory.entries"},
                {"target_id": "e1", "target_collection": "memory.entries"},
                {"source_id": "n1", "source_collection": "memory.semantic"},
                {"target_id": "n1", "target_collection": "memory.semantic"},
            ])
        );
    }

    #[test]
    fn only_a_node_collection_counts_as_a_pointer() {
        let semantic = json!({"promoted_to": {"collection": "memory.semantic", "id": "n1"}});
        assert_eq!(promoted_pointer(&semantic), Some(("memory.semantic", "n1".to_string())));
        assert_eq!(promoted_pointer(&json!({"promoted_to": null})), None);
        assert_eq!(promoted_pointer(&json!({})), None);
        let identity = json!({"promoted_to": {"collection": "identity.graph", "id": "operator"}});
        assert_eq!(promoted_pointer(&identity), None);
    }

    #[test]
    fn a_kept_node_is_named_with_its_reason() {
        let shared = kept_node_note("memory.semantic", "n1", &NodeFate::KeepShared(ids(&["e2"])));
        assert!(shared.contains("memory.semantic:n1 stays: 1 other entry point at it (e2)"), "{shared}");
        assert!(shared.contains("knowledge_unlink_node"), "{shared}");
        let seed = kept_node_note("memory.semantic", "seed_x", &NodeFate::KeepSeed);
        assert!(seed.contains("seed-pack node"), "{seed}");
        assert_eq!(kept_node_note("memory.semantic", "n1", &NodeFate::Remove), "");
    }

    // ── against a stub server ────────────────────────────────────────────

    fn doc(v: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": v, "meta": {}}))
    }

    fn entry_e1() -> serde_json::Value {
        json!({
            "_id": "e1", "content": "the cert refresh works", "tags": [], "session": "s",
            "promoted_to": {"collection": "memory.semantic", "id": "n1"},
        })
    }

    /// One promoted memory: entry `e1`, node `n1`. `node_read` answers the
    /// node's read; `entries` is what the pointer scan returns.
    async fn one_memory(node_read: ResponseTemplate, entries: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/memory.entries/docs/e1")).respond_with(doc(entry_e1())).mount(&server).await;
        Mock::given(method("GET")).and(path("/memory.entries/docs/n1")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
        Mock::given(method("GET")).and(path("/memory.semantic/docs/n1")).respond_with(node_read).mount(&server).await;
        Mock::given(method("POST")).and(path("/memory.entries/query")).respond_with(doc(entries)).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/memory.edges/docs/_delete_by_query"))
            .respond_with(doc(json!({"deleted": 7})))
            .mount(&server)
            .await;
        for gone in ["/memory.semantic/docs/n1", "/memory.entries/docs/e1"] {
            Mock::given(method("DELETE")).and(path(gone)).respond_with(doc(json!({}))).mount(&server).await;
        }
        server
    }

    fn live_node() -> ResponseTemplate {
        doc(json!({"_id": "n1", "content": "the cert refresh works", "category": "fact"}))
    }

    /// What the server was asked to change, in order, as "METHOD /path".
    async fn writes_seen(server: &MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "DELETE" || r.url.path().ends_with("_delete_by_query"))
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect()
    }

    #[tokio::test]
    async fn forget_removes_the_edges_the_node_and_then_the_entry() {
        let server = one_memory(live_node(), json!([entry_e1()])).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let out = forget(&db, "e1").await;
        assert_eq!(
            out,
            "Memory removed: entry e1 and its node memory.semantic:n1 (\"the cert refresh works\"). 7 edge(s) cascaded."
        );
        assert_eq!(
            writes_seen(&server).await,
            [
                "POST /memory.edges/docs/_delete_by_query",
                "DELETE /memory.semantic/docs/n1",
                "DELETE /memory.entries/docs/e1",
            ]
        );
    }

    #[tokio::test]
    async fn a_node_id_with_one_entry_is_forgotten_as_that_entry() {
        let server = one_memory(live_node(), json!([entry_e1()])).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let out = forget(&db, "n1").await;
        assert!(out.starts_with("Memory removed: entry e1 and its node memory.semantic:n1"), "{out}");
        assert_eq!(writes_seen(&server).await.len(), 3);
    }

    #[tokio::test]
    async fn a_shared_node_outlives_the_entry() {
        let other = json!({"_id": "e2", "promoted_to": {"collection": "memory.semantic", "id": "n1"}});
        let server = one_memory(live_node(), json!([other, entry_e1()])).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let out = forget(&db, "e1").await;
        assert!(out.starts_with("Memory entry e1 removed; 7 referencing edge(s) cascaded."), "{out}");
        assert!(out.contains("memory.semantic:n1 stays: 1 other entry point at it (e2)"), "{out}");
        assert_eq!(
            writes_seen(&server).await,
            ["DELETE /memory.entries/docs/e1", "POST /memory.edges/docs/_delete_by_query"]
        );
    }

    #[tokio::test]
    async fn a_node_that_cannot_be_read_stops_forget_with_nothing_removed() {
        let server = one_memory(ResponseTemplate::new(500), json!([entry_e1()])).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let out = forget(&db, "e1").await;
        assert!(out.starts_with("Error: the node memory.semantic:n1 could not be read"), "{out}");
        assert!(out.contains("nothing was removed"), "{out}");
        assert!(writes_seen(&server).await.is_empty());
    }

    #[tokio::test]
    async fn an_entry_whose_node_is_gone_is_removed_alone() {
        let server = one_memory(ResponseTemplate::new(404), json!([])).await;
        let db = WardsonDbClient::from_url(&server.uri());
        assert_eq!(forget(&db, "e1").await, "Memory entry e1 removed; 7 referencing edge(s) cascaded.");
    }

    #[tokio::test]
    async fn an_id_that_is_nothing_says_so() {
        let server = MockServer::start().await;
        let db = WardsonDbClient::from_url(&server.uri());
        assert_eq!(forget(&db, "zzz").await, "No memory entry or node with id zzz.");
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

/// Filter soul document keys by focus keyword.
/// Uses keyword-to-pattern mapping for semantic matches, plus direct key name matching.
/// Searches both top-level keys and one level deep into sub-objects.
fn filter_soul_keys(soul: &serde_json::Value, focus: &str) -> serde_json::Map<String, serde_json::Value> {
    let empty = serde_json::Map::new();
    let obj = match soul.as_object() {
        Some(o) => o,
        None => return empty,
    };

    // Keyword mapping: focus terms → key substrings to match
    let mappings: &[(&str, &[&str])] = &[
        ("ethics", &["ethical", "boundaries", "non_negotiable"]),
        ("purpose", &["invariant", "declaration", "core_truths", "purpose"]),
        ("constraints", &["boundaries", "operational", "continuity_protocol", "constraint"]),
        ("values", &["non_negotiable", "core_truths", "values"]),
    ];

    // Resolve focus to search patterns
    let mut patterns: Vec<&str> = Vec::new();
    for (keyword, terms) in mappings {
        if focus.contains(keyword) {
            patterns.extend_from_slice(terms);
        }
    }
    // Always also include the raw focus term itself as a pattern
    // (handles cases not in the mapping, e.g. "continuity")

    let matches_any_pattern = |key: &str| -> bool {
        let k = key.to_lowercase();
        // Check mapped patterns
        if patterns.iter().any(|p| k.contains(p)) {
            return true;
        }
        // Check raw focus term
        k.contains(focus)
    };

    // Filter: keep keys whose NAME matches at top level OR whose sub-keys match.
    // Only match on key names, never on values (values often contain the focus
    // term in prose, which would cause every key to match).
    obj.iter()
        .filter(|(k, v)| {
            if matches_any_pattern(k) {
                return true;
            }
            // Check sub-object key names (one level deep)
            if let Some(sub_obj) = v.as_object() {
                return sub_obj.keys().any(|sk| matches_any_pattern(sk));
            }
            false
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

async fn introspect(db: &WardsonDbClient, focus: &str) -> String {
    let focus_lower = focus.to_lowercase();

    // Knowledge graph focus — delegate to knowledge_graph_stats
    if matches!(focus_lower.as_str(), "knowledge" | "knowledge_graph" | "graph") {
        return knowledge::tools::knowledge_graph_stats(db).await;
    }

    let mut output = String::new();

    // Load soul (direct GET, fallback to query)
    let soul_doc = db.read("soul.invariant", "soul").await.ok();
    let soul_doc = match soul_doc {
        Some(doc) => Some(doc),
        None => db.query("soul.invariant", &crate::db::client::first_doc_query_body()).await.ok().and_then(|v| v.into_iter().next()),
    };
    if let Some(doc) = soul_doc {
        let mut soul = doc.get("soul").unwrap_or(&doc);

        // Unwrap double-wrapped soul: if the Brain proposed {"soul": {...}},
        // seal_soul wraps it again as {"soul": {"soul": {...}}}.
        // Keep unwrapping until we reach the actual content keys.
        while let Some(inner) = soul.get("soul") {
            if inner.is_object() {
                soul = inner;
            } else {
                break;
            }
        }

        if crate::identity_graph::is_graph_soul(soul) {
            // Graph mode: grouped prose for the full view; key-name
            // filtering makes no sense over a graph value, so focused
            // views get the render + an explanatory line.
            if focus.is_empty() || focus_lower == "soul" {
                output.push_str("=== SOUL (IMMUTABLE, IDENTITY GRAPH) ===\n");
                output.push_str(&crate::brain::render_sealed_graph(soul));
            } else {
                output.push_str(&format!(
                    "=== SOUL — {} ===\n(The sealed soul is an identity graph; focused key \
                     filtering does not apply. Full graph below — traverse it with the \
                     knowledge tools.)\n",
                    focus
                ));
                output.push_str(&crate::brain::render_sealed_graph(soul));
            }
            output.push('\n');
        } else if focus.is_empty() || focus_lower == "soul" {
            // No focus or "soul" → show full soul document
            output.push_str("=== SOUL (IMMUTABLE) ===\n");
            output.push_str(&serde_json::to_string_pretty(soul).unwrap_or_default());
            output.push('\n');
        } else {
            // Focused view — build a filtered soul object
            let filtered = filter_soul_keys(soul, &focus_lower);
            if !filtered.is_empty() {
                output.push_str(&format!("=== SOUL — {} ===\n", focus));
                output.push_str(&serde_json::to_string_pretty(&serde_json::Value::Object(filtered)).unwrap_or_default());
                output.push('\n');
            }
        }
    }

    // Load identity (direct GET, fallback to query)
    if focus.is_empty() || focus_lower.contains("identity") || focus_lower.contains("personality") || focus_lower.contains("traits") {
        let id_doc = db.read("memory.identity", "identity").await.ok();
        let id_doc = match id_doc {
            Some(doc) => Some(doc),
            None => db.query("memory.identity", &crate::db::client::first_doc_query_body()).await.ok().and_then(|v| v.into_iter().next()),
        };
        if let Some(doc) = id_doc {
            output.push_str("\n=== IDENTITY ===\n");
            output.push_str(&serde_json::to_string_pretty(&doc).unwrap_or_default());
            output.push('\n');
        } else if !output.is_empty() && output.contains("IDENTITY GRAPH") {
            // Graph mode without a memory.identity doc (imported
            // instances): identity lives in the sealed graph above.
            output.push_str("\n=== IDENTITY ===\n(part of the sealed identity graph above)\n");
        }
    }

    // Load user profile (direct GET, fallback to query)
    if focus.is_empty() || focus_lower.contains("user") || focus_lower.contains("operator") {
        let user_doc = db.read("memory.user", "user").await.ok();
        let user_doc = match user_doc {
            Some(doc) => Some(doc),
            None => db.query("memory.user", &crate::db::client::first_doc_query_body()).await.ok().and_then(|v| v.into_iter().next()),
        };
        if let Some(doc) = user_doc {
            output.push_str("\n=== USER PROFILE ===\n");
            if crate::identity_graph::is_graph_soul(&doc) {
                // Post-transition memory.user is graph-shaped — grouped
                // operator prose instead of the raw graph JSON.
                output.push_str(&crate::brain::render_user_graph(&doc));
            } else {
                output.push_str(&serde_json::to_string_pretty(&doc).unwrap_or_default());
            }
            output.push('\n');
        }
    }

    if output.is_empty() {
        "No documents found for the requested focus area.".into()
    } else {
        output
    }
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

// ── Time & Context Tools ──

fn time_now(config_tz: &str) -> String {
    let now = Utc::now();

    // Resolve abbreviations to IANA names before parsing (BUG-007)
    let resolved = resolve_timezone(config_tz);
    let config_tz = &resolved;

    // Try to parse the configured timezone
    if let Ok(tz) = config_tz.parse::<chrono_tz::Tz>() {
        let local = now.with_timezone(&tz);
        format!(
            "Current time: {} ({})\nDay: {}\nUnix timestamp: {}",
            local.format("%Y-%m-%d %H:%M:%S %Z"),
            config_tz,
            local.format("%A"),
            now.timestamp()
        )
    } else {
        // Fallback to UTC with timezone label
        format!(
            "Current time: {} (configured tz: {})\nDay: {}\nUnix timestamp: {}",
            now.format("%Y-%m-%d %H:%M:%S UTC"),
            config_tz,
            now.format("%A"),
            now.timestamp()
        )
    }
}

async fn countdown(db: &WardsonDbClient, param: &str, act: bool) -> String {
    if param.is_empty() {
        return "Usage: countdown <duration> <message>\nExample: countdown 5m Check the build".into();
    }

    // Parse: "5m Check the build" or "20 minutes reminder text"
    let parts: Vec<&str> = param.splitn(2, ' ').collect();
    let (duration_str, message) = if parts.len() == 2 {
        (parts[0], parts[1])
    } else {
        (parts[0], "Reminder")
    };

    let seconds = parse_duration(duration_str);
    if seconds == 0 {
        return format!("Could not parse duration '{}'. Use formats like: 5m, 30s, 1h, '20 minutes'", duration_str);
    }

    let now = Utc::now();
    let trigger_at = now + chrono::Duration::seconds(seconds as i64);

    ensure_collection(db, "reminders").await;

    let doc = reminder_doc(message, trigger_at, now, act);

    match db.write("reminders", &doc).await {
        Ok(id) => format!(
            "Reminder set. Will fire at {} (in {}s).{}\nID: {}",
            trigger_at.format("%H:%M:%S UTC"),
            seconds,
            if act { " You will be given a turn to act on it when it fires." } else { "" },
            id
        ),
        Err(e) => format!("Failed to set reminder: {}", e),
    }
}

/// A reminder as it is stored. `act` asks for a model turn when it fires
/// (`proactive::ActTrigger`); a record without the field does not.
fn reminder_doc(
    message: &str,
    trigger_at: chrono::DateTime<Utc>,
    now: chrono::DateTime<Utc>,
    act: bool,
) -> serde_json::Value {
    serde_json::json!({
        "message": message,
        "trigger_at": trigger_at.to_rfc3339(),
        "created_at": now.to_rfc3339(),
        "fired": false,
        "act": act,
    })
}

/// Which reminders `reminder_list` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReminderFilter {
    #[default]
    Pending,
    Fired,
    All,
}

impl ReminderFilter {
    fn label(self) -> &'static str {
        match self {
            ReminderFilter::Pending => "pending",
            ReminderFilter::Fired => "fired",
            ReminderFilter::All => "all",
        }
    }
}

/// `reminder_list`'s default and most.
pub(crate) const REMINDER_LIST_DEFAULT: usize = 20;
pub(crate) const REMINDER_LIST_MAX: usize = 100;

/// The limit a call asked for, within bounds.
pub(crate) fn reminder_list_limit(limit: Option<u32>) -> usize {
    (limit.map(|n| n as usize).unwrap_or(REMINDER_LIST_DEFAULT)).clamp(1, REMINDER_LIST_MAX)
}

/// The query behind a filter: pending reminders soonest first (a record
/// without `fired` is pending, hence the `$or`, as in
/// `due_reminders_query_body`), fired ones newest first. `All` reads the
/// whole collection instead (`fetch_collection`); there is no query.
pub(crate) fn reminder_list_query_body(filter: ReminderFilter, limit: usize) -> Option<serde_json::Value> {
    match filter {
        ReminderFilter::Pending => Some(serde_json::json!({
            "filter": {"$or": [{"fired": false}, {"fired": {"$exists": false}}]},
            "sort": [{"trigger_at": "asc"}, {"_id": "asc"}],
            "limit": limit,
        })),
        ReminderFilter::Fired => Some(serde_json::json!({
            "filter": {"fired": true},
            "sort": [{"trigger_at": "desc"}, {"_id": "desc"}],
            "limit": limit,
        })),
        ReminderFilter::All => None,
    }
}

/// A stored time in the operator's zone, or the text as stored.
fn reminder_local_time(rfc3339: &str, tz: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(rfc3339) else {
        return rfc3339.to_string();
    };
    let zone: chrono_tz::Tz = tz.parse().unwrap_or(chrono_tz::UTC);
    t.with_timezone(&zone).format("%Y-%m-%d %H:%M %Z").to_string()
}

/// `reminder_list`'s text: the short id, the message, when it is or was
/// due, and for a fired one when it fired.
pub(crate) fn render_reminder_list(
    docs: &[serde_json::Value],
    filter: ReminderFilter,
    limit: usize,
    tz: &str,
) -> String {
    if docs.is_empty() {
        return match filter {
            ReminderFilter::Pending => "No pending reminders. Set one with countdown.".to_string(),
            ReminderFilter::Fired => "No fired reminders in the last seven days.".to_string(),
            ReminderFilter::All => "No reminders.".to_string(),
        };
    }
    let more = if docs.len() >= limit {
        format!(" (the first {limit}; raise limit for more)")
    } else {
        String::new()
    };
    let mut out = format!("=== Reminders ({}: {}{}) ===\n", filter.label(), docs.len(), more);
    for doc in docs {
        let id = doc
            .get("_id")
            .or(doc.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let short: String = id.chars().take(8).collect();
        let message = doc.get("message").and_then(|v| v.as_str()).unwrap_or("Reminder");
        let due = doc.get("trigger_at").and_then(|v| v.as_str()).unwrap_or("?");
        let fired = doc.get("fired").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut line = format!("  {}  {}  due {}", short, message, reminder_local_time(due, tz));
        if fired {
            match doc.get("fired_at").and_then(|v| v.as_str()) {
                Some(at) => line.push_str(&format!("  fired {}", reminder_local_time(at, tz))),
                None => line.push_str("  fired"),
            }
        }
        line.push('\n');
        out.push_str(&line);
    }
    out
}

async fn reminder_list(db: &WardsonDbClient, filter: ReminderFilter, limit: usize, tz: &str) -> String {
    ensure_collection(db, "reminders").await;
    let docs = match reminder_list_query_body(filter, limit) {
        Some(body) => db.query("reminders", &body).await.unwrap_or_default(),
        None => {
            let mut all = db.fetch_collection("reminders").await.unwrap_or_default();
            all.sort_by(|a, b| {
                let ta = a.get("trigger_at").and_then(|v| v.as_str()).unwrap_or("");
                let tb = b.get("trigger_at").and_then(|v| v.as_str()).unwrap_or("");
                tb.cmp(ta)
            });
            all.truncate(limit);
            all
        }
    };
    render_reminder_list(&docs, filter, limit, tz)
}

/// What a reminder's lifetime is counted from, and for how long: it is
/// removed `REMINDER_RETENTION_DAYS` after it was DUE. Counted from
/// `created_at`, as migration v4 set it up, a reminder for more than seven
/// days ahead was removed before it could fire. The policy is asserted on
/// every boot (`migrations::ensure_ttl_policies`).
pub(crate) const REMINDER_TTL_FIELD: &str = "trigger_at";
pub(crate) const REMINDER_RETENTION_DAYS: u64 = 7;

/// Most due reminders one check looks at, earliest trigger first.
const REMINDER_WINDOW: usize = 500;

/// The reminders that are due at `now` and have not fired, earliest trigger
/// first, under an explicit window. A record without `fired` counts as not
/// fired (BUG-003), and WardSONDB matches a missing field with `$exists`
/// only, hence the `$or`. An `$or` is a collection scan; this collection is
/// small and reaped after seven days (migration v4), and nothing here is a
/// hot path. `trigger_at` compares as a string on the server, exactly as
/// `due_reminders` compares it here.
pub(crate) fn due_reminders_query_body(now: &str, limit: usize) -> serde_json::Value {
    serde_json::json!({
        "filter": {
            "trigger_at": {"$lte": now},
            "$or": [{"fired": false}, {"fired": {"$exists": false}}],
        },
        "sort": [{"trigger_at": "asc"}, {"_id": "asc"}],
        "limit": limit,
    })
}

/// Of `docs`, the reminders due at `now` that have not fired — earliest
/// trigger first, at most `max`. Timestamps compare as the RFC 3339 strings
/// they are stored as.
fn due_reminders<'a>(
    docs: &'a [serde_json::Value],
    now: &str,
    max: usize,
) -> Vec<&'a serde_json::Value> {
    let mut due: Vec<(&str, &serde_json::Value)> = docs
        .iter()
        .filter(|doc| {
            // Missing `fired` field means not yet fired (BUG-003 fix)
            !doc.get("fired").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .filter_map(|doc| {
            let trigger = doc.get("trigger_at").and_then(|v| v.as_str())?;
            (!trigger.is_empty() && trigger <= now).then_some((trigger, doc))
        })
        .collect();
    due.sort_by_key(|(trigger, _)| *trigger);
    due.into_iter().take(max).map(|(_, doc)| doc).collect()
}

/// A reminder that fired: the line the console shows, the message, and
/// whether the reminder asked for a turn (`act`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiredReminder {
    pub text: String,
    pub message: String,
    pub act: bool,
}

/// Fire the reminders that are due, at most `max` of them, and return them.
/// Called by the proactive engine, which passes the number of
/// notifications it can hand over right now: firing marks a reminder as
/// fired, so one that fires without a place to go is lost. What does not
/// fit stays in the store and fires on a later check.
pub async fn check_reminders(db: &WardsonDbClient, max: usize) -> Vec<FiredReminder> {
    if max == 0 {
        return Vec::new();
    }
    let now = Utc::now().to_rfc3339();
    let reminders = db
        .query("reminders", &due_reminders_query_body(&now, REMINDER_WINDOW))
        .await
        .unwrap_or_default();
    if crate::db::client::window_saturated(reminders.len(), REMINDER_WINDOW) {
        tracing::warn!(
            target: "wardsondb::window",
            collection = "reminders",
            limit = REMINDER_WINDOW,
            "due-reminder window saturated — the latest triggers wait for earlier ones to fire"
        );
    }

    let mut fired = Vec::new();

    for doc in due_reminders(&reminders, &now, max) {
        let message = doc
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Reminder");

        fired.push(FiredReminder {
            text: format!("Reminder: {}", message),
            message: message.to_string(),
            act: doc.get("act").and_then(|v| v.as_bool()).unwrap_or(false),
        });

        // Mark as fired, and when.
        if let Some(id) = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()) {
            let _ = db.update("reminders", id, &mark_fired(doc, &now)).await;
        }
    }

    fired
}

/// The reminder as it is stored once it fired: `fired` and the time it
/// fired (`fired_at`), which the events block of the next turn and
/// `reminder_list` read. A record from before `fired_at` existed still
/// reads as fired, without a time.
fn mark_fired(doc: &serde_json::Value, now: &str) -> serde_json::Value {
    let mut updated = doc.clone();
    updated["fired"] = serde_json::json!(true);
    updated["fired_at"] = serde_json::json!(now);
    updated
}

/// Byte cap for `session_summary` transcript-line previews. Boundary-safe:
/// the previous raw `&content[..200]` slice panicked when a multi-byte char
/// straddled the boundary. 500 (up from 200) is a modest raise; the 20-turn
/// window stays — it is pinned by the tool's description ("last 20 turns"),
/// and widening it would spend a description-string prompt-cache event.
const SESSION_SUMMARY_PREVIEW_MAX: usize = 500;

fn summary_preview(content: &str) -> String {
    if content.len() > SESSION_SUMMARY_PREVIEW_MAX {
        format!(
            "{}...",
            sessions::truncate_str(content, SESSION_SUMMARY_PREVIEW_MAX)
        )
    } else {
        content.to_string()
    }
}

async fn session_summary(db: &WardsonDbClient, session_name: &str) -> String {
    let collection = format!("sessions.{}.history", session_name);
    let results = db
        .query(&collection, &crate::sessions::history_query_body())
        .await
        .unwrap_or_default();

    if let Some(doc) = results.into_iter().next()
        && let Some(turns) = doc.get("turns").and_then(|v| v.as_array())
    {
        let total = turns.len();
        let user_msgs = turns.iter().filter(|t| t.get("role").and_then(|r| r.as_str()) == Some("user")).count();
        let ai_msgs = total - user_msgs;

        let mut output = format!(
            "Session '{}' summary:\nTotal messages: {} ({} from user, {} from assistant)\n\nConversation:\n",
            session_name, total, user_msgs, ai_msgs
        );

        // Include the last 20 messages for context
        let recent = if turns.len() > 20 {
            &turns[turns.len() - 20..]
        } else {
            turns
        };

        for turn in recent {
            let role = turn.get("role").and_then(|r| r.as_str()).unwrap_or("?");
            let content = turn.get("content").and_then(|c| c.as_str()).unwrap_or("");
            output.push_str(&format!("[{}]: {}\n", role, summary_preview(content)));
        }

        return output;
    }

    format!("No conversation history found for session '{}'.", session_name)
}

#[cfg(test)]
mod session_summary_preview_tests {
    use super::{summary_preview, SESSION_SUMMARY_PREVIEW_MAX};

    #[test]
    fn boundary_safe_on_multibyte() {
        // '€' is 3 bytes: byte 500 lands mid-char (500 % 3 == 2) — the raw
        // `&content[..N]` slice this replaced panicked exactly here.
        let content = "€".repeat(200); // 600 bytes
        let preview = summary_preview(&content);
        assert!(preview.ends_with("..."));
        assert!(preview.len() <= SESSION_SUMMARY_PREVIEW_MAX + 3);
        assert!(preview.strip_suffix("...").unwrap().len().is_multiple_of(3));
    }

    #[test]
    fn short_content_untouched() {
        let content = "short and sweet";
        assert_eq!(summary_preview(content), content);
    }
}

// ── Utility Tools ──

fn calculate(expression: &str) -> String {
    if expression.is_empty() {
        return "Usage: calculate <expression>\nExample: calculate 2 ** 10".into();
    }

    // Exponent is ** (Python/Rust convention). Reject bare ^ up-front so it
    // never silently resolves to the evaluator's native power operator — in
    // Python ^ is XOR, and this tool does not support XOR. Detect ^ before
    // translating ** → ^ for the evaluator.
    if expression.contains('^') {
        return format!(
            "Could not evaluate '{}': '^' is not supported. Use ** for exponent (e.g. 2 ** 10). XOR is not available in this tool.",
            expression
        );
    }
    let normalized = expression.replace("**", "^");

    match calc::eval(&normalized) {
        Ok(result) => {
            if result == result.floor() && result.abs() < 1e15 {
                format!("{} = {}", expression, result as i64)
            } else {
                format!("{} = {}", expression, result)
            }
        }
        Err(e) => format!("Could not evaluate '{}': {}", expression, e),
    }
}

#[cfg(test)]
mod calculate_tests {
    use super::calculate;

    /// The quoted names that open the match arms of one function of
    /// `calc.rs`, read from its source: what the evaluator knows.
    fn arm_names(from: &str, to: &str) -> Vec<&'static str> {
        let src = include_str!("calc.rs");
        let start = src.find(from).expect("start marker in calc.rs");
        let end = start + src[start..].find(to).expect("end marker in calc.rs");
        let mut names = Vec::new();
        let mut rest = &src[start..end];
        while let Some(open) = rest.find('"') {
            let after = &rest[open + 1..];
            let Some(close) = after.find('"') else { break };
            let tail = &after[close + 1..];
            if tail.trim_start().starts_with("=>") {
                names.push(&after[..close]);
            }
            rest = tail;
        }
        names
    }

    /// The description names every function and constant the evaluator
    /// knows. It used to list the operators only, and the model took a
    /// working `sin(0.5)` for something leaking through.
    #[test]
    fn the_description_names_every_function_and_constant() {
        let desc = crate::tools::registry::all_descriptors()
            .find(|d| d.name == "calculate")
            .expect("calculate registered")
            .description;
        let words: Vec<&str> = desc
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .collect();
        let functions = arm_names("fn call(", "fn eval_rpn(");
        let constants = arm_names("fn constant(", "fn call(");
        // The scan itself works: a few names it must find.
        for known in ["sqrt", "atan2", "max", "min"] {
            assert!(functions.contains(&known), "{known} not found in calc.rs: {functions:?}");
        }
        assert_eq!(constants, ["pi", "e"]);
        for name in functions.iter().chain(&constants) {
            assert!(words.contains(name), "`{name}` is not named in the description: {desc}");
        }
    }

    #[test]
    fn the_exponent_is_written_with_two_stars() {
        assert_eq!(calculate("2 ** 10"), "2 ** 10 = 1024");
        // Right-associative, as the evaluator's own `^`.
        assert_eq!(calculate("2 ** 3 ** 2"), "2 ** 3 ** 2 = 512");
    }

    #[test]
    fn a_bare_caret_is_refused_and_not_read_as_a_power() {
        let out = calculate("2 ^ 10");
        assert!(out.contains("'^' is not supported"), "{out}");
        assert!(!out.contains("1024"), "{out}");
    }
}

async fn define(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: define <term> to look up, define <term> | <definition> to add/update, or define delete <term> to remove".into();
    }

    ensure_collection(db, "knowledge.definitions").await;

    // Delete form: `delete <term>` (case-insensitive prefix).
    let trimmed = param.trim();
    if let Some(rest) = trimmed
        .strip_prefix("delete ")
        .or_else(|| trimmed.strip_prefix("Delete "))
        .or_else(|| trimmed.strip_prefix("DELETE "))
    {
        let term = rest.trim();
        if term.is_empty() {
            return "define rejected (delete requires a term)".into();
        }
        let results = db
            .fetch_collection("knowledge.definitions")
            .await
            .unwrap_or_default();
        let match_id = results.iter().find_map(|doc| {
            let doc_term = doc.get("term").and_then(|v| v.as_str()).unwrap_or("");
            if doc_term.eq_ignore_ascii_case(term) {
                doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()).map(|s| s.to_string())
            } else {
                None
            }
        });
        return match match_id {
            Some(id) => match db.delete("knowledge.definitions", &id).await {
                Ok(()) => format!("Definition deleted: {}", term),
                Err(e) => format!("define failed (delete '{}': {})", term, e),
            },
            None => format!("define rejected (no definition for '{}' found)", term),
        };
    }

    // DESIGN-003: If param contains ` | `, split into term + definition and write
    if let Some(pipe_pos) = param.find(" | ") {
        let term = param[..pipe_pos].trim();
        let definition = param[pipe_pos + 3..].trim();

        if term.is_empty() || definition.is_empty() {
            return "Usage: define <term> | <definition>".into();
        }

        let results = db
            .fetch_collection("knowledge.definitions")
            .await
            .unwrap_or_default();

        // Check for existing definition to upsert
        let existing_id = results.iter().find_map(|doc| {
            let doc_term = doc.get("term").and_then(|v| v.as_str()).unwrap_or("");
            if doc_term.to_lowercase() == term.to_lowercase() {
                doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()).map(|s| s.to_string())
            } else {
                None
            }
        });

        let doc = serde_json::json!({
            "term": term,
            "definition": definition,
            "updated_at": Utc::now().to_rfc3339(),
        });

        if let Some(id) = existing_id {
            match db.update("knowledge.definitions", &id, &doc).await {
                Ok(()) => return format!("Definition updated: {} — {}", term, definition),
                Err(e) => return format!("Failed to update definition: {}", e),
            }
        } else {
            match db.write("knowledge.definitions", &doc).await {
                Ok(id) => return format!("Definition saved: {} — {} (ID: {})", term, definition, id),
                Err(e) => return format!("Failed to save definition: {}", e),
            }
        }
    }

    // Lookup mode
    let term = param;
    let term_lower = term.to_lowercase();
    let results = db
        .fetch_collection("knowledge.definitions")
        .await
        .unwrap_or_default();

    for doc in &results {
        let doc_term = doc
            .get("term")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if doc_term.to_lowercase() == term_lower {
            let definition = doc
                .get("definition")
                .and_then(|v| v.as_str())
                .unwrap_or("(no definition)");
            return format!("{}: {}", doc_term, definition);
        }
    }

    // Not found — offer to add (plain text, no tool tag syntax to avoid BUG-001 re-parse)
    format!(
        "No local definition found for '{}'. To add one, use: define {} | your definition here",
        term, term
    )
}

async fn draft(db: &WardsonDbClient, param: &str, session: &str) -> String {
    if param.is_empty() {
        return "draft rejected (missing arguments). Usage: draft <title> | <content> or draft delete <title>\nSeparate title and content with ' | '.\nExample: draft Meeting Notes | Key decisions: ...".into();
    }

    ensure_collection(db, "drafts").await;

    // Delete form: `delete <title>` (case-insensitive prefix).
    let trimmed = param.trim();
    if let Some(rest) = trimmed
        .strip_prefix("delete ")
        .or_else(|| trimmed.strip_prefix("Delete "))
        .or_else(|| trimmed.strip_prefix("DELETE "))
    {
        let title = rest.trim();
        if title.is_empty() {
            return "draft rejected (delete requires a title)".into();
        }
        let existing = db.fetch_collection("drafts").await.unwrap_or_default();
        let match_id = existing.iter().find_map(|doc| {
            let doc_title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("");
            if doc_title.eq_ignore_ascii_case(title) {
                doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()).map(|s| s.to_string())
            } else {
                None
            }
        });
        return match match_id {
            Some(id) => match db.delete("drafts", &id).await {
                Ok(()) => format!("Draft deleted: '{}' (ID: {})", title, id),
                Err(e) => format!("draft failed (delete '{}': {})", title, e),
            },
            None => format!("draft rejected (no draft titled '{}' found)", title),
        };
    }

    // Parse "title | content" or treat entire param as content with auto-title.
    let (title, content) = if let Some(pos) = param.find(" | ") {
        let t = param[..pos].trim();
        let c = param[pos + 3..].trim();
        if t.is_empty() {
            return "draft rejected (title is empty before the '|')".into();
        }
        if c.is_empty() {
            return "draft rejected (content is empty after the '|')".into();
        }
        (t, c)
    } else {
        ("Untitled Draft", param)
    };

    // DESIGN-001: Check for existing draft with same title and upsert
    let existing = db.fetch_collection("drafts").await.unwrap_or_default();
    let existing_id = existing.iter().find_map(|doc| {
        let doc_title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if doc_title.eq_ignore_ascii_case(title) {
            doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()).map(|s| s.to_string())
        } else {
            None
        }
    });

    let doc = serde_json::json!({
        "title": title,
        "content": content,
        "session": session,
        "updated_at": Utc::now().to_rfc3339(),
    });

    if let Some(id) = existing_id {
        match db.update("drafts", &id, &doc).await {
            Ok(()) => format!("Draft updated: '{}' (ID: {})", title, id),
            Err(e) => format!("draft failed (update '{}': {})", title, e),
        }
    } else {
        let mut doc = doc;
        doc["created_at"] = serde_json::json!(Utc::now().to_rfc3339());
        match db.write("drafts", &doc).await {
            Ok(id) => format!("Draft created: '{}' (ID: {})", title, id),
            Err(e) => format!("draft failed (save '{}': {})", title, e),
        }
    }
}

// ── Get Tool (DESIGN-002) ──

async fn get(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: get <collection> <id>\nExample: get memory.entries abc123".into();
    }

    let parts: Vec<&str> = param.splitn(2, ' ').collect();
    if parts.len() < 2 {
        return "Usage: get <collection> <id>".into();
    }

    let (collection, id) = (parts[0], parts[1].trim());

    match db.read(collection, id).await {
        Ok(doc) => serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "Failed to format document".into()),
        Err(e) => format!("Failed to read {}/{}: {}", collection, id, e),
    }
}


// ── Timezone Resolution ──

/// Map common timezone abbreviations to IANA names.
/// Passes through anything that doesn't match a known abbreviation.
pub fn resolve_timezone(input: &str) -> String {
    match input.trim().to_uppercase().as_str() {
        "PST" | "PDT" => "America/Los_Angeles".into(),
        "EST" | "EDT" => "America/New_York".into(),
        "CST" | "CDT" => "America/Chicago".into(),
        "MST" | "MDT" => "America/Denver".into(),
        "AKST" | "AKDT" => "America/Anchorage".into(),
        "HST" => "Pacific/Honolulu".into(),
        "UTC" | "GMT" => "Etc/UTC".into(),
        _ => input.trim().to_string(),
    }
}

// ── Helpers ──

pub fn parse_duration(s: &str) -> u64 {
    let s = s.trim().to_lowercase();

    // Try "5m", "30s", "1h" patterns
    if let Some(num) = s.strip_suffix('s') {
        return num.parse().unwrap_or(0);
    }
    if let Some(num) = s.strip_suffix('m') {
        return num.parse::<u64>().unwrap_or(0) * 60;
    }
    if let Some(num) = s.strip_suffix('h') {
        return num.parse::<u64>().unwrap_or(0) * 3600;
    }

    // Try "5 minutes", "30 seconds", "1 hour"
    if let Some(num) = s.strip_suffix("minutes").or(s.strip_suffix("minute")) {
        return num.trim().parse::<u64>().unwrap_or(0) * 60;
    }
    if let Some(num) = s.strip_suffix("seconds").or(s.strip_suffix("second")) {
        return num.trim().parse::<u64>().unwrap_or(0);
    }
    if let Some(num) = s.strip_suffix("hours").or(s.strip_suffix("hour")) {
        return num.trim().parse::<u64>().unwrap_or(0) * 3600;
    }

    // Try bare number as seconds
    s.parse().unwrap_or(0)
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

// ── Native tool-use registrations (NATIVE-TOOLS-01) ──
//
// Typed args structs for every tool whose implementation lives in this
// module. Each `#[embra_tool(name, description)]` attribute submits a
// `ToolDescriptor` into the global inventory at compile time. The legacy
// string dispatcher at the top of this file remains the active call path
// through Stage 2; Stage 3 removes it and routes exclusively through
// `registry::dispatch`.

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;

use crate::tools::registry::DispatchContext;

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

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "time",
    description = "Current date, time, and day of week in the configured timezone."
)]
pub struct TimeArgs {}

impl TimeArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(time_now(ctx.config_tz))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "session_summary",
    description = "Summarize the current conversation: message counts and a preview of the last 20 turns."
)]
pub struct SessionSummaryArgs {}

impl SessionSummaryArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(session_summary(ctx.db, ctx.session_name).await)
    }
}

// -- Single-field tools --------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "recall",
    description = "Search past conversations and saved memories. Free-text query; unquoted terms AND-match (all must appear); hashtags supported; empty query lists recent entries. A memory that is in the knowledge graph is listed once, as its node. Set unpromoted_only=true to list the memory.entries that are not in the knowledge graph (up to 200 shown, newest first; query still narrows it)."
)]
pub struct RecallArgs {
    /// Search query. Free-text; hashtags supported; empty to list all.
    #[serde(default)]
    pub query: String,
    /// When true, list only memory.entries that have NOT been promoted to
    /// the knowledge graph (no promoted_to pointer): saved before remember
    /// wrote nodes, left by a failed promotion, or un-promoted when their
    /// node was removed.
    #[serde(default)]
    pub unpromoted_only: bool,
}

impl RecallArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(recall(ctx.db, &self.query, self.unpromoted_only).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "memory_search",
    description = "Alias for recall. Search past memories by free-text query; unquoted terms AND-match; hashtags supported."
)]
pub struct MemorySearchArgs {
    #[serde(default)]
    pub query: String,
}

impl MemorySearchArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        RecallArgs { query: self.query, unpromoted_only: false }.run(ctx).await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "search_memory",
    description = "Alias for recall. Search past memories by free-text query; hashtags supported."
)]
pub struct SearchMemoryArgs {
    #[serde(default)]
    pub query: String,
}

impl SearchMemoryArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        RecallArgs { query: self.query, unpromoted_only: false }.run(ctx).await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "remember",
    is_side_effectful = true,
    description = "Save a memory worth keeping across sessions. One call writes the episodic entry and its node in the knowledge graph: a semantic node with the category you give, or a procedural node when procedure is given. No knowledge_promote call follows. Hashtag tokens (e.g. #architecture, #soul) are extracted into the tags array; the remaining words become the content. Keep content to a single line. The result names the new node and lists the nearest existing nodes: in the same turn, without being asked, link the new node with knowledge_link to each one it has a real relation to, and to none that is only similar in wording."
)]
pub struct RememberArgs {
    /// Content to save. Letter-start `#tag` tokens are extracted into tags.
    pub content: String,
    /// Category of the semantic node. fact: something that is the case.
    /// preference: how the operator likes to work. decision: a choice that
    /// was made, with its reason. observation: something noticed and not
    /// yet established. pattern: something that recurs. Give it on every
    /// call; without it the node is an observation. Not used when procedure
    /// is given.
    #[serde(default = "default_remember_category")]
    pub category: knowledge::types::SemanticCategory,
    /// A how-to with steps, as a JSON object serialized to a string:
    /// {"title": "...", "description": "...", "preconditions": ["..."],
    /// "steps": [{"order": 1, "action": "...", "notes": "..."}], "outcomes":
    /// {"success": "...", "failure": "..."}}. When given, the node is
    /// procedural.
    #[serde(default)]
    pub procedure: Option<String>,
}

fn default_remember_category() -> knowledge::types::SemanticCategory {
    knowledge::types::SemanticCategory::Observation
}

impl RememberArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let promotion = match remember_plan(self.category, self.procedure.as_deref()) {
            Ok(promotion) => promotion,
            Err(refusal) => return Ok(refusal),
        };
        Ok(remember(ctx.db, &self.content, promotion, ctx.session_name, ctx.config).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "forget",
    is_side_effectful = true,
    description = "Remove a memory: its entry, the node promoted from it, and every edge that touches either. id is the entry id or the node id. The node stays when another entry also points at it or when it comes from a seed pack; the result says so. To remove a node and keep its entry, use knowledge_unlink_node. Destructive; confirm with the user first."
)]
pub struct ForgetArgs {
    /// Entry id or node id of the memory to remove.
    pub id: String,
}

impl ForgetArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(forget(ctx.db, &self.id).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "introspect",
    description = "Reflect on your own soul and identity documents. Pass focus to narrow the output to a specific soul key (e.g. \"purpose\", \"ethics\", \"constraints\"); omit for a full read."
)]
pub struct IntrospectArgs {
    /// Optional focus keyword (soul key to read). Empty for full.
    #[serde(default)]
    pub focus: String,
}

impl IntrospectArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(introspect(ctx.db, &self.focus).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "set_name",
    is_side_effectful = true,
    description = "Change your own display name — the name shown in the console message prefix and status bar, and the one your system prompt opens with. Updates SystemConfig.name and the identity document's name field; the sealed soul is untouched. Use ONLY after the operator has explicitly agreed to the new name in this conversation — never rename unilaterally. Takes effect immediately in the console and from your next turn in the prompt."
)]
pub struct SetNameArgs {
    /// The new display name (single line, 1–40 characters).
    pub new_name: String,
}

impl SetNameArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let new_name = crate::config::validate_intelligence_name(&self.new_name)
            .map_err(|e| DispatchError::Handler(format!("set_name rejected: {e}")))?;

        // Load fresh — ctx.config is the turn's snapshot and may be stale.
        let mut cfg = crate::config::load_config(ctx.db)
            .await
            .map_err(|e| DispatchError::Handler(format!("set_name: config load failed: {e}")))?;
        let old_name = cfg.name.clone();
        if old_name == new_name {
            return Ok(format!("Name is already '{new_name}' — nothing changed."));
        }
        cfg.name = new_name.clone();
        crate::config::save_config(ctx.db, &cfg)
            .await
            .map_err(|e| DispatchError::Handler(format!("set_name: config save failed: {e}")))?;

        // Keep the identity portrait's Name: line in step. The identity
        // doc is unsealed and mutable (unlike the soul — which never
        // contains the name, so trustd verification is unaffected).
        // Missing doc (unusual, but possible pre-learning) is not an
        // error — config is the authoritative display name.
        let identity_note = match ctx.db.read("memory.identity", "identity").await {
            Ok(mut doc) => {
                if let Some(obj) = doc.as_object_mut() {
                    obj.insert("name".into(), serde_json::json!(new_name.clone()));
                }
                match ctx.db.update("memory.identity", "identity", &doc).await {
                    Ok(_) => "config and identity documents updated",
                    Err(_) => "config updated; identity document update failed (non-fatal)",
                }
            }
            Err(_) => "config updated; no identity document found to sync",
        };

        info!(
            target: "dispatch",
            old = %old_name,
            new = %new_name,
            "intelligence display name changed via set_name"
        );
        Ok(format!(
            "Display name changed: '{old_name}' → '{new_name}' ({identity_note}). \
             The console prefix and status bar refresh immediately; your system \
             prompt carries the new name from the next turn. The sealed soul is \
             unchanged."
        ))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "countdown",
    description = "Set a reminder to fire after a duration. duration examples: \"5m\", \"30s\", \"1h\", \"20 minutes\". message defaults to \"Reminder\" if omitted. When it fires the operator is notified and your next turn carries it; with act=true you are also given a turn of your own when it fires, in the active session, to act on the message with your tools."
)]
pub struct CountdownArgs {
    /// Duration: "5m", "30s", "1h", "20 minutes".
    pub duration: String,
    /// Reminder message shown when the countdown fires.
    #[serde(default = "default_countdown_message")]
    pub message: String,
    /// Start a model turn in the active session when this fires, so you
    /// can act on it. Default false: the reminder is shown and listed only.
    #[serde(default)]
    pub act: Option<bool>,
}

fn default_countdown_message() -> String {
    "Reminder".into()
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "reminder_list",
    description = "List the reminders set with countdown: pending ones soonest first (the default), fired ones newest first with when they fired, or all. Each line gives the reminder's short id, its message and when it is or was due. A fired reminder is kept seven days past its due time."
)]
pub struct ReminderListArgs {
    /// Which reminders: pending (default), fired, or all.
    #[serde(default)]
    pub filter: ReminderFilter,
    /// How many to show (default 20, at most 100).
    #[serde(default)]
    pub limit: Option<u32>,
}

impl ReminderListArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(reminder_list(ctx.db, self.filter, reminder_list_limit(self.limit), ctx.config_tz).await)
    }
}

impl CountdownArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let joined = if self.message.is_empty() {
            self.duration
        } else {
            format!("{} {}", self.duration, self.message)
        };
        Ok(countdown(ctx.db, &joined, self.act.unwrap_or(false)).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "calculate",
    description = "Evaluate a math expression. Operators: + - * / % ( ) and ** for exponent. Functions, called with parentheses: sqrt, exp, ln, abs, sin, cos, tan, asin, acos, atan, sinh, cosh, tanh, asinh, acosh, atanh, floor, ceil, round, signum (one argument each), atan2(y, x), and max and min (one or more arguments). Constants: pi, e. Angles are in radians; ln is the natural logarithm and there is no log. Bare ^ is rejected (XOR is unsupported). Example: 2 ** 10 returns 1024."
)]
pub struct CalculateArgs {
    /// The expression to evaluate, e.g. `2 ** 10`.
    pub expression: String,
}

impl CalculateArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(calculate(&self.expression))
    }
}

// -- Multi-field / sub-command tools ------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DefineAction {
    Get,
    Save,
    Delete,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "define",
    is_side_effectful = true,
    description = "Look up, save, or delete a definition. action=get with term to read, action=save with term+definition to create/update, action=delete with term to remove."
)]
pub struct DefineArgs {
    /// get (default) | save | delete.
    #[serde(default = "default_define_action")]
    pub action: DefineAction,
    /// The term (noun or phrase) to operate on.
    pub term: String,
    /// Required for action=save; ignored otherwise.
    #[serde(default)]
    pub definition: Option<String>,
}

fn default_define_action() -> DefineAction {
    DefineAction::Get
}

impl DefineArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = match self.action {
            DefineAction::Get => self.term,
            DefineAction::Save => match self.definition {
                Some(d) => format!("{} | {}", self.term, d),
                None => self.term,
            },
            DefineAction::Delete => format!("delete {}", self.term),
        };
        Ok(define(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DraftAction {
    Save,
    Delete,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "draft",
    is_side_effectful = true,
    description = "Save or delete a text draft. action=save with title+content creates or updates; action=delete with title removes a draft by title."
)]
pub struct DraftArgs {
    /// save (default) | delete.
    #[serde(default = "default_draft_action")]
    pub action: DraftAction,
    /// Draft title (identifier).
    pub title: String,
    /// Required for action=save; ignored for delete.
    #[serde(default)]
    pub content: Option<String>,
}

fn default_draft_action() -> DraftAction {
    DraftAction::Save
}

impl DraftArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = match self.action {
            DraftAction::Save => match self.content {
                Some(c) => format!("{} | {}", self.title, c),
                None => self.title,
            },
            DraftAction::Delete => format!("delete {}", self.title),
        };
        Ok(draft(ctx.db, &param, ctx.session_name).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "get",
    description = "Read a specific document from WardSONDB by collection and id."
)]
pub struct GetArgs {
    /// WardSONDB collection name (e.g. `memory.entries`, `soul.invariant`).
    pub collection: String,
    /// Document id within the collection.
    pub id: String,
}

impl GetArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {}", self.collection, self.id);
        Ok(get(ctx.db, &param).await)
    }
}

#[cfg(test)]
mod native_args_tests {
    use super::*;

    #[test]
    fn recall_round_trips_empty_and_filled() {
        let a: RecallArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(a.query, "");
        assert!(!a.unpromoted_only, "unpromoted_only must default false");
        let b: RecallArgs = serde_json::from_value(serde_json::json!({"query": "alerts"})).unwrap();
        assert_eq!(b.query, "alerts");
        let c: RecallArgs =
            serde_json::from_value(serde_json::json!({"unpromoted_only": true})).unwrap();
        assert!(c.unpromoted_only);
        assert_eq!(c.query, "");
    }

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

    #[test]
    fn recall_schema_has_optional_query() {
        let schema = schemars::schema_for!(RecallArgs);
        let v = serde_json::to_value(&schema).unwrap();
        assert_eq!(v["properties"]["query"]["type"], "string");
        assert_eq!(v["properties"]["unpromoted_only"]["type"], "boolean");
    }

    #[test]
    fn countdown_requires_duration_message_defaults() {
        let a: CountdownArgs =
            serde_json::from_value(serde_json::json!({"duration": "5m"})).unwrap();
        assert_eq!(a.duration, "5m");
        assert_eq!(a.message, "Reminder");

        let b: CountdownArgs = serde_json::from_value(serde_json::json!({
            "duration": "30s", "message": "check build"
        }))
        .unwrap();
        assert_eq!(b.message, "check build");

        // Missing required field
        let err = serde_json::from_value::<CountdownArgs>(serde_json::json!({})).unwrap_err();
        assert!(err.to_string().contains("duration"));
    }

    #[test]
    fn define_action_deserializes_lowercase() {
        let a: DefineArgs = serde_json::from_value(serde_json::json!({
            "action": "save", "term": "soul", "definition": "identity core"
        }))
        .unwrap();
        assert!(matches!(a.action, DefineAction::Save));
        assert_eq!(a.term, "soul");
        assert_eq!(a.definition.as_deref(), Some("identity core"));

        let b: DefineArgs = serde_json::from_value(serde_json::json!({"term": "soul"})).unwrap();
        assert!(matches!(b.action, DefineAction::Get));
    }

    #[test]
    fn draft_default_save() {
        let a: DraftArgs =
            serde_json::from_value(serde_json::json!({"title": "x", "content": "y"})).unwrap();
        assert!(matches!(a.action, DraftAction::Save));

        let d: DraftArgs =
            serde_json::from_value(serde_json::json!({"action": "delete", "title": "x"}))
                .unwrap();
        assert!(matches!(d.action, DraftAction::Delete));
    }

    #[test]
    fn get_requires_collection_and_id() {
        let a: GetArgs =
            serde_json::from_value(serde_json::json!({"collection": "soul.invariant", "id": "soul"}))
                .unwrap();
        assert_eq!(a.collection, "soul.invariant");

        let err =
            serde_json::from_value::<GetArgs>(serde_json::json!({"collection": "x"})).unwrap_err();
        assert!(err.to_string().contains("id"));
    }

    #[test]
    fn aliases_register_distinct_names() {
        // Descriptors are accumulated via inventory at startup; confirm the
        // three memory-search descriptors all exist as distinct names.
        let names: Vec<&'static str> = inventory::iter::<crate::tools::registry::ToolDescriptor>()
            .map(|d| d.name)
            .filter(|n| matches!(*n, "recall" | "memory_search" | "search_memory"))
            .collect();
        assert!(names.contains(&"recall"), "recall registered");
        assert!(names.contains(&"memory_search"), "memory_search alias registered");
        assert!(names.contains(&"search_memory"), "search_memory alias registered");
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

#[cfg(test)]
mod reminder_tests {
    #[test]
    fn act_defaults_to_false_and_round_trips_on_a_reminder() {
        let now = chrono::Utc::now();
        let plain = super::reminder_doc("m", now, now, false);
        assert_eq!(plain["act"], false);
        assert_eq!(plain["fired"], false);
        let acting = super::reminder_doc("m", now, now, true);
        assert_eq!(acting["act"], true);
        // The tool's flag defaults to absent, read as false.
        let args: super::CountdownArgs = serde_json::from_value(serde_json::json!({"duration": "5m"})).unwrap();
        assert_eq!(args.act, None);
        let args: super::CountdownArgs =
            serde_json::from_value(serde_json::json!({"duration": "5m", "message": "x", "act": true})).unwrap();
        assert_eq!(args.act, Some(true));
    }

    #[test]
    fn the_pending_filter_matches_records_without_the_fired_field() {
        let body = super::reminder_list_query_body(super::ReminderFilter::Pending, 7).unwrap();
        assert_eq!(
            body["filter"],
            serde_json::json!({"$or": [{"fired": false}, {"fired": {"$exists": false}}]})
        );
        assert_eq!(body["sort"], serde_json::json!([{"trigger_at": "asc"}, {"_id": "asc"}]));
        assert_eq!(body["limit"], serde_json::json!(7));
        let fired = super::reminder_list_query_body(super::ReminderFilter::Fired, 7).unwrap();
        assert_eq!(fired["filter"], serde_json::json!({"fired": true}));
        assert_eq!(fired["sort"], serde_json::json!([{"trigger_at": "desc"}, {"_id": "desc"}]));
        assert!(super::reminder_list_query_body(super::ReminderFilter::All, 7).is_none());
    }

    #[test]
    fn reminder_list_caps_its_limit() {
        assert_eq!(super::reminder_list_limit(None), 20);
        assert_eq!(super::reminder_list_limit(Some(0)), 1);
        assert_eq!(super::reminder_list_limit(Some(5)), 5);
        assert_eq!(super::reminder_list_limit(Some(1000)), 100);
    }

    #[test]
    fn reminder_list_renders_id_message_due_and_fired() {
        let docs = vec![
            serde_json::json!({"_id": "0199abcd-aaaa-7000-8000-000000000001", "message": "check the build",
                               "trigger_at": "2026-10-08T12:00:00+00:00", "fired": true,
                               "fired_at": "2026-10-08T12:00:07+00:00"}),
            serde_json::json!({"_id": "0199abce-bbbb-7000-8000-000000000002", "message": "stand up",
                               "trigger_at": "2026-10-08T13:00:00+00:00"}),
        ];
        let out = super::render_reminder_list(&docs, super::ReminderFilter::All, 20, "America/Los_Angeles");
        assert_eq!(
            out,
            "=== Reminders (all: 2) ===\n  0199abcd  check the build  due 2026-10-08 05:00 PDT  fired 2026-10-08 05:00 PDT\n  0199abce  stand up  due 2026-10-08 06:00 PDT\n"
        );
        let full = super::render_reminder_list(&docs, super::ReminderFilter::Pending, 2, "UTC");
        assert!(full.starts_with("=== Reminders (pending: 2 (the first 2; raise limit for more)) ===\n"), "{full}");
        assert_eq!(
            super::render_reminder_list(&[], super::ReminderFilter::Pending, 20, "UTC"),
            "No pending reminders. Set one with countdown."
        );
    }

    #[test]
    fn a_fired_reminder_is_stamped_with_when_it_fired() {
        let doc = serde_json::json!({
            "_id": "r1", "message": "check the build",
            "trigger_at": "2026-10-08T12:00:00+00:00", "created_at": "2026-10-08T11:55:00+00:00",
            "fired": false,
        });
        let fired = mark_fired(&doc, "2026-10-08T12:00:07+00:00");
        assert_eq!(fired["fired"], true);
        assert_eq!(fired["fired_at"], "2026-10-08T12:00:07+00:00");
        assert_eq!(fired["message"], "check the build");
        assert_eq!(fired["trigger_at"], "2026-10-08T12:00:00+00:00");
    }
    use super::*;
    use serde_json::json;

    const NOW: &str = "2026-09-27T12:00:00+00:00";

    fn reminder(id: &str, trigger_at: &str, fired: Option<bool>) -> serde_json::Value {
        let mut doc = json!({"_id": id, "message": id, "trigger_at": trigger_at});
        if let Some(f) = fired {
            doc["fired"] = json!(f);
        }
        doc
    }

    fn ids(due: &[&serde_json::Value]) -> Vec<String> {
        due.iter().map(|d| d["_id"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn a_reminder_outlives_its_own_wait() {
        // What the database compares is the stored string, as a string.
        let now = chrono::DateTime::parse_from_rfc3339(NOW).unwrap().with_timezone(&Utc);
        let hours = |h: i64| now + chrono::Duration::hours(h);
        let doc = reminder_doc("the quarterly review", hours(200), now, false);
        let kept_from = doc[REMINDER_TTL_FIELD].as_str().expect("the lifetime field is written");
        assert_eq!(kept_from, hours(200).to_rfc3339());
        assert_eq!(doc["fired"], json!(false));

        // The worker removes what is older than now - retention. On the day
        // the reminder is due it has been stored for 200 hours: more than
        // the retention, and it must still be there.
        let retention = chrono::Duration::days(REMINDER_RETENTION_DAYS as i64);
        assert!(hours(200) - now > retention, "the case: a wait longer than the retention");
        let cutoff_when_due = (hours(200) - retention).to_rfc3339();
        assert!(kept_from >= cutoff_when_due.as_str(), "kept until it is due");
        assert!(
            doc["created_at"].as_str().unwrap() < cutoff_when_due.as_str(),
            "counted from created_at it would be gone by then"
        );
        // And it goes once the retention has passed AFTER it was due.
        let cutoff_later = (hours(200) + chrono::Duration::hours(1)).to_rfc3339();
        assert!(kept_from < cutoff_later.as_str());
    }

    #[test]
    fn due_query_is_windowed_sorted_and_matches_records_without_the_field() {
        let body = due_reminders_query_body(NOW, 500);
        assert_eq!(body["limit"], json!(500));
        // One key per array element: a multi-key object sorts alphabetically.
        assert_eq!(body["sort"], json!([{"trigger_at": "asc"}, {"_id": "asc"}]));
        assert_eq!(
            body["filter"],
            json!({
                "trigger_at": {"$lte": NOW},
                "$or": [{"fired": false}, {"fired": {"$exists": false}}],
            })
        );
    }

    #[test]
    fn only_due_unfired_reminders_fire() {
        let docs = vec![
            reminder("due", "2026-09-27T11:59:00+00:00", Some(false)),
            reminder("exactly-now", NOW, Some(false)),
            reminder("later", "2026-09-27T12:00:01+00:00", Some(false)),
            reminder("already-fired", "2026-09-27T11:00:00+00:00", Some(true)),
            // Written before the field existed: counts as not fired.
            reminder("legacy", "2026-09-27T10:00:00+00:00", None),
            reminder("no-trigger", "", Some(false)),
            json!({"_id": "no-trigger-field", "message": "x", "fired": false}),
        ];
        assert_eq!(ids(&due_reminders(&docs, NOW, 16)), ["legacy", "due", "exactly-now"]);
    }

    #[test]
    fn the_earliest_fire_first_and_the_rest_wait() {
        // Stored newest first; three are due and there is room for two.
        let docs = vec![
            reminder("c", "2026-09-27T11:30:00+00:00", Some(false)),
            reminder("a", "2026-09-27T09:00:00+00:00", Some(false)),
            reminder("b", "2026-09-27T10:00:00+00:00", Some(false)),
        ];
        assert_eq!(ids(&due_reminders(&docs, NOW, 2)), ["a", "b"]);
        assert_eq!(ids(&due_reminders(&docs, NOW, 3)), ["a", "b", "c"]);
        assert!(due_reminders(&docs, NOW, 0).is_empty());
    }
}
