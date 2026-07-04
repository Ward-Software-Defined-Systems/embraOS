//! BFS traversal over `memory.edges`.

use anyhow::Result;
use chrono::Utc;
use futures::stream::{self, StreamExt};
use serde_json::json;
use std::collections::HashSet;
use tracing::warn;

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::node_store::{graph_node_from_doc, NodeStore};
use super::types::{EdgeType, KnowledgeEdge, TraversalResult};

/// Bounded fan-out for the per-level arm queries. A module const, not a
/// SystemConfig field — it tunes HTTP pipelining against the local DB, not
/// retrieval semantics.
const HOP_CONCURRENCY: usize = 8;

/// Multi-source, level-synchronous, breadth-first traversal.
///
/// - **Undirected expansion, arm-split hops (2026-07-04):** each hop fetches
///   the edges touching a node via TWO indexed equality queries — the source
///   arm (`source_id` + `source_collection`) and the target arm (`target_id`
///   + `target_collection`) — merged client-side into one ranked window.
///   The old single `$or` filter forced WardSONDB into a full collection
///   scan per hop (its planner cannot index `$or`), which at ~99k edges ×
///   hundreds of hops put 5–8 minute latencies on every retrieval. The
///   arm-split window is EXACTLY the `$or` window (any member of the union's
///   top-K is in its own arm's top-K); only membership at an exact
///   weight/created_at tie on the truncation boundary can differ, and our
///   `_id` tie-break makes the client window deterministic where the
///   server's was scan-order. Reachability semantics are unchanged from the
///   2026-07-03 undirected fix: brain-created edges stored as one directed
///   doc are followed from either endpoint; result edges keep their true
///   stored direction; the visited-check dedupes auto-derived twin docs.
/// - **Multi-source:** `starts` seeds one shared BFS (depth 0 = every seed).
///   With N>1 seeds, edges BETWEEN seeds are not recorded in `edges` (both
///   endpoints are pre-visited) — callers that need a spanning edge set pass
///   a single start, as `knowledge_traverse` does. `nodes` is unaffected.
/// - `max_depth` is clamped to `config.kg_traversal_depth_ceiling`; the node
///   budget (`kg_traversal_node_budget`) is GLOBAL to the call, checked
///   before each node expansion (overshoot bounded by one edge window).
/// - Node docs load through the caller's `NodeStore` (prefetched collections
///   resolve in memory; anything else falls back to a cached point read).
/// - Access tracking moved to the callers (2026-07-04): only RETURNED nodes
///   are touched, via one `spawn_access_touches` task — visiting a node in
///   BFS no longer bumps `access_count`.
pub async fn traverse_multi(
    db: &WardsonDbClient,
    starts: &[(String, String)],
    max_depth: u32,
    edge_type_filter: Option<Vec<EdgeType>>,
    min_weight: Option<f64>,
    config: &SystemConfig,
    store: &mut NodeStore,
) -> Result<TraversalResult> {
    let max_depth = max_depth.min(config.kg_traversal_depth_ceiling);
    let edge_limit = config.kg_traversal_edge_limit;

    let (mut visited, mut level) = seed_level(starts);

    let mut result_nodes = Vec::new();
    let mut result_edges = Vec::new();
    let mut depth_reached: u32 = 0;
    let mut truncated = false;

    // Include the start nodes in the result set for downstream rendering.
    for (coll, id) in &level {
        if let Some(doc) = store.get_or_fetch(db, coll, id).await {
            result_nodes.push(graph_node_from_doc(&doc, coll, id, 0));
        }
    }

    let mut depth = 0u32;
    while !level.is_empty() && depth < max_depth && !truncated {
        // Fetch both arms for every node in this level, bounded fan-out.
        // `buffered` preserves input order, so processing is deterministic.
        let fetches: Vec<_> = stream::iter(level.iter().cloned().map(|(coll, id)| {
            let etf = edge_type_filter.as_deref();
            async move {
                let src_body =
                    edge_query_body(source_arm_filter(&coll, &id, etf, min_weight), edge_limit);
                let tgt_body =
                    edge_query_body(target_arm_filter(&coll, &id, etf, min_weight), edge_limit);
                let (src, tgt) = tokio::join!(
                    db.query("memory.edges", &src_body),
                    db.query("memory.edges", &tgt_body)
                );
                (coll, id, src, tgt)
            }
        }))
        .buffered(HOP_CONCURRENCY)
        .collect()
        .await;

        let mut next_level: Vec<(String, String)> = Vec::new();
        for (coll, id, src_res, tgt_res) in fetches {
            let (src_docs, tgt_docs) = match (src_res, tgt_res) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => {
                    warn!("traversal arm query failed: {}", e);
                    continue;
                }
            };
            // Ranked window (FIX-7): saturation prunes the weakest/oldest
            // edges for this hub. Per locked D3 the escalation on a real
            // saturation is a type-partitioned fetch, NOT raising the cap.
            let (edges, saturated) = merge_arm_edges(src_docs, tgt_docs, edge_limit as usize);
            if saturated {
                warn!(
                    target: "kg::traversal",
                    node_id = %id,
                    collection = %coll,
                    limit = edge_limit,
                    "per-hop edge window saturated — lowest-ranked edges pruned for this hub"
                );
            }

            // Node budget (FIX-7, locked D3): bounds dense-graph BFS cost
            // below the depth ceiling; overshoot within the final expansion
            // is bounded by kg_traversal_edge_limit.
            let (admitted, budget_hit) =
                expand_node_edges(&coll, &id, edges, &mut visited, config.kg_traversal_node_budget);
            if budget_hit {
                warn!(
                    target: "kg::traversal",
                    budget = config.kg_traversal_node_budget,
                    "traversal node budget reached — BFS truncated"
                );
                truncated = true;
                break;
            }

            let next_depth = depth + 1;
            for (edge, (n_coll, n_id)) in admitted {
                if let Some(doc) = store.get_or_fetch(db, &n_coll, &n_id).await {
                    result_nodes.push(graph_node_from_doc(&doc, &n_coll, &n_id, next_depth));
                    result_edges.push(edge);
                    if next_depth > depth_reached {
                        depth_reached = next_depth;
                    }
                    next_level.push((n_coll, n_id));
                }
            }
        }
        level = next_level;
        depth += 1;
    }

    Ok(TraversalResult {
        nodes: result_nodes,
        edges: result_edges,
        depth_reached,
        nodes_visited: visited.len(),
        truncated,
    })
}

