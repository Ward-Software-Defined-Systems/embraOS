//! Migration v14: a `superseded_by` field becomes a declared `supersedes`
//! edge (`knowledge::supersession`).
//!
//! The field was a documented convention that nothing read and no read
//! surface showed. The edge is shown wherever a node is read and makes its
//! target history for the audit. `knowledge_update` refuses the field from
//! now on, so the step converts what was written before.
//!
//! The step finishes or it does not. A transient error leaves the version
//! unrecorded, and the next boot runs it again: a written edge is a
//! duplicate the second time and a cleared field is not found, so a second
//! run changes nothing. A field whose successor cannot be found, or that
//! names its own node, or whose successor already supersedes the node,
//! stays where it is and is named in the journal; `knowledge_update` clears
//! it with a null.

use std::collections::HashSet;

use serde_json::json;
use tracing::{info, warn};

use crate::db::error::is_not_found;
use crate::db::WardsonDbClient;
use crate::knowledge::audit::resolve_kg_collection;
use crate::knowledge::tools::write_link;
use crate::knowledge::types::EdgeType;

/// The knowledge-node collections, in the order a bare id is looked up.
const NODE_COLLECTIONS: [&str; 2] = ["memory.semantic", "memory.procedural"];
/// The most fields one read returns. A graph that holds more converts the
/// rest on the next read of the same run (a converted field is gone).
const FIELD_WINDOW: usize = 10_000;

/// Every node that still carries the field. `$ne` does not match a document
/// without it, so this is no recency window.
pub(super) fn superseded_by_query_body() -> serde_json::Value {
    json!({
        "filter": {"superseded_by": {"$ne": null}},
        "sort": [{"created_at": "desc"}],
        "limit": FIELD_WINDOW,
        "fields": ["superseded_by"],
    })
}

/// What became of one field.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// The edge exists and the field is gone.
    Converted,
    /// The field stays: the reason is named in the journal.
    Left(String),
    /// A read or write failed; the next boot tries again.
    Transient(String),
}

