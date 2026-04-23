use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;
use crate::knowledge;

pub mod cron;
pub mod engineering;
pub mod express;
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

#[derive(Debug, Serialize)]
pub struct SystemStatus {
    pub version: String,
    pub uptime_seconds: u64,
    pub wardsondb_healthy: bool,
    pub wardsondb_collections: Vec<String>,
    pub memory_usage_mb: Option<u64>,
    pub soul_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_poisoned: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifetime_requests: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifetime_inserts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifetime_queries: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifetime_deletes: Option<u64>,
}

pub async fn system_status(db: &WardsonDbClient) -> SystemStatus {
    let healthy = db.health().await.unwrap_or(false);
    let collections = db.list_collections().await.unwrap_or_default();
    let soul_status = if db.collection_exists("soul.invariant").await.unwrap_or(false) {
        "sealed"
    } else {
        "unsealed"
    };

    // Fetch expanded stats
    let stats = db.stats().await.ok();
    let storage_poisoned = stats
        .as_ref()
        .and_then(|s| s.get("storage_poisoned"))
        .and_then(|v| v.as_bool());
    let lifetime = stats.as_ref().and_then(|s| s.get("lifetime"));

    SystemStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: process_uptime_secs(),
        wardsondb_healthy: healthy,
        wardsondb_collections: collections,
        memory_usage_mb: get_memory_usage_mb(),
        soul_status: soul_status.into(),
        storage_poisoned,
        lifetime_requests: lifetime
            .and_then(|l| l.get("requests"))
            .and_then(|v| v.as_u64()),
        lifetime_inserts: lifetime
            .and_then(|l| l.get("inserts"))
            .and_then(|v| v.as_u64()),
        lifetime_queries: lifetime
            .and_then(|l| l.get("queries"))
            .and_then(|v| v.as_u64()),
        lifetime_deletes: lifetime
            .and_then(|l| l.get("deletes"))
            .and_then(|v| v.as_u64()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    pub download_url: String,
}

pub async fn check_wardsondb_update() -> Option<UpdateInfo> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.github.com/repos/ward-software-defined-systems/wardsondb/releases/latest")
        .header("User-Agent", "embraOS/0.1.0")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let latest_tag = data.get("tag_name")?.as_str()?;
    let latest_version = latest_tag.trim_start_matches('v');
    let current_version = "0.1.0";
    if latest_version != current_version {
        let download_url = data
            .get("assets")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(|a| a.get("browser_download_url"))
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string();
        Some(UpdateInfo {
            version: latest_version.to_string(),
            current_version: current_version.to_string(),
            download_url,
        })
    } else {
        None
    }
}

// ── Memory & Knowledge Tools ──

async fn ensure_collection(db: &WardsonDbClient, name: &str) {
    if !db.collection_exists(name).await.unwrap_or(true) {
        let _ = db.create_collection(name).await;
    }
}

async fn recall(db: &WardsonDbClient, query: &str) -> String {
    ensure_collection(db, "memory.entries").await;

    let entries = db.query("memory.entries", &serde_json::json!({})).await.unwrap_or_default();
    let semantic = db.query("memory.semantic", &serde_json::json!({})).await.unwrap_or_default();
    let procedural = db.query("memory.procedural", &serde_json::json!({})).await.unwrap_or_default();

    if entries.is_empty() && semantic.is_empty() && procedural.is_empty() {
        return "No memory entries found.".into();
    }

    let query_lower = query.trim_start_matches('#').to_lowercase();
    let query_tokens: Vec<String> = query_lower
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

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
        tokens_all_match(&hay, &query_tokens)
    };

    // Promoted entries: content-bearing fields come from the promoted node, not the episodic entry.
    // Mark entries with [promoted → collection] suffix.
    let mut output_lines: Vec<String> = Vec::new();

    // Promoted collections first (ranked higher)
    for doc in semantic.iter().chain(procedural.iter()) {
        let id = doc.get("_id").and_then(|v| v.as_str()).unwrap_or("?");
        let collection = if doc.get("category").is_some() { "memory.semantic" } else { "memory.procedural" };
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
    }

    // Episodic entries
    for doc in entries.iter() {
        let content = doc.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let tags = tags_to_str(doc);
        if !matches_query(content, &tags) { continue; }
        let id = doc.get("_id").and_then(|v| v.as_str()).unwrap_or("?");
        let ts = doc.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let promoted_marker = doc.get("promoted_to")
            .and_then(|v| if v.is_null() { None } else { v.get("collection") })
            .and_then(|v| v.as_str())
            .map(|c| format!(" [promoted → {}]", c))
            .unwrap_or_default();
        output_lines.push(format!("  [memory.entries] [{}] {}{} (tags: {}) — {}", id, content, promoted_marker, tags, ts));
    }

    if output_lines.is_empty() {
        info!(target: "recall", query = %query, token_count = query_tokens.len(), "zero-result recall");
        let mut msg = format!("No memory entries matching '{}'.", query);
        if query_tokens.len() > 1 {
            msg.push_str(" Multi-token queries require ALL tokens to appear; try a single word, or omit the query to list recent entries.");
        }
        return msg;
    }

    let total = output_lines.len();
    output_lines.truncate(10);
    format!("Found {} matching entries:\n{}", total, output_lines.join("\n"))
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

