//! The brain's tools. The domains have their own files (`engineering/`,
//! `knowledge` through `registry`, `memory`, `reminders`, `status`,
//! `sessions`, `security`, `media`, `guardian`, the file tools); this file
//! keeps the shared helpers (`ensure_collection`, `resolve_timezone`,
//! `parse_duration`) and the small tools with no domain of their own:
//! `time`, `session_summary`, `introspect`, `set_name`, `calculate`,
//! `define`, `draft` and `get`.

use chrono::Utc;
use serde::Deserialize;
use tracing::info;

use crate::db::WardsonDbClient;
use crate::knowledge;

mod calc;
pub mod cron;
pub mod engineering;
pub mod express;
pub mod file_copy;
pub mod file_offer;
pub mod file_patch;
pub mod guardian;
pub mod jail;
pub mod media;
pub mod memory;
pub mod registry;
pub mod reminders;
pub mod security;
pub mod sessions;
pub mod status;

pub(crate) use memory::{remember_record, untag, Recorded};
pub use reminders::{check_reminders, FiredReminder};
pub(crate) use reminders::{REMINDER_RETENTION_DAYS, REMINDER_TTL_FIELD};
pub use status::{init_start_time, system_status};

/// Every subprocess a tool starts begins here: the child is killed when the
/// future that owns it is dropped, so the dispatcher's timeout
/// (`registry::enforce_timeout`) ends the process and not only the wait.
/// `engineering::git::git_command` and `security::ssh_command` build on it.
pub(crate) fn subprocess(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.kill_on_drop(true);
    cmd
}
#[cfg(test)]
pub(crate) use status::ProviderStatus;
// Reached as `super::…` by sibling modules and by a test of this one.
use memory::promoted_pointer;
#[cfg(test)]
use memory::forget;

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;

use crate::tools::registry::DispatchContext;

// ── Memory & Knowledge Tools ──

/// Best effort, before a write to a collection a boot normally creates: the
/// collection is created when the probe says it is absent or cannot say (a
/// failing probe reads as absent, so the create is tried; a 409 means it was
/// there). A failure is logged, and the caller reports its own write.
async fn ensure_collection(db: &WardsonDbClient, name: &str) {
    if db.collection_exists(name).await.unwrap_or(false) {
        return;
    }
    if let Err(e) = db.create_collection(name).await
        && !crate::db::error::is_conflict(&e)
    {
        tracing::warn!(target: "tools", collection = name, "could not create the collection: {e}");
    }
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
        let existing = match db.fetch_collection("drafts").await {
            Ok(docs) => docs,
            Err(e) => return format!("draft failed (could not read the drafts: {e})"),
        };
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

    // DESIGN-001: Check for existing draft with same title and upsert. A
    // read that fails is reported: read as "none", it would create a second
    // draft under the same title (#60).
    let existing = match db.fetch_collection("drafts").await {
        Ok(docs) => docs,
        Err(e) => return format!("draft failed (could not check for an existing draft '{title}': {e})"),
    };
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
        Ok(doc) => {
            // The configuration holds the operator's credentials; the model
            // gets the document with those fields redacted.
            let doc = if collection == "config.system" && id == "config" {
                crate::config::redact(doc)
            } else {
                doc
            };
            serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "Failed to format document".into())
        }
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
    description = "Read a specific document from WardSONDB by collection and id. The configuration document (config.system/config) comes back with its credential fields redacted."
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
}

#[cfg(test)]
mod id_gate_tests {
    //! A model-supplied id that cannot stand in a URL is refused by the
    //! database client before any request; the tools carry the refusal.
    use super::{forget, get};
    use crate::db::WardsonDbClient;
    use wiremock::MockServer;

