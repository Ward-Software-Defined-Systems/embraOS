//! Promotion: episodic entry → semantic/procedural node.
//!
//! Creates a provenance chain:
//! - New semantic/procedural node carries `source_entry_id` and `source_session`.
//! - Source `memory.entries` doc gets `promoted_to: {collection, id}` PATCHed in.
//! - A directed `derived_from` edge (new_node → source_entry) is inserted.
//! - Auto edges are derived for the new node (same_session, temporal, tag_overlap).
//!
//! `write_semantic_node` and `write_procedural_node` write the first three
//! and leave the automatic edges to the caller; `promote_to_semantic` and
//! `promote_to_procedural` derive them inline.

use anyhow::{anyhow, Result};
use chrono::Utc;
use serde_json::json;

use crate::config::SystemConfig;
use crate::db::error::is_not_found;
use crate::db::WardsonDbClient;

use super::edges::derive_edges;
use super::types::{EdgeType, SemanticCategory};

const PROCEDURAL_SCHEMA_HINT: &str = r#"{"title": "...", "description": "...", "preconditions": [...], "steps": [{"order": N, "action": "...", "notes": "..."}], "outcomes": {"success": "...", "failure": "..."}}"#;

/// What a promotion wrote: enough to name the node and to derive its
/// automatic edges.
#[derive(Debug, Clone)]
pub(crate) struct NewNode {
    pub collection: &'static str,
    pub id: String,
    /// The category of a semantic node, the title of a procedure.
    pub label: String,
    pub session: String,
    pub tags: Vec<String>,
    pub created_at: String,
}

/// What a node takes over from its entry.
struct SourceEntry {
    content: String,
    tags: Vec<String>,
    session: String,
}

/// A node about to be written: its document, the metadata of its
/// `derived_from` edge, and the one timestamp both carry.
struct Draft {
    collection: &'static str,
    label: String,
    doc: serde_json::Value,
    provenance: serde_json::Value,
    now: String,
}

/// A procedure as the model writes it: the JSON object of
/// `PROCEDURAL_SCHEMA_HINT`, checked.
#[derive(Debug)]
pub(crate) struct Procedure {
    pub title: String,
    pub description: String,
    pub preconditions: Vec<String>,
    pub steps: serde_json::Value,
    pub success: String,
    pub failure: String,
}

/// Check a procedure before anything is written. Every refusal names the
/// field and repeats the expected shape.
pub(crate) fn parse_procedure(procedure_json: &str) -> Result<Procedure> {
    let parsed: serde_json::Value = serde_json::from_str(procedure_json)
        .map_err(|e| anyhow!("Invalid procedural data: {}\nExpected schema: {}", e, PROCEDURAL_SCHEMA_HINT))?;
    let missing = |field: &str| {
        anyhow!("Invalid procedural data: missing field '{}'\nExpected schema: {}", field, PROCEDURAL_SCHEMA_HINT)
    };

    let title = parsed.get("title").and_then(|v| v.as_str()).ok_or_else(|| missing("title"))?.to_string();
    let description = parsed
        .get("description")
        .and_then(|v| v.as_str())
        .ok_or_else(|| missing("description"))?
        .to_string();
    let preconditions: Vec<String> = super::text::string_array(&parsed, "preconditions");
    let steps = parsed.get("steps").ok_or_else(|| missing("steps"))?.clone();
    let outcomes = parsed.get("outcomes").ok_or_else(|| missing("outcomes"))?;
    let success = outcomes
        .get("success")
        .and_then(|v| v.as_str())
        .ok_or_else(|| missing("outcomes.success"))?
        .to_string();
    let failure = outcomes
        .get("failure")
        .and_then(|v| v.as_str())
        .ok_or_else(|| missing("outcomes.failure"))?
        .to_string();

    Ok(Procedure { title, description, preconditions, steps, success, failure })
}

/// The semantic node of an entry: its content and tags verbatim.
fn semantic_doc(source: &SourceEntry, entry_id: &str, category: &SemanticCategory, now: &str) -> serde_json::Value {
    json!({
        "content": source.content,
        "category": category.as_str(),
        "tags": source.tags,
        "source_entry_id": entry_id,
        "source_session": source.session,
        "confidence": 0.9,
        "access_count": 0,
        "last_accessed": serde_json::Value::Null,
        "created_at": now,
        "updated_at": now,
    })
}