/// Dedup starts preserving first-occurrence order; the deduped set seeds the
/// visited set (depth-0 frontier).
fn seed_level(starts: &[(String, String)]) -> (HashSet<(String, String)>, Vec<(String, String)>) {
    let mut visited = HashSet::new();
    let mut level = Vec::new();
    for (coll, id) in starts {
        if visited.insert((coll.clone(), id.clone())) {
            level.push((coll.clone(), id.clone()));
        }
    }
    (visited, level)
}

/// Per-hop edge query body (FIX-7): explicit ranked window. Sort keys are doc
/// fields (`weight`, `created_at`), one per array element, matching the
/// edge-derivation reference pattern in `edges.rs`.
fn edge_query_body(filter: serde_json::Map<String, serde_json::Value>, limit: u32) -> serde_json::Value {
    json!({
        "filter": filter,
        "sort": [{"weight": "desc"}, {"created_at": "desc"}],
        "limit": limit,
    })
}

/// Source arm of the undirected hop: edges LEAVING `(coll, id)`.
///
/// The id+collection pair MUST stay top-level sibling equality keys — that
/// exact shape is what lets WardSONDB's planner ride the `source_id` prefix
/// of `idx_edge_source`/`idx_edge_source_target`. NEVER wrap the arms in a
/// `$or` (or any combinator): WardSONDB plans every `$or` as a full
/// collection scan, which is the 5–8 min production latency this split
/// removed (2026-07-04). Optional type/weight constraints ride as siblings;
/// the planner applies them as a post-filter over the index matches.
fn source_arm_filter(
    coll: &str,
    id: &str,
    edge_type_filter: Option<&[EdgeType]>,
    min_weight: Option<f64>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut filter = serde_json::Map::new();
    filter.insert("source_id".into(), json!(id));
    filter.insert("source_collection".into(), json!(coll));
    append_common_constraints(&mut filter, edge_type_filter, min_weight);
    filter
}

