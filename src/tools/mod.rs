use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::db::WardsonDbClient;

pub mod engineering;
pub mod security;

// ── Startup Time ──

static START_TIME: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

pub fn init_start_time() {
    START_TIME.get_or_init(std::time::Instant::now);
}

fn uptime_secs() -> u64 {
    START_TIME.get().map(|t| t.elapsed().as_secs()).unwrap_or(0)
}

// ── Tool Dispatch ──

/// Parse a `[TOOL:name]` or `[TOOL:name param...]` tag and execute the tool.
/// Returns `(tool_name, result_string)` or None if not a recognized tool.
pub async fn dispatch(
    tag: &str,
    db: &WardsonDbClient,
    config_tz: &str,
    session_name: &str,
) -> Option<String> {
    // Strip brackets: "[TOOL:recall hello world]" -> "recall hello world"
    let inner = tag
        .strip_prefix("[TOOL:")
        .and_then(|s| s.strip_suffix(']'))?;

    let (name, param) = match inner.find(' ') {
        Some(pos) => (&inner[..pos], inner[pos + 1..].trim()),
        None => (inner, ""),
    };

    let result = match name {
        "system_status" => {
            let status = system_status(db).await;
            serde_json::to_string_pretty(&status).unwrap_or_default()
        }
        "check_update" => match check_wardsondb_update().await {
            Some(info) => format!("WardSONDB update available: v{} (current: v{})", info.version, info.current_version),
            None => "WardSONDB is up to date.".into(),
        },
        "recall" => recall(db, param).await,
        "remember" => remember(db, param, session_name).await,
        "forget" => forget(db, param).await,
        "uptime_report" => uptime_report(db).await,
        "introspect" => introspect(db, param).await,
        "changelog" => changelog(db, session_name).await,
        "time" => time_now(config_tz),
        "countdown" => countdown(db, param).await,
        "session_summary" => session_summary(db, session_name).await,
        "calculate" => calculate(param),
        "define" => define(db, param).await,
        "draft" => draft(db, param, session_name).await,
        "get" => get(db, param).await,
        "search_memory" => recall(db, param).await, // alias
        // Security tools (FEATURE-003)
        "security_check" => security::security_check().await,
        "port_scan" => security::port_scan(param).await,
        "firewall_status" => security::firewall_status(),
        "ssh_sessions" => security::ssh_sessions(),
        "security_audit" => security::security_audit(),
        // Engineering tools (FEATURE-004)
        "git_status" => engineering::git_status(param).await,
        "git_log" => engineering::git_log(param).await,
        "plan" => engineering::plan(db, param).await,
        "tasks" => engineering::tasks(db, param).await,
        "task_add" => engineering::task_add(db, param).await,
        "task_done" => engineering::task_done(db, param).await,
        "gh_issues" => engineering::gh_issues(param).await,
        "gh_prs" => engineering::gh_prs(param).await,
        _ => return None,
    };

    Some(result)
}

// ── Existing Tools ──

#[derive(Debug, Serialize)]
pub struct SystemStatus {
    pub version: String,
    pub uptime_seconds: u64,
    pub wardsondb_healthy: bool,
    pub wardsondb_collections: Vec<String>,
    pub memory_usage_mb: Option<u64>,
    pub soul_status: String,
}

pub async fn system_status(db: &WardsonDbClient) -> SystemStatus {
    let healthy = db.health().await.unwrap_or(false);
    let collections = db.list_collections().await.unwrap_or_default();
    let soul_status = if db.collection_exists("soul.invariant").await.unwrap_or(false) {
        "sealed"
    } else {
        "unsealed"
    };

    SystemStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: uptime_secs(),
        wardsondb_healthy: healthy,
        wardsondb_collections: collections,
        memory_usage_mb: get_memory_usage_mb(),
        soul_status: soul_status.into(),
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

    let results = db
        .query("memory.entries", &serde_json::json!({}))
        .await
        .unwrap_or_default();

    if results.is_empty() {
        return "No memory entries found.".into();
    }

    let query_lower = query.to_lowercase();
    let matching: Vec<&serde_json::Value> = if query.is_empty() {
        // Return all (latest first, up to 20)
        results.iter().rev().take(20).collect()
    } else {
        results
            .iter()
            .filter(|doc| {
                let content = doc
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let tags = doc
                    .get("tags")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                content.to_lowercase().contains(&query_lower)
                    || tags.to_lowercase().contains(&query_lower)
            })
            .collect()
    };

    if matching.is_empty() {
        return format!("No memory entries matching '{}'.", query);
    }

    let mut output = format!("Found {} matching entries:\n", matching.len());
    for doc in matching.iter().take(10) {
        let id = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()).unwrap_or("?");
        let content = doc.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let ts = doc.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let tags = doc.get("tags").and_then(|v| v.as_str()).unwrap_or("");
        output.push_str(&format!("  [{}] {} (tags: {}) — {}\n", id, content, tags, ts));
    }
    output
}