/// The procedural node of an entry. Its text is the procedure's; the entry
/// gives the tags, the session and the provenance.
fn procedural_doc(source: &SourceEntry, entry_id: &str, procedure: &Procedure, now: &str) -> serde_json::Value {
    json!({
        "title": procedure.title,
        "description": procedure.description,
        "preconditions": procedure.preconditions,
        "steps": procedure.steps,
        "outcomes": { "success": procedure.success, "failure": procedure.failure },
        "tags": source.tags,
        "source_entry_id": entry_id,
        "source_session": source.session,
        "access_count": 0,
        "last_accessed": serde_json::Value::Null,
        "created_at": now,
        "updated_at": now,
    })
}

/// The pointer an entry carries once it is promoted.
pub(crate) fn pointer_patch(collection: &str, id: &str) -> serde_json::Value {
    json!({ "promoted_to": { "collection": collection, "id": id } })
}

/// Write the semantic node of an entry: the node, the entry's pointer, the
/// vector and the `derived_from` edge. The automatic edges are the caller's.
pub(crate) async fn write_semantic_node(
    db: &WardsonDbClient,
    entry_id: &str,
    category: &SemanticCategory,
    config: &SystemConfig,
) -> Result<NewNode> {
    let source = unpromoted_source(db, entry_id).await?;
    let now = Utc::now().to_rfc3339();
    let draft = Draft {
        collection: "memory.semantic",
        label: category.as_str().to_string(),
        doc: semantic_doc(&source, entry_id, category, &now),
        provenance: json!({ "promotion_type": "semantic", "category": category.as_str() }),
        now,
    };
    write_node(db, entry_id, draft, source, config).await
}

/// Write the procedural node of an entry; see `write_semantic_node`.
pub(crate) async fn write_procedural_node(
    db: &WardsonDbClient,
    entry_id: &str,
    procedure: &Procedure,
    config: &SystemConfig,
) -> Result<NewNode> {
    let source = unpromoted_source(db, entry_id).await?;
    write_procedure(db, entry_id, procedure, source, config).await
}

async fn write_procedure(
    db: &WardsonDbClient,
    entry_id: &str,
    procedure: &Procedure,
    source: SourceEntry,
    config: &SystemConfig,
) -> Result<NewNode> {
    let now = Utc::now().to_rfc3339();
    let draft = Draft {
        collection: "memory.procedural",
        label: procedure.title.clone(),
        doc: procedural_doc(&source, entry_id, procedure, &now),
        provenance: json!({ "promotion_type": "procedural" }),
        now,
    };
    write_node(db, entry_id, draft, source, config).await
}

/// Promote an episodic entry to `memory.semantic`. Returns the new semantic node _id.
pub async fn promote_to_semantic(
    db: &WardsonDbClient,
    entry_id: &str,
    category: SemanticCategory,
    config: &SystemConfig,
) -> Result<String> {
    let node = write_semantic_node(db, entry_id, &category, config).await?;
    derive_node_edges(db, &node, config).await;
    Ok(node.id)
}

/// Promote an episodic entry to `memory.procedural`. Returns the new procedural node _id.
pub async fn promote_to_procedural(
    db: &WardsonDbClient,
    entry_id: &str,
    procedure_json: &str,
    config: &SystemConfig,
) -> Result<String> {
    // The entry is checked before the procedure: an id that names nothing
    // is the first thing to say.
    let source = unpromoted_source(db, entry_id).await?;
    let procedure = parse_procedure(procedure_json)?;
    let node = write_procedure(db, entry_id, &procedure, source, config).await?;
    derive_node_edges(db, &node, config).await;
    Ok(node.id)
}

/// Auto-derive edges for a node promotion just wrote.
async fn derive_node_edges(db: &WardsonDbClient, node: &NewNode, config: &SystemConfig) {
    let _ = derive_edges(
        db,
        &node.id,
        node.collection,
        &node.session,
        &node.tags,
        &node.created_at,
        config,
    ).await;
}

