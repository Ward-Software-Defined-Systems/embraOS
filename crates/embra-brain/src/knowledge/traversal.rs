//! BFS traversal over `memory.edges`.

use anyhow::Result;
use chrono::Utc;
use serde_json::json;
use std::collections::{HashSet, VecDeque};
use tracing::warn;

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::types::{
    content_preview, EdgeType, GraphNode, KnowledgeEdge, NodeType, SemanticCategory, TraversalResult,
};

/// Breadth-first traversal starting from a single node.
///
/// - `max_depth` is clamped to `config.kg_traversal_depth_ceiling`.
/// - Increments `access_count` + sets `last_accessed` on each visited node
///   via fire-and-forget PATCH.
pub async fn traverse(
    db: &WardsonDbClient,
    start_id: &str,
    start_collection: &str,
    max_depth: u32,
    edge_type_filter: Option<Vec<EdgeType>>,
    min_weight: Option<f64>,
    config: &SystemConfig,
) -> Result<TraversalResult> {
    let max_depth = max_depth.min(config.kg_traversal_depth_ceiling);

    let mut visited: HashSet<(String, String)> = HashSet::new();
    visited.insert((start_collection.to_string(), start_id.to_string()));

    let mut queue: VecDeque<(String, String, u32)> = VecDeque::new();
    queue.push_back((start_collection.to_string(), start_id.to_string(), 0));

    let mut result_nodes: Vec<GraphNode> = Vec::new();
    let mut result_edges: Vec<KnowledgeEdge> = Vec::new();
    let mut depth_reached: u32 = 0;
    let mut truncated = false;

    // Include the start node in result set for downstream rendering.
    if let Some(start_node) = load_graph_node(db, start_collection, start_id, 0).await {
        result_nodes.push(start_node);
        spawn_access_touch(db.clone(), start_collection.to_string(), start_id.to_string());
    }

    while let Some((coll, id, depth)) = queue.pop_front() {
        // Node budget (FIX-7, locked D3): bounds dense-graph BFS cost below
        // the depth ceiling. Overshoot within the final hop is bounded by
        // kg_traversal_edge_limit.
        if visited.len() as u32 >= config.kg_traversal_node_budget {
            warn!(
                target: "kg::traversal",
                budget = config.kg_traversal_node_budget,
                "traversal node budget reached — BFS truncated"
            );
            truncated = true;
            break;
        }
        if depth >= max_depth { continue; }

        // Build edge filter
        let mut filter = serde_json::Map::new();
        filter.insert("source_id".into(), json!(id));
        filter.insert("source_collection".into(), json!(coll));
        if let Some(types) = &edge_type_filter {
            let names: Vec<&str> = types.iter().map(|t| t.as_str()).collect();
            filter.insert("edge_type".into(), json!({ "$in": names }));
        }
        if let Some(w) = min_weight {
            filter.insert("weight".into(), json!({ "$gte": w }));
        }

        let edges_docs = match db
            .query("memory.edges", &edge_query_body(filter, config.kg_traversal_edge_limit))
            .await
        {
            Ok(docs) => docs,
            Err(e) => {
                warn!("traversal edge query failed: {}", e);
                continue;
            }
        };
        // Ranked window (FIX-7): saturation prunes the weakest/oldest edges
        // for this hub. Per locked D3 the escalation on a real saturation is
        // a type-partitioned fetch, NOT raising the cap.
        if crate::db::client::window_saturated(edges_docs.len(), config.kg_traversal_edge_limit as usize) {
            warn!(
                target: "kg::traversal",
                node_id = %id,
                collection = %coll,
                limit = config.kg_traversal_edge_limit,
                "per-hop edge window saturated — lowest-ranked edges pruned for this hub"
            );
        }

        for edge_val in edges_docs {
            let Some(edge) = parse_edge(&edge_val) else { continue; };
            let target_key = (edge.target_collection.clone(), edge.target_id.clone());
            if visited.contains(&target_key) { continue; }
            visited.insert(target_key.clone());

            let next_depth = depth + 1;

            if let Some(node) = load_graph_node(db, &edge.target_collection, &edge.target_id, next_depth).await {
                result_edges.push(edge.clone());
                result_nodes.push(node);
                spawn_access_touch(
                    db.clone(),
                    edge.target_collection.clone(),
                    edge.target_id.clone(),
                );
                if next_depth > depth_reached { depth_reached = next_depth; }
                queue.push_back((edge.target_collection, edge.target_id, next_depth));
            }
        }
    }

    Ok(TraversalResult {
        nodes: result_nodes,
        edges: result_edges,
        depth_reached,
        nodes_visited: visited.len(),
        truncated,
    })
}

