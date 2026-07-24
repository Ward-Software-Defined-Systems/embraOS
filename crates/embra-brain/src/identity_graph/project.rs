//! KG projection of the sealed graphs + the every-boot reconcile.
//!
//! The sealed doc (`soul.invariant`) and the graph-shaped `memory.user`
//! doc are the sources of truth; this module derives the LIVE projection —
//! one `identity.graph` doc per node, one `memory.edges` doc per edge —
//! and heals drift insert-missing-only (existing docs are never patched,
//! so `access_count` accrual and operator edge deletions... the former
//! survives; the latter is deliberately restored, see the reconcile note).

use serde_json::{json, Value};
use tracing::{info, warn};

use crate::db::WardsonDbClient;

use super::format::{GraphEdge, GraphNode, IdentityGraph};
use super::{
    EDGE_ORIGIN_IDENTITY, EDGE_ORIGIN_USER, IDENTITY_COLLECTION, ORIGIN_USER,
};

/// Projection doc for one graph node. Field names align with semantic
/// nodes (`content`, `tags`, access fields) so the classifier/scorer arms
/// and `spawn_access_touches` work unmodified.
pub(crate) fn identity_node_doc(node: &GraphNode, origin: &str, now: &str) -> Value {
    json!({
        "_id": node.id,
        "content": node.text,
        "node_type": node.node_type,
        "tags": [],
        "origin": origin,
        "access_count": 0,
        "last_accessed": Value::Null,
        "created_at": now,
        "updated_at": now,
    })
}

/// Projection doc for one graph edge — exactly the memory.edges shape the
/// tool/auto writers produce, weight 1.0, provenance under
/// `metadata.origin`.
pub(crate) fn identity_edge_doc(edge: &GraphEdge, edge_origin: &str, now: &str) -> Value {
    json!({
        "source_id": edge.src,
        "source_collection": IDENTITY_COLLECTION,
        "target_id": edge.dst,
        "target_collection": IDENTITY_COLLECTION,
        "edge_type": edge.relation,
        "weight": 1.0,
        "metadata": { "origin": edge_origin },
        "created_at": now,
    })
}

/// Bulk-write a graph into the projection. Partial success (custom-id 409s
/// on re-runs) is expected and benign — the reconcile converges the rest.
/// Best-effort by contract: callers sit AFTER the irreversible seal, so
/// failures warn loudly and heal at next boot rather than aborting.
pub async fn project_to_kg(
    db: &WardsonDbClient,
    graph: &IdentityGraph,
    node_origin: &str,
    edge_origin: &str,
) {
    let now = chrono::Utc::now().to_rfc3339();
    if !db.collection_exists(IDENTITY_COLLECTION).await.unwrap_or(false) {
        if let Err(e) = db.create_collection(IDENTITY_COLLECTION).await {
            warn!(target: "identity_graph", "projection: create_collection failed: {e} — boot reconcile will retry");
        }
    }

    let node_docs: Vec<Value> = graph
        .nodes
        .iter()
        .map(|n| identity_node_doc(n, node_origin, &now))
        .collect();
    match db.bulk_write(IDENTITY_COLLECTION, &node_docs).await {
        Ok(inserted) if inserted as usize == node_docs.len() => {
            info!(target: "identity_graph", "projection: {} nodes written", inserted);
        }
        Ok(inserted) => warn!(
            target: "identity_graph",
            "projection: {} of {} nodes written (duplicates on re-run are benign; reconcile heals the rest)",
            inserted, node_docs.len()
        ),
        Err(e) => warn!(target: "identity_graph", "projection: node bulk_write failed: {e} — boot reconcile will heal"),
    }

    let edge_docs: Vec<Value> = graph
        .edges
        .iter()
        .map(|e| identity_edge_doc(e, edge_origin, &now))
        .collect();
    match db.bulk_write("memory.edges", &edge_docs).await {
        Ok(inserted) if inserted as usize == edge_docs.len() => {
            info!(target: "identity_graph", "projection: {} edges written", inserted);
        }
        Ok(inserted) => warn!(
            target: "identity_graph",
            "projection: {} of {} edges written (reconcile heals the rest)",
            inserted, edge_docs.len()
        ),
        Err(e) => warn!(target: "identity_graph", "projection: edge bulk_write failed: {e} — boot reconcile will heal"),
    }
}