/// The node, then the entry's pointer, then the vector and the provenance
/// edge.
///
/// The pointer is what says "promoted" (`tools::entry_is_promoted`). A node
/// whose entry does not point at it reads as an unpromoted entry and would
/// be promoted a second time, so a pointer that cannot be written takes the
/// node back: it is deleted again, before it has a vector or an edge.
async fn write_node(
    db: &WardsonDbClient,
    entry_id: &str,
    draft: Draft,
    source: SourceEntry,
    config: &SystemConfig,
) -> Result<NewNode> {
    // The write sequence runs as a task of its own, awaited through its
    // handle. The dispatch future that called it is abandoned on an
    // operator stop (and by the dispatcher's timeout), and a future dropped
    // between the node write and the pointer patch left a node no entry
    // pointed at, which the entry's next promotion duplicated (#57). A
    // dropped handle cancels no task, so a promotion that has started runs
    // to its end: node, pointer, vector, edge.
    let db = db.clone();
    let entry_id = entry_id.to_string();
    let config = config.clone();
    tokio::spawn(async move { write_node_to_the_end(&db, &entry_id, draft, source, &config).await })
        .await
        .unwrap_or_else(|e| Err(anyhow!("the promotion task ended early: {e}")))
}

/// The writes of `write_node`, in order; see there for why they run as a
/// task.
async fn write_node_to_the_end(
    db: &WardsonDbClient,
    entry_id: &str,
    draft: Draft,
    source: SourceEntry,
    config: &SystemConfig,
) -> Result<NewNode> {
    let Draft { collection, label, doc, provenance, now } = draft;
    let new_id = db.write(collection, &doc).await?;

    if let Err(e) = db.patch_document("memory.entries", entry_id, &pointer_patch(collection, &new_id)).await {
        if let Err(undo) = db.delete(collection, &new_id).await {
            tracing::warn!(
                "promotion of {entry_id}: the pointer write failed and the node {collection}:{new_id} could not be removed again: {undo}"
            );
        }
        return Err(anyhow!("the promotion pointer could not be written on entry {}: {}", entry_id, e));
    }

    // Embed AFTER the document is durably written; a failure here leaves the
    // node unembedded and fully usable (KG-02 spec §6.1).
    crate::embedding::write::embed_node(db, config, collection, &new_id, &doc, true).await;

    // Directed derived_from edge.
    insert_derived_from_edge(db, &new_id, collection, entry_id, "memory.entries", provenance, &now).await;

    Ok(NewNode { collection, id: new_id, label, session: source.session, tags: source.tags, created_at: now })
}

/// Load a source entry that has no live node. Errors if it is not found or
/// already promoted.
///
/// A pointer at a node that is gone (404) is stale: it is cleared and the
/// entry promotes. Any other failure of that read says nothing about the
/// node — clearing the pointer on it would write a second node next to a
/// live one — so it is returned.
async fn unpromoted_source(db: &WardsonDbClient, entry_id: &str) -> Result<SourceEntry> {
    let doc = read_entry(db, entry_id).await?;

    if doc.get("promoted_to").is_some_and(|p| !p.is_null()) {
        if let Some((collection, id, _)) = live_node(db, entry_id, &doc).await? {
            return Err(anyhow!("Entry {} already promoted to {}:{}", entry_id, collection, id));
        }
        if let Err(e) = db.patch_document("memory.entries", entry_id, &json!({"promoted_to": null})).await {
            tracing::warn!("promotion of {entry_id}: the stale pointer could not be cleared: {e}");
        }
    }

    let content = doc.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let tags: Vec<String> = super::text::string_array(&doc, "tags");
    let session = doc.get("session").and_then(|v| v.as_str()).unwrap_or("").to_string();

    Ok(SourceEntry { content, tags, session })
}

async fn read_entry(db: &WardsonDbClient, entry_id: &str) -> Result<serde_json::Value> {
    db.read("memory.entries", entry_id).await.map_err(|e| {
        if is_not_found(&e) {
            anyhow!("Entry {} not found in memory.entries", entry_id)
        } else {
            anyhow!("Entry {} could not be read: {}", entry_id, e)
        }
    })
}