async fn remember(db: &WardsonDbClient, content: &str, session: &str) -> String {
    if content.is_empty() {
        return "Nothing to remember. Provide content after [TOOL:remember ...].".into();
    }

    ensure_collection(db, "memory.entries").await;

    // Parse optional tags: "content text #tag1 #tag2"
    let mut tags = Vec::new();
    let mut text_parts = Vec::new();
    for word in content.split_whitespace() {
        if word.starts_with('#') {
            tags.push(word.trim_start_matches('#'));
        } else {
            text_parts.push(word);
        }
    }
    let text = text_parts.join(" ");
    let tags_str = tags.join(", ");

    let doc = serde_json::json!({
        "content": text,
        "tags": tags_str,
        "session": session,
        "created_at": Utc::now().to_rfc3339(),
    });

    match db.write("memory.entries", &doc).await {
        Ok(id) => format!("Remembered. Entry ID: {}", id),
        Err(e) => format!("Failed to save memory: {}", e),
    }
}

async fn forget(db: &WardsonDbClient, id: &str) -> String {
    if id.is_empty() {
        return "Provide the entry ID to forget: [TOOL:forget <id>]".into();
    }

    match db.delete("memory.entries", id.trim()).await {
        Ok(()) => format!("Memory entry {} has been removed.", id.trim()),
        Err(e) => format!("Failed to remove entry: {}", e),
    }
}

// ── Self-Awareness Tools ──

async fn uptime_report(db: &WardsonDbClient) -> String {
    let uptime = uptime_secs();
    let hours = uptime / 3600;
    let mins = (uptime % 3600) / 60;

    let collections = db.list_collections().await.unwrap_or_default();

    // Count sessions
    let session_count = collections
        .iter()
        .filter(|c| c.starts_with("sessions.") && c.ends_with(".meta"))
        .count();

    // Count memory entries
    let memory_count = db
        .query("memory.entries", &serde_json::json!({}))
        .await
        .map(|v| v.len())
        .unwrap_or(0);

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
         Uptime: {}h {}m\n\
         WardSONDB: {}\n\
         Collections: {}\n\
         Sessions created: {}\n\
         Total messages exchanged: {}\n\
         Memory entries stored: {}\n\
         Soul: {}",
        hours,
        mins,
        if healthy { "healthy" } else { "unhealthy" },
        collections.len(),
        session_count,
        total_messages,
        memory_count,
        if soul_sealed { "sealed" } else { "unsealed" }
    )
}