/// Target arm of the undirected hop: edges ARRIVING at `(coll, id)` — the
/// reachability the outgoing-only hop lacked (2026-07-03). Same top-level
/// sibling-equality contract as `source_arm_filter` (rides `idx_edge_target`).
fn target_arm_filter(
    coll: &str,
    id: &str,
    edge_type_filter: Option<&[EdgeType]>,
    min_weight: Option<f64>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut filter = serde_json::Map::new();
    filter.insert("target_id".into(), json!(id));
    filter.insert("target_collection".into(), json!(coll));
    append_common_constraints(&mut filter, edge_type_filter, min_weight);
    filter
}

fn append_common_constraints(
    filter: &mut serde_json::Map<String, serde_json::Value>,
    edge_type_filter: Option<&[EdgeType]>,
    min_weight: Option<f64>,
) {
    if let Some(types) = edge_type_filter {
        let names: Vec<&str> = types.iter().map(|t| t.as_str()).collect();
        filter.insert("edge_type".into(), json!({ "$in": names }));
    }
    if let Some(w) = min_weight {
        filter.insert("weight".into(), json!({ "$gte": w }));
    }
}

/// Merge the two arm windows into one ranked window of `limit` edges.
///
/// Concat → parse → dedupe by `_id` (self-loops are rejected at link time,
/// so the arms are disjoint in practice — the dedupe is a guard) → sort by
/// the server comparator (`weight desc, created_at desc`, RFC3339 strings so
/// lexicographic = chronological, parse-defaulted fields sort last) plus an
/// `_id desc` tie-break the server doesn't have → truncate. Returns the
/// window and whether it saturated (either arm came back full, or the merged
/// unique set overflowed the limit).
fn merge_arm_edges(
    src_docs: Vec<serde_json::Value>,
    tgt_docs: Vec<serde_json::Value>,
    limit: usize,
) -> (Vec<KnowledgeEdge>, bool) {
    let arm_saturated = crate::db::client::window_saturated(src_docs.len(), limit)
        || crate::db::client::window_saturated(tgt_docs.len(), limit);

    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut merged: Vec<KnowledgeEdge> = Vec::new();
    for doc in src_docs.into_iter().chain(tgt_docs) {
        let Some(edge) = parse_edge(&doc) else { continue };
        match &edge._id {
            Some(id) => {
                if seen_ids.insert(id.clone()) {
                    merged.push(edge);
                }
            }
            // An edge without _id can't collide; admit it.
            None => merged.push(edge),
        }
    }
    merged.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| b._id.cmp(&a._id))
    });
    let overflow = merged.len() > limit;
    merged.truncate(limit);
    (merged, arm_saturated || overflow)
}

