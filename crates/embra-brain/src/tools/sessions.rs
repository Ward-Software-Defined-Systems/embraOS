use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::db::WardsonDbClient;

use super::ensure_collection;

// ── Helpers ──

/// Truncate a string to at most `max_bytes`, snapping to a char boundary.
fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Get all session collection prefixes (including learning session).
async fn session_names(db: &WardsonDbClient) -> Vec<String> {
    let collections = db.list_collections().await.unwrap_or_default();
    collections
        .iter()
        .filter(|c| c.starts_with("sessions.") && c.ends_with(".meta"))
        .map(|c| {
            c.strip_prefix("sessions.")
                .unwrap_or(c)
                .strip_suffix(".meta")
                .unwrap_or(c)
                .to_string()
        })
        .collect()
}

/// Fetch the turns array for a session. Returns (turns_vec, total_count).
async fn fetch_turns(db: &WardsonDbClient, name: &str) -> (Vec<serde_json::Value>, usize) {
    let collection = format!("sessions.{}.history", name);
    let results = db
        .query(&collection, &serde_json::json!({}))
        .await
        .unwrap_or_default();

    if let Some(doc) = results.into_iter().next() {
        if let Some(turns) = doc.get("turns").and_then(|v| v.as_array()) {
            let len = turns.len();
            return (turns.clone(), len);
        }
    }
    (Vec::new(), 0)
}

/// Format a single turn for output. Truncates content to `max_chars`.
fn format_turn(index: usize, turn: &serde_json::Value, max_chars: usize) -> String {
    let role = turn
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("?");
    let content = turn
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let preview = if content.len() > max_chars {
        format!("{}...", truncate_str(content, max_chars))
    } else {
        content.to_string()
    };
    // 1-indexed for user display
    format!("[{}] [{}]: {}", index + 1, role, preview)
}

/// Parse a turn range string like "1-20", "80-", "5", or "" into
/// (start_0indexed, end_exclusive). Returns a human-readable Err when the
/// request cannot be satisfied (e.g. start beyond total) so the caller can
/// surface the problem instead of printing an inverted header.
fn parse_range(range_str: &str, total: usize) -> Result<(usize, usize), String> {
    if total == 0 {
        return Err("session has no turns".into());
    }
    if range_str.is_empty() {
        // Default: last 30 turns.
        let start = if total > 30 { total - 30 } else { 0 };
        return Ok((start, total));
    }

    if let Some(dash_pos) = range_str.find('-') {
        let start_str = range_str[..dash_pos].trim();
        let end_str = range_str[dash_pos + 1..].trim();

        let start_1: usize = start_str
            .parse()
            .map_err(|_| format!("bad start '{}' — expected a 1-based turn number", start_str))?;
        if start_1 < 1 {
            return Err("start must be ≥ 1".into());
        }
        if start_1 > total {
            return Err(format!("start {} exceeds total {} turns", start_1, total));
        }
        let end = if end_str.is_empty() {
            total
        } else {
            let raw: usize = end_str
                .parse()
                .map_err(|_| format!("bad end '{}' — expected a 1-based turn number", end_str))?;
            raw.min(total)
        };
        if end < start_1 {
            return Err(format!("end {} is before start {}", end, start_1));
        }
        Ok((start_1 - 1, end))
    } else {
        // Single number — show that one turn.
        let n: usize = range_str
            .trim()
            .parse()
            .map_err(|_| format!("bad turn '{}' — expected a 1-based turn number", range_str))?;
        if n < 1 || n > total {
            return Err(format!("turn {} out of range 1..={}", n, total));
        }
        Ok((n - 1, n))
    }
}

#[cfg(test)]
mod parse_range_tests {
    use super::parse_range;

    #[test]
    fn empty_returns_tail_30_of_larger_session() {
        assert_eq!(parse_range("", 50).unwrap(), (20, 50));
    }

    #[test]
    fn empty_on_small_session_returns_all() {
        assert_eq!(parse_range("", 10).unwrap(), (0, 10));
    }

    #[test]
    fn single_turn_at_valid_index() {
        assert_eq!(parse_range("5", 10).unwrap(), (4, 5));
    }

    #[test]
    fn single_turn_out_of_range_errors() {
        assert!(parse_range("11", 10).is_err());
        assert!(parse_range("0", 10).is_err());
    }

    #[test]
    fn range_end_clamps_to_total() {
        assert_eq!(parse_range("1-9999", 10).unwrap(), (0, 10));
    }

    #[test]
    fn range_out_of_bounds_reproducer_finding16() {
        // 500-600 on a 20-turn session used to silently return "turns 21-20 of 20".
        let err = parse_range("500-600", 20).unwrap_err();
        assert!(err.contains("exceeds total"), "unexpected message: {}", err);
    }

    #[test]
    fn zero_total_errors() {
        assert!(parse_range("1-5", 0).is_err());
    }

    #[test]
    fn end_before_start_errors() {
        assert!(parse_range("10-5", 20).is_err());
    }

    #[test]
    fn bad_input_errors() {
        assert!(parse_range("foo", 10).is_err());
        assert!(parse_range("1-foo", 10).is_err());
    }
}

// ── Phase A: Session Access Tools ──

