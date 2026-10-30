//! The memory tools: `recall` (and its aliases), `remember` and `forget`,
//! with the write path they share (`save_memory`, `remember_record`).

use chrono::Utc;
use serde::Deserialize;
use tracing::info;
use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;

use super::ensure_collection;
use crate::config::SystemConfig;
use crate::db::{WardsonDbClient, MEMORY_FETCH_WINDOW};
use crate::knowledge;
use crate::tools::registry::DispatchContext;
#[cfg(test)]
use super::registry;

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
    // A read that fails is reported as itself, never as "no memories" (#60).
    // The three windows are independent reads of one pooled client, so they
    // go out at once (#39): the wait is the slowest of three, not their sum.
    // Unpromoted-worklist mode only concerns memory.entries — semantic and
    // procedural docs are promoted by definition.
    let (entries, semantic, procedural) = if unpromoted_only {
        let entries = match db.fetch_recent("memory.entries", MEMORY_FETCH_WINDOW).await {
            Ok(docs) => docs,
            Err(e) => return format!("Failed to load memories from the database: {e}"),
        };
        (entries, Vec::new(), Vec::new())
    } else {
        let (entries, semantic, procedural) = tokio::join!(
            db.fetch_recent("memory.entries", MEMORY_FETCH_WINDOW),
            db.fetch_recent("memory.semantic", MEMORY_FETCH_WINDOW),
            db.fetch_recent("memory.procedural", MEMORY_FETCH_WINDOW),
        );
        let entries = match entries {
            Ok(docs) => docs,
            Err(e) => return format!("Failed to load memories from the database: {e}"),
        };
        let semantic = match semantic {
            Ok(docs) => docs,
            Err(e) => return format!("Failed to load semantic memories from the database: {e}"),
        };
        let procedural = match procedural {
            Ok(docs) => docs,
            Err(e) => return format!("Failed to load procedural memories from the database: {e}"),
        };
        (entries, semantic, procedural)
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

    // The node lines mark a superseded node; the worklist lists no nodes.
    let superseders = if unpromoted_only {
        std::collections::HashMap::new()
    } else {
        crate::knowledge::supersession::superseders(db).await
    };
    let mut output_lines =
        recall_lines(&entries, &semantic, &procedural, &query_tokens, unpromoted_only, &superseders);

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
    superseders: &std::collections::HashMap<
        crate::knowledge::supersession::NodeKey,
        crate::knowledge::supersession::NodeKey,
    >,
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
            let mark = crate::knowledge::supersession::superseded_note(
                crate::knowledge::supersession::successor_of(superseders, collection, id),
            );
            output_lines.push(format!("  [{}] [{}] {} (tags: {}) — {}{}", collection, id, display, tags, ts, mark));
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
mod read_failure_tests {
    //! A failed database read is reported as itself, never as "no memories".
    use super::recall;
    use crate::db::WardsonDbClient;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn a_failed_memory_read_is_reported_and_not_an_empty_recall() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/memory.entries")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("POST")).and(path("/memory.entries/query")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let out = recall(&db, "anything", false).await;
        assert!(out.starts_with("Failed to load memories from the database"), "{out}");
        assert!(!out.contains("No memory entries found"), "{out}");
    }
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
        let lines = recall_lines(&[promoted_entry()], &[node], &[], &tokens("refresh"), false, &std::collections::HashMap::new());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("[memory.semantic] [n1]"), "{lines:?}");
    }

    #[test]
    fn a_superseded_node_line_names_its_successor() {
        let node = json!({
            "_id": "n1", "content": "the cert refresh works after manual generation", "category": "fact",
            "tags": ["certs"], "created_at": "2026-10-01T00:00:01Z",
        });
        let mut superseders = std::collections::HashMap::new();
        superseders.insert(
            ("memory.semantic".to_string(), "n1".to_string()),
            ("memory.semantic".to_string(), "n9".to_string()),
        );
        let lines = recall_lines(&[], &[node], &[], &tokens("refresh"), false, &superseders);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].ends_with("2026-10-01T00:00:01Z — superseded by memory.semantic:n9"), "{lines:?}");
    }

    #[test]
    fn an_entry_whose_node_did_not_match_keeps_its_marker() {
        // The node's text was changed since the promotion; the query only
        // finds the entry's original wording.
        let node = json!({
            "_id": "n1", "content": "certificates rotate by hand", "category": "fact",
            "tags": ["certs"], "created_at": "2026-10-01T00:00:01Z",
        });
        let lines = recall_lines(&[promoted_entry()], &[node], &[], &tokens("refresh"), false, &std::collections::HashMap::new());
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
        let lines = recall_lines(&[entry], &[], &[procedure], &tokens("rotate"), false, &std::collections::HashMap::new());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("[memory.procedural] [p1] Rotate the cert: "), "{lines:?}");
    }

    #[test]
    fn the_worklist_lists_unpromoted_entries_only() {
        let unpromoted = json!({
            "_id": "e3", "content": "refresh the docs", "tags": [], "promoted_to": null,
            "created_at": "2026-10-01T00:00:00Z",
        });
        let lines = recall_lines(&[promoted_entry(), unpromoted], &[], &[], &tokens("refresh"), true, &std::collections::HashMap::new());
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

/// The entry `knowledge_sweep_orphans` writes back for a promotion node
/// whose entry is gone (adopt): `entry_doc`'s five fields with the pointer
/// set at the node, under the id the node names as its source when
/// `entry_id` is given, so the node's provenance and its `derived_from`
/// edge resolve again.
pub(crate) fn adopted_entry_doc(
    entry_id: Option<&str>,
    text: &str,
    tags: &[String],
    session: &str,
    created_at: &str,
    node: (&str, &str),
) -> serde_json::Value {
    let mut doc = entry_doc(text, tags, session, created_at);
    doc["promoted_to"] = serde_json::json!({ "collection": node.0, "id": node.1 });
    if let Some(id) = entry_id {
        doc["_id"] = serde_json::Value::String(id.to_string());
    }
    doc
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

    match save_memory(db, &text, tags, &promotion, session, config).await {
        Err(e) => format!("Failed to save memory: {}", e),
        Ok(Saved { entry_id, node: Ok(node) }) => {
            let candidates = knowledge::neighbors::candidates_block(
                &knowledge::neighbors::link_candidates(db, config, node.collection, &node.id).await,
            );
            remember_reply(&entry_id, &node, &candidates)
        }
        Ok(Saved { entry_id, node: Err(e) }) => remember_unpromoted_reply(&entry_id, &e),
    }
}

/// What a memory the brain writes itself came to (`remember_record`).
#[derive(Debug, PartialEq)]
pub(crate) enum Recorded {
    /// The entry and its node.
    Node { collection: &'static str, id: String },
    /// The entry without its node: the node could not be written.
    EntryOnly { entry_id: String, error: String },
    /// Nothing was saved.
    Failed(String),
}

/// A memory the brain writes on its own behalf, the record of a deleted
/// session: `remember`'s write path, a semantic node of `category`,
/// attributed to `session`. No model reads the result, so no link
/// candidates are read.
pub(crate) async fn remember_record(
    db: &WardsonDbClient,
    content: &str,
    category: knowledge::types::SemanticCategory,
    session: &str,
    config: &SystemConfig,
) -> Recorded {
    let (text, tags) = split_tags(content);
    if text.is_empty() {
        return Recorded::Failed("no content besides the tags".into());
    }
    match save_memory(db, &text, tags, &Promotion::Semantic(category), session, config).await {
        Err(e) => Recorded::Failed(e),
        Ok(Saved { node: Ok(node), .. }) => Recorded::Node { collection: node.collection, id: node.id },
        Ok(Saved { entry_id, node: Err(error) }) => Recorded::EntryOnly { entry_id, error },
    }
}

/// `text` with every word that `split_tags` would lift into the tags
/// written without its `#`, and its whitespace folded to single spaces: an
/// operator's words quoted into a memory stay text.
pub(crate) fn untag(text: &str) -> String {
    text.split_whitespace()
        .map(|w| if is_tag_token(w) { w.trim_start_matches('#') } else { w })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What `save_memory` wrote: the entry, and its node or why the node could
/// not be written.
struct Saved {
    entry_id: String,
    node: Result<knowledge::promotion::NewNode, String>,
}

/// The writes behind a memory: the episodic entry, the node it is promoted
/// to, and one background task that derives the automatic edges of both.
/// `Err` when the entry itself could not be written: nothing was saved.
async fn save_memory(
    db: &WardsonDbClient,
    text: &str,
    tags: Vec<String>,
    promotion: &Promotion,
    session: &str,
    config: &SystemConfig,
) -> Result<Saved, String> {
    ensure_collection(db, "memory.entries").await;

    let created_at = Utc::now().to_rfc3339();
    let entry_id = db
        .write("memory.entries", &entry_doc(text, &tags, session, &created_at))
        .await
        .map_err(|e| e.to_string())?;

    let node = match promotion {
        Promotion::Semantic(category) => {
            knowledge::promotion::write_semantic_node(db, &entry_id, category, config).await
        }
        Promotion::Procedural(procedure) => {
            knowledge::promotion::write_procedural_node(db, &entry_id, procedure, config).await
        }
    }
    .map_err(|e| e.to_string());

    // Background edge derivation (spec §4.8), for the entry and its node in
    // one task. Both gather their candidates at once, as the entry's always
    // did; the entry's leaves its own node out, and the node's plans the
    // pair (`derive_edges_except`).
    let db = db.clone();
    let config = config.clone();
    let session = session.to_string();
    let task_entry_id = entry_id.clone();
    let task_node = node.as_ref().ok().cloned();
    tokio::spawn(async move {
        let entry = ("memory.entries", task_entry_id.as_str());
        match &task_node {
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

    Ok(Saved { entry_id, node })
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

    /// The sweep's adopted entry is an entry: the same five fields, the
    /// pointer set, and the node's old entry id when it is free.
    #[test]
    fn the_adopted_entry_is_an_entry_with_its_pointer_set() {
        let doc = adopted_entry_doc(Some("e7"), "the text", &["t".to_string()], "ops", "2026-05-01T00:00:00Z", ("memory.semantic", "n7"));
        assert_eq!(
            doc,
            json!({
                "_id": "e7", "content": "the text", "tags": ["t"], "session": "ops",
                "promoted_to": {"collection": "memory.semantic", "id": "n7"}, "created_at": "2026-05-01T00:00:00Z",
            })
        );
        let fresh = adopted_entry_doc(None, "the text", &[], "ops", "2026-05-01T00:00:00Z", ("memory.semantic", "n7"));
        assert!(fresh.get("_id").is_none(), "no id: the server mints one");
        assert_eq!(fresh.as_object().unwrap().len(), 5);
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

    /// The memorize turn of a guided delete saves what the deleted session
    /// taught: the entry names that session, not the one the flow runs in.
    /// The node's `source_session` is read back from the entry
    /// (`promotion.rs`), so the entry write is what this test asserts.
    #[tokio::test]
    async fn remember_attributes_a_memory_to_the_session_being_deleted() {
        for (deleting, expected) in [(Some("old-proj"), "old-proj"), (None, "ops")] {
            let server = server("memory.semantic", data(json!({"_id": "n1"}))).await;
            let db = WardsonDbClient::from_url(&server.uri());
            let config = test_config();
            let trace = embra_tools_core::new_turn_trace_handle();
            let ctx = registry::DispatchContext {
                db: &db,
                config: &config,
                session_name: "ops",
                config_tz: "UTC",
                trace: &trace,
                turn_index: 0,
                deleting_session: deleting,
            };
            let out = registry::dispatch("remember", json!({"content": "the cert refresh works #certs"}), ctx).await;
            assert!(out.is_ok(), "{out:?}");
            let writes = writes_seen(&server).await;
            assert_eq!(writes[0].0, "POST /memory.entries/docs");
            assert_eq!(writes[0].1["session"], expected, "deleting_session = {deleting:?}");
        }
    }

    /// The brain's own memory, the record of a deleted session, tells the
    /// three outcomes apart: the operator's note words each differently.
    #[tokio::test]
    async fn remember_record_reports_the_node_the_entry_or_nothing() {
        let config = test_config();

        let written = server("memory.semantic", data(json!({"_id": "n1"}))).await;
        let db = WardsonDbClient::from_url(&written.uri());
        let recorded =
            remember_record(&db, "the record #session-deleted", SemanticCategory::Observation, "old-proj", &config).await;
        assert_eq!(recorded, Recorded::Node { collection: "memory.semantic", id: "n1".into() });
        let writes = writes_seen(&written).await;
        assert_eq!((&writes[0].1["session"], &writes[0].1["tags"]), (&json!("old-proj"), &json!(["session-deleted"])));
        assert_eq!(writes[1].1["category"], "observation");

        let no_node = server("memory.semantic", ResponseTemplate::new(500)).await;
        let db = WardsonDbClient::from_url(&no_node.uri());
        let recorded = remember_record(&db, "the record", SemanticCategory::Observation, "old-proj", &config).await;
        assert!(matches!(&recorded, Recorded::EntryOnly { entry_id, .. } if entry_id == "e1"), "{recorded:?}");

        let down = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/memory.entries/docs"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&down)
            .await;
        let db = WardsonDbClient::from_url(&down.uri());
        let recorded = remember_record(&db, "the record", SemanticCategory::Observation, "old-proj", &config).await;
        assert!(matches!(recorded, Recorded::Failed(_)), "{recorded:?}");
    }

    #[test]
    fn a_word_that_reads_as_a_tag_loses_its_mark() {
        assert_eq!(untag("done #cleanup, see #5\nand ##x"), "done cleanup, see #5 and ##x");
        let (text, tags) = split_tags(&untag("drop #scratch now"));
        assert_eq!((text.as_str(), tags.len()), ("drop scratch now", 0));
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
pub(super) fn promoted_pointer(entry: &serde_json::Value) -> Option<(&'static str, String)> {
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
/// One delete per edge, by id: what the record holds, and nothing it does
/// not. The count is of the deletes that succeeded.
async fn delete_edges(db: &WardsonDbClient, edges: &[serde_json::Value]) -> u64 {
    let mut deleted = 0;
    for edge in edges {
        let Some(edge_id) = edge.get("_id").and_then(|v| v.as_str()) else { continue };
        if db.delete("memory.edges", edge_id).await.is_ok() {
            deleted += 1;
        }
    }
    deleted
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
pub(super) async fn forget(db: &WardsonDbClient, id: &str) -> String {
    forget_at(db, id, crate::knowledge::tools::KG_DUMPS_DIR).await
}

/// `forget` with the directory of its removal record as a parameter, so a
/// test runs it under a temporary one.
pub(crate) async fn forget_at(db: &WardsonDbClient, id: &str, dumps_dir: &str) -> String {
    if id.is_empty() {
        return "Provide the entry ID to forget: forget <id>".into();
    }
    let id = id.trim();

    match db.read("memory.entries", id).await {
        Ok(entry) => forget_entry(db, id, &entry, dumps_dir).await,
        Err(e) if crate::db::error::is_not_found(&e) => forget_by_node(db, id, dumps_dir).await,
        Err(e) => format!("Failed to remove entry: {}", e),
    }
}

/// `id` is no entry. When it is a node with one entry behind it, that
/// memory is forgotten; otherwise the answer says what the id is.
async fn forget_by_node(db: &WardsonDbClient, id: &str, dumps_dir: &str) -> String {
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
            [entry_id] => forget_entry(db, entry_id, &entries[0], dumps_dir).await,
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

async fn forget_entry(db: &WardsonDbClient, id: &str, entry: &serde_json::Value, dumps_dir: &str) -> String {
    use crate::knowledge::removal::{self, RemovalSummary};
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
    let target = format!("memory.entries:{id}");

    if let (NodeFate::Remove, Some((collection, node_id)), Some(doc)) = (&fate, &pointer, &node) {
        // The edges of the node and of the entry, whole, then the record;
        // then edges first, the node next, the entry last: whatever fails,
        // the entry is still there for a second `forget` to finish from.
        let edges = match removal::edges_of_all(db, &[(collection, node_id.as_str()), ("memory.entries", id)]).await {
            Ok(edges) => edges,
            Err(e) => {
                return format!(
                    "Error: the edges of entry {} and its node {}:{} could not be read ({}); nothing was removed. Run forget again.",
                    id, collection, node_id, e
                )
            }
        };
        let summary = RemovalSummary::of(&edges).with_nodes(vec![format!("{collection}:{node_id}")]);
        let source = serde_json::json!({ "tool": "forget", "target": target, "node": format!("{collection}:{node_id}") });
        let record = match removal::write_removal_record(
            dumps_dir,
            "forget",
            source,
            &[(collection, doc), ("memory.entries", entry)],
            &edges,
        )
        .await
        {
            Ok(path) => path,
            Err(e) => return format!("Error: the removal record could not be written ({}); nothing was removed. Run forget again.", e),
        };
        let edge_count = delete_edges(db, &edges).await;
        if let Err(e) = db.delete(collection, node_id).await
            && !crate::db::error::is_not_found(&e)
        {
            return format!(
                "Error: the node {}:{} could not be removed ({}); entry {} was left in place. Run forget again. The record is {}.",
                collection,
                node_id,
                e,
                id,
                record.display()
            );
        }
        // The node is gone — drop its vector, as `knowledge_unlink_node` does.
        crate::embedding::write::forget_node(collection, node_id).await;
        if let Err(e) = db.delete("memory.entries", id).await {
            return format!(
                "Error: the node {}:{} was removed, and entry {} could not be ({}). Run forget again. The record is {}.",
                collection,
                node_id,
                id,
                e,
                record.display()
            );
        }
        removal::log_removal("forget", &target, &summary, &record);
        let preview_src = doc
            .get("content")
            .or_else(|| doc.get("title"))
            .and_then(|v| v.as_str())
            .unwrap_or("(no preview)");
        return format!(
            "Memory removed: entry {} and its node {}:{} (\"{}\"). {} edge(s) cascaded.{}",
            id,
            collection,
            node_id,
            knowledge::types::content_preview(preview_src, 80),
            edge_count,
            removal::removal_report(&summary, Some(&record), false)
        );
    }

    // The entry alone: it has no live node, or the node stays. Its edges
    // first, then the entry, with the record in front of both.
    let edges = match removal::node_edges(db, "memory.entries", id).await {
        Ok(edges) => edges,
        Err(e) => {
            return format!(
                "Error: the edges of entry {} could not be read ({}); nothing was removed. Run forget again.",
                id, e
            )
        }
    };
    let summary = RemovalSummary::of(&edges);
    let source = serde_json::json!({ "tool": "forget", "target": target, "node": serde_json::Value::Null });
    let record = match removal::write_removal_record(dumps_dir, "forget", source, &[("memory.entries", entry)], &edges).await {
        Ok(path) => path,
        Err(e) => return format!("Error: the removal record could not be written ({}); nothing was removed. Run forget again.", e),
    };
    let edge_count = delete_edges(db, &edges).await;
    if let Err(e) = db.delete("memory.entries", id).await {
        return format!("Failed to remove entry: {}. The record is {}.", e, record.display());
    }
    removal::log_removal("forget", &target, &summary, &record);
    let note = match &pointer {
        Some((collection, node_id)) => kept_node_note(collection, node_id, &fate),
        None => String::new(),
    };
    format!(
        "Memory entry {} removed; {} referencing edge(s) cascaded.{}{}",
        id,
        edge_count,
        note,
        removal::removal_report(&summary, Some(&record), false)
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

    use crate::knowledge::removal::tests::{record_lines, TempDir};
    use wiremock::Request;

    fn body(req: &Request) -> serde_json::Value {
        serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null)
    }

    fn edge(id: &str, kind: &str, src: (&str, &str), tgt: (&str, &str), weight: f64) -> serde_json::Value {
        json!({
            "_id": id, "edge_type": kind, "weight": weight,
            "source_collection": src.0, "source_id": src.1, "target_collection": tgt.0, "target_id": tgt.1,
        })
    }

    /// The edges that name `id` on one side.
    async fn edge_arm(server: &MockServer, field: &'static str, id: &'static str, edges: Vec<serde_json::Value>) {
        Mock::given(method("POST"))
            .and(path("/memory.edges/query"))
            .and(move |req: &Request| body(req)["filter"][field] == id)
            .respond_with(doc(json!(edges)))
            .mount(server)
            .await;
    }

    /// One promoted memory: entry `e1`, node `n1`. `node_read` answers the
    /// node's read; `entries` is what the pointer scan returns. Four edges
    /// name the pair: the provenance edge `d1` (read from both sides), the
    /// automatic `x1` and `y1`, and `x2`, a link the brain authored.
    async fn one_memory(node_read: ResponseTemplate, entries: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/memory.entries/docs/e1")).respond_with(doc(entry_e1())).mount(&server).await;
        Mock::given(method("GET")).and(path("/memory.entries/docs/n1")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
        Mock::given(method("GET")).and(path("/memory.semantic/docs/n1")).respond_with(node_read).mount(&server).await;
        Mock::given(method("POST")).and(path("/memory.entries/query")).respond_with(doc(entries)).mount(&server).await;
        let d1 = edge("d1", "derived_from", ("memory.semantic", "n1"), ("memory.entries", "e1"), 1.0);
        let x1 = edge("x1", "same_session", ("memory.semantic", "n1"), ("memory.semantic", "o1"), 0.5);
        let x2 = edge("x2", "related_to", ("memory.semantic", "o2"), ("memory.semantic", "n1"), 0.8);
        let y1 = edge("y1", "same_session", ("memory.entries", "e1"), ("memory.semantic", "o3"), 0.5);
        edge_arm(&server, "source_id", "n1", vec![d1.clone(), x1]).await;
        edge_arm(&server, "target_id", "n1", vec![x2]).await;
        edge_arm(&server, "source_id", "e1", vec![y1]).await;
        edge_arm(&server, "target_id", "e1", vec![d1]).await;
        Mock::given(method("DELETE")).respond_with(doc(json!({}))).mount(&server).await;
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
            .filter(|r| r.method.as_str() == "DELETE")
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect()
    }

    /// The record first (the entry, the node and every edge of either, in
    /// the dump format), then the edges by id, the node, the entry; the
    /// reply splits the edges and names the link the brain authored.
    #[tokio::test]
    async fn forget_removes_the_edges_the_node_and_then_the_entry() {
        let server = one_memory(live_node(), json!([entry_e1()])).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let dir = TempDir::new("forget");
        let out = forget_at(&db, "e1", dir.path()).await;
        assert!(
            out.starts_with("Memory removed: entry e1 and its node memory.semantic:n1 (\"the cert refresh works\"). 4 edge(s) cascaded.\n  edges removed: 4 (auto 2, curated 1, other 1)\n  auto: same_session=2\n  curated: related_to=1\n    x2 memory.semantic:o2 → memory.semantic:n1 related_to 0.80\n  other: derived_from=1\n  nodes removed: memory.semantic:n1\n  record: "),
            "{out}"
        );
        assert_eq!(
            writes_seen(&server).await,
            [
                "DELETE /memory.edges/docs/d1",
                "DELETE /memory.edges/docs/x1",
                "DELETE /memory.edges/docs/x2",
                "DELETE /memory.edges/docs/y1",
                "DELETE /memory.semantic/docs/n1",
                "DELETE /memory.entries/docs/e1",
            ]
        );
        let record = std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().path()).next().expect("one record");
        assert!(out.ends_with(&format!("\n  record: {}", record.display())), "{out}");
        let lines = record_lines(&record);
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[0]["source"], json!({"tool": "forget", "target": "memory.entries:e1", "node": "memory.semantic:n1"}));
        assert_eq!(lines[0]["collections"], json!(["memory.semantic", "memory.entries", "memory.edges"]));
        assert_eq!((lines[1]["collection"].as_str(), lines[1]["data"]["content"].as_str()), (Some("memory.semantic"), Some("the cert refresh works")));
        assert_eq!((lines[2]["collection"].as_str(), lines[2]["_id"].as_str()), (Some("memory.entries"), Some("e1")));
        let ids: Vec<&str> = lines[3..].iter().map(|l| l["_id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["d1", "x1", "x2", "y1"], "every edge once, the one read from both sides included once");
    }

    #[tokio::test]
    async fn a_node_id_with_one_entry_is_forgotten_as_that_entry() {
        let server = one_memory(live_node(), json!([entry_e1()])).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let dir = TempDir::new("forget-node-id");
        let out = forget_at(&db, "n1", dir.path()).await;
        assert!(out.starts_with("Memory removed: entry e1 and its node memory.semantic:n1"), "{out}");
        assert_eq!(writes_seen(&server).await.len(), 6);
    }

    /// The entry alone: its own edges by id, then the entry; the node stays
    /// and the reply says why. The record holds the entry and its edges.
    #[tokio::test]
    async fn a_shared_node_outlives_the_entry() {
        let other = json!({"_id": "e2", "promoted_to": {"collection": "memory.semantic", "id": "n1"}});
        let server = one_memory(live_node(), json!([other, entry_e1()])).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let dir = TempDir::new("forget-shared");
        let out = forget_at(&db, "e1", dir.path()).await;
        assert!(out.starts_with("Memory entry e1 removed; 2 referencing edge(s) cascaded."), "{out}");
        assert!(out.contains("memory.semantic:n1 stays: 1 other entry point at it (e2)"), "{out}");
        assert!(out.contains("\n  edges removed: 2 (auto 1, curated 0, other 1)\n  auto: same_session=1\n  other: derived_from=1\n  record: "), "{out}");
        assert_eq!(
            writes_seen(&server).await,
            ["DELETE /memory.edges/docs/y1", "DELETE /memory.edges/docs/d1", "DELETE /memory.entries/docs/e1"]
        );
        let record = std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().path()).next().expect("one record");
        let lines = record_lines(&record);
        assert_eq!(lines[0]["source"], json!({"tool": "forget", "target": "memory.entries:e1", "node": null}));
        assert_eq!(lines.len(), 4, "the meta line, the entry, two edges");
    }

    /// The record gates every delete: when it cannot be written, nothing
    /// goes, and the reply says to run again.
    #[tokio::test]
    async fn a_record_that_cannot_be_written_removes_nothing() {
        let server = one_memory(live_node(), json!([entry_e1()])).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let dir = TempDir::new("forget-blocked");
        let blocked = dir.0.join("not-a-directory");
        std::fs::write(&blocked, b"x").unwrap();
        let out = forget_at(&db, "e1", blocked.to_str().unwrap()).await;
        assert!(out.starts_with("Error: the removal record could not be written ("), "{out}");
        assert!(out.ends_with("nothing was removed. Run forget again."), "{out}");
        assert!(writes_seen(&server).await.is_empty());
    }

    /// An edge read that fails is no shorter list: nothing is removed.
    #[tokio::test]
    async fn edges_that_cannot_be_read_stop_forget_with_nothing_removed() {
        let server = one_memory(live_node(), json!([entry_e1()])).await;
        Mock::given(method("POST"))
            .and(path("/memory.edges/query"))
            .respond_with(ResponseTemplate::new(500).set_body_string("timeout"))
            .with_priority(1)
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());
        let dir = TempDir::new("forget-unread");
        let out = forget_at(&db, "e1", dir.path()).await;
        assert!(out.starts_with("Error: the edges of entry e1 and its node memory.semantic:n1 could not be read ("), "{out}");
        assert!(writes_seen(&server).await.is_empty());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0, "no record of a removal that did not happen");
    }

    #[tokio::test]
    async fn a_node_that_cannot_be_read_stops_forget_with_nothing_removed() {
        let server = one_memory(ResponseTemplate::new(500), json!([entry_e1()])).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let dir = TempDir::new("forget-node-unread");
        let out = forget_at(&db, "e1", dir.path()).await;
        assert!(out.starts_with("Error: the node memory.semantic:n1 could not be read"), "{out}");
        assert!(out.contains("nothing was removed"), "{out}");
        assert!(writes_seen(&server).await.is_empty());
    }

    #[tokio::test]
    async fn an_entry_whose_node_is_gone_is_removed_alone() {
        let server = one_memory(ResponseTemplate::new(404), json!([])).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let dir = TempDir::new("forget-alone");
        let out = forget_at(&db, "e1", dir.path()).await;
        assert!(out.starts_with("Memory entry e1 removed; 2 referencing edge(s) cascaded.\n  edges removed: 2 (auto 1, curated 0, other 1)\n"), "{out}");
        assert_eq!(writes_seen(&server).await, ["DELETE /memory.edges/docs/y1", "DELETE /memory.edges/docs/d1", "DELETE /memory.entries/docs/e1"]);
    }

    #[tokio::test]
    async fn an_id_that_is_nothing_says_so() {
        let server = MockServer::start().await;
        let db = WardsonDbClient::from_url(&server.uri());
        assert_eq!(forget(&db, "zzz").await, "No memory entry or node with id zzz.");
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
        // A memory comes from the session being deleted in the memorize
        // turn of a guided delete, and from the active session otherwise.
        let session = ctx.deleting_session.unwrap_or(ctx.session_name);
        Ok(remember(ctx.db, &self.content, promotion, session, ctx.config).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "forget",
    is_side_effectful = true,
    description = "Remove a memory: its entry, the node promoted from it, and every edge that touches either. id is the entry id or the node id. The node stays when another entry also points at it or when it comes from a seed pack; the result says so. Before anything is deleted the entry, the node and their edges are written to a record under /embra/workspace/KG_DUMPS (a dump file), and the reply lists the removed edges by type with every brain-authored link named. To remove a node and keep its entry, use knowledge_unlink_node. Destructive; confirm with the user first."
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

    #[test]
    fn recall_schema_has_optional_query() {
        let schema = schemars::schema_for!(RecallArgs);
        let v = serde_json::to_value(&schema).unwrap();
        assert_eq!(v["properties"]["query"]["type"], "string");
        assert_eq!(v["properties"]["unpromoted_only"]["type"], "boolean");
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