/// Pure per-node expansion: apply the node budget, then the visited/twin-doc
/// dedupe, to one node's merged edge window. Returns the admitted
/// `(edge, neighbor)` pairs in window rank order, and whether the budget
/// stopped this expansion before it started.
fn expand_node_edges(
    coll: &str,
    id: &str,
    edges: Vec<KnowledgeEdge>,
    visited: &mut HashSet<(String, String)>,
    node_budget: u32,
) -> (Vec<(KnowledgeEdge, (String, String))>, bool) {
    if visited.len() as u32 >= node_budget {
        return (Vec::new(), true);
    }
    let mut admitted = Vec::new();
    for edge in edges {
        // Neighbor = whichever endpoint is NOT the node being expanded (for
        // an incoming edge that's the source). The visited check on the
        // neighbor also dedupes the twin docs of auto-derived bidirectional
        // edges — the second doc resolves to an already-visited neighbor and
        // is skipped, so the edge set stays a spanning set, as before.
        let (n_coll, n_id) = neighbor_of(&edge, coll, id);
        let key = (n_coll.to_string(), n_id.to_string());
        if visited.contains(&key) {
            continue;
        }
        visited.insert(key.clone());
        admitted.push((edge, key));
    }
    (admitted, false)
}

/// The endpoint of `edge` that is NOT the node currently being expanded.
/// Self-loops are rejected at link time; if neither endpoint matches
/// (unreachable given the arm filters) the target arm wins and the
/// visited check makes the choice harmless.
fn neighbor_of<'a>(edge: &'a KnowledgeEdge, coll: &str, id: &str) -> (&'a str, &'a str) {
    if edge.source_collection == coll && edge.source_id == id {
        (&edge.target_collection, &edge.target_id)
    } else {
        (&edge.source_collection, &edge.source_id)
    }
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

/// Best-effort access tracking for RETURNED nodes (2026-07-04): one
/// background task, sequential read→patch per key. BFS-visited-but-not-
/// returned nodes are no longer touched — `access_count` measures retrieval
/// hits, not sweep wavefronts, and a worst-case query stops spawning
/// thousands of concurrent PATCH tasks against the DB.
pub(crate) fn spawn_access_touches(db: WardsonDbClient, keys: Vec<(String, String)>) {
    if keys.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for (collection, id) in keys {
            // Non-atomic: read → increment → patch (fresh-read semantics).
            let Ok(doc) = db.read(&collection, &id).await else { continue };
            let current = doc.get("access_count").and_then(|v| v.as_u64()).unwrap_or(0);
            let patch = json!({
                "access_count": current + 1,
                "last_accessed": Utc::now().to_rfc3339(),
            });
            let _ = db.patch_document(&collection, &id, &patch).await;
        }
    });
}

#[cfg(test)]
mod edge_query_body_tests {
    //! Arm-split hop guards (no DB mock in this crate — the indexed-shape,
    //! ranked-window, and both-endpoint contracts are enforced at the
    //! builder/pure-fn level).
    use super::super::types::{EdgeType, KnowledgeEdge};
    use super::{
        edge_query_body, expand_node_edges, merge_arm_edges, neighbor_of, seed_level,
        source_arm_filter, target_arm_filter,
    };
    use serde_json::json;
    use std::collections::HashSet;

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
    fn source_arm_is_top_level_eq_pair_with_sibling_constraints() {
        // The planner contract: id+collection as TOP-LEVEL sibling equality
        // keys (rides the source_id index prefix); type/weight as siblings.
        let f = source_arm_filter(
            "memory.semantic",
            "node-1",
            Some(&[EdgeType::Enables, EdgeType::DependsOn]),
            Some(0.7),
        );
        assert_eq!(f["source_id"], json!("node-1"));
        assert_eq!(f["source_collection"], json!("memory.semantic"));
        assert_eq!(f["edge_type"], json!({ "$in": ["enables", "depends_on"] }));
        assert_eq!(f["weight"], json!({ "$gte": 0.7 }));
        assert_eq!(f.len(), 4);
    }

    #[test]
    fn target_arm_is_top_level_eq_pair_with_sibling_constraints() {
        let f = target_arm_filter(
            "memory.semantic",
            "node-1",
            Some(&[EdgeType::SameSession]),
            Some(0.5),
        );
        assert_eq!(f["target_id"], json!("node-1"));
        assert_eq!(f["target_collection"], json!("memory.semantic"));
        assert_eq!(f["edge_type"], json!({ "$in": ["same_session"] }));
        assert_eq!(f["weight"], json!({ "$gte": 0.5 }));
        assert_eq!(f.len(), 4);
    }