/// List all sessions with metadata and turn counts.
pub async fn session_list(db: &WardsonDbClient) -> String {
    let names = session_names(db).await;

    if names.is_empty() {
        return "No sessions found.".into();
    }

    struct SessionInfo {
        name: String,
        status: String,
        turns: usize,
        last_active: String,
        created_at: String,
    }

    let mut sessions = Vec::new();

    for name in &names {
        let meta_col = format!("sessions.{}.meta", name);
        let meta_docs = db
            .query(
                &meta_col,
                &serde_json::json!({"fields": ["session_name", "state", "status", "last_active", "created_at", "message_count"]}),
            )
            .await
            .unwrap_or_default();

        let (status, last_active, created_at) = if let Some(meta) = meta_docs.into_iter().next() {
            (
                meta.get("state")
                    .or_else(|| meta.get("status"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                meta.get("last_active")
                    .and_then(|v| v.as_str())
                    .unwrap_or("—")
                    .to_string(),
                meta.get("created_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("—")
                    .to_string(),
            )
        } else {
            ("unknown".into(), "—".into(), "—".into())
        };

        let (_, turn_count) = fetch_turns(db, name).await;

        sessions.push(SessionInfo {
            name: name.clone(),
            status,
            turns: turn_count,
            last_active,
            created_at,
        });
    }

    // Sort by last_active descending
    sessions.sort_by(|a, b| b.last_active.cmp(&a.last_active));

    let mut output = format!("Sessions ({}):\n", sessions.len());
    output.push_str(&format!(
        "{:<16} {:<10} {:>5}  {:<24} {:<24}\n",
        "Name", "Status", "Turns", "Last Active", "Created"
    ));
    output.push_str(&"-".repeat(85));
    output.push('\n');

    for s in &sessions {
        // Truncate timestamps to readable length
        let last = if s.last_active.len() > 19 {
            &s.last_active[..19]
        } else {
            &s.last_active
        };
        let created = if s.created_at.len() > 19 {
            &s.created_at[..19]
        } else {
            &s.created_at
        };
        output.push_str(&format!(
            "{:<16} {:<10} {:>5}  {:<24} {:<24}\n",
            s.name, s.status, s.turns, last, created
        ));
    }

    output
}

/// Read session transcript with pagination.
/// Param: `<name> [start-end]`
pub async fn session_read(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: session_read <name> or session_read <name> <start>-<end>".into();
    }

    let parts: Vec<&str> = param.splitn(2, ' ').collect();
    let name = parts[0];
    let range_str = if parts.len() > 1 { parts[1].trim() } else { "" };

    let (turns, total) = fetch_turns(db, name).await;
    if total == 0 {
        return format!("No conversation history found for session '{}'.", name);
    }

    let (start, end) = match parse_range(range_str, total) {
        Ok(v) => v,
        Err(msg) => return format!("session_read rejected ({}) for session '{}' (total turns: {})", msg, name, total),
    };

    let mut output = format!(
        "Session '{}': turns {}-{} of {}\n\n",
        name,
        start + 1,
        end,
        total
    );

    for (i, turn) in turns[start..end].iter().enumerate() {
        output.push_str(&format_turn(start + i, turn, 500));
        output.push('\n');
    }

    output
}

/// Full-text search across sessions.
///
/// Query semantics:
/// - Double-quoted (`"tool sweep"`): literal phrase match.
/// - Unquoted (`tool sweep`): whitespace-tokenized; all tokens must appear
///   in the same turn (AND). Matches `recall`'s token semantics.
pub async fn session_search(
    db: &WardsonDbClient,
    query: &str,
    session: Option<&str>,
) -> String {
    let Some(plan) = SessionQueryPlan::parse(query) else {
        return "Usage: session_search <query> (unquoted terms AND-match; wrap in double quotes for literal phrase). Pass `session` to narrow to a single session.".into();
    };

    let names_to_search = match session {
        Some(s) if !s.is_empty() => vec![s.to_string()],
        _ => session_names(db).await,
    };

    let mut results = Vec::new();

    for name in &names_to_search {
        let (turns, _) = fetch_turns(db, name).await;
        for (i, turn) in turns.iter().enumerate() {
            let content = turn
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            let content_lower = content.to_lowercase();

            if let Some((anchor_pos, anchor_len)) = plan.match_turn(&content_lower) {
                let role = turn.get("role").and_then(|r| r.as_str()).unwrap_or("?");

                let mut start = anchor_pos.saturating_sub(50);
                while start > 0 && !content.is_char_boundary(start) {
                    start -= 1;
                }
                let mut end = (anchor_pos + anchor_len + 50).min(content.len());
                while end < content.len() && !content.is_char_boundary(end) {
                    end += 1;
                }
                let snippet = &content[start..end];

                results.push(format!(
                    "{} turn #{} [{}]: ...{}...",
                    name,
                    i + 1,
                    role,
                    snippet
                ));
            }

            if results.len() >= 20 {
                break;
            }
        }
        if results.len() >= 20 {
            break;
        }
    }

    if results.is_empty() {
        return format!(
            "No matches found for '{}'. Unquoted terms AND-match (all must appear in the same turn); wrap in double quotes for literal phrase.",
            plan.display
        );
    }

    let mut output = format!(
        "Search results for '{}' ({} matches):\n\n",
        plan.display,
        results.len()
    );
    for r in &results {
        output.push_str(r);
        output.push('\n');
    }
    output
}

/// Parsed query plan. Phrase mode captures the literal substring; AND mode
/// captures whitespace-split tokens. Both match against already-lowercased
/// turn content.
#[derive(Debug)]
struct SessionQueryPlan {
    phrase_mode: bool,
    core_lower: String,
    tokens: Vec<String>,
    display: String,
}

impl SessionQueryPlan {
    fn parse(query: &str) -> Option<Self> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return None;
        }
        let (phrase_mode, core) =
            if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
                (true, &trimmed[1..trimmed.len() - 1])
            } else {
                (false, trimmed)
            };
        let core_lower = core.to_lowercase();
        let tokens: Vec<String> = if phrase_mode {
            Vec::new()
        } else {
            core_lower.split_whitespace().map(|s| s.to_string()).collect()
        };
        if !phrase_mode && tokens.is_empty() {
            return None;
        }
        Some(Self {
            phrase_mode,
            core_lower,
            tokens,
            display: trimmed.to_string(),
        })
    }

    /// `content_lower` must be already lowercased. Returns the snippet anchor
    /// `(pos, len)` — phrase uses the substring match, AND uses the first
    /// token's position so the snippet centers on something relevant.
    fn match_turn(&self, content_lower: &str) -> Option<(usize, usize)> {
        if self.phrase_mode {
            content_lower
                .find(&self.core_lower)
                .map(|p| (p, self.core_lower.len()))
        } else if self.tokens.iter().all(|t| content_lower.contains(t)) {
            let first = &self.tokens[0];
            let p = content_lower.find(first.as_str()).unwrap_or(0);
            Some((p, first.len()))
        } else {
            None
        }
    }
}