async fn remember(db: &WardsonDbClient, content: &str, session: &str, config: &SystemConfig) -> String {
    if content.is_empty() {
        return "Nothing to remember. Provide content after remember ....".into();
    }

    ensure_collection(db, "memory.entries").await;

    // Parse optional tags: "content text #tag1 #tag2". GitHub-style issue
    // references like `#5` stay in content (is_tag_token requires letter start).
    let mut tags: Vec<String> = Vec::new();
    let mut text_parts = Vec::new();
    for word in content.split_whitespace() {
        if is_tag_token(word) {
            tags.push(word.trim_start_matches('#').to_string());
        } else {
            text_parts.push(word);
        }
    }
    let text = text_parts.join(" ");
    let created_at = Utc::now().to_rfc3339();

    let doc = serde_json::json!({
        "content": text,
        "tags": tags,
        "session": session,
        "promoted_to": serde_json::Value::Null,
        "created_at": created_at,
    });

    match db.write("memory.entries", &doc).await {
        Ok(id) => {
            // Background edge derivation (spec §4.8)
            let db_clone = db.clone();
            let id_clone = id.clone();
            let session_clone = session.to_string();
            let tags_clone = tags.clone();
            let created_at_clone = created_at.clone();
            let config_clone = config.clone();
            tokio::spawn(async move {
                let _ = knowledge::edges::derive_edges(
                    &db_clone,
                    &id_clone,
                    "memory.entries",
                    &session_clone,
                    &tags_clone,
                    &created_at_clone,
                    &config_clone,
                ).await;
            });
            format!("Remembered. Entry ID: {}", id)
        }
        Err(e) => format!("Failed to save memory: {}", e),
    }
}

