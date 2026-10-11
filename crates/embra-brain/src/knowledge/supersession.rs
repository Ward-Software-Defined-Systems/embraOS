//! Declared supersessions: a `supersedes` edge whose metadata carries
//! `SUPERSESSION_KEY` (`write_link` sets it). The edge runs from the newer
//! node to the one it replaces. The target is history: every read surface
//! marks it (`superseded_note`) and the audit leaves it out of its checks.
//!
//! One query finds every declaration, through the single-field index
//! `idx_edge_supersession` (`migrations::hot_path_index_specs`). Looking the
//! marks up by the ids of the printed nodes instead goes through
//! `idx_edge_target` and reads every edge into them, automatic ones
//! included: on the 2026-10-11 backup (472,457 edges) 1,216 edges for 10
//! nodes and 176,866 for 1,000, against none for the marker's index.

use std::collections::HashMap;

use serde_json::json;
use tracing::warn;

use crate::db::WardsonDbClient;

use super::types::{EdgeType, SUPERSESSION_FIELD, SUPERSESSION_KEY};

/// The most declarations one read collects. A graph with more warns, and the
/// marks past the window are not shown.
pub(crate) const SUPERSESSION_WINDOW: usize = 10_000;

/// `(collection, id)` of a node.
pub(crate) type NodeKey = (String, String);

/// Every declaration, the strongest and then the newest first: the order in
/// which `superseders_from` picks one successor per node.
pub(crate) fn supersessions_query_body() -> serde_json::Value {
    json!({
        "filter": {"metadata.supersession": true},
        "sort": [{"weight": "desc"}, {"created_at": "desc"}],
        "limit": SUPERSESSION_WINDOW,
    })
}

/// Each superseded node and its successor, from the declarations in query
/// order: the first one per node wins. An edge without the marker, of
/// another type or outside the knowledge nodes declares nothing.
pub(crate) fn superseders_from(edges: &[serde_json::Value]) -> HashMap<NodeKey, NodeKey> {
    let mut out: HashMap<NodeKey, NodeKey> = HashMap::new();
    for edge in edges {
        let get = |field: &str| edge.get(field).and_then(|v| v.as_str()).unwrap_or("");
        let marked = edge
            .get("metadata")
            .and_then(|m| m.get(SUPERSESSION_KEY))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !marked || EdgeType::from_str(get("edge_type")) != Some(EdgeType::Supersedes) {
            continue;
        }
        let (source, target) = (
            (get("source_collection").to_string(), get("source_id").to_string()),
            (get("target_collection").to_string(), get("target_id").to_string()),
        );
        let knowledge = |key: &NodeKey| matches!(key.0.as_str(), "memory.semantic" | "memory.procedural") && !key.1.is_empty();
        if knowledge(&source) && knowledge(&target) && source != target {
            out.entry(target).or_insert(source);
        }
    }
    out
}

/// Every declared supersession, read once per read surface. A read that fails
/// gives none: the surface prints its nodes without the marks rather than
/// failing.
pub(crate) async fn superseders(db: &WardsonDbClient) -> HashMap<NodeKey, NodeKey> {
    match db.query("memory.edges", &supersessions_query_body()).await {
        Ok(edges) => {
            if crate::db::client::window_saturated(edges.len(), SUPERSESSION_WINDOW) {
                warn!(
                    target: "wardsondb::window",
                    collection = "memory.edges",
                    field = SUPERSESSION_FIELD,
                    limit = SUPERSESSION_WINDOW,
                    "supersession window saturated: the marks of the oldest declarations are not shown"
                );
            }
            superseders_from(&edges)
        }
        Err(e) => {
            warn!(target: "kg::supersession", "declared supersessions not read: {e}");
            HashMap::new()
        }
    }
}

/// What a read surface prints after a superseded node; nothing for a live one.
pub(crate) fn superseded_note(successor: Option<&NodeKey>) -> String {
    successor
        .map(|(collection, id)| format!(" — superseded by {collection}:{id}"))
        .unwrap_or_default()
}

/// The successor of `collection:id`, if a declaration names one.
pub(crate) fn successor_of<'a>(
    superseders: &'a HashMap<NodeKey, NodeKey>,
    collection: &str,
    id: &str,
) -> Option<&'a NodeKey> {
    superseders.get(&(collection.to_string(), id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(src: &str, tgt: &str, marked: bool) -> serde_json::Value {
        json!({
            "source_collection": "memory.semantic", "source_id": src,
            "target_collection": "memory.semantic", "target_id": tgt,
            "edge_type": "supersedes", "weight": 1.0,
            "metadata": if marked { json!({"supersession": true}) } else { json!({}) },
        })
    }

    fn key(id: &str) -> NodeKey {
        ("memory.semantic".to_string(), id.to_string())
    }

    #[test]
    fn the_query_reads_every_declaration_through_the_marker() {
        let body = supersessions_query_body();
        assert_eq!(body["filter"], json!({ SUPERSESSION_FIELD: true }));
        assert_eq!(body["sort"], json!([{"weight": "desc"}, {"created_at": "desc"}]));
        assert_eq!(body["limit"], json!(SUPERSESSION_WINDOW));
    }

    #[test]
    fn the_first_declaration_in_query_order_names_the_successor() {
        let edges = [declaration("w1", "n", true), declaration("w2", "n", true)];
        let map = superseders_from(&edges);
        assert_eq!(map.get(&key("n")), Some(&key("w1")));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn only_a_marked_supersedes_edge_between_knowledge_nodes_declares() {
        let mut identity = declaration("w", "self", true);
        identity["target_collection"] = json!("identity.graph");
        let mut refines = declaration("w", "r", true);
        refines["edge_type"] = json!("refines");
        let edges = [declaration("w", "unmarked", false), identity, refines, declaration("w", "n", true)];
        let map = superseders_from(&edges);
        assert_eq!(map.keys().collect::<Vec<_>>(), vec![&key("n")]);
    }

    #[test]
    fn a_superseded_node_gets_the_note_and_a_live_one_nothing() {
        let map = superseders_from(&[declaration("w", "n", true)]);
        assert_eq!(
            superseded_note(successor_of(&map, "memory.semantic", "n")),
            " — superseded by memory.semantic:w"
        );
        assert_eq!(superseded_note(successor_of(&map, "memory.semantic", "w")), "");
    }

    #[tokio::test]
    async fn a_failed_read_gives_no_marks() {
        let db = WardsonDbClient::from_url("http://127.0.0.1:9");
        assert!(superseders(&db).await.is_empty());
    }
}