/// Structured metadata for a session.
pub async fn session_meta(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: session_meta <name>".into();
    }

    let name = param.trim();
    let meta_col = format!("sessions.{}.meta", name);
    let meta_docs = db
        .query(&meta_col, &serde_json::json!({}))
        .await
        .unwrap_or_default();

    let (status, last_active, created_at) = if let Some(meta) = meta_docs.into_iter().next() {
        (
            meta.get("state")
                .or_else(|| meta.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            meta.get("last_active")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            meta.get("created_at")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        )
    } else {
        return format!("Session '{}' not found.", name);
    };

    let (turns, total) = fetch_turns(db, name).await;

    let user_msgs = turns
        .iter()
        .filter(|t| t.get("role").and_then(|r| r.as_str()) == Some("user"))
        .count();
    let assistant_msgs = total - user_msgs;

    // Check for summary
    let summary_col = format!("sessions.{}.summary", name);
    let summary_status = if db.collection_exists(&summary_col).await.unwrap_or(false) {
        let summary_docs = db
            .query(&summary_col, &serde_json::json!({}))
            .await
            .unwrap_or_default();
        if summary_docs.is_empty() {
            "not generated"
        } else {
            "available"
        }
    } else {
        "not generated"
    };

    format!(
        "Session: {}\n\
         Status: {}\n\
         Created: {}\n\
         Last Active: {}\n\
         Total Turns: {}\n\
         User Messages: {}\n\
         Assistant Messages: {}\n\
         Summary: {}",
        name, status, created_at, last_active, total, user_msgs, assistant_msgs, summary_status
    )
}

/// Changes since a turn number.
/// Param: `<name> <since_turn>`
pub async fn session_delta(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: session_delta <name> <since_turn>".into();
    }

    let parts: Vec<&str> = param.splitn(2, ' ').collect();
    if parts.len() < 2 {
        return "Usage: session_delta <name> <since_turn>".into();
    }

    let name = parts[0];
    let since_turn = match parts[1].trim().parse::<usize>() {
        Ok(n) if n >= 1 => n,
        _ => return "since_turn must be a positive integer.".into(),
    };

    let (turns, total) = fetch_turns(db, name).await;
    if total == 0 {
        return format!("No conversation history found for session '{}'.", name);
    }

    let start_0 = (since_turn - 1).min(total);
    let new_turns = total - start_0;

    if new_turns == 0 {
        return format!("No new turns in '{}' since turn #{}.", name, since_turn);
    }

    let mut output = format!(
        "Delta for '{}' since turn #{}: {} new turns\n\n",
        name, since_turn, new_turns
    );

    for (i, turn) in turns[start_0..].iter().enumerate() {
        output.push_str(&format_turn(start_0 + i, turn, 500));
        output.push('\n');
    }

    output
}

// ── Phase B: Memory Consolidation Tools ──