/// Per-hop edge query (FIX-7): explicit ranked window instead of the old
/// unsorted `limit: 200` (which returned edges in key order — creation
/// order — silently dropping the newest edges of any hub past the limit).
/// Sort keys are doc fields (`weight`, `created_at`), one per array element,
/// matching the edge-derivation reference pattern in `edges.rs`.
fn edge_query_body(filter: serde_json::Map<String, serde_json::Value>, limit: u32) -> serde_json::Value {
    json!({
        "filter": filter,
        "sort": [{"weight": "desc"}, {"created_at": "desc"}],
        "limit": limit,
    })
}

fn parse_edge(v: &serde_json::Value) -> Option<KnowledgeEdge> {
    let edge_type = EdgeType::from_str(v.get("edge_type")?.as_str()?)?;
    Some(KnowledgeEdge {
        _id: v.get("_id").and_then(|x| x.as_str()).map(|s| s.to_string()),
        source_id: v.get("source_id")?.as_str()?.to_string(),
        source_collection: v.get("source_collection")?.as_str()?.to_string(),
        target_id: v.get("target_id")?.as_str()?.to_string(),
        target_collection: v.get("target_collection")?.as_str()?.to_string(),
        edge_type,
        weight: v.get("weight").and_then(|x| x.as_f64()).unwrap_or(0.0),
        metadata: v.get("metadata").cloned().unwrap_or(serde_json::Value::Null),
        created_at: v.get("created_at").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    })
}

async fn load_graph_node(
    db: &WardsonDbClient,
    collection: &str,
    id: &str,
    depth: u32,
) -> Option<GraphNode> {
    let doc = db.read(collection, id).await.ok()?;
    let (preview_source, node_type) = match collection {
        "memory.entries" => (
            doc.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            NodeType::Episodic,
        ),
        "memory.semantic" => {
            let category = doc.get("category").and_then(|v| v.as_str())
                .and_then(SemanticCategory::from_str)
                .unwrap_or(SemanticCategory::Fact);
            (
                doc.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                NodeType::Semantic { category },
            )
        }
        "memory.procedural" => {
            let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let desc = doc.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
            (desc, NodeType::Procedural { title })
        }
        _ => (String::new(), NodeType::Episodic),
    };
    Some(GraphNode {
        id: id.to_string(),
        collection: collection.to_string(),
        content_preview: content_preview(&preview_source, 200),
        node_type,
        depth,
    })
}

fn spawn_access_touch(db: WardsonDbClient, collection: String, id: String) {
    tokio::spawn(async move {
        // Non-atomic: read → increment → patch.
        let doc = match db.read(&collection, &id).await {
            Ok(d) => d,
            Err(_) => return,
        };
        let current = doc.get("access_count").and_then(|v| v.as_u64()).unwrap_or(0);
        let patch = json!({
            "access_count": current + 1,
            "last_accessed": Utc::now().to_rfc3339(),
        });
        let _ = db.patch_document(&collection, &id, &patch).await;
    });
}

#[cfg(test)]
mod edge_query_body_tests {
    //! FIX-7 body-shape guards (no DB mock in this crate — the ranked-window
    //! contract is enforced at the builder level).
    use super::edge_query_body;
    use serde_json::json;

    fn sample_filter() -> serde_json::Map<String, serde_json::Value> {
        let mut filter = serde_json::Map::new();
        filter.insert("source_id".into(), json!("node-1"));
        filter.insert("source_collection".into(), json!("memory.semantic"));
        filter.insert("edge_type".into(), json!({ "$in": ["same_session"] }));
        filter.insert("weight".into(), json!({ "$gte": 0.5 }));
        filter
    }

    #[test]
    fn edge_body_ranked_weight_then_recency() {
        let body = edge_query_body(serde_json::Map::new(), 500);
        assert_eq!(
            body["sort"],
            json!([{"weight": "desc"}, {"created_at": "desc"}])
        );
    }

    #[test]
    fn edge_body_limit_from_config_value() {
        let body = edge_query_body(serde_json::Map::new(), 750);
        assert_eq!(body["limit"], json!(750));
    }

    #[test]
    fn edge_body_preserves_filter_keys() {
        let body = edge_query_body(sample_filter(), 500);
        assert_eq!(body["filter"]["source_id"], json!("node-1"));
        assert_eq!(body["filter"]["source_collection"], json!("memory.semantic"));
        assert_eq!(body["filter"]["edge_type"], json!({ "$in": ["same_session"] }));
        assert_eq!(body["filter"]["weight"], json!({ "$gte": 0.5 }));
    }
}