/// The complete post-seal transition, shared by the conversational path
/// (Stage 6) and the import path (Stage 7):
///   1. project the sealed IDENTITY+SOUL graph;
///   2. rewrite `memory.user` from flat to graph shape (idempotent — a
///      graph-shaped doc passes through unchanged);
///   3. project the operator subgraph.
/// Best-effort throughout: the seal already happened (the irreversible
/// step); everything here is derivable and boot-heals.
pub async fn complete_graph_transition(
    db: &WardsonDbClient,
    sealed_graph: &IdentityGraph,
    node_origin: &str,
) {
    project_to_kg(db, sealed_graph, node_origin, EDGE_ORIGIN_IDENTITY).await;

    let user_doc = match db.read("memory.user", "user").await {
        Ok(doc) => doc,
        Err(e) => {
            warn!(target: "identity_graph", "user transition skipped: memory.user unreadable: {e}");
            return;
        }
    };
    let link = sealed_graph.nodes.iter().any(|n| n.id == "operator");
    let Some(user_graph) = super::transform::user_to_graph(&user_doc, link) else {
        warn!(target: "identity_graph", "user transition skipped: memory.user is not an object");
        return;
    };

    // Rewrite the doc in graph shape (full replace, _id preserved). The
    // passthrough above makes this byte-stable on re-runs.
    let operator_name = user_graph.name.clone().unwrap_or_default();
    let mut new_doc = user_graph.canonicalize(&operator_name);
    if let Some(obj) = new_doc.as_object_mut() {
        obj.insert("_id".to_string(), json!("user"));
    }
    match db.update("memory.user", "user", &new_doc).await {
        Ok(_) => info!(target: "identity_graph", "memory.user rewritten in graph shape ({} nodes)", user_graph.nodes.len()),
        Err(e) => warn!(target: "identity_graph", "memory.user graph rewrite failed: {e} — profile stays flat; rerun heals"),
    }

    project_to_kg(db, &user_graph, ORIGIN_USER, EDGE_ORIGIN_USER).await;
}

/// Every-boot reconcile (called from the run_migrations tail, beside
/// `ensure_hot_path_indexes` — unversioned on purpose: it must re-run
/// forever). Insert-missing-only against BOTH sources of truth:
/// the sealed graph and, when graph-shaped, `memory.user`.
///
/// Fast path on a healthy boot: one soul read + one user read + three
/// counts. Heal path: per-node point reads / per-edge existence probes for
/// the missing set only. Existing docs are never patched (access_count
/// accrual survives); a tool-deleted identity edge is deliberately
/// RESTORED here — the sealed doc outranks runtime mutation.
pub async fn ensure_identity_projection(db: &WardsonDbClient) {
    let Ok(Some(soul)) = crate::learning::load_soul(db).await else {
        return; // no soul (first boot mid-learning) — nothing to reconcile
    };
    let Some(sealed_graph) = super::format::parse_sealed(&soul) else {
        return; // legacy flat soul — graph mode not active
    };

    reconcile_graph(db, &sealed_graph, super::ORIGIN_IMPORT, EDGE_ORIGIN_IDENTITY, "sealed").await;

    if let Ok(user_doc) = db.read("memory.user", "user").await {
        if super::is_graph_soul(&user_doc) {
            if let Some(user_graph) = super::format::parse_sealed(&user_doc) {
                reconcile_graph(db, &user_graph, ORIGIN_USER, EDGE_ORIGIN_USER, "user").await;
            }
        }
    }
}