/// Inventory and analysis of memory.entries.
/// Param: optional tag filter.
pub async fn memory_scan(db: &WardsonDbClient, param: &str) -> String {
    ensure_collection(db, "memory.entries").await;

    let entries = db
        .query(
            "memory.entries",
            &serde_json::json!({"fields": ["content", "tags", "session", "created_at"]}),
        )
        .await
        .unwrap_or_default();

    if entries.is_empty() {
        return "No memory entries found.".into();
    }

    let tag_filter = if param.is_empty() {
        None
    } else {
        Some(param.trim_start_matches('#').to_lowercase())
    };

    // Helper: extract tags from doc, handling both array (v5+) and string (legacy) formats.
    fn extract_tags(doc: &serde_json::Value) -> Vec<String> {
        match doc.get("tags") {
            Some(v) if v.is_array() => v.as_array().unwrap().iter()
                .filter_map(|t| t.as_str().map(|s| s.to_string())).collect(),
            Some(v) if v.is_string() => v.as_str().unwrap_or("").split(", ")
                .filter(|t| !t.is_empty()).map(|t| t.trim_start_matches('#').to_string()).collect(),
            _ => Vec::new(),
        }
    }

    // Filter entries by tag if specified
    let entries: Vec<&serde_json::Value> = if let Some(ref filter) = tag_filter {
        entries
            .iter()
            .filter(|doc| {
                extract_tags(doc).iter().any(|t| t.to_lowercase().contains(filter.as_str()))
            })
            .collect()
    } else {
        entries.iter().collect()
    };

    let total = entries.len();

    // Tag frequency
    let mut tag_counts = std::collections::HashMap::new();
    for doc in &entries {
        for tag in extract_tags(doc) {
            if tag.is_empty() { continue; }
            *tag_counts.entry(tag).or_insert(0u32) += 1;
        }
    }

    // Group by session
    let mut session_counts = std::collections::HashMap::new();
    for doc in &entries {
        let session = doc
            .get("session")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        *session_counts.entry(session.to_string()).or_insert(0u32) += 1;
    }

    // Age buckets
    let now = Utc::now();
    let mut bucket_1d = 0u32;
    let mut bucket_7d = 0u32;
    let mut bucket_30d = 0u32;
    let mut bucket_90d = 0u32;
    let mut bucket_old = 0u32;

    for doc in &entries {
        let created = doc
            .get("created_at")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        if let Some(dt) = created {
            let age_days = (now - dt).num_days();
            match age_days {
                0 => bucket_1d += 1,
                1..=6 => bucket_7d += 1,
                7..=29 => bucket_30d += 1,
                30..=89 => bucket_90d += 1,
                _ => bucket_old += 1,
            }
        } else {
            bucket_old += 1;
        }
    }

    // Duplicate candidates: find pairs where content is very similar
    let mut dupes = Vec::new();
    let normalized: Vec<(usize, String, String)> = entries
        .iter()
        .enumerate()
        .map(|(i, doc)| {
            let id = doc
                .get("_id")
                .or_else(|| doc.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let content = doc
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            (i, id, content)
        })
        .collect();

    for i in 0..normalized.len() {
        for j in (i + 1)..normalized.len() {
            let (shorter, longer) = if normalized[i].2.len() <= normalized[j].2.len() {
                (&normalized[i], &normalized[j])
            } else {
                (&normalized[j], &normalized[i])
            };

            if !shorter.2.is_empty() && longer.2.contains(&shorter.2) {
                dupes.push(format!(
                    "  Subset: [{}] is contained in [{}]",
                    shorter.1, longer.1
                ));
            }
        }
        if dupes.len() >= 10 {
            break;
        }
    }

    // Format output
    let mut output = format!("Memory Scan Report\n==================\n\nTotal entries: {}\n", total);

    if let Some(ref f) = tag_filter {
        output.push_str(&format!("Filter: tag contains '{}'\n", f));
    }

    output.push_str(&format!(
        "\nAge distribution:\n  <1 day: {}\n  1-7 days: {}\n  7-30 days: {}\n  30-90 days: {}\n  >90 days: {}\n",
        bucket_1d, bucket_7d, bucket_30d, bucket_90d, bucket_old
    ));

    output.push_str("\nTag frequency:\n");
    let mut tags_sorted: Vec<_> = tag_counts.iter().collect();
    tags_sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (tag, count) in tags_sorted.iter().take(20) {
        output.push_str(&format!("  #{}: {}\n", tag, count));
    }
    if tag_counts.is_empty() {
        output.push_str("  (no tags)\n");
    }

    output.push_str("\nBy session:\n");
    let mut sessions_sorted: Vec<_> = session_counts.iter().collect();
    sessions_sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (session, count) in &sessions_sorted {
        output.push_str(&format!("  {}: {}\n", session, count));
    }

    if !dupes.is_empty() {
        output.push_str(&format!("\nDuplicate candidates ({}):\n", dupes.len()));
        for d in &dupes {
            output.push_str(d);
            output.push('\n');
        }
    } else {
        output.push_str("\nNo duplicate candidates detected.\n");
    }

    // Knowledge Graph section — delegate to knowledge_graph_stats so both tools
    // return the same numbers. The inline query variant here used `json!({})`
    // with no `limit`, which silently capped at the WardSONDB default and
    // undercounted edges and promoted entries vs knowledge_graph_stats.
    output.push('\n');
    output.push_str(&crate::knowledge::tools::knowledge_graph_stats(db).await);

    output
}

/// Find duplicates and propose merges (read-only plan).
/// Param: optional comma-separated IDs to check.
pub async fn memory_dedup(db: &WardsonDbClient, param: &str) -> String {
    ensure_collection(db, "memory.entries").await;

    let all_entries = db
        .query("memory.entries", &serde_json::json!({}))
        .await
        .unwrap_or_default();

    if all_entries.is_empty() {
        return "No memory entries to deduplicate.".into();
    }

    // Filter to specific IDs if provided
    let entries: Vec<&serde_json::Value> = if param.is_empty() {
        all_entries.iter().collect()
    } else {
        let ids: Vec<&str> = param.split(',').map(|s| s.trim()).collect();
        all_entries
            .iter()
            .filter(|doc| {
                let id = doc
                    .get("_id")
                    .or_else(|| doc.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                ids.iter().any(|target| id == *target)
            })
            .collect()
    };

    if entries.len() < 2 {
        return "Need at least 2 entries to check for duplicates.".into();
    }

    // Normalize entries
    struct NormalizedEntry {
        id: String,
        content: String,
        normalized: String,
        tags: String,
        created_at: String,
    }

    let normalized: Vec<NormalizedEntry> = entries
        .iter()
        .map(|doc| {
            let id = doc
                .get("_id")
                .or_else(|| doc.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let content = doc
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let norm = content
                .to_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let tags = match doc.get("tags") {
                Some(v) if v.is_array() => v.as_array().unwrap().iter()
                    .filter_map(|t| t.as_str()).collect::<Vec<_>>().join(", "),
                Some(v) if v.is_string() => v.as_str().unwrap_or("").to_string(),
                _ => String::new(),
            };
            let created_at = doc
                .get("created_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            NormalizedEntry {
                id,
                content,
                normalized: norm,
                tags,
                created_at,
            }
        })
        .collect();

    // Find duplicate groups
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new(); // (strategy, indices)

    let mut used = vec![false; normalized.len()];
    for i in 0..normalized.len() {
        if used[i] || normalized[i].normalized.is_empty() {
            continue;
        }
        let mut group = vec![i];
        let mut strategy = String::new();

        for j in (i + 1)..normalized.len() {
            if used[j] || normalized[j].normalized.is_empty() {
                continue;
            }

            if normalized[i].normalized == normalized[j].normalized {
                group.push(j);
                strategy = "Identical".into();
            } else {
                let (shorter, longer) = if normalized[i].normalized.len()
                    <= normalized[j].normalized.len()
                {
                    (&normalized[i].normalized, &normalized[j].normalized)
                } else {
                    (&normalized[j].normalized, &normalized[i].normalized)
                };

                if longer.contains(shorter.as_str()) {
                    group.push(j);
                    strategy = "Subset".into();
                }
            }
        }

        if group.len() > 1 {
            for &idx in &group {
                used[idx] = true;
            }
            if strategy.is_empty() {
                strategy = "Near-duplicate".into();
            }
            groups.push((strategy, group));
        }
    }

    if groups.is_empty() {
        return "No duplicates found. Memory entries appear unique.".into();
    }

    let mut output = format!(
        "Deduplication Plan\n==================\nFound {} duplicate group(s):\n\n",
        groups.len()
    );

    for (g_idx, (strategy, indices)) in groups.iter().enumerate() {
        output.push_str(&format!("Group {} — Strategy: {}\n", g_idx + 1, strategy));

        // Find newest entry in group
        let newest_idx = indices
            .iter()
            .max_by_key(|&&i| &normalized[i].created_at)
            .copied()
            .unwrap_or(indices[0]);

        for &idx in indices {
            let e = &normalized[idx];
            let action = if idx == newest_idx { "KEEP" } else { "DELETE" };
            let preview = if e.content.len() > 80 {
                format!("{}...", truncate_str(&e.content, 80))
            } else {
                e.content.clone()
            };
            output.push_str(&format!(
                "  [{}] {} — \"{}\" (tags: {})\n    → {}\n",
                e.id, action, preview, e.tags, action
            ));
        }
        output.push('\n');
    }

    output.push_str(
        "To execute this plan, approve and I will use remember and forget to merge/delete entries.\n",
    );

    // Cross-collection duplicate detection (Sprint 2): flag unpromoted entries whose
    // normalized content is a subset/superset of an existing semantic node's content.
    let semantic = db.query("memory.semantic", &serde_json::json!({})).await.unwrap_or_default();
    if !semantic.is_empty() {
        let mut cross_dupes: Vec<String> = Vec::new();
        for entry in &normalized {
            // Only flag unpromoted entries (we don't have promoted_to in NormalizedEntry,
            // so re-scan entries for this; simple heuristic here).
            for sem in &semantic {
                let sem_id = sem.get("_id").and_then(|v| v.as_str()).unwrap_or("");
                let sem_content = sem.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let sem_norm: String = sem_content.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
                if sem_norm.is_empty() || entry.normalized.is_empty() { continue; }
                let (shorter, longer) = if entry.normalized.len() <= sem_norm.len() {
                    (&entry.normalized, &sem_norm)
                } else {
                    (&sem_norm, &entry.normalized)
                };
                if longer.contains(shorter.as_str()) {
                    cross_dupes.push(format!(
                        "  Potential cross-collection duplicate: entry {} ≈ semantic node {}",
                        entry.id, sem_id
                    ));
                    if cross_dupes.len() >= 10 { break; }
                }
            }
            if cross_dupes.len() >= 10 { break; }
        }
        if !cross_dupes.is_empty() {
            output.push_str(&format!("\nCross-collection overlap ({}):\n", cross_dupes.len()));
            for line in &cross_dupes {
                output.push_str(line);
                output.push('\n');
            }
        }
    }

    output
}

// ── Phase C: Session Consolidation Tools ──

/// Generate structured summary for a session (Option B: returns context for Brain).
/// Param: session name.
pub async fn session_summarize(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: session_summarize <name>".into();
    }

    let name = param.trim();

    // Cache check: see if summary already exists and is current
    let summary_col = format!("sessions.{}.summary", name);
    if db.collection_exists(&summary_col).await.unwrap_or(false) {
        let summary_docs = db
            .query(&summary_col, &serde_json::json!({}))
            .await
            .unwrap_or_default();

        if let Some(summary_doc) = summary_docs.into_iter().next() {
            let cached_count = summary_doc
                .get("turn_count_at_generation")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;

            let (_, current_count) = fetch_turns(db, name).await;

            if cached_count == current_count && cached_count > 0 {
                // Return cached summary
                let summary = summary_doc
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no summary text)");
                let topics = summary_doc
                    .get("key_topics")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let decisions = summary_doc
                    .get("key_decisions")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();

                return format!(
                    "Cached summary for '{}' (generated at {} turns, current: {}):\n\n{}\n\nKey topics: {}\nKey decisions: {}",
                    name, cached_count, current_count, summary, topics, decisions
                );
            }
        }
    }

    // Not cached or stale — fetch transcript and return context for Brain
    let (turns, total) = fetch_turns(db, name).await;
    if total == 0 {
        return format!("No conversation history found for session '{}'.", name);
    }

    // Select turns for large sessions
    let selected_turns: Vec<(usize, &serde_json::Value)> = if total > 50 {
        let mut selected = Vec::new();
        // First 5 turns
        for i in 0..5.min(total) {
            selected.push((i, &turns[i]));
        }
        // Turns referencing tool activity. Post-NATIVE-TOOLS-01 sessions
        // store tool calls as structured blocks rather than text, so this
        // substring check is a LEGACY fallback that still catches
        // pre-v7 session content where tool calls appear as [TOOL:...]
        // strings. Safe to keep indefinitely: false positives on natural
        // prose containing "[TOOL:" as a literal quote are rare and
        // harmless (the turn is merely included in the summary corpus).
        for i in 5..total.saturating_sub(10) {
            let content = turns[i]
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            if content.contains("[TOOL:") {
                selected.push((i, &turns[i]));
            }
        }
        // Last 10 turns
        let last_start = total.saturating_sub(10);
        for i in last_start..total {
            if !selected.iter().any(|(idx, _)| *idx == i) {
                selected.push((i, &turns[i]));
            }
        }
        selected.sort_by_key(|(i, _)| *i);
        selected
    } else {
        turns.iter().enumerate().collect()
    };

    let mut formatted = String::new();
    for (i, turn) in &selected_turns {
        formatted.push_str(&format_turn(*i, turn, 500));
        formatted.push('\n');
    }

    format!(
        "[SESSION_SUMMARIZE_REQUEST]\n\
         Session: {} | Turns: {}\n\
         Transcript ({} turns shown):\n\
         {}\n\n\
         Generate a summary with: (1) multi-paragraph overview, (2) key topics list, \
         (3) key decisions list, (4) important turn numbers.\n\
         After generating, save with: session_summary_save {} | <your summary JSON>\n\
         JSON format: {{\"summary\": \"...\", \"key_topics\": [...], \"key_decisions\": [...], \"key_turns\": [...]}}",
        name,
        total,
        selected_turns.len(),
        formatted,
        name
    )
}

/// Persist a generated session summary.
/// Param: `<name> | <summary_json>`
pub async fn session_summary_save(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: session_summary_save <name> | <summary_json>".into();
    }

    let pipe_pos = match param.find(" | ") {
        Some(pos) => pos,
        None => return "Usage: session_summary_save <name> | <summary_json>".into(),
    };

    let name = param[..pipe_pos].trim();
    let json_str = param[pipe_pos + 3..].trim();

    // Parse the summary JSON
    let summary_data: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => return format!("Failed to parse summary JSON: {}", e),
    };

    // Get current turn count and compute source hash
    let (turns, total) = fetch_turns(db, name).await;
    if total == 0 {
        return format!("Session '{}' has no history to summarize.", name);
    }

    let turns_json = serde_json::to_string(&turns).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(turns_json.as_bytes());
    let source_hash = format!("{:x}", hasher.finalize());

    // Build the summary document
    let doc = serde_json::json!({
        "session_name": name,
        "summary": summary_data.get("summary").and_then(|v| v.as_str()).unwrap_or(""),
        "key_topics": summary_data.get("key_topics").unwrap_or(&serde_json::json!([])),
        "key_decisions": summary_data.get("key_decisions").unwrap_or(&serde_json::json!([])),
        "key_turns": summary_data.get("key_turns").unwrap_or(&serde_json::json!([])),
        "generated_at": Utc::now().to_rfc3339(),
        "turn_count_at_generation": total,
        "source_hash": source_hash,
    });

    // Write to sessions.{name}.summary collection
    let summary_col = format!("sessions.{}.summary", name);
    ensure_collection(db, &summary_col).await;

    // Upsert: try PATCH if exists, otherwise insert with well-known _id
    let doc_with_id = {
        let mut d = doc.clone();
        if let Some(obj) = d.as_object_mut() {
            obj.insert("_id".into(), serde_json::json!("summary"));
        }
        d
    };

    let write_result = match db.read(&summary_col, "summary").await {
        Ok(_) => {
            // Document exists — patch it
            db.patch_document(&summary_col, "summary", &doc)
                .await
                .map(|_| "summary".to_string())
        }
        Err(_) => {
            // Document doesn't exist — insert with well-known _id
            db.write(&summary_col, &doc_with_id).await
        }
    };

    match write_result {
        Ok(id) => {
            // Write to consolidation log
            ensure_collection(db, "system.consolidation_log").await;
            let log_entry = serde_json::json!({
                "action": "session_summary_save",
                "session": name,
                "turn_count": total,
                "source_hash": source_hash,
                "timestamp": Utc::now().to_rfc3339(),
            });
            let _ = db.write("system.consolidation_log", &log_entry).await;

            format!(
                "Summary saved for session '{}' ({} turns, hash: {}). ID: {}",
                name,
                total,
                &source_hash[..12],
                id
            )
        }
        Err(e) => format!("Failed to save summary: {}", e),
    }
}

