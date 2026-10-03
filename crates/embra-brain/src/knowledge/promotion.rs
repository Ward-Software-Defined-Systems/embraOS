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
#[derive(Debug)]
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
    let preconditions: Vec<String> = parsed.get("preconditions")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
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
fn pointer_patch(collection: &str, id: &str) -> serde_json::Value {
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
        let _ = db.patch_document("memory.entries", entry_id, &json!({"promoted_to": null})).await;
    }

    let content = doc.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let tags: Vec<String> = doc.get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
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

async fn insert_derived_from_edge(
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
    let _ = db.write("memory.edges", &edge).await;
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
}