/// The node an entry's pointer names, when that node is there:
/// `(collection, id, document)`. `None` for a pointer that names nothing
/// readable as a node, and for a node that is gone (404). Any other failure
/// of the read is returned: it says nothing about the node.
async fn live_node(
    db: &WardsonDbClient,
    entry_id: &str,
    entry: &serde_json::Value,
) -> Result<Option<(String, String, serde_json::Value)>> {
    let Some(pointer) = entry.get("promoted_to").filter(|p| !p.is_null()) else {
        return Ok(None);
    };
    let (Some(collection), Some(id)) = (
        pointer.get("collection").and_then(|v| v.as_str()),
        pointer.get("id").and_then(|v| v.as_str()),
    ) else {
        return Ok(None);
    };
    match db.read(collection, id).await {
        Ok(node) => Ok(Some((collection.to_string(), id.to_string(), node))),
        Err(e) if is_not_found(&e) => Ok(None),
        Err(e) => Err(anyhow!(
            "Entry {} points at {}:{}, which could not be read ({}); nothing was promoted",
            entry_id, collection, id, e
        )),
    }
}

/// The node an entry already has, for the caller that decides what a second
/// promotion means. Reads only; a stale pointer is left for the promotion
/// to clear.
pub(crate) async fn node_of_entry(
    db: &WardsonDbClient,
    entry_id: &str,
) -> Result<Option<(String, String, serde_json::Value)>> {
    let entry = read_entry(db, entry_id).await?;
    live_node(db, entry_id, &entry).await
}

/// Where an entry stands, for the node that names it as its source.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EntryState {
    /// The entry exists and has no live node: a null pointer, or a pointer
    /// at a node that answers 404 (stale, the rule of `unpromoted_source`).
    Unpromoted,
    /// The entry exists and points at a live node.
    PointsAt { collection: String, id: String },
    /// No entry with that id.
    Missing,
}

/// The state of an entry. A 404 of the entry is `Missing`; a 404 of the node
/// its pointer names is `Unpromoted`; any other failure of either read is
/// returned, since it says nothing about the entry.
pub(crate) async fn entry_state(db: &WardsonDbClient, entry_id: &str) -> Result<EntryState> {
    let doc = match db.read("memory.entries", entry_id).await {
        Ok(doc) => doc,
        Err(e) if is_not_found(&e) => return Ok(EntryState::Missing),
        Err(e) => return Err(anyhow!("Entry {} could not be read: {}", entry_id, e)),
    };
    Ok(match live_node(db, entry_id, &doc).await? {
        Some((collection, id, _)) => EntryState::PointsAt { collection, id },
        None => EntryState::Unpromoted,
    })
}

/// What a node still lacks of its promotion: the vector, the `derived_from`
/// edge. Read by `unfinished`, written by `finish_promotion`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Unfinished {
    pub vector: bool,
    pub edge: bool,
}

impl Unfinished {
    /// `", vector, derived_from"`: what the writes add after the pointer,
    /// for a reply line.
    pub(crate) fn listed(&self) -> String {
        let mut out = String::new();
        if self.vector {
            out.push_str(", vector");
        }
        if self.edge {
            out.push_str(", derived_from");
        }
        out
    }
}

/// What `finish_promotion` would write for a node that exists: the vector
/// when the node carries no `embedding`, the `derived_from` edge when none
/// joins it to its entry. A read of the edges that fails is an error, never
/// an absence: a second edge would be written on it.
pub(crate) async fn unfinished(
    db: &WardsonDbClient,
    collection: &str,
    node_id: &str,
    entry_id: &str,
    node: &serde_json::Value,
) -> Result<Unfinished> {
    let vector = node.get("embedding").and_then(|v| v.as_str()).is_none_or(|s| s.is_empty());
    let body = json!({
        "filter": {
            "source_id": node_id,
            "target_id": entry_id,
            "edge_type": EdgeType::DerivedFrom.as_str(),
        },
        "limit": 1,
    });
    let found = db
        .query("memory.edges", &body)
        .await
        .map_err(|e| anyhow!("the derived_from edge of {}:{} could not be checked: {}", collection, node_id, e))?;
    Ok(Unfinished { vector, edge: found.is_empty() })
}