/// A successor that exists, as `(collection, id)`.
enum Successor {
    Found(&'static str, String),
    Missing,
    Transient(String),
}

/// Whether `collection:id` is there: `Ok(false)` only on a 404.
async fn exists(db: &WardsonDbClient, collection: &str, id: &str) -> Result<bool, String> {
    match db.read(collection, id).await {
        Ok(_) => Ok(true),
        Err(e) if is_not_found(&e) => Ok(false),
        Err(e) => Err(format!("{collection}:{id} could not be read: {e}")),
    }
}

/// The node an entry was promoted to, when the field named an entry.
async fn node_of_entry(db: &WardsonDbClient, id: &str) -> Successor {
    let entry = match db.read("memory.entries", id).await {
        Ok(entry) => entry,
        Err(e) if is_not_found(&e) => return Successor::Missing,
        Err(e) => return Successor::Transient(format!("memory.entries:{id} could not be read: {e}")),
    };
    let pointer = entry.get("promoted_to");
    let collection = pointer.and_then(|p| p.get("collection")).and_then(|v| v.as_str());
    let node = pointer.and_then(|p| p.get("id")).and_then(|v| v.as_str());
    match (collection.and_then(|c| resolve_kg_collection(c).ok()), node) {
        (Some(collection), Some(node)) => match exists(db, collection, node).await {
            Ok(true) => Successor::Found(collection, node.to_string()),
            Ok(false) => Successor::Missing,
            Err(e) => Successor::Transient(e),
        },
        _ => Successor::Missing,
    }
}

/// Resolve a field's value: the documented `{collection, id}`, the
/// `collection:id` form every tool reply prints (full or short collection
/// names), a bare node id, or an entry id that was promoted.
async fn successor(db: &WardsonDbClient, value: &serde_json::Value) -> Successor {
    let (collection, id) = match value {
        serde_json::Value::Object(o) => (
            o.get("collection").and_then(|v| v.as_str()).map(str::to_string),
            o.get("id").and_then(|v| v.as_str()).unwrap_or("").trim().to_string(),
        ),
        serde_json::Value::String(s) => match s.trim().split_once(':') {
            Some((c, i)) => (Some(c.trim().to_string()), i.trim().to_string()),
            None => (None, s.trim().to_string()),
        },
        _ => return Successor::Missing,
    };
    if id.is_empty() {
        return Successor::Missing;
    }
    match collection.as_deref() {
        Some(c) if matches!(c.trim().to_lowercase().as_str(), "entries" | "memory.entries") => node_of_entry(db, &id).await,
        Some(c) => match resolve_kg_collection(c) {
            Ok(collection) => match exists(db, collection, &id).await {
                Ok(true) => Successor::Found(collection, id),
                Ok(false) => Successor::Missing,
                Err(e) => Successor::Transient(e),
            },
            Err(_) => Successor::Missing,
        },
        None => {
            for collection in NODE_COLLECTIONS {
                match exists(db, collection, &id).await {
                    Ok(true) => return Successor::Found(collection, id),
                    Ok(false) => {}
                    Err(e) => return Successor::Transient(e),
                }
            }
            node_of_entry(db, &id).await
        }
    }
}

/// Convert one field: write the declaration, then clear the field.
async fn convert(db: &WardsonDbClient, collection: &'static str, id: &str, value: &serde_json::Value) -> Outcome {
    let (succ_collection, succ_id) = match successor(db, value).await {
        Successor::Found(c, i) => (c, i),
        Successor::Missing => return Outcome::Left(format!("its successor {value} is not a node")),
        Successor::Transient(e) => return Outcome::Transient(e),
    };
    if succ_collection == collection && succ_id == id {
        return Outcome::Left("it names its own node".to_string());
    }
    match write_link(db, (succ_collection, &succ_id), &EdgeType::Supersedes, (collection, id), 1.0).await {
        Ok(_) => {}
        Err(e) if e.contains("Edge already exists") => {}
        Err(e) if e.contains("already supersedes") => return Outcome::Left(e),
        Err(e) => return Outcome::Transient(e),
    }
    match db.patch_document(collection, id, &json!({"superseded_by": null})).await {
        Ok(()) => Outcome::Converted,
        Err(e) => Outcome::Transient(format!("the field could not be cleared: {e}")),
    }
}

/// Convert every `superseded_by` field. Returns whether the step finished;
/// only then is the version recorded.
pub(super) async fn run_v14_supersession(db: &WardsonDbClient) -> bool {
    info!("Running migration v14: superseded_by fields become supersedes edges");
    let mut finished = true;
    let (mut converted, mut left) = (0usize, 0usize);
    for collection in NODE_COLLECTIONS {
        let mut seen: HashSet<String> = HashSet::new();
        loop {
            let docs = match db.query(collection, &superseded_by_query_body()).await {
                Ok(docs) => docs,
                Err(e) if is_not_found(&e) => break,
                Err(e) => {
                    warn!("Migration v14: {collection} could not be read ({e}); the next boot tries again");
                    finished = false;
                    break;
                }
            };
            let fresh: Vec<&serde_json::Value> = docs
                .iter()
                .filter(|d| d.get("_id").and_then(|v| v.as_str()).is_some_and(|id| !seen.contains(id)))
                .collect();
            if fresh.is_empty() {
                break;
            }
            for doc in fresh {
                let id = doc.get("_id").and_then(|v| v.as_str()).unwrap_or_default();
                seen.insert(id.to_string());
                let value = doc.get("superseded_by").cloned().unwrap_or(serde_json::Value::Null);
                match convert(db, collection, id, &value).await {
                    Outcome::Converted => converted += 1,
                    Outcome::Left(why) => {
                        left += 1;
                        warn!("Migration v14: {collection}:{id} keeps its superseded_by field: {why}");
                    }
                    Outcome::Transient(e) => {
                        finished = false;
                        warn!("Migration v14: {collection}:{id} not converted ({e}); the next boot tries again");
                    }
                }
            }
        }
    }
    info!("Migration v14: {converted} field(s) became supersedes edges, {left} left in place");
    finished
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn data(v: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": v, "meta": {}}))
    }

    fn missing() -> ResponseTemplate {
        ResponseTemplate::new(404).set_body_json(json!({"ok": false, "error": {"code": "DOCUMENT_NOT_FOUND", "message": "not found"}}))
    }

    /// A graph of semantic nodes `w1`, `w2` and those named, an entry `e1`
    /// promoted to `w2`, no edges. The first read of the field returns
    /// `fields`; every later read returns nothing.
    async fn graph(nodes: &[&str], fields: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        for id in nodes.iter().chain(["w1", "w2"].iter()) {
            Mock::given(method("GET"))
                .and(path(format!("/memory.semantic/docs/{id}")))
                .respond_with(data(json!({"_id": id})))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/memory.entries/docs/e1"))
            .respond_with(data(json!({"_id": "e1", "promoted_to": {"collection": "memory.semantic", "id": "w2"}})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.semantic/query"))
            .and(body_partial_json(json!({"filter": {"superseded_by": {"$ne": null}}, "limit": FIELD_WINDOW})))
            .respond_with(data(fields))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.semantic/query"))
            .and(body_partial_json(json!({"filter": {"superseded_by": {"$ne": null}}, "limit": FIELD_WINDOW})))
            .respond_with(data(json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.procedural/query"))
            .and(body_partial_json(json!({"filter": {"superseded_by": {"$ne": null}}, "limit": FIELD_WINDOW})))
            .respond_with(data(json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/memory.edges/query")).respond_with(data(json!([]))).mount(&server).await;
        Mock::given(method("POST")).and(path("/memory.edges/docs")).respond_with(data(json!({"_id": "edge"}))).mount(&server).await;
        Mock::given(method("PATCH")).respond_with(data(json!({}))).mount(&server).await;
        Mock::given(method("GET")).respond_with(missing()).mount(&server).await;
        server
    }

    fn requests_of(requests: &[wiremock::Request], verb: &str, at: &str) -> Vec<serde_json::Value> {
        requests
            .iter()
            .filter(|r| r.method.as_str() == verb && r.url.path().starts_with(at))
            .map(|r| serde_json::from_slice(&r.body).unwrap_or(serde_json::Value::Null))
            .collect()
    }

    #[test]
    fn the_read_finds_every_field_and_no_document_without_one() {
        let body = superseded_by_query_body();
        assert_eq!(body["filter"], json!({"superseded_by": {"$ne": null}}));
        assert_eq!(body["limit"], json!(FIELD_WINDOW));
        assert_eq!(body["sort"], json!([{"created_at": "desc"}]));
    }

    /// Every shape a field was written in becomes the same declaration,
    /// and the field is cleared.
    #[tokio::test]
    async fn every_shape_of_the_field_becomes_a_declaration() {
        let fields = json!([
            {"_id": "n1", "superseded_by": "w1"},
            {"_id": "n2", "superseded_by": {"collection": "memory.semantic", "id": "w1"}},
            {"_id": "n3", "superseded_by": "memory.semantic:w1"},
            {"_id": "n4", "superseded_by": "semantic:w1"},
            {"_id": "n5", "superseded_by": "e1"},
        ]);
        let server = graph(&["n1", "n2", "n3", "n4", "n5"], fields).await;
        let db = WardsonDbClient::from_url(&server.uri());
        assert!(run_v14_supersession(&db).await);

        let requests = server.received_requests().await.unwrap_or_default();
        let edges = requests_of(&requests, "POST", "/memory.edges/docs");
        let declared: Vec<(&str, &str)> = edges
            .iter()
            .map(|e| (e["source_id"].as_str().unwrap(), e["target_id"].as_str().unwrap()))
            .collect();
        assert_eq!(declared, vec![("w1", "n1"), ("w1", "n2"), ("w1", "n3"), ("w1", "n4"), ("w2", "n5")]);
        assert!(edges.iter().all(|e| e["edge_type"] == "supersedes" && e["metadata"] == json!({"supersession": true})));
        let patches = requests_of(&requests, "PATCH", "/memory.semantic/docs/");
        assert_eq!(patches.len(), 5);
        assert!(patches.iter().all(|p| *p == json!({"superseded_by": null})));
    }

    /// A successor that is not a node, and a field that names its own node,
    /// stay in place: nothing is written and the step still finishes.
    #[tokio::test]
    async fn a_missing_successor_and_a_self_reference_are_left_in_place() {
        let fields = json!([
            {"_id": "n1", "superseded_by": "gone"},
            {"_id": "n2", "superseded_by": "n2"},
        ]);
        let server = graph(&["n1", "n2"], fields).await;
        let db = WardsonDbClient::from_url(&server.uri());
        assert!(run_v14_supersession(&db).await);
        let requests = server.received_requests().await.unwrap_or_default();
        assert!(requests_of(&requests, "POST", "/memory.edges/docs").is_empty());
        assert!(requests_of(&requests, "PATCH", "/").is_empty());
    }

    /// A read that fails for another reason than a 404 is no missing
    /// successor: the field stays and the step does not finish.
    #[tokio::test]
    async fn a_transient_error_leaves_the_step_unfinished() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/memory.semantic/query"))
            .respond_with(data(json!([{"_id": "n1", "superseded_by": "w1"}])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.procedural/query"))
            .respond_with(data(json!([])))
            .mount(&server)
            .await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
        let db = WardsonDbClient::from_url(&server.uri());
        assert!(!run_v14_supersession(&db).await);
        let requests = server.received_requests().await.unwrap_or_default();
        assert!(requests_of(&requests, "PATCH", "/").is_empty());
    }

    /// A graph without the field: the step finishes and writes nothing.
    #[tokio::test]
    async fn a_graph_without_the_field_is_left_as_it_is() {
        let server = graph(&[], json!([])).await;
        let db = WardsonDbClient::from_url(&server.uri());
        assert!(run_v14_supersession(&db).await);
        let requests = server.received_requests().await.unwrap_or_default();
        assert!(requests_of(&requests, "POST", "/memory.edges/docs").is_empty());
        assert!(requests_of(&requests, "PATCH", "/").is_empty());
    }

    /// Runs the step against a live WardSONDB, for a check on a copy of an
    /// instance's data. Never against the instance itself:
    ///
    /// `EMBRA_MIGRATION_DB=http://127.0.0.1:18090 cargo test -p embra-brain v14_on_a_live_copy -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a WardSONDB holding a copy of an instance's data"]
    async fn v14_on_a_live_copy() {
        let url = std::env::var("EMBRA_MIGRATION_DB").expect("EMBRA_MIGRATION_DB=<url of the copy>");
        let db = WardsonDbClient::from_url(&url);
        let before = db.query("memory.semantic", &superseded_by_query_body()).await.expect("read").len();
        let finished = run_v14_supersession(&db).await;
        let after = db.query("memory.semantic", &superseded_by_query_body()).await.expect("read").len();
        let declarations = db
            .query("memory.edges", &crate::knowledge::supersession::supersessions_query_body())
            .await
            .expect("declarations");
        println!("fields before {before}, after {after}; declarations {}; finished {finished}", declarations.len());
        let again = run_v14_supersession(&db).await;
        let declarations_again = db
            .query("memory.edges", &crate::knowledge::supersession::supersessions_query_body())
            .await
            .expect("declarations")
            .len();
        println!("second run finished {again}; declarations {declarations_again}");
        assert!(finished && again);
        assert_eq!(declarations.len(), declarations_again);
    }
}