async fn forget(db: &WardsonDbClient, id: &str) -> String {
    if id.is_empty() {
        return "Provide the entry ID to forget: forget <id>".into();
    }
    let id = id.trim();

    if let Err(e) = db.delete("memory.entries", id).await {
        return format!("Failed to remove entry: {}", e);
    }

    let edge_filter = serde_json::json!({
        "$or": [
            {"source_id": id, "source_collection": "memory.entries"},
            {"target_id": id, "target_collection": "memory.entries"},
        ]
    });
    let edge_count = db
        .delete_by_query("memory.edges", &edge_filter)
        .await
        .unwrap_or(0);
    format!(
        "Memory entry {} removed; {} referencing edge(s) cascaded.",
        id, edge_count
    )
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
            .query(&meta_col, &serde_json::json!({}))
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
        if col.starts_with("sessions.") && col.ends_with(".history") {
            if let Ok(docs) = db.query(col, &serde_json::json!({})).await {
                for doc in &docs {
                    if let Some(turns) = doc.get("turns").and_then(|v| v.as_array()) {
                        total_messages += turns.len() as u64;
                    }
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
    let soul_doc = db.read("soul.invariant", "soul").await.ok().or_else(|| None);
    let soul_doc = match soul_doc {
        Some(doc) => Some(doc),
        None => db.query("soul.invariant", &serde_json::json!({})).await.ok().and_then(|v| v.into_iter().next()),
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

        if focus.is_empty() || focus_lower == "soul" {
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
        let id_doc = db.read("memory.identity", "identity").await.ok().or_else(|| None);
        let id_doc = match id_doc {
            Some(doc) => Some(doc),
            None => db.query("memory.identity", &serde_json::json!({})).await.ok().and_then(|v| v.into_iter().next()),
        };
        if let Some(doc) = id_doc {
            output.push_str("\n=== IDENTITY ===\n");
            output.push_str(&serde_json::to_string_pretty(&doc).unwrap_or_default());
            output.push('\n');
        }
    }

    // Load user profile (direct GET, fallback to query)
    if focus.is_empty() || focus_lower.contains("user") || focus_lower.contains("operator") {
        let user_doc = db.read("memory.user", "user").await.ok().or_else(|| None);
        let user_doc = match user_doc {
            Some(doc) => Some(doc),
            None => db.query("memory.user", &serde_json::json!({})).await.ok().and_then(|v| v.into_iter().next()),
        };
        if let Some(doc) = user_doc {
            output.push_str("\n=== USER PROFILE ===\n");
            output.push_str(&serde_json::to_string_pretty(&doc).unwrap_or_default());
            output.push('\n');
        }
    }

    if output.is_empty() {
        "No documents found for the requested focus area.".into()
    } else {
        output
    }
}

async fn changelog(db: &WardsonDbClient, current_session: &str) -> String {
    // Find the current session's creation time
    let meta_col = format!("sessions.{}.meta", current_session);
    let session_start = db
        .query(&meta_col, &serde_json::json!({}))
        .await
        .ok()
        .and_then(|docs| docs.into_iter().next())
        .and_then(|doc| doc.get("created_at").and_then(|v| v.as_str()).map(|s| s.to_string()));

    let mut output = String::from("Changes since last session:\n");

    // Recent memory entries (with projection)
    let entries = db
        .query(
            "memory.entries",
            &serde_json::json!({"fields": ["content", "tags", "created_at"]}),
        )
        .await
        .unwrap_or_default();

    const DISPLAY_CAP: usize = 5;

    // Newest first in both branches so the list's order matches expectations.
    // The session_start branch filters to entries created after session start
    // (variable count); the no-session-start branch shows the tail of history.
    let recent_entries: Vec<_> = if let Some(ref start) = session_start {
        let mut filtered: Vec<_> = entries
            .iter()
            .filter(|doc| {
                doc.get("created_at")
                    .and_then(|v| v.as_str())
                    .map(|ts| ts > start.as_str())
                    .unwrap_or(false)
            })
            .collect();
        filtered.reverse();
        filtered
    } else {
        entries.iter().rev().collect()
    };

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

    // List sessions. Learning sessions are a one-time setup artifact; exclude
    // from "operational" count but note their presence so the total doesn't
    // appear to drift vs `session_list` (which shows every session).
    let collections = db.list_collections().await.unwrap_or_default();
    let all_session_metas: Vec<_> = collections
        .iter()
        .filter(|c| c.starts_with("sessions.") && c.ends_with(".meta"))
        .collect();
    let learning_count = all_session_metas.iter().filter(|c| c.contains("learning")).count();
    let operational_count = all_session_metas.len() - learning_count;
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

async fn countdown(db: &WardsonDbClient, param: &str) -> String {
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

    let trigger_at = Utc::now() + chrono::Duration::seconds(seconds as i64);

    ensure_collection(db, "reminders").await;

    let doc = serde_json::json!({
        "message": message,
        "trigger_at": trigger_at.to_rfc3339(),
        "created_at": Utc::now().to_rfc3339(),
        "fired": false,
    });

    match db.write("reminders", &doc).await {
        Ok(id) => format!(
            "Reminder set. Will fire at {} (in {}s).\nID: {}",
            trigger_at.format("%H:%M:%S UTC"),
            seconds,
            id
        ),
        Err(e) => format!("Failed to set reminder: {}", e),
    }
}

/// Check for due reminders. Called by the proactive engine.
pub async fn check_reminders(db: &WardsonDbClient) -> Vec<String> {
    let reminders = db
        .query("reminders", &serde_json::json!({}))
        .await
        .unwrap_or_default();

    let now = Utc::now().to_rfc3339();
    let mut fired = Vec::new();

    for doc in &reminders {
        // Missing `fired` field means not yet fired (BUG-003 fix)
        let already_fired = doc.get("fired").and_then(|v| v.as_bool()).unwrap_or(false);
        if already_fired {
            continue;
        }
        tracing::debug!("Checking reminder: {:?}", doc.get("message"));

        let trigger = doc
            .get("trigger_at")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if !trigger.is_empty() && trigger <= now.as_str() {
            let message = doc
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Reminder");

            fired.push(format!("Reminder: {}", message));

            // Mark as fired
            if let Some(id) = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()) {
                let mut updated = doc.clone();
                updated["fired"] = serde_json::json!(true);
                let _ = db.update("reminders", id, &updated).await;
            }
        }
    }

    fired
}

async fn session_summary(db: &WardsonDbClient, session_name: &str) -> String {
    let collection = format!("sessions.{}.history", session_name);
    let results = db
        .query(&collection, &serde_json::json!({}))
        .await
        .unwrap_or_default();

    if let Some(doc) = results.into_iter().next() {
        if let Some(turns) = doc.get("turns").and_then(|v| v.as_array()) {
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
                // Truncate long messages
                let preview = if content.len() > 200 {
                    format!("{}...", &content[..200])
                } else {
                    content.to_string()
                };
                output.push_str(&format!("[{}]: {}\n", role, preview));
            }

            return output;
        }
    }

    format!("No conversation history found for session '{}'.", session_name)
}

// ── Utility Tools ──

fn calculate(expression: &str) -> String {
    if expression.is_empty() {
        return "Usage: calculate <expression>\nExample: calculate 2 ** 10".into();
    }

    // Exponent is ** (Python/Rust convention). Reject bare ^ up-front so it
    // never silently resolves to meval's native power operator — in Python ^
    // is XOR, and this tool does not support XOR. Detect ^ before translating
    // ** → ^ for meval.
    if expression.contains('^') {
        return format!(
            "Could not evaluate '{}': '^' is not supported. Use ** for exponent (e.g. 2 ** 10). XOR is not available in this tool.",
            expression
        );
    }
    let normalized = expression.replace("**", "^");

    match meval::eval_str(&normalized) {
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
            .query("knowledge.definitions", &serde_json::json!({}))
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
            .query("knowledge.definitions", &serde_json::json!({}))
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
        .query("knowledge.definitions", &serde_json::json!({}))
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
        let existing = db.query("drafts", &serde_json::json!({})).await.unwrap_or_default();
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
    let existing = db.query("drafts", &serde_json::json!({})).await.unwrap_or_default();
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
    description = "Report system status: version, uptime, WardSONDB health, collections, memory usage, soul status, and lifetime operation counters."
)]
pub struct SystemStatusArgs {}

impl SystemStatusArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let status = system_status(ctx.db).await;
        Ok(serde_json::to_string_pretty(&status).unwrap_or_default())
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "check_update",
    description = "Check for available updates to WardSONDB. Returns \"up to date\" or the available version and download URL."
)]
pub struct CheckUpdateArgs {}

impl CheckUpdateArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(match check_wardsondb_update().await {
            Some(info) => format!(
                "WardSONDB update available: v{} (current: v{})",
                info.version, info.current_version
            ),
            None => "WardSONDB is up to date.".into(),
        })
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
    description = "Search past conversations and saved memories. Free-text query; unquoted terms AND-match (all must appear); hashtags supported; empty query lists recent entries."
)]
pub struct RecallArgs {
    /// Search query. Free-text; hashtags supported; empty to list all.
    #[serde(default)]
    pub query: String,
}