/// Extract learnings from a session into memory (Option B: returns context for Brain).
/// Param: `<name> [start-end]`
pub async fn session_extract(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: session_extract <name> or session_extract <name> <start>-<end>"
            .into();
    }

    let parts: Vec<&str> = param.splitn(2, ' ').collect();
    let name = parts[0];
    let range_str = if parts.len() > 1 { parts[1].trim() } else { "" };

    let (turns, total) = fetch_turns(db, name).await;
    if total == 0 {
        return format!("No conversation history found for session '{}'.", name);
    }

    let (start, end) = if range_str.is_empty() {
        (0, total)
    } else {
        match parse_range(range_str, total) {
            Ok(v) => v,
            Err(msg) => return format!("session_extract rejected ({}) for session '{}' (total turns: {})", msg, name, total),
        }
    };

    let turn_slice = &turns[start..end];

    // For large selections, pick key turns
    let selected: Vec<(usize, &serde_json::Value)> = if turn_slice.len() > 50 {
        let mut sel = Vec::new();
        // First 5
        for i in 0..5.min(turn_slice.len()) {
            sel.push((start + i, &turn_slice[i]));
        }
        // Tool-containing turns — legacy [TOOL:...] substring fallback
        // for pre-NATIVE-TOOLS-01 session content. New sessions store
        // tool calls as structured blocks; this path is a best-effort
        // for frozen legacy sessions that remain readable.
        for i in 5..turn_slice.len().saturating_sub(10) {
            let content = turn_slice[i]
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            if content.contains("[TOOL:") {
                sel.push((start + i, &turn_slice[i]));
            }
        }
        // Last 10
        let last_start = turn_slice.len().saturating_sub(10);
        for i in last_start..turn_slice.len() {
            if !sel.iter().any(|(idx, _)| *idx == start + i) {
                sel.push((start + i, &turn_slice[i]));
            }
        }
        sel.sort_by_key(|(i, _)| *i);
        sel
    } else {
        turn_slice.iter().enumerate().map(|(i, t)| (start + i, t)).collect()
    };

    let mut formatted = String::new();
    for (i, turn) in &selected {
        formatted.push_str(&format_turn(*i, turn, 500));
        formatted.push('\n');
    }

    format!(
        "[SESSION_EXTRACT_REQUEST]\n\
         Session: {} | Turns shown: {} (of {} in range {}-{})\n\
         Transcript:\n\
         {}\n\n\
         Identify durable learnings from this session. For each:\n\
         - Content (the fact, preference, decision, or action item)\n\
         - Suggested #tags\n\
         - Category: factual / preference / decision / action-item\n\n\
         Present the proposed extractions for approval. After approval, save each with:\n\
         remember <content> #tags",
        name,
        selected.len(),
        end - start,
        start + 1,
        end,
        formatted
    )
}