/// The tail of a promotion for a node that exists and whose entry points at
/// it: the vector and the `derived_from` edge, each when `missing` says so.
/// Both are best-effort, as in `write_node_to_the_end`; the edge's
/// `created_at` is the node's.
pub(crate) async fn finish_promotion(
    db: &WardsonDbClient,
    config: &SystemConfig,
    collection: &str,
    node_id: &str,
    entry_id: &str,
    node: &serde_json::Value,
    missing: Unfinished,
) {
    if missing.vector {
        crate::embedding::write::embed_node(db, config, collection, node_id, node, false).await;
    }
    if missing.edge {
        let created_at = node
            .get("created_at")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| Utc::now().to_rfc3339());
        insert_derived_from_edge(
            db,
            node_id,
            collection,
            entry_id,
            "memory.entries",
            derived_from_metadata(collection, node),
            &created_at,
        )
        .await;
    }
}

/// The metadata of a node's `derived_from` edge, from its collection: the
/// promotion type, and a semantic node's category.
pub(crate) fn derived_from_metadata(collection: &str, node: &serde_json::Value) -> serde_json::Value {
    if collection == "memory.semantic" {
        json!({
            "promotion_type": "semantic",
            "category": node.get("category").cloned().unwrap_or(serde_json::Value::Null),
        })
    } else {
        json!({ "promotion_type": "procedural" })
    }
}