impl RecallArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(recall(ctx.db, &self.query).await)
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
        RecallArgs { query: self.query }.run(ctx).await
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
        RecallArgs { query: self.query }.run(ctx).await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "remember",
    is_side_effectful = true,
    description = "Save a note or fact to persistent memory. Hashtag tokens (e.g. #architecture, #soul) are extracted into the tags array; the remaining words become the content. Keep content to a single line."
)]
pub struct RememberArgs {
    /// Content to save. Letter-start `#tag` tokens are extracted into tags.
    pub content: String,
}

impl RememberArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(remember(ctx.db, &self.content, ctx.session_name, ctx.config).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "forget",
    is_side_effectful = true,
    description = "Remove a specific memory entry by its id. Destructive; confirm with the user first."
)]
pub struct ForgetArgs {
    /// WardSONDB document id of the memory entry to delete.
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
    name = "countdown",
    description = "Set a reminder to fire after a duration. duration examples: \"5m\", \"30s\", \"1h\", \"20 minutes\". message defaults to \"Reminder\" if omitted."
)]
pub struct CountdownArgs {
    /// Duration: "5m", "30s", "1h", "20 minutes".
    pub duration: String,
    /// Reminder message shown when the countdown fires.
    #[serde(default = "default_countdown_message")]
    pub message: String,
}

fn default_countdown_message() -> String {
    "Reminder".into()
}

impl CountdownArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let joined = if self.message.is_empty() {
            self.duration
        } else {
            format!("{} {}", self.duration, self.message)
        };
        Ok(countdown(ctx.db, &joined).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "calculate",
    description = "Evaluate a math expression. Operators: + - * / % ( ) and ** for exponent. Bare ^ is rejected (XOR is unsupported). Example: 2 ** 10 returns 1024."
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
        let b: RecallArgs = serde_json::from_value(serde_json::json!({"query": "alerts"})).unwrap();
        assert_eq!(b.query, "alerts");
    }

    #[test]
    fn recall_schema_has_optional_query() {
        let schema = schemars::schema_for!(RecallArgs);
        let v = serde_json::to_value(&schema).unwrap();
        assert_eq!(v["properties"]["query"]["type"], "string");
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
            .into_iter()
            .map(|d| d.name)
            .filter(|n| matches!(*n, "recall" | "memory_search" | "search_memory"))
            .collect();
        assert!(names.contains(&"recall"), "recall registered");
        assert!(names.contains(&"memory_search"), "memory_search alias registered");
        assert!(names.contains(&"search_memory"), "search_memory alias registered");
    }
}