// ── Native tool-use registrations (NATIVE-TOOLS-01) ──

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tools::registry::DispatchContext;

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "session_list",
    description = "List all sessions with turn counts, status, and dates."
)]
pub struct SessionListArgs {}

impl SessionListArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(session_list(ctx.db).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "session_read",
    description = "Read session transcript. range is an optional turn range like \"1-20\", \"1-\", or \"5\"; when absent, the last 30 turns are returned."
)]
pub struct SessionReadArgs {
    pub name: String,
    #[serde(default)]
    pub range: String,
}

impl SessionReadArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = if self.range.is_empty() {
            self.name
        } else {
            format!("{} {}", self.name, self.range)
        };
        Ok(session_read(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "session_search",
    description = "Full-text search across sessions. Unquoted terms AND-match (all must appear in the same turn); wrap in double quotes for literal phrase. `session` (optional) narrows the scope to a single session."
)]
pub struct SessionSearchArgs {
    pub query: String,
    /// Optional session name to scope the search.
    #[serde(default)]
    pub session: Option<String>,
}

impl SessionSearchArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(session_search(ctx.db, &self.query, self.session.as_deref()).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "session_meta",
    description = "Structured metadata for a session: turn count, created-at, last activity."
)]
pub struct SessionMetaArgs {
    pub name: String,
}