    #[test]
    fn arm_filters_omit_type_and_weight_when_absent() {
        let src = source_arm_filter("memory.entries", "e-1", None, None);
        assert_eq!(src.len(), 2, "only the eq pair expected: {src:?}");
        let tgt = target_arm_filter("memory.entries", "e-1", None, None);
        assert_eq!(tgt.len(), 2, "only the eq pair expected: {tgt:?}");
    }

    #[test]
    fn hot_path_arm_bodies_never_contain_or() {
        // THE guard for the 2026-07-04 fix: WardSONDB plans every `$or` as a
        // full collection scan, so the traversal hot path must never emit
        // one. If this fails, retrieval latency regresses ~300x.
        for f in [
            source_arm_filter("memory.semantic", "n", Some(&[EdgeType::Refines]), Some(0.1)),
            target_arm_filter("memory.semantic", "n", Some(&[EdgeType::Refines]), Some(0.1)),
        ] {
            let body = edge_query_body(f, 500);
            let s = serde_json::to_string(&body).unwrap();
            assert!(!s.contains("\"$or\""), "hot-path body contains $or: {s}");
        }
    }

    #[test]
    fn arm_bodies_share_sort_and_limit() {
        let src = edge_query_body(source_arm_filter("c", "n", None, None), 500);
        let tgt = edge_query_body(target_arm_filter("c", "n", None, None), 500);
        assert_eq!(src["sort"], tgt["sort"]);
        assert_eq!(src["sort"], json!([{"weight": "desc"}, {"created_at": "desc"}]));
        assert_eq!(src["limit"], tgt["limit"]);
    }

    fn edge_doc(id: &str, weight: f64, created: &str) -> serde_json::Value {
        json!({
            "_id": id,
            "source_id": "a", "source_collection": "memory.semantic",
            "target_id": "b", "target_collection": "memory.semantic",
            "edge_type": "enables", "weight": weight, "created_at": created,
        })
    }

    #[test]
    fn merge_arms_dedupes_by_id_and_truncates_to_limit() {
        let src = vec![edge_doc("e1", 0.9, "2026-07-01T00:00:00Z"), edge_doc("e2", 0.8, "2026-07-02T00:00:00Z")];
        let tgt = vec![edge_doc("e1", 0.9, "2026-07-01T00:00:00Z"), edge_doc("e3", 0.7, "2026-07-03T00:00:00Z")];
        let (merged, saturated) = merge_arm_edges(src, tgt, 2);
        let ids: Vec<_> = merged.iter().map(|e| e._id.as_deref().unwrap()).collect();
        assert_eq!(ids, vec!["e1", "e2"], "deduped, ranked, truncated");
        assert!(saturated, "3 unique edges into a 2-window = overflow");
    }

    #[test]
    fn merge_arms_orders_weight_then_created_then_id() {
        let src = vec![
            edge_doc("b", 0.5, "2026-07-01T00:00:00Z"),
            edge_doc("a", 0.5, "2026-07-01T00:00:00Z"), // exact tie -> _id desc
        ];
        let tgt = vec![
            edge_doc("c", 0.5, "2026-07-02T00:00:00Z"), // same weight, newer
            edge_doc("d", 0.9, "2026-01-01T00:00:00Z"), // heaviest wins outright
        ];
        let (merged, saturated) = merge_arm_edges(src, tgt, 10);
        let ids: Vec<_> = merged.iter().map(|e| e._id.as_deref().unwrap()).collect();
        assert_eq!(ids, vec!["d", "c", "b", "a"]);
        assert!(!saturated);
    }

