//! BFS traversal over `memory.edges`.

use anyhow::Result;
use chrono::Utc;
use futures::stream::{self, StreamExt};
use serde_json::json;
use std::collections::HashSet;
use tracing::{debug, warn};

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::node_store::{graph_node_from_doc, NodeStore};
use super::types::{EdgeType, KnowledgeEdge, TraversalResult};

/// Bounded fan-out for the per-level arm queries. A module const, not a
/// SystemConfig field — it tunes HTTP pipelining against the local DB, not
/// retrieval semantics.
const HOP_CONCURRENCY: usize = 8;

/// Wire names of the auto-derived partition — kept in lockstep with
/// `EdgeType::is_auto_derived` (pinned by
/// `auto_edge_type_names_match_is_auto_derived`).
pub(crate) const AUTO_EDGE_TYPE_NAMES: [&str; 3] = ["same_session", "temporal", "tag_overlap"];

/// Meaningful-partition window (locked D3 escalation, landed 2026-07-31):
/// the non-auto types — brain-created, `derived_from`, free-form identity
/// relations — ride their own per-hop window so weight-1.0 `same_session`
/// floods can never prune them. Module const, NOT SystemConfig: 2000 is
/// more than 2× ALL meaningful edge docs in the production graph (856), so
/// saturation here is real signal (warn) rather than working-as-designed
/// pruning (the auto window's debug).
const MEANINGFUL_EDGE_LIMIT: u32 = 2000;

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
/// - **Type-partitioned hop (locked D3 escalation, landed 2026-07-31):**
///   each arm pair is fetched TWICE — the auto partition (`edge_type $in`
///   the three write-time types) under the ranked `kg_traversal_edge_limit`
///   window, and the meaningful partition (everything else: brain-created,
///   `derived_from`, free-form identity relations) under its own
///   `MEANINGFUL_EDGE_LIMIT` window — so `same_session` floods can never
///   prune the globally-rare meaningful edges at a dense hub. Partitions
///   concat MEANINGFUL-FIRST (disjoint type sets, no `_id` overlap), so the
///   visited-check records meaningful witness edges in preference to auto
///   twins. A caller-supplied `edge_type_filter` is split across the
///   partitions (`partition_edge_types`) — explicit lists always ride `$in`.
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

    // Partition the caller's type filter once — loop-invariant.
    let (auto_types, meaningful_types) = partition_edge_types(edge_type_filter.as_deref());

    let mut depth = 0u32;
    while !level.is_empty() && depth < max_depth && !truncated {
        // Fetch the partitioned arms for every node in this level, bounded
        // fan-out. `buffered` preserves input order, so processing is
        // deterministic. A skipped partition costs nothing (run_arm None).
        // Take ownership of the level (it is rebuilt as next_level below) —
        // avoids cloning every key into the fetch closures.
        let fetches: Vec<_> = stream::iter(std::mem::take(&mut level).into_iter().map(|(coll, id)| {
            let auto = auto_types.as_deref();
            let meaningful = &meaningful_types;
            async move {
                let auto_bodies = auto.map(|types| {
                    (
                        edge_query_body(
                            source_arm_filter(&coll, &id, Some(types), min_weight),
                            edge_limit,
                        ),
                        edge_query_body(
                            target_arm_filter(&coll, &id, Some(types), min_weight),
                            edge_limit,
                        ),
                    )
                });
                let mean_bodies = match meaningful {
                    MeaningfulTypes::NinAuto => Some((
                        edge_query_body(
                            source_arm_filter_nin_auto(&coll, &id, min_weight),
                            MEANINGFUL_EDGE_LIMIT,
                        ),
                        edge_query_body(
                            target_arm_filter_nin_auto(&coll, &id, min_weight),
                            MEANINGFUL_EDGE_LIMIT,
                        ),
                    )),
                    MeaningfulTypes::In(types) => Some((
                        edge_query_body(
                            source_arm_filter(&coll, &id, Some(types.as_slice()), min_weight),
                            MEANINGFUL_EDGE_LIMIT,
                        ),
                        edge_query_body(
                            target_arm_filter(&coll, &id, Some(types.as_slice()), min_weight),
                            MEANINGFUL_EDGE_LIMIT,
                        ),
                    )),
                    MeaningfulTypes::Skip => None,
                };
                let (a_src, a_tgt, m_src, m_tgt) = tokio::join!(
                    run_arm(db, auto_bodies.as_ref().map(|(s, _)| s)),
                    run_arm(db, auto_bodies.as_ref().map(|(_, t)| t)),
                    run_arm(db, mean_bodies.as_ref().map(|(s, _)| s)),
                    run_arm(db, mean_bodies.as_ref().map(|(_, t)| t)),
                );
                (coll, id, a_src, a_tgt, m_src, m_tgt)
            }
        }))
        .buffered(HOP_CONCURRENCY)
        .collect()
        .await;

        let mut next_level: Vec<(String, String)> = Vec::new();
        for (coll, id, a_src, a_tgt, m_src, m_tgt) in fetches {
            let (a_src, a_tgt, m_src, m_tgt) = match (a_src, a_tgt, m_src, m_tgt) {
                (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
                (Err(e), _, _, _)
                | (_, Err(e), _, _)
                | (_, _, Err(e), _)
                | (_, _, _, Err(e)) => {
                    warn!("traversal arm query failed: {}", e);
                    continue;
                }
            };
            // Locked D3 escalation LANDED (2026-07-31): the meaningful
            // partition rides its own window, so meaningful edges can no
            // longer be pruned by weight-1.0 auto floods. Saturation here
            // should not happen at current graph scale — real signal.
            let (mean_edges, mean_saturated) =
                merge_arm_edges(m_src, m_tgt, MEANINGFUL_EDGE_LIMIT as usize);
            if mean_saturated {
                warn!(
                    target: "kg::traversal",
                    node_id = %id,
                    collection = %coll,
                    limit = MEANINGFUL_EDGE_LIMIT,
                    "meaningful-edge window saturated — meaningful edges pruned for this hub; unexpected at current graph scale, investigate"
                );
            }
            // Auto-partition saturation is working-as-designed ranked
            // pruning of structural noise (dense hubs saturate on nearly
            // every hop) — debug since the partition landed, not warn.
            let (auto_edges, auto_saturated) =
                merge_arm_edges(a_src, a_tgt, edge_limit as usize);
            if auto_saturated {
                debug!(
                    target: "kg::traversal",
                    node_id = %id,
                    collection = %coll,
                    limit = edge_limit,
                    "auto-edge window saturated (expected on dense hubs) — lowest-ranked auto edges pruned; meaningful edges ride their own window"
                );
            }
            // Concat MEANINGFUL-FIRST: type sets are disjoint (no `_id`
            // overlap), and expand_node_edges keeps the FIRST edge doc that
            // reaches a neighbor — result edges prefer meaningful witnesses
            // over same_session twins.
            let edges: Vec<KnowledgeEdge> =
                mean_edges.into_iter().chain(auto_edges).collect();

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
pub(crate) fn edge_query_body(filter: serde_json::Map<String, serde_json::Value>, limit: u32) -> serde_json::Value {
    json!({
        "filter": filter,
        "sort": [{"weight": "desc"}, {"created_at": "desc"}],
        "limit": limit,
    })
}

/// Source arm of the undirected hop: edges LEAVING `(coll, id)`.
///
/// The id+collection pair MUST stay top-level sibling equality keys — that
/// exact shape is what lets WardSONDB's planner serve the arm from the
/// single-field `idx_edge_source_id` (boot-ensured; post-F2 planners refuse
/// to serve single-field lookups from compound indexes, while pre-F2 builds
/// ride a `source_id`-leading compound's prefix instead). NEVER wrap the
/// arms in a `$or` (or any combinator): WardSONDB plans every `$or` as a
/// full collection scan, which is the 5–8 min production latency this split
/// removed (2026-07-04). Optional type/weight constraints ride as siblings;
/// the planner applies them as a post-filter over the index matches.
pub(crate) fn source_arm_filter(
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
pub(crate) fn target_arm_filter(
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

/// Meaningful-partition type selection for one traversal call.
pub(crate) enum MeaningfulTypes {
    /// No caller filter: everything except the auto types (`$nin`).
    NinAuto,
    /// Caller filter minus the auto types — explicit `$in`; NEVER `$nin`
    /// when the caller named types.
    In(Vec<EdgeType>),
    /// The caller's filter contained no meaningful types — skip the
    /// partition entirely.
    Skip,
}

/// Split a caller's edge-type filter across the two hop partitions.
/// `None` → auto `$in` ALL-AUTO + meaningful `$nin` AUTO. `Some(list)` →
/// auto = list ∩ AUTO (`$in`, `None` when empty), meaningful = list − AUTO
/// (explicit `$in`, `Skip` when empty). Splits on `is_auto_derived`, so
/// free-form `Other` relations always route meaningful.
pub(crate) fn partition_edge_types(
    filter: Option<&[EdgeType]>,
) -> (Option<Vec<EdgeType>>, MeaningfulTypes) {
    match filter {
        None => (
            Some(vec![EdgeType::SameSession, EdgeType::Temporal, EdgeType::TagOverlap]),
            MeaningfulTypes::NinAuto,
        ),
        Some(list) => {
            let auto: Vec<EdgeType> =
                list.iter().filter(|t| t.is_auto_derived()).cloned().collect();
            let meaningful: Vec<EdgeType> =
                list.iter().filter(|t| !t.is_auto_derived()).cloned().collect();
            (
                (!auto.is_empty()).then_some(auto),
                if meaningful.is_empty() {
                    MeaningfulTypes::Skip
                } else {
                    MeaningfulTypes::In(meaningful)
                },
            )
        }
    }
}

/// Meaningful source arm (locked D3 partition): the SAME top-level eq pair
/// — the planner serves it from `idx_edge_source_id` with every sibling as
/// post-filter — plus `edge_type $nin` the auto types. `$nin` is parsed by
/// WardSONDB but never index-served, so it always rides as a post-filter
/// over this arm's index bucket, and `limit` applies AFTER post-filtering:
/// the window is a true matched top-K of meaningful edges. Every edge doc
/// carries `edge_type`, so the server's missing-field-never-matches `$nin`
/// quirk cannot bite here. NEVER wrap arms in `$or` (full scan — module
/// doc).
pub(crate) fn source_arm_filter_nin_auto(
    coll: &str,
    id: &str,
    min_weight: Option<f64>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut filter = serde_json::Map::new();
    filter.insert("source_id".into(), json!(id));
    filter.insert("source_collection".into(), json!(coll));
    filter.insert("edge_type".into(), json!({ "$nin": AUTO_EDGE_TYPE_NAMES }));
    if let Some(w) = min_weight {
        filter.insert("weight".into(), json!({ "$gte": w }));
    }
    filter
}

/// Target-arm twin of `source_arm_filter_nin_auto` (rides `idx_edge_target`).
pub(crate) fn target_arm_filter_nin_auto(
    coll: &str,
    id: &str,
    min_weight: Option<f64>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut filter = serde_json::Map::new();
    filter.insert("target_id".into(), json!(id));
    filter.insert("target_collection".into(), json!(coll));
    filter.insert("edge_type".into(), json!({ "$nin": AUTO_EDGE_TYPE_NAMES }));
    if let Some(w) = min_weight {
        filter.insert("weight".into(), json!({ "$gte": w }));
    }
    filter
}

/// One optional arm query — `None` (a skipped partition) is an empty `Ok`,
/// so the 4-way join stays shape-stable.
async fn run_arm(
    db: &WardsonDbClient,
    body: Option<&serde_json::Value>,
) -> Result<Vec<serde_json::Value>> {
    match body {
        Some(b) => db.query("memory.edges", b).await,
        None => Ok(Vec::new()),
    }
}

/// Merge the two arm windows into one ranked window of `limit` edges.
///
/// Concat → parse → dedupe by `_id` (self-loops are rejected at link time,
/// so the arms are disjoint in practice — the dedupe is a guard) → sort by
/// the server comparator (`weight desc, created_at desc`, RFC3339 strings so
/// lexicographic = chronological, parse-defaulted fields sort last) plus an
/// `_id desc` tie-break → truncate. Post-F2 servers tie-break `_id` in the
/// last sort field's direction (desc here), so the merge and a single-window
/// server sort agree; older servers had no tie-break at all. Returns the
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

pub(crate) fn parse_edge(v: &serde_json::Value) -> Option<KnowledgeEdge> {
    // parse_lossy, not from_str: identity-graph projections store free-form
    // per-intelligence relations, which must traverse — the old strict
    // parse silently DROPPED any unknown edge_type from every walk.
    let edge_type = EdgeType::parse_lossy(v.get("edge_type")?.as_str()?)?;
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
        edge_query_body, expand_node_edges, merge_arm_edges, neighbor_of, parse_edge,
        partition_edge_types, seed_level, source_arm_filter, source_arm_filter_nin_auto,
        target_arm_filter, target_arm_filter_nin_auto, MeaningfulTypes, AUTO_EDGE_TYPE_NAMES,
        MEANINGFUL_EDGE_LIMIT,
    };
    use serde_json::json;
    use std::collections::HashSet;

    #[test]
    fn free_form_edge_types_survive_parse_and_merge() {
        // The kg-native-identity gate-open: before parse_lossy, an unknown
        // edge_type made parse_edge return None and the edge silently
        // vanished from every traversal window.
        let doc = json!({
            "_id": "e1",
            "source_id": "embra",
            "source_collection": "identity.graph",
            "target_id": "voice",
            "target_collection": "identity.graph",
            "edge_type": "has_trait",
            "weight": 1.0,
            "metadata": {"origin": "identity_import"},
            "created_at": "2026-07-24T00:00:00Z"
        });
        let edge = parse_edge(&doc).expect("free-form relation parses");
        assert_eq!(edge.edge_type, EdgeType::Other("has_trait".to_string()));
        assert_eq!(edge.edge_type.as_str(), "has_trait");

        let (merged, saturated) = merge_arm_edges(vec![doc], vec![], 500);
        assert_eq!(merged.len(), 1, "Other edge admitted through the window");
        assert!(!saturated);

        // Empty edge_type is still dropped (degenerate doc).
        let bad = json!({
            "source_id": "a", "source_collection": "c",
            "target_id": "b", "target_collection": "c",
            "edge_type": "", "weight": 1.0, "created_at": ""
        });
        assert!(parse_edge(&bad).is_none());
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

    #[test]
    fn nin_arm_filters_are_eq_pair_plus_exact_auto_nin() {
        let src = source_arm_filter_nin_auto("memory.semantic", "n", Some(0.1));
        assert_eq!(src["source_id"], json!("n"));
        assert_eq!(src["source_collection"], json!("memory.semantic"));
        assert_eq!(src["edge_type"], json!({"$nin": ["same_session", "temporal", "tag_overlap"]}));
        assert_eq!(src["weight"], json!({"$gte": 0.1}));
        assert_eq!(src.len(), 4);

        let tgt = target_arm_filter_nin_auto("memory.semantic", "n", None);
        assert_eq!(tgt["target_id"], json!("n"));
        assert_eq!(tgt["target_collection"], json!("memory.semantic"));
        assert_eq!(tgt["edge_type"], json!({"$nin": ["same_session", "temporal", "tag_overlap"]}));
        assert_eq!(tgt.len(), 3, "weight omitted when absent: {tgt:?}");
    }

    #[test]
    fn nin_bodies_never_contain_or_and_pin_the_meaningful_window() {
        assert_eq!(MEANINGFUL_EDGE_LIMIT, 2000);
        for f in [
            source_arm_filter_nin_auto("memory.semantic", "n", Some(0.1)),
            target_arm_filter_nin_auto("memory.semantic", "n", Some(0.1)),
        ] {
            let body = edge_query_body(f, MEANINGFUL_EDGE_LIMIT);
            let s = serde_json::to_string(&body).unwrap();
            assert!(!s.contains("\"$or\""), "hot-path body contains $or: {s}");
            assert_eq!(body["sort"], json!([{"weight": "desc"}, {"created_at": "desc"}]));
            assert_eq!(body["limit"], json!(2000));
        }
    }

    #[test]
    fn auto_edge_type_names_match_is_auto_derived() {
        // Const/method lockstep: the $in/$nin wire list and the enum
        // predicate must describe the same three types.
        for name in AUTO_EDGE_TYPE_NAMES {
            let t = EdgeType::parse_lossy(name).expect("auto name parses");
            assert!(t.is_auto_derived(), "{name} must be auto-derived");
            assert!(!matches!(t, EdgeType::Other(_)), "{name} must be a built-in");
        }
        let autos = [EdgeType::SameSession, EdgeType::Temporal, EdgeType::TagOverlap];
        assert_eq!(AUTO_EDGE_TYPE_NAMES.len(), autos.len());
        for t in autos {
            assert!(AUTO_EDGE_TYPE_NAMES.contains(&t.as_str()));
        }
        // And nothing else qualifies.
        for t in [
            EdgeType::DerivedFrom,
            EdgeType::Enables,
            EdgeType::Contradicts,
            EdgeType::Refines,
            EdgeType::DependsOn,
            EdgeType::RelatedTo,
            EdgeType::Other("navigatesFor".into()),
        ] {
            assert!(!t.is_auto_derived(), "{:?} must not be auto-derived", t);
        }
    }

    #[test]
    fn partition_edge_types_none_gives_full_auto_in_and_nin_auto() {
        let (auto, meaningful) = partition_edge_types(None);
        let auto = auto.expect("auto partition present");
        assert_eq!(auto.len(), 3);
        assert!(auto.iter().all(|t| t.is_auto_derived()));
        assert!(matches!(meaningful, MeaningfulTypes::NinAuto));
    }

    #[test]
    fn partition_edge_types_splits_user_list_never_nin() {
        // Mixed list: intersection rides auto, remainder rides an EXPLICIT
        // $in (never $nin when the caller named types).
        let list = [EdgeType::SameSession, EdgeType::Refines, EdgeType::Other("has_trait".into())];
        let (auto, meaningful) = partition_edge_types(Some(&list));
        assert_eq!(auto.as_deref(), Some(&[EdgeType::SameSession][..]));
        match meaningful {
            MeaningfulTypes::In(types) => {
                assert_eq!(types.len(), 2);
                assert!(types.contains(&EdgeType::Refines));
                assert!(types.contains(&EdgeType::Other("has_trait".into())));
            }
            _ => panic!("expected explicit In partition"),
        }

        // Pure-auto list: meaningful partition skipped entirely.
        let (auto, meaningful) = partition_edge_types(Some(&[EdgeType::Temporal]));
        assert_eq!(auto.as_deref(), Some(&[EdgeType::Temporal][..]));
        assert!(matches!(meaningful, MeaningfulTypes::Skip));

        // Pure-meaningful list: auto partition skipped entirely.
        let (auto, meaningful) = partition_edge_types(Some(&[EdgeType::DerivedFrom]));
        assert!(auto.is_none());
        assert!(matches!(meaningful, MeaningfulTypes::In(ref t) if t == &[EdgeType::DerivedFrom]));
    }

    #[test]
    fn meaningful_witness_edge_wins_neighbor_dedupe() {
        // The hop concats meaningful-first; expand_node_edges keeps the
        // FIRST edge doc reaching a neighbor, so the recorded witness edge
        // prefers the meaningful label over a same_session twin.
        let mk = |id: &str, etype: EdgeType| KnowledgeEdge {
            _id: Some(id.into()),
            source_id: "n".into(),
            source_collection: "memory.semantic".into(),
            target_id: "x".into(),
            target_collection: "memory.semantic".into(),
            edge_type: etype,
            weight: 1.0,
            metadata: serde_json::Value::Null,
            created_at: "2026-07-01T00:00:00Z".into(),
        };
        let edges = vec![mk("m1", EdgeType::Refines), mk("a1", EdgeType::SameSession)];
        let mut visited: HashSet<(String, String)> =
            [("memory.semantic".to_string(), "n".to_string())].into_iter().collect();
        let (admitted, budget_hit) =
            expand_node_edges("memory.semantic", "n", edges, &mut visited, 1000);
        assert!(!budget_hit);
        assert_eq!(admitted.len(), 1, "one neighbor, one witness edge");
        assert_eq!(admitted[0].0.edge_type, EdgeType::Refines);
        assert_eq!(admitted[0].1, ("memory.semantic".to_string(), "x".to_string()));
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