impl SessionMetaArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(session_meta(ctx.db, &self.name).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "session_delta",
    description = "Return turns added to a session since a given turn number (useful for incremental follow-up)."
)]
pub struct SessionDeltaArgs {
    pub name: String,
    pub since_turn: u32,
}

impl SessionDeltaArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {}", self.name, self.since_turn);
        Ok(session_delta(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "memory_scan",
    description = "Inventory memory entries: counts, tags, age distribution, duplicate candidates. tag (optional) filters to entries with that tag."
)]
pub struct MemoryScanArgs {
    #[serde(default)]
    pub tag: String,
}

impl MemoryScanArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(memory_scan(ctx.db, &self.tag).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "memory_dedup",
    description = "Find duplicate memory entries and propose merges (read-only, no writes). ids (optional) is a comma-separated list to narrow the check."
)]
pub struct MemoryDedupArgs {
    /// Comma-separated memory entry ids to restrict the dedup check.
    #[serde(default)]
    pub ids: String,
}

impl MemoryDedupArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(memory_dedup(ctx.db, &self.ids).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "session_summarize",
    description = "Generate or retrieve a structured summary for a session."
)]
pub struct SessionSummarizeArgs {
    pub name: String,
}

impl SessionSummarizeArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(session_summarize(ctx.db, &self.name).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "session_summary_save",
    description = "Save a generated session summary as JSON to the summaries collection."
)]
pub struct SessionSummarySaveArgs {
    pub name: String,
    /// Summary as a JSON object (serialized as string for tool transport).
    pub summary_json: String,
}

impl SessionSummarySaveArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} | {}", self.name, self.summary_json);
        Ok(session_summary_save(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "session_extract",
    description = "Extract durable learnings from a session into memory. range (optional) narrows to a specific turn range like \"10-30\"."
)]
pub struct SessionExtractArgs {
    pub name: String,
    #[serde(default)]
    pub range: String,
}

impl SessionExtractArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = if self.range.is_empty() {
            self.name
        } else {
            format!("{} {}", self.name, self.range)
        };
        Ok(session_extract(ctx.db, &param).await)
    }
}

// turn_trace (NATIVE-TOOLS-01 follow-up) — expose the current turn's
// tool-call trace to the Brain. Current turn is served from in-memory
// state; prior turns fall back to the `tools.turn_trace` WardSONDB
// collection populated by the dispatch site in grpc_service.rs.

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "turn_trace",
    description = "Inspect tool calls made in the current or recent turns (tool name, args preview, outcome, elapsed_ms). turn_index_back=0 (default) returns the current turn in-memory; pass turn_index_back=1,2,... for prior turns from persisted history. session (optional) defaults to the current session."
)]
pub struct TurnTraceArgs {
    /// Max entries to return (default 20, capped 100).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Look back N turns from the current turn. 0 (default) reads the
    /// current turn's in-memory trace; >=1 queries tools.turn_trace.
    #[serde(default)]
    pub turn_index_back: Option<usize>,
    /// Narrow to a specific session. Default: current session.
    #[serde(default)]
    pub session: Option<String>,
}

impl TurnTraceArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let n = self.limit.unwrap_or(20).min(100);
        let back = self.turn_index_back.unwrap_or(0);

        if back == 0 && self.session.is_none() {
            // Current turn — read the in-memory trace (cheap, no DB round-trip).
            let entries: Vec<embra_tools_core::TraceEntry> = match ctx.trace.lock() {
                Ok(g) => g.iter().rev().take(n).cloned().collect(),
                Err(_) => Vec::new(),
            };
            if entries.is_empty() {
                return Ok(
                    "turn_trace (current turn): no tool calls dispatched yet this turn.".into(),
                );
            }
            return Ok(
                serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".into()),
            );
        }

        // Prior turn or cross-session — query persistence.
        let sess = self
            .session
            .unwrap_or_else(|| ctx.session_name.to_string());
        let target_index = ctx.turn_index.saturating_sub(back);
        let filter = serde_json::json!({
            "filter": {"session": sess, "turn_index": target_index},
            "limit": n,
            "sort": {"started_at": 1},
        });
        let docs = ctx
            .db
            .query("tools.turn_trace", &filter)
            .await
            .unwrap_or_default();
        if docs.is_empty() {
            return Ok(format!(
                "turn_trace (session='{}', turn_index={}): no entries.",
                sess, target_index
            ));
        }
        Ok(serde_json::to_string_pretty(&docs).unwrap_or_else(|_| "[]".into()))
    }
}

#[cfg(test)]
mod native_args_tests {
    use super::*;