async fn reconcile_graph(
    db: &WardsonDbClient,
    graph: &IdentityGraph,
    node_origin: &str,
    edge_origin: &str,
    label: &str,
) {
    let now = chrono::Utc::now().to_rfc3339();

    // Nodes: count fast-path, then per-id point reads only on mismatch.
    // The node count compares per-origin-source via the node ids
    // themselves (point reads), not a filtered count — origins share one
    // collection and ids are namespace-disjoint (user_ prefix reserved).
    let mut missing_nodes = 0usize;
    let total = db.count(IDENTITY_COLLECTION).await.unwrap_or(0);
    // Cheap sufficiency check: if the collection holds at least as many
    // docs as this source expects AND a spot read of the first node
    // succeeds, do the exhaustive walk only when the first probe fails.
    let needs_walk = match graph.nodes.first() {
        Some(first) => db.read(IDENTITY_COLLECTION, &first.id).await.is_err(),
        None => false,
    } || (total as usize) < graph.nodes.len();
    if needs_walk {
        for node in &graph.nodes {
            if db.read(IDENTITY_COLLECTION, &node.id).await.is_err() {
                let doc = identity_node_doc(node, node_origin, &now);
                if db.write(IDENTITY_COLLECTION, &doc).await.is_ok() {
                    missing_nodes += 1;
                }
            }
        }
    }

    // Edges: filtered count fast-path, existence probes on mismatch.
    let expected_edges = graph.edges.len() as u64;
    let actual_edges = db
        .count_filtered("memory.edges", &json!({"metadata.origin": edge_origin}))
        .await
        .unwrap_or(0);
    let mut missing_edges = 0usize;
    if actual_edges < expected_edges {
        for edge in &graph.edges {
            let probe = json!({
                "filter": {
                    "source_id": edge.src,
                    "target_id": edge.dst,
                    "edge_type": edge.relation,
                },
                "limit": 1,
            });
            let exists = db
                .query("memory.edges", &probe)
                .await
                .map(|docs| !docs.is_empty())
                .unwrap_or(true); // probe failure: don't double-insert
            if !exists {
                let doc = identity_edge_doc(edge, edge_origin, &now);
                if db.write("memory.edges", &doc).await.is_ok() {
                    missing_edges += 1;
                }
            }
        }
    }

    if missing_nodes > 0 || missing_edges > 0 {
        info!(
            target: "identity_graph",
            "reconcile[{label}]: healed {missing_nodes} nodes, {missing_edges} edges"
        );
    }
}

#[cfg(test)]
mod projection_shape_tests {
    use super::*;
    use crate::identity_graph::format::{GraphEdge, GraphNode};

    #[test]
    fn node_doc_aligns_with_semantic_field_names() {
        let node = GraphNode {
            id: "truth_over_comfort".into(),
            node_type: "value".into(),
            text: "Truth over comfort.".into(),
        };
        let doc = identity_node_doc(&node, "import", "2026-07-24T00:00:00Z");
        assert_eq!(doc["_id"], "truth_over_comfort");
        assert_eq!(doc["content"], "Truth over comfort.");
        assert_eq!(doc["node_type"], "value");
        assert_eq!(doc["origin"], "import");
        // Access-touch compatibility fields present from birth.
        assert_eq!(doc["access_count"], 0);
        assert!(doc["last_accessed"].is_null());
        assert!(doc["tags"].as_array().unwrap().is_empty());
        assert_eq!(doc["created_at"], "2026-07-24T00:00:00Z");
        assert_eq!(doc["updated_at"], "2026-07-24T00:00:00Z");
    }

    #[test]
    fn edge_doc_matches_memory_edges_shape() {
        let edge = GraphEdge {
            src: "embra".into(),
            dst: "voice".into(),
            relation: "has_trait".into(),
        };
        let doc = identity_edge_doc(&edge, "identity_import", "2026-07-24T00:00:00Z");
        assert_eq!(doc["source_id"], "embra");
        assert_eq!(doc["source_collection"], "identity.graph");
        assert_eq!(doc["target_id"], "voice");
        assert_eq!(doc["target_collection"], "identity.graph");
        assert_eq!(doc["edge_type"], "has_trait");
        assert_eq!(doc["weight"], 1.0);
        assert_eq!(doc["metadata"]["origin"], "identity_import");
        assert_eq!(doc["created_at"], "2026-07-24T00:00:00Z");
    }

    #[test]
    fn user_rewrite_doc_shape_is_graph_marked_with_id() {
        let user_graph = crate::identity_graph::transform::user_to_graph(
            &serde_json::json!({"name": "William", "role": "operator"}),
            false,
        )
        .unwrap();
        let mut doc = user_graph.canonicalize("William");
        doc.as_object_mut()
            .unwrap()
            .insert("_id".into(), serde_json::json!("user"));
        assert!(crate::identity_graph::is_graph_soul(&doc));
        assert_eq!(doc["_id"], "user");
        assert_eq!(doc["name"], "William");
    }
}