    // Deserialized, not a struct literal: see `learning::phases` tests.
    fn config() -> crate::config::SystemConfig {
        serde_json::from_value(serde_json::json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
            "created_at": "", "version": "test", "api_provider": "anthropic"
        }))
        .expect("minimal config deserializes")
    }

    #[tokio::test]
    async fn a_traversal_id_is_refused_by_every_entry_tool_before_the_database() {
        let server = MockServer::start().await;
        let db = WardsonDbClient::from_url(&server.uri());
        let id = "../../soul.invariant/docs/soul";
        let outs = [
            get(&db, &format!("memory.entries {id}")).await,
            forget(&db, id).await,
            crate::tools::engineering::task_delete(&db, id).await,
            crate::tools::engineering::plan::task_done(&db, id).await,
            crate::tools::engineering::plan::plan_delete(&db, id, false).await,
            crate::knowledge::tools::knowledge_unlink_node_at(&format!("memory.semantic:{id}"), &db, false, crate::knowledge::tools::KG_DUMPS_DIR).await,
            crate::knowledge::tools::knowledge_update(&format!("memory.semantic:{id} | {{\"content\": \"x\"}}"), &db, &config()).await,
            crate::knowledge::tools::knowledge_link(&format!("memory.semantic:{id} | related_to | memory.semantic:other | 0.5"), &db).await,
        ];
        for out in &outs {
            assert!(out.contains("is not a valid name"), "{out}");
            assert!(!out.contains("not found"), "a refused id is not 'not found': {out}");
        }
        assert!(server.received_requests().await.unwrap_or_default().is_empty(), "nothing was sent");
    }
}

#[cfg(test)]
mod draft_read_failure_tests {
    //! A drafts read that fails is reported, and no draft is written over it.
    use super::draft;
    use crate::db::WardsonDbClient;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn a_failed_drafts_read_is_reported_and_writes_no_duplicate() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/drafts")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("POST")).and(path("/drafts/query")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
        Mock::given(method("GET")).and(path("/drafts/docs")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let out = draft(&db, "Notes | first line", "s1").await;
        assert!(out.starts_with("draft failed (could not check for an existing draft"), "{out}");
        let writes = server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/drafts/docs")
            .count();
        assert_eq!(writes, 0, "nothing was written");
        let out = draft(&db, "delete Notes", "s1").await;
        assert!(out.starts_with("draft failed (could not read the drafts"), "{out}");
    }
}

#[cfg(test)]
mod get_tests {
    //! `get` on the configuration document: the credential fields come
    //! back redacted, everything else as stored.
    use super::get;
    use crate::db::WardsonDbClient;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn get_redacts_the_configuration_secrets() {
        let server = MockServer::start().await;
        let db = WardsonDbClient::from_url(&server.uri());
        let doc = serde_json::json!({
            "_id": "config", "name": "Embra", "timezone": "UTC", "api_provider": "anthropic",
            "api_key": "sk-ant-api03-0123456789",
            "anthropic_api_key": "sk-ant-second-0123456789",
            "gemini_api_key": "AIzaSy-0123456789",
            "github_token": "ghp_0123456789abcdefghij",
            "git_tokens": {"gitlab.ops.wsds": "glpat-0123456789"},
            "guardian_secrets": {"graph.microsoft.com": {"Authorization": "Bearer eyJ0eXAi.body"}},
        });
        Mock::given(method("GET"))
            .and(path("/config.system/docs/config"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "data": doc, "meta": {}})),
            )
            .mount(&server)
            .await;

        let out = get(&db, "config.system config").await;
        for secret in ["sk-ant-", "AIzaSy", "ghp_", "glpat-", "eyJ0eXAi"] {
            assert!(!out.contains(secret), "{secret} leaked: {out}");
        }
        assert_eq!(out.matches("[redacted]").count(), 6, "{out}");
        assert!(out.contains("\"name\": \"Embra\"") && out.contains("gitlab.ops.wsds") && out.contains("Authorization"), "{out}");
    }
}

#[cfg(test)]
mod subprocess_tests {
    use super::subprocess;

    /// The child goes with the future: a `sleep` started through the
    /// builder is dead or a zombie once its handle is dropped, never still
    /// sleeping (Linux reads the state from /proc; tokio reaps the zombie
    /// in the background).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_subprocess_is_killed_when_its_handle_is_dropped() {
        let child = subprocess("sleep").arg("30").spawn().expect("sleep spawns");
        let pid = child.id().expect("a pid before the drop");
        drop(child);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => {}
            Ok(stat) => {
                // `pid (comm) STATE …`: the state follows the closing parenthesis.
                let state = stat.rsplit(')').next().unwrap_or("").split_whitespace().next().unwrap_or("");
                assert!(matches!(state, "Z" | "X"), "sleep {pid} is still alive in state {state:?}");
            }
        }
    }
}