async fn introspect(db: &WardsonDbClient, focus: &str) -> String {
    let mut output = String::new();
    let focus_lower = focus.to_lowercase();

    // Keyword mapping: focus terms → soul key substrings to match (searches one level deep)
    let soul_key_mappings: &[(&str, &[&str])] = &[
        ("ethics", &["ethical", "boundaries", "non_negotiable"]),
        ("purpose", &["invariant", "declaration", "core_truths", "purpose"]),
        ("constraints", &["boundaries", "operational", "continuity_protocol", "constraint"]),
        ("values", &["non_negotiable", "core_truths", "values"]),
        ("soul", &[]),  // empty = show full soul
    ];

    // Load soul
    let soul_docs = db.query("soul.invariant", &serde_json::json!({})).await.unwrap_or_default();
    if let Some(doc) = soul_docs.into_iter().next() {
        let soul = doc.get("soul").unwrap_or(&doc);

        if focus.is_empty() {
            // No focus — show full soul
            output.push_str("=== SOUL (IMMUTABLE) ===\n");
            output.push_str(&serde_json::to_string_pretty(soul).unwrap_or_default());
            output.push('\n');
        } else if let Some(obj) = soul.as_object() {
            // Resolve focus to a set of key substrings via keyword mapping
            let mapped_terms: Vec<&str> = soul_key_mappings
                .iter()
                .filter(|(keyword, _)| focus_lower.contains(keyword))
                .flat_map(|(_, terms)| terms.iter().copied())
                .collect();

            let filtered: serde_json::Map<String, serde_json::Value> = if mapped_terms.is_empty() {
                // No keyword mapping matched — fall back to direct key name matching
                // Also search one level deep: match if any sub-key contains the focus
                obj.iter()
                    .filter(|(k, v)| {
                        let k_lower = k.to_lowercase();
                        if k_lower.contains(&focus_lower) {
                            return true;
                        }
                        // Search one level deeper into sub-objects
                        if let Some(sub_obj) = v.as_object() {
                            sub_obj.keys().any(|sk| sk.to_lowercase().contains(&focus_lower))
                        } else {
                            false
                        }
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            } else if mapped_terms.is_empty() {
                // "soul" keyword with empty terms → show full document
                obj.clone()
            } else {
                // Use mapped terms to find matching keys (one level deep)
                obj.iter()
                    .filter(|(k, v)| {
                        let k_lower = k.to_lowercase();
                        let key_matches = mapped_terms.iter().any(|term| k_lower.contains(term));
                        if key_matches {
                            return true;
                        }
                        // Also check sub-object keys
                        if let Some(sub_obj) = v.as_object() {
                            sub_obj.keys().any(|sk| {
                                let sk_lower = sk.to_lowercase();
                                mapped_terms.iter().any(|term| sk_lower.contains(term))
                            })
                        } else {
                            false
                        }
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            };

            // Check if focus is exactly "soul" — show full document
            if focus_lower == "soul" {
                output.push_str("=== SOUL (IMMUTABLE) ===\n");
                output.push_str(&serde_json::to_string_pretty(soul).unwrap_or_default());
                output.push('\n');
            } else if !filtered.is_empty() {
                output.push_str(&format!("=== SOUL — {} ===\n", focus));
                output.push_str(&serde_json::to_string_pretty(&filtered).unwrap_or_default());
                output.push('\n');
            }
            // If nothing matched, soul section omitted (not an error)
        } else {
            // Soul isn't an object — show fully if focus is "soul"
            if focus_lower == "soul" {
                output.push_str("=== SOUL (IMMUTABLE) ===\n");
                output.push_str(&serde_json::to_string_pretty(soul).unwrap_or_default());
                output.push('\n');
            }
        }
    }

    // Load identity
    if focus.is_empty() || focus_lower.contains("identity") || focus_lower.contains("personality") || focus_lower.contains("traits") {
        let id_docs = db.query("memory.identity", &serde_json::json!({})).await.unwrap_or_default();
        if let Some(doc) = id_docs.into_iter().next() {
            output.push_str("\n=== IDENTITY ===\n");
            output.push_str(&serde_json::to_string_pretty(&doc).unwrap_or_default());
            output.push('\n');
        }
    }

    // Load user profile
    if focus.is_empty() || focus_lower.contains("user") || focus_lower.contains("operator") {
        let user_docs = db.query("memory.user", &serde_json::json!({})).await.unwrap_or_default();
        if let Some(doc) = user_docs.into_iter().next() {
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

    // Recent memory entries
    let entries = db
        .query("memory.entries", &serde_json::json!({}))
        .await
        .unwrap_or_default();

    let recent_entries: Vec<_> = if let Some(ref start) = session_start {
        entries
            .iter()
            .filter(|doc| {
                doc.get("created_at")
                    .and_then(|v| v.as_str())
                    .map(|ts| ts > start.as_str())
                    .unwrap_or(false)
            })
            .collect()
    } else {
        entries.iter().rev().take(5).collect()
    };

    if recent_entries.is_empty() {
        output.push_str("  No new memory entries.\n");
    } else {
        output.push_str(&format!("  {} new memory entries:\n", recent_entries.len()));
        for entry in recent_entries.iter().take(5) {
            let content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("?");
            output.push_str(&format!("    - {}\n", content));
        }
    }

    // List sessions
    let collections = db.list_collections().await.unwrap_or_default();
    let sessions: Vec<_> = collections
        .iter()
        .filter(|c| c.starts_with("sessions.") && c.ends_with(".meta") && !c.contains("learning"))
        .collect();
    output.push_str(&format!("  Total sessions: {}\n", sessions.len()));

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
        return "Usage: [TOOL:countdown <duration> <message>]\nExample: [TOOL:countdown 5m Check the build]".into();
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
        return "Usage: [TOOL:calculate <expression>]\nExample: [TOOL:calculate 1024 * 1024]".into();
    }

    match meval::eval_str(expression) {
        Ok(result) => {
            // Format nicely: no trailing zeros for whole numbers
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
        return "Usage: [TOOL:define <term>] to look up, or [TOOL:define <term> | <definition>] to add".into();
    }

    ensure_collection(db, "knowledge.definitions").await;

    // DESIGN-003: If param contains ` | `, split into term + definition and write
    if let Some(pipe_pos) = param.find(" | ") {
        let term = param[..pipe_pos].trim();
        let definition = param[pipe_pos + 3..].trim();

        if term.is_empty() || definition.is_empty() {
            return "Usage: [TOOL:define <term> | <definition>]".into();
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
        return "Usage: [TOOL:draft <title> | <content>]\nSeparate title and content with ' | '.\nExample: [TOOL:draft Meeting Notes | Key decisions: ...]".into();
    }

    ensure_collection(db, "drafts").await;

    // Parse "title | content" or just treat entire param as content with auto-title
    let (title, content) = if let Some(pos) = param.find(" | ") {
        (&param[..pos], &param[pos + 3..])
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
            Err(e) => format!("Failed to update draft: {}", e),
        }
    } else {
        let mut doc = doc;
        doc["created_at"] = serde_json::json!(Utc::now().to_rfc3339());
        match db.write("drafts", &doc).await {
            Ok(id) => format!("Draft created: '{}' (ID: {})", title, id),
            Err(e) => format!("Failed to save draft: {}", e),
        }
    }
}

// ── Get Tool (DESIGN-002) ──

async fn get(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: [TOOL:get <collection> <id>]\nExample: [TOOL:get memory.entries abc123]".into();
    }

    let parts: Vec<&str> = param.splitn(2, ' ').collect();
    if parts.len() < 2 {
        return "Usage: [TOOL:get <collection> <id>]".into();
    }

    let (collection, id) = (parts[0], parts[1].trim());

    match db.read(collection, id).await {
        Ok(doc) => serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "Failed to format document".into()),
        Err(e) => format!("Failed to read {}/{}: {}", collection, id, e),
    }
}

// ── Tool Tag Extraction ──

/// Extract `[TOOL:...]` tags from AI output safely.
/// Strips fenced code blocks and inline code first so that examples/documentation
/// are never mis-parsed as real tool invocations (BUG-001 fix).
/// Only lines whose trimmed content is exactly one `[TOOL:...]` tag are matched.
pub fn extract_tool_tags(text: &str) -> Vec<String> {
    // Step 1: Strip fenced code blocks (``` ... ```)
    let mut stripped = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            stripped.push('\n');
            continue;
        }
        if in_fence {
            stripped.push('\n');
        } else {
            stripped.push_str(line);
            stripped.push('\n');
        }
    }

    // Step 2: Strip inline backtick spans
    let mut no_inline = String::with_capacity(stripped.len());
    let mut in_backtick = false;
    for ch in stripped.chars() {
        if ch == '`' {
            in_backtick = !in_backtick;
        } else if !in_backtick {
            no_inline.push(ch);
        }
    }

    // Step 3: Match lines that are exactly one [TOOL:...] tag
    let mut tags = Vec::new();
    for line in no_inline.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[TOOL:") && trimmed.ends_with(']') && trimmed.matches("[TOOL:").count() == 1 {
            tags.push(trimmed.to_string());
        }
    }

    tags
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

fn parse_duration(s: &str) -> u64 {
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