pub(crate) async fn insert_derived_from_edge(
    db: &WardsonDbClient,
    source_id: &str,
    source_collection: &str,
    target_id: &str,
    target_collection: &str,
    metadata: serde_json::Value,
    created_at: &str,
) {
    let edge = json!({
        "source_id": source_id,
        "source_collection": source_collection,
        "target_id": target_id,
        "target_collection": target_collection,
        "edge_type": EdgeType::DerivedFrom.as_str(),
        "weight": 1.0,
        "metadata": metadata,
        "created_at": created_at,
    });
    if let Err(e) = db.write("memory.edges", &edge).await {
        tracing::warn!(
            "promotion of {target_id}: the derived_from edge of {source_collection}:{source_id} was not written: {e}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn source() -> SourceEntry {
        SourceEntry {
            content: "the cert refresh works after manual generation".into(),
            tags: vec!["certs".into()],
            session: "ops".into(),
        }
    }

    const PROCEDURE: &str = r#"{"title": "Rotate the cert", "description": "when trustd complains",
        "preconditions": ["trustd is running"], "steps": [{"order": 1, "action": "stop embra-web"}],
        "outcomes": {"success": "a new cert", "failure": "the old one stays"}}"#;

    #[test]
    fn a_procedure_is_checked_field_by_field() {
        let p = parse_procedure(PROCEDURE).expect("valid");
        assert_eq!(p.title, "Rotate the cert");
        assert_eq!(p.preconditions, ["trustd is running"]);
        assert_eq!(p.success, "a new cert");

        for field in ["title", "description", "steps", "outcomes"] {
            let mut v: serde_json::Value = serde_json::from_str(PROCEDURE).unwrap();
            v.as_object_mut().unwrap().remove(field);
            let why = parse_procedure(&v.to_string()).expect_err("refused").to_string();
            assert!(why.contains(&format!("missing field '{field}'")), "{field}: {why}");
            assert!(why.contains("Expected schema"), "{field}: {why}");
        }
        let why = parse_procedure(r#"{"title": "t", "description": "d", "steps": [], "outcomes": {"success": "s"}}"#)
            .expect_err("refused")
            .to_string();
        assert!(why.contains("missing field 'outcomes.failure'"), "{why}");
        // Preconditions are optional.
        let none = parse_procedure(
            r#"{"title": "t", "description": "d", "steps": [], "outcomes": {"success": "s", "failure": "f"}}"#,
        )
        .expect("valid");
        assert!(none.preconditions.is_empty());
        assert!(parse_procedure("not json").is_err());
    }

    #[test]
    fn the_semantic_node_carries_the_entry_verbatim() {
        let doc = semantic_doc(&source(), "e1", &SemanticCategory::Decision, "2026-10-03T00:00:00Z");
        assert_eq!(doc["content"], "the cert refresh works after manual generation");
        assert_eq!(doc["tags"], json!(["certs"]));
        assert_eq!(doc["category"], "decision");
        assert_eq!(doc["source_entry_id"], "e1");
        assert_eq!(doc["source_session"], "ops");
        assert_eq!(doc["created_at"], doc["updated_at"]);
        assert_eq!(doc.as_object().unwrap().len(), 10);
    }

    #[test]
    fn the_procedural_node_takes_its_text_from_the_procedure() {
        let procedure = parse_procedure(PROCEDURE).unwrap();
        let doc = procedural_doc(&source(), "e1", &procedure, "2026-10-03T00:00:00Z");
        assert_eq!(doc["title"], "Rotate the cert");
        assert_eq!(doc["outcomes"], json!({"success": "a new cert", "failure": "the old one stays"}));
        assert_eq!(doc["tags"], json!(["certs"]));
        assert_eq!(doc["source_entry_id"], "e1");
        assert!(doc.get("content").is_none(), "the entry's text stays in the entry");
        assert!(doc.get("category").is_none());
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

    /// Entry `e1` with the given pointer; a node write answers `n1`.
    async fn server_with_entry(pointer: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        let entry = json!({
            "_id": "e1", "content": "the cert refresh works", "tags": ["certs"], "session": "ops",
            "promoted_to": pointer,
        });
        Mock::given(method("GET")).and(path("/memory.entries/docs/e1")).respond_with(data(entry)).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/memory.semantic/docs"))
            .respond_with(data(json!({"_id": "n1"})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.edges/docs"))
            .respond_with(data(json!({"_id": "edge1"})))
            .mount(&server)
            .await;
        Mock::given(method("DELETE")).and(path("/memory.semantic/docs/n1")).respond_with(data(json!({}))).mount(&server).await;
        server
    }

    async fn pointer_write(server: &MockServer, answer: ResponseTemplate) {
        Mock::given(method("PATCH")).and(path("/memory.entries/docs/e1")).respond_with(answer).mount(server).await;
    }

    /// Every request that changes something, in order: "METHOD /path".
    async fn writes_seen(server: &MockServer) -> Vec<(String, serde_json::Value)> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() != "GET")
            .map(|r| {
                (
                    format!("{} {}", r.method, r.url.path()),
                    serde_json::from_slice(&r.body).unwrap_or(serde_json::Value::Null),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn promotion_writes_the_node_the_pointer_and_the_provenance_edge() {
        let server = server_with_entry(serde_json::Value::Null).await;
        pointer_write(&server, data(json!({}))).await;
        let db = WardsonDbClient::from_url(&server.uri());

        let node = write_semantic_node(&db, "e1", &SemanticCategory::Fact, &test_config()).await.expect("promoted");
        assert_eq!((node.collection, node.id.as_str()), ("memory.semantic", "n1"));
        assert_eq!((node.session.as_str(), node.tags.as_slice()), ("ops", ["certs".to_string()].as_slice()));

        let writes = writes_seen(&server).await;
        let order: Vec<&str> = writes.iter().map(|w| w.0.as_str()).collect();
        assert_eq!(order, ["POST /memory.semantic/docs", "PATCH /memory.entries/docs/e1", "POST /memory.edges/docs"]);
        assert_eq!(writes[0].1["source_entry_id"], "e1");
        assert_eq!(writes[0].1["category"], "fact");
        assert_eq!(writes[1].1, json!({"promoted_to": {"collection": "memory.semantic", "id": "n1"}}));
        let edge = &writes[2].1;
        assert_eq!(
            (edge["source_id"].as_str(), edge["target_id"].as_str(), edge["edge_type"].as_str()),
            (Some("n1"), Some("e1"), Some("derived_from"))
        );
    }

    /// #57: an operator stop drops the dispatch future; the node write and
    /// the pointer patch run on in a task of their own, so no node is left
    /// without the pointer that says it exists.
    #[tokio::test]
    async fn a_dropped_caller_does_not_cut_the_node_write_from_its_pointer() {
        let server = server_with_entry(serde_json::Value::Null).await;
        Mock::given(method("POST"))
            .and(path("/memory.semantic/docs"))
            .respond_with(data(json!({"_id": "n1"})).set_delay(std::time::Duration::from_millis(300)))
            .with_priority(1)
            .mount(&server)
            .await;
        pointer_write(&server, data(json!({}))).await;
        let db = WardsonDbClient::from_url(&server.uri());

        let cut = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            write_semantic_node(&db, "e1", &SemanticCategory::Fact, &test_config()),
        )
        .await;
        assert!(cut.is_err(), "the caller was dropped while the node write was in flight");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let order: Vec<String> = writes_seen(&server).await.into_iter().map(|w| w.0).collect();
            if order.iter().any(|w| w == "PATCH /memory.entries/docs/e1") {
                assert_eq!(order[0], "POST /memory.semantic/docs");
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the pointer never landed: {order:?}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn a_failed_pointer_write_removes_the_node_again() {
        let server = server_with_entry(serde_json::Value::Null).await;
        pointer_write(&server, ResponseTemplate::new(500)).await;
        let db = WardsonDbClient::from_url(&server.uri());

        let why = write_semantic_node(&db, "e1", &SemanticCategory::Fact, &test_config())
            .await
            .expect_err("refused")
            .to_string();
        assert!(why.contains("promotion pointer could not be written"), "{why}");
        let writes = writes_seen(&server).await;
        let order: Vec<&str> = writes.iter().map(|w| w.0.as_str()).collect();
        assert_eq!(
            order,
            ["POST /memory.semantic/docs", "PATCH /memory.entries/docs/e1", "DELETE /memory.semantic/docs/n1"],
            "no provenance edge for a node that was taken back"
        );
    }

    #[tokio::test]
    async fn a_transient_read_error_does_not_clear_the_pointer() {
        let server = server_with_entry(json!({"collection": "memory.semantic", "id": "n9"})).await;
        Mock::given(method("GET"))
            .and(path("/memory.semantic/docs/n9"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());

        let why = write_semantic_node(&db, "e1", &SemanticCategory::Fact, &test_config())
            .await
            .expect_err("refused")
            .to_string();
        assert!(why.contains("memory.semantic:n9, which could not be read"), "{why}");
        assert!(writes_seen(&server).await.is_empty(), "nothing is written on a read that failed");
    }

    #[tokio::test]
    async fn a_pointer_at_a_node_that_is_gone_is_cleared_and_the_entry_promoted() {
        // n9 answers 404: wiremock's reply to a path nothing is mounted on.
        let server = server_with_entry(json!({"collection": "memory.semantic", "id": "n9"})).await;
        pointer_write(&server, data(json!({}))).await;
        let db = WardsonDbClient::from_url(&server.uri());

        write_semantic_node(&db, "e1", &SemanticCategory::Fact, &test_config()).await.expect("promoted");
        let writes = writes_seen(&server).await;
        assert_eq!(writes[0].0, "PATCH /memory.entries/docs/e1");
        assert_eq!(writes[0].1, json!({"promoted_to": null}));
        assert_eq!(writes[1].0, "POST /memory.semantic/docs");
    }

    #[tokio::test]
    async fn an_entry_with_a_live_node_is_refused_with_the_node_named() {
        let server = server_with_entry(json!({"collection": "memory.semantic", "id": "n9"})).await;
        Mock::given(method("GET"))
            .and(path("/memory.semantic/docs/n9"))
            .respond_with(data(json!({"_id": "n9", "content": "x", "category": "fact"})))
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());

        let why = promote_to_procedural(&db, "e1", PROCEDURE, &test_config())
            .await
            .expect_err("refused")
            .to_string();
        assert_eq!(why, "Entry e1 already promoted to memory.semantic:n9");
        assert!(writes_seen(&server).await.is_empty());
    }

    #[tokio::test]
    async fn an_entry_that_is_not_there_is_said_before_the_procedure_is_read() {
        let server = MockServer::start().await;
        let db = WardsonDbClient::from_url(&server.uri());
        let why = promote_to_procedural(&db, "nope", "not json", &test_config())
            .await
            .expect_err("refused")
            .to_string();
        assert_eq!(why, "Entry nope not found in memory.entries");
    }

    #[tokio::test]
    async fn entry_state_tells_missing_unpromoted_and_pointing_apart() {
        let server = MockServer::start().await;
        let entry = |pointer: serde_json::Value| data(json!({"_id": "e", "content": "c", "tags": [], "session": "s", "promoted_to": pointer}));
        Mock::given(method("GET")).and(path("/memory.entries/docs/unpromoted")).respond_with(entry(serde_json::Value::Null)).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/memory.entries/docs/stale"))
            .respond_with(entry(json!({"collection": "memory.semantic", "id": "gone"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/memory.entries/docs/live"))
            .respond_with(entry(json!({"collection": "memory.semantic", "id": "n1"})))
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/memory.semantic/docs/n1")).respond_with(data(json!({"_id": "n1"}))).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/memory.entries/docs/unreadable"))
            .respond_with(ResponseTemplate::new(500).set_body_string("timeout"))
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());

        assert_eq!(entry_state(&db, "missing").await.unwrap(), EntryState::Missing);
        assert_eq!(entry_state(&db, "unpromoted").await.unwrap(), EntryState::Unpromoted);
        assert_eq!(entry_state(&db, "stale").await.unwrap(), EntryState::Unpromoted, "a pointer at a node that is gone");
        assert_eq!(
            entry_state(&db, "live").await.unwrap(),
            EntryState::PointsAt { collection: "memory.semantic".into(), id: "n1".into() }
        );
        let why = entry_state(&db, "unreadable").await.expect_err("a failed read is no state").to_string();
        assert!(why.starts_with("Entry unreadable could not be read"), "{why}");
    }

    /// The sweep's heal and adopt write what a node still lacks and nothing
    /// twice: a node with a vector is not embedded again, an existing
    /// `derived_from` edge is not duplicated, and an edge read that fails
    /// is an error rather than an absence.
    #[tokio::test]
    async fn finish_promotion_writes_only_what_is_missing() {
        let server = MockServer::start().await;
        let edge_query = |req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
            body["filter"]["edge_type"] == "derived_from"
        };
        Mock::given(method("POST"))
            .and(path("/memory.edges/query"))
            .and(move |req: &wiremock::Request| edge_query(req) && {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
                body["filter"]["source_id"] == "n2"
            })
            .respond_with(data(json!([{"_id": "d2"}])))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.edges/query"))
            .and(move |req: &wiremock::Request| edge_query(req) && {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
                body["filter"]["source_id"] == "n3"
            })
            .respond_with(ResponseTemplate::new(500).set_body_string("timeout"))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/memory.edges/query")).respond_with(data(json!([]))).mount(&server).await;
        Mock::given(method("POST")).and(path("/memory.edges/docs")).respond_with(data(json!({"_id": "edge1"}))).mount(&server).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let bare = json!({"_id": "n1", "content": "one", "category": "fact", "created_at": "2026-05-01T00:00:00Z"});
        let done = json!({"_id": "n2", "content": "two", "category": "fact", "embedding": "AAAA", "embedding_model": "m"});

        assert_eq!(unfinished(&db, "memory.semantic", "n1", "e1", &bare).await.unwrap(), Unfinished { vector: true, edge: true });
        assert_eq!(unfinished(&db, "memory.semantic", "n2", "e2", &done).await.unwrap(), Unfinished { vector: false, edge: false });
        let why = unfinished(&db, "memory.semantic", "n3", "e3", &bare).await.expect_err("unread edges are no absence").to_string();
        assert!(why.starts_with("the derived_from edge of memory.semantic:n3 could not be checked"), "{why}");
        assert_eq!(Unfinished { vector: true, edge: true }.listed(), ", vector, derived_from");
        assert_eq!(Unfinished::default().listed(), "");

        finish_promotion(&db, &test_config(), "memory.semantic", "n1", "e1", &bare, Unfinished { vector: true, edge: true }).await;
        finish_promotion(&db, &test_config(), "memory.semantic", "n2", "e2", &done, Unfinished::default()).await;
        let writes = writes_seen(&server).await;
        let docs: Vec<&(String, serde_json::Value)> = writes.iter().filter(|w| w.0 == "POST /memory.edges/docs").collect();
        assert_eq!(docs.len(), 1, "one edge, for the node that lacked it: {writes:?}");
        let edge = &docs[0].1;
        assert_eq!(
            (edge["source_id"].as_str(), edge["target_id"].as_str(), edge["edge_type"].as_str(), edge["created_at"].as_str()),
            (Some("n1"), Some("e1"), Some("derived_from"), Some("2026-05-01T00:00:00Z"))
        );
        assert_eq!(edge["metadata"], json!({"promotion_type": "semantic", "category": "fact"}));
        assert_eq!(derived_from_metadata("memory.procedural", &bare), json!({"promotion_type": "procedural"}));
    }
}