    #[test]
    fn session_read_range_optional() {
        let a: SessionReadArgs =
            serde_json::from_value(serde_json::json!({"name": "main"})).unwrap();
        assert_eq!(a.name, "main");
        assert_eq!(a.range, "");

        let b: SessionReadArgs = serde_json::from_value(serde_json::json!({
            "name": "main", "range": "1-20"
        }))
        .unwrap();
        assert_eq!(b.range, "1-20");
    }

    #[test]
    fn session_delta_requires_since_turn() {
        let a: SessionDeltaArgs = serde_json::from_value(serde_json::json!({
            "name": "main", "since_turn": 42
        }))
        .unwrap();
        assert_eq!(a.since_turn, 42);

        let err =
            serde_json::from_value::<SessionDeltaArgs>(serde_json::json!({"name": "main"}))
                .unwrap_err();
        assert!(err.to_string().contains("since_turn"));
    }

    #[test]
    fn session_tools_register() {
        let names: Vec<&'static str> = inventory::iter::<crate::tools::registry::ToolDescriptor>()
            .into_iter()
            .map(|d| d.name)
            .collect();
        for expected in [
            "session_list",
            "session_read",
            "session_search",
            "session_meta",
            "session_delta",
            "memory_scan",
            "memory_dedup",
            "session_summarize",
            "session_summary_save",
            "session_extract",
            "turn_trace",
        ] {
            assert!(names.contains(&expected), "{} not registered", expected);
        }
    }

    #[test]
    fn turn_trace_args_defaults() {
        let a: TurnTraceArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(a.limit.is_none());
        assert!(a.turn_index_back.is_none());
        assert!(a.session.is_none());
    }

    #[test]
    fn turn_trace_index_back_math_is_logical_turns() {
        // Regression guard for Embra_Debug #44.
        //
        // `SessionHistory.turns: Vec<Message>` stores one entry per
        // role-message — a single logical turn appends both a user message
        // and an assistant message, so a session with N turns has 2N
        // entries.
        //
        // `grpc_service.rs` derives `turn_index = history.len() / 2` so
        // that `turn_index` names the logical turn the model is now
        // executing. The `turn_trace` read path computes
        // `target = ctx.turn_index - back`. The two must agree on units —
        // if either side reverts to message-count semantics, `back=1`
        // queries an index that was never written and returns empty.
        for completed_turns in 0usize..6 {
            let history_len: usize = completed_turns * 2; // 2 messages per logical turn
            let turn_index: usize = history_len / 2;
            assert_eq!(
                turn_index, completed_turns,
                "turn_index must be the logical turn number, not message count"
            );
            // back=1 from the upcoming turn (turn_index = completed_turns)
            // must point at the last completed turn (turn_index - 1).
            if completed_turns > 0 {
                let target = turn_index.saturating_sub(1);
                assert_eq!(
                    target,
                    completed_turns - 1,
                    "back=1 must address the immediately prior logical turn"
                );
            }
        }
    }

    #[test]
    fn session_search_args_session_optional() {
        let a: SessionSearchArgs =
            serde_json::from_value(serde_json::json!({"query": "foo"})).unwrap();
        assert_eq!(a.query, "foo");
        assert!(a.session.is_none());

        let b: SessionSearchArgs = serde_json::from_value(serde_json::json!({
            "query": "foo", "session": "main"
        }))
        .unwrap();
        assert_eq!(b.session.as_deref(), Some("main"));
    }
}

#[cfg(test)]
mod session_query_plan_tests {
    use super::SessionQueryPlan;

    fn lower(s: &str) -> String {
        s.to_lowercase()
    }

    #[test]
    fn empty_query_is_none() {
        assert!(SessionQueryPlan::parse("").is_none());
        assert!(SessionQueryPlan::parse("   ").is_none());
    }

    #[test]
    fn single_token_matches_substring() {
        let p = SessionQueryPlan::parse("Anthropic").unwrap();
        assert!(!p.phrase_mode);
        assert!(p.match_turn(&lower("we called the Anthropic API")).is_some());
        assert!(p.match_turn(&lower("something unrelated")).is_none());
    }

    #[test]
    fn unquoted_multi_word_is_and_match() {
        // Closes #34: the previous behavior treated "tool sweep" as a phrase
        // and missed turns where both words appeared non-adjacent.
        let p = SessionQueryPlan::parse("tool sweep").unwrap();
        assert!(!p.phrase_mode);
        assert!(p
            .match_turn(&lower("the Sprint 3 TOOL verification SWEEP finished"))
            .is_some());
        assert!(p.match_turn(&lower("only mentions tool")).is_none());
        assert!(p.match_turn(&lower("only mentions sweep")).is_none());
    }

    #[test]
    fn quoted_multi_word_is_phrase_match() {
        let p = SessionQueryPlan::parse("\"tool sweep\"").unwrap();
        assert!(p.phrase_mode);
        assert!(p.match_turn(&lower("the tool sweep happened")).is_some());
        // AND-style non-adjacent should NOT match in phrase mode.
        assert!(p
            .match_turn(&lower("tool verification sweep"))
            .is_none());
    }

    #[test]
    fn phrase_anchor_points_at_substring() {
        let p = SessionQueryPlan::parse("\"tool sweep\"").unwrap();
        let (pos, len) = p.match_turn(&lower("the tool sweep was clean")).unwrap();
        assert_eq!(pos, 4); // "the " prefix
        assert_eq!(len, 10); // "tool sweep"
    }

    #[test]
    fn and_mode_anchor_points_at_first_token() {
        let p = SessionQueryPlan::parse("sweep tool").unwrap();
        let (pos, len) = p
            .match_turn(&lower("the tool verification sweep"))
            .unwrap();
        // First token is "sweep" — its position in content_lower
        assert_eq!(&"the tool verification sweep".to_lowercase()[pos..pos + len], "sweep");
    }
}