    #[test]
    fn merge_arms_flags_saturation_on_full_arm_or_overflow() {
        // A full arm means the server pruned that side's tail.
        let src = vec![edge_doc("e1", 0.9, "2026-07-01T00:00:00Z")];
        let (_, saturated) = merge_arm_edges(src, Vec::new(), 1);
        assert!(saturated, "arm returned == limit must flag saturation");
        let (_, quiet) = merge_arm_edges(
            vec![edge_doc("e1", 0.9, "2026-07-01T00:00:00Z")],
            Vec::new(),
            5,
        );
        assert!(!quiet);
    }

    fn kedge(id: &str, src: &str, tgt: &str) -> KnowledgeEdge {
        KnowledgeEdge {
            _id: Some(id.into()),
            source_id: src.into(),
            source_collection: "memory.semantic".into(),
            target_id: tgt.into(),
            target_collection: "memory.semantic".into(),
            edge_type: EdgeType::Enables,
            weight: 0.9,
            metadata: serde_json::Value::Null,
            created_at: "2026-07-03T00:00:00Z".into(),
        }
    }

    #[test]
    fn expand_skips_visited_and_dedupes_auto_twin_docs() {
        let mut visited: HashSet<(String, String)> =
            [("memory.semantic".to_string(), "n".to_string())].into();
        // Twin docs of one auto link: n->x and x->n both resolve neighbor x.
        let edges = vec![kedge("e1", "n", "x"), kedge("e2", "x", "n")];
        let (admitted, hit) = expand_node_edges("memory.semantic", "n", edges, &mut visited, 100);
        assert!(!hit);
        assert_eq!(admitted.len(), 1, "twin doc must dedupe on the visited check");
        assert_eq!(admitted[0].1, ("memory.semantic".to_string(), "x".to_string()));
    }

    #[test]
    fn expand_stops_at_node_budget_and_reports_truncated() {
        let mut visited: HashSet<(String, String)> = (0..3)
            .map(|i| ("memory.semantic".to_string(), format!("v{i}")))
            .collect();
        let edges = vec![kedge("e1", "n", "x")];
        let (admitted, hit) = expand_node_edges("memory.semantic", "n", edges, &mut visited, 3);
        assert!(hit, "at/over budget the expansion must not start");
        assert!(admitted.is_empty());
    }

    #[test]
    fn dedup_starts_preserves_first_occurrence_order() {
        let starts = vec![
            ("memory.semantic".to_string(), "a".to_string()),
            ("memory.entries".to_string(), "a".to_string()),
            ("memory.semantic".to_string(), "a".to_string()),
            ("memory.semantic".to_string(), "b".to_string()),
        ];
        let (visited, level) = seed_level(&starts);
        assert_eq!(level.len(), 3, "exact dupes collapse, cross-collection ids don't");
        assert_eq!(level[0].1, "a");
        assert_eq!(level[1].0, "memory.entries");
        assert_eq!(level[2].1, "b");
        assert_eq!(visited.len(), 3);
    }

    #[test]
    fn neighbor_of_outgoing_picks_target() {
        let e = kedge("e", "a", "b");
        let mut e = e;
        e.target_collection = "memory.procedural".into();
        assert_eq!(
            neighbor_of(&e, "memory.semantic", "a"),
            ("memory.procedural", "b")
        );
    }

    #[test]
    fn neighbor_of_incoming_picks_source() {
        // Standing on the TARGET of a directed edge, the neighbor is the
        // source — this is the reachability the outgoing-only hop lacked.
        let mut e = kedge("e", "a", "b");
        e.target_collection = "memory.procedural".into();
        assert_eq!(
            neighbor_of(&e, "memory.procedural", "b"),
            ("memory.semantic", "a")
        );
        // Same id in a different collection is NOT the same node.
        let mut e2 = kedge("e2", "a", "a");
        e2.target_collection = "memory.procedural".into();
        assert_eq!(
            neighbor_of(&e2, "memory.procedural", "a"),
            ("memory.semantic", "a")
        );
    }
}
