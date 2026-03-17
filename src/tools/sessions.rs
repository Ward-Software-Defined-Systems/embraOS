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

/// Get all session collection prefixes (excluding learning sessions).
async fn session_names(db: &WardsonDbClient) -> Vec<String> {
    let collections = db.list_collections().await.unwrap_or_default();
    collections
        .iter()
        .filter(|c| c.starts_with("sessions.") && c.ends_with(".meta") && !c.contains("learning"))
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

/// Parse a turn range string like "1-20", "80-", or "" into (start_0indexed, end_exclusive).
fn parse_range(range_str: &str, total: usize) -> (usize, usize) {
    if range_str.is_empty() {
        // Default: last 30 turns
        let start = if total > 30 { total - 30 } else { 0 };
        return (start, total);
    }

    if let Some(dash_pos) = range_str.find('-') {
        let start_str = &range_str[..dash_pos];
        let end_str = &range_str[dash_pos + 1..];

        let start_1 = start_str.parse::<usize>().unwrap_or(1).max(1);
        let start_0 = start_1 - 1;

        let end = if end_str.is_empty() {
            total
        } else {
            end_str.parse::<usize>().unwrap_or(total).min(total)
        };

        (start_0.min(total), end.min(total))
    } else {
        // Single number — show that one turn
        let n = range_str.parse::<usize>().unwrap_or(1).max(1);
        let idx = (n - 1).min(total.saturating_sub(1));
        (idx, (idx + 1).min(total))
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
        return "Usage: [TOOL:session_read <name>] or [TOOL:session_read <name> <start>-<end>]".into();
    }

    let parts: Vec<&str> = param.splitn(2, ' ').collect();
    let name = parts[0];
    let range_str = if parts.len() > 1 { parts[1].trim() } else { "" };

    let (turns, total) = fetch_turns(db, name).await;
    if total == 0 {
        return format!("No conversation history found for session '{}'.", name);
    }

    let (start, end) = parse_range(range_str, total);

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
/// Param: `<query> [session_name]`
pub async fn session_search(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: [TOOL:session_search <query>] or [TOOL:session_search <query> <session>]"
            .into();
    }

    // Strip surrounding double quotes (phrase delimiter support)
    let param = if param.starts_with('"') && param.ends_with('"') && param.len() >= 2 {
        &param[1..param.len() - 1]
    } else {
        param
    };

    // Check if the last word is a session name
    let parts: Vec<&str> = param.rsplitn(2, ' ').collect();
    let (query, specific_session) = if parts.len() == 2 {
        // Could be "query session" or "multi word query"
        // Check if the last word matches a session name
        let candidate = parts[0];
        let names = session_names(db).await;
        if names.iter().any(|n| n == candidate) {
            (parts[1], Some(candidate.to_string()))
        } else {
            (param, None)
        }
    } else {
        (param, None)
    };

    // Also strip quotes from the resolved query (handles `"phrase" session` form)
    let query = query
        .strip_prefix('"')
        .and_then(|q| q.strip_suffix('"'))
        .unwrap_or(query);

    let query_lower = query.to_lowercase();
    let names_to_search = if let Some(ref s) = specific_session {
        vec![s.clone()]
    } else {
        session_names(db).await
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

            if let Some(match_pos) = content_lower.find(&query_lower) {
                let role = turn
                    .get("role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("?");

                // Extract context around match (snap to char boundaries)
                let mut start = match_pos.saturating_sub(50);
                while start > 0 && !content.is_char_boundary(start) {
                    start -= 1;
                }
                let mut end = (match_pos + query.len() + 50).min(content.len());
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
        return format!("No matches found for '{}'.", query);
    }

    let mut output = format!("Search results for '{}' ({} matches):\n\n", query, results.len());
    for r in &results {
        output.push_str(r);
        output.push('\n');
    }
    output
}

/// Structured metadata for a session.
pub async fn session_meta(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: [TOOL:session_meta <name>]".into();
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
        return "Usage: [TOOL:session_delta <name> <since_turn>]".into();
    }

    let parts: Vec<&str> = param.splitn(2, ' ').collect();
    if parts.len() < 2 {
        return "Usage: [TOOL:session_delta <name> <since_turn>]".into();
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
        .query("memory.entries", &serde_json::json!({}))
        .await
        .unwrap_or_default();

    if entries.is_empty() {
        return "No memory entries found.".into();
    }

    let tag_filter = if param.is_empty() {
        None
    } else {
        Some(param.to_lowercase())
    };

    // Filter entries by tag if specified
    let entries: Vec<&serde_json::Value> = if let Some(ref filter) = tag_filter {
        entries
            .iter()
            .filter(|doc| {
                let tags = doc
                    .get("tags")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                tags.contains(filter.as_str())
            })
            .collect()
    } else {
        entries.iter().collect()
    };

    let total = entries.len();

    // Tag frequency
    let mut tag_counts = std::collections::HashMap::new();
    for doc in &entries {
        let tags = doc
            .get("tags")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        for tag in tags.split(", ").filter(|t| !t.is_empty()) {
            *tag_counts.entry(tag.to_string()).or_insert(0u32) += 1;
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
            let tags = doc
                .get("tags")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
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
        "To execute this plan, approve and I will use [TOOL:remember] and [TOOL:forget] to merge/delete entries.",
    );

    output
}

// ── Phase C: Session Consolidation Tools ──

/// Generate structured summary for a session (Option B: returns context for Brain).
/// Param: session name.
pub async fn session_summarize(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: [TOOL:session_summarize <name>]".into();
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
        // Turns containing [TOOL: tags
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
         After generating, save with: [TOOL:session_summary_save {} | <your summary JSON>]\n\
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
        return "Usage: [TOOL:session_summary_save <name> | <summary_json>]".into();
    }

    let pipe_pos = match param.find(" | ") {
        Some(pos) => pos,
        None => return "Usage: [TOOL:session_summary_save <name> | <summary_json>]".into(),
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

    // Upsert: delete existing docs first, then write
    let existing = db
        .query(&summary_col, &serde_json::json!({}))
        .await
        .unwrap_or_default();

    for existing_doc in &existing {
        if let Some(id) = existing_doc
            .get("_id")
            .or_else(|| existing_doc.get("id"))
            .and_then(|v| v.as_str())
        {
            let _ = db.delete(&summary_col, id).await;
        }
    }

    match db.write(&summary_col, &doc).await {
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
        return "Usage: [TOOL:session_extract <name>] or [TOOL:session_extract <name> <start>-<end>]"
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
        parse_range(range_str, total)
    };

    let turn_slice = &turns[start..end];

    // For large selections, pick key turns
    let selected: Vec<(usize, &serde_json::Value)> = if turn_slice.len() > 50 {
        let mut sel = Vec::new();
        // First 5
        for i in 0..5.min(turn_slice.len()) {
            sel.push((start + i, &turn_slice[i]));
        }
        // Tool-containing turns
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
         [TOOL:remember <content> #tags]",
        name,
        selected.len(),
        end - start,
        start + 1,
        end,
        formatted
    )
}
