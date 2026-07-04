//! Context-aware retrieval with graph expansion.
//!
//! Multi-signal ranking:
//!   score = tag_relevance*0.4 + recency*0.3 + access_frequency*0.2 + confidence*0.1
//!
//! Since 2026-07-04 the pipeline joins against a per-call `NodeStore`
//! prefetch instead of issuing point reads: Step 1 tag-matches in memory,
//! Steps 2–4 resolve node docs from the store (point-read fallback for
//! window misses), Step 4 runs ONE multi-source arm-split traversal from the
//! top-scored seeds, and only the finally-returned nodes get access-touched.

use anyhow::Result;
use chrono::DateTime;
use serde_json::json;
use std::collections::HashMap;

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::node_store::{doc_tag_contains, sort_created_desc, NodeStore};
use super::traversal::{spawn_access_touches, traverse_multi};
use super::types::{content_preview, GraphNode, NodeType, RankedNode, SemanticCategory};

/// Step-1 window: newest matches admitted per tag per collection — mirrors
/// the old per-tag server query's `sort: created_at desc, limit: 20`.
const STEP1_PER_TAG_CAP: usize = 20;

/// Graph-expansion fan-out: the top-scored candidates seeding Step 4's
/// single multi-source traversal (was: 10 seeds in HashMap iteration order,
/// each walking its own ~90%-overlapping BFS).
const EXPANSION_SEED_CAP: usize = 10;

/// Collected node prior to scoring.
#[derive(Clone)]
struct Collected {
    collection: String,
    id: String,
    content: String,
    tags: Vec<String>,
    created_at: String,
    access_count: u64,
    confidence: f64,
    node_type: NodeType,
    source: String,
}

pub async fn retrieve_relevant_knowledge(
    db: &WardsonDbClient,
    session_name: &str,
    tags: &[String],
    query_text: &str,
    max_results: usize,
    config: &SystemConfig,
) -> Result<Vec<RankedNode>> {
    let mut collected: HashMap<(String, String), Collected> = HashMap::new();

    // Prefetch the promoted-node collections once; every later lookup joins
    // in memory (2026-07-04 — replaces hundreds of sequential point reads).
    let mut store = NodeStore::new();
    let mut prefetched: Vec<(&str, Vec<serde_json::Value>)> = Vec::new();
    for coll in ["memory.semantic", "memory.procedural"] {
        let docs = db
            .fetch_recent(coll, crate::db::MEMORY_FETCH_WINDOW)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("retrieval prefetch of {} failed: {}", coll, e);
                Vec::new()
            });
        prefetched.push((coll, docs));
    }

    // Step 1: Direct tag query on semantic + procedural — in memory over the
    // prefetch, mirroring the old per-tag `$contains` server queries exactly
    // (case-sensitive membership, newest 20 per tag per collection).
    for (_, docs) in prefetched.iter_mut() {
        sort_created_desc(docs);
    }
    for tag in tags {
        if tag.is_empty() { continue; }
        for (coll, docs) in &prefetched {
            for doc in step1_tag_hits(docs, tag) {
                insert_collected(&mut collected, doc, coll, "direct_query");
            }
        }
    }
    for (coll, docs) in prefetched {
        store.insert_docs(coll, docs);
    }

    // Step 2: Session-based — find edges from current-session entries.
    if let Ok(entries) = db.query("memory.entries", &session_entries_query_body(session_name)).await {
        let session_ids: Vec<String> = entries
            .iter()
            .filter_map(|d| d.get("_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();
        for entry_id in session_ids.iter().take(20) {
            if let Ok(edges) = db.query("memory.edges", &session_edge_query_body(entry_id)).await {
                for edge in edges {
                    let Some(target_coll) = edge.get("target_collection").and_then(|v| v.as_str()) else { continue; };
                    let Some(target_id) = edge.get("target_id").and_then(|v| v.as_str()) else { continue; };
                    if target_coll == "memory.entries" { continue; }
                    if let Some(doc) = store.get_or_fetch(db, target_coll, target_id).await {
                        insert_collected(&mut collected, &doc, target_coll, "session_based");
                    }
                }
            }
        }
    }

    // Step 3: Content-based substring match on memory.entries. Recency
    // window via fetch_recent (FIX-4) — the old `limit: 500` with no sort
    // was a second latent freeze at entry #500 in key (oldest-first) order.
    if !query_text.is_empty() {
        let q_lower = query_text.to_lowercase();
        let all_entries = db
            .fetch_recent("memory.entries", crate::db::MEMORY_FETCH_WINDOW)
            .await
            .unwrap_or_default();
        for doc in &all_entries {
            let content = doc.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if !content.to_lowercase().contains(&q_lower) { continue; }
            if let Some((pdoc, pcoll)) = redirect_if_promoted(&mut store, db, doc, "memory.entries").await {
                insert_collected(&mut collected, &pdoc, &pcoll, "direct_query");
            } else {
                insert_collected(&mut collected, doc, "memory.entries", "direct_query");
            }
        }
        // The window becomes the entries slice of the store, so Step 4's
        // traversal resolves entry neighbors without point reads.
        store.insert_docs("memory.entries", all_entries);
    }

    // Step 4: Graph expansion — ONE multi-source traversal (depth 2) from
    // the top-scored candidates (2026-07-04; was 10 independent traversals
    // from HashMap-order seeds re-walking ~90%-overlapping neighborhoods).
    let seeds = seed_keys(&collected, tags, EXPANSION_SEED_CAP);
    if !seeds.is_empty() {
        if let Ok(tr) = traverse_multi(db, &seeds, 2, None, None, config, &mut store).await {
            // depth > 0 (not a positional skip): every seed is a depth-0
            // node under multi-source BFS, and all are already collected.
            for node in tr.nodes.iter().filter(|n| n.depth > 0) {
                let key = (node.collection.clone(), node.id.clone());
                if collected.contains_key(&key) { continue; }
                // Load full doc for scoring fields.
                if let Some(doc) = store.get_or_fetch(db, &node.collection, &node.id).await {
                    if let Some((pdoc, pcoll)) = redirect_if_promoted(&mut store, db, &doc, &node.collection).await {
                        let redir_key = (pcoll.clone(), pdoc.get("_id").and_then(|v| v.as_str()).unwrap_or_default().to_string());
                        if collected.contains_key(&redir_key) { continue; }
                        insert_collected(&mut collected, &pdoc, &pcoll, "graph_expansion");
                    } else {
                        insert_collected(&mut collected, &doc, &node.collection, "graph_expansion");
                    }
                }
            }
        }
    }

    // Step 5: Score and rank; access-touch ONLY what is returned (the
    // 2026-07-04 semantics change: access_count = retrieval hits, not BFS
    // sweep wavefronts).
    let ranked = score_and_rank(collected.into_values().collect(), tags, max_results);
    spawn_access_touches(
        db.clone(),
        ranked.iter().map(|r| (r.node.collection.clone(), r.node.id.clone())).collect(),
    );
    Ok(ranked)
}

// --- Step query bodies (FIX-4) ---------------------------------------------
// Every retrieval window carries an explicit limit AND a recency/rank sort so
// it covers the most relevant documents, never key-order (oldest-first) ones.
// Sort keys are doc fields, one per array element (WardSONDB requirement).
// (Step 1's former per-tag query body is gone — tag matching happens in
// memory over the prefetched node collections, same semantics, zero round
// trips.)

/// Step 1 (in-memory): the newest `STEP1_PER_TAG_CAP` docs whose `tags`
/// array contains `tag` — the exact `$contains` semantics of the old server
/// query (case-sensitive membership; docs must be pre-sorted with
/// `sort_created_desc`).
fn step1_tag_hits<'a>(sorted_docs: &'a [serde_json::Value], tag: &str) -> Vec<&'a serde_json::Value> {
    sorted_docs
        .iter()
        .filter(|d| doc_tag_contains(d, tag))
        .take(STEP1_PER_TAG_CAP)
        .collect()
}

/// Step 2a: current-session entries — newest 50.
fn session_entries_query_body(session: &str) -> serde_json::Value {
    json!({
        "filter": { "session": session },
        "sort": [{"created_at": "desc"}],
        "limit": 50,
    })
}

/// Step 2b: same-session edges from one entry, ranked `weight desc,
/// created_at desc`, limit 50 (locked D3). The `memory.entries` exclusion is
/// server-side (`$ne`) so the window is spent only on useful targets — safe
/// because every edge doc carries `target_collection` (`edges.rs::
/// push_bidirectional` and the manual `knowledge_link` write both set it
/// unconditionally; WardSONDB's `$ne` would drop docs missing the field).
fn session_edge_query_body(entry_id: &str) -> serde_json::Value {
    json!({
        "filter": {
            "source_id": entry_id,
            "edge_type": "same_session",
            "target_collection": { "$ne": "memory.entries" },
        },
        "sort": [{"weight": "desc"}, {"created_at": "desc"}],
        "limit": 50,
    })
}

fn insert_collected(
    out: &mut HashMap<(String, String), Collected>,
    doc: &serde_json::Value,
    collection: &str,
    source: &str,
) {
    let Some(id) = doc.get("_id").and_then(|v| v.as_str()).map(|s| s.to_string()) else { return; };
    let key = (collection.to_string(), id.clone());
    if out.contains_key(&key) { return; }

    let (content, node_type, confidence) = match collection {
        "memory.semantic" => {
            let category = doc.get("category").and_then(|v| v.as_str())
                .and_then(SemanticCategory::from_str)
                .unwrap_or(SemanticCategory::Fact);
            let conf = doc.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.9);
            (
                doc.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                NodeType::Semantic { category },
                conf,
            )
        }
        "memory.procedural" => {
            let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let desc = doc.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
            (desc, NodeType::Procedural { title }, 1.0)
        }
        _ => (
            doc.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            NodeType::Episodic,
            1.0,
        ),
    };

    let tags = doc.get("tags").and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|t| t.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let created_at = doc.get("created_at").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let access_count = doc.get("access_count").and_then(|v| v.as_u64()).unwrap_or(0);

    out.insert(key, Collected {
        collection: collection.to_string(),
        id,
        content,
        tags,
        created_at,
        access_count,
        confidence,
        node_type,
        source: source.to_string(),
    });
}

/// If `doc` is a `memory.entries` doc with a non-null `promoted_to`, resolve the
/// target semantic/procedural node (store hit or cached point read) and return
/// `(target_doc, target_collection)`. Returns `None` for non-entries,
/// unpromoted entries, or when the target fails to load. Callers fall back to
/// inserting the original doc when this returns None.
async fn redirect_if_promoted(
    store: &mut NodeStore,
    db: &WardsonDbClient,
    doc: &serde_json::Value,
    collection: &str,
) -> Option<(serde_json::Value, String)> {
    if collection != "memory.entries" { return None; }
    let promoted = doc.get("promoted_to")?;
    if promoted.is_null() { return None; }
    let coll = promoted.get("collection").and_then(|v| v.as_str())?;
    let id = promoted.get("id").and_then(|v| v.as_str())?;
    let pdoc = store.get_or_fetch(db, coll, id).await?;
    Some((pdoc, coll.to_string()))
}

// --- Scoring ---------------------------------------------------------------
// One scoring core (`score_one` + `ScoreCtx`) drives both the final ranking
// and Step 4's seed selection, so the two can't drift.

/// Set-normalization context for the multi-signal score.
struct ScoreCtx {
    input_tag_count: f64,
    ts_min: i64,
    ts_range: f64,
    max_access: f64,
}

fn build_score_ctx(items: &[&Collected], input_tags: &[String]) -> ScoreCtx {
    // Normalize recency: oldest=0.0, newest=1.0.
    let timestamps: Vec<i64> = items.iter()
        .filter_map(|c| DateTime::parse_from_rfc3339(&c.created_at).ok())
        .map(|d| d.timestamp())
        .collect();
    let (ts_min, ts_max) = match (timestamps.iter().min().copied(), timestamps.iter().max().copied()) {
        (Some(a), Some(b)) if a != b => (a, b),
        _ => (0, 1),
    };
    ScoreCtx {
        input_tag_count: input_tags.len().max(1) as f64,
        ts_min,
        ts_range: (ts_max - ts_min).max(1) as f64,
        max_access: items.iter().map(|c| c.access_count).max().unwrap_or(1).max(1) as f64,
    }
}

fn score_one(c: &Collected, ctx: &ScoreCtx, input_tags: &[String]) -> f64 {
    let matching_tags = c.tags.iter()
        .filter(|t| input_tags.iter().any(|it| it.eq_ignore_ascii_case(t)))
        .count() as f64;
    let tag_relevance = (matching_tags / ctx.input_tag_count).min(1.0);

    let recency = DateTime::parse_from_rfc3339(&c.created_at).ok()
        .map(|d| (d.timestamp() - ctx.ts_min) as f64 / ctx.ts_range)
        .unwrap_or(0.0);

    let access_frequency = (c.access_count as f64) / ctx.max_access;

    let base = tag_relevance * 0.4 + recency * 0.3 + access_frequency * 0.2 + c.confidence * 0.1;
    // Source-quality multiplier separates direct matches from graph-expansion noise.
    let source_mult = match c.source.as_str() {
        "direct_query" => 1.0,
        "session_based" => 0.75,
        "graph_expansion" => 0.5,
        _ => 0.5,
    };
    base * source_mult
}

/// Step 4 seeds: the top-`n` collected candidates by the shared score
/// (deterministic `(collection, id)` tie-break) — never HashMap iteration
/// order.
fn seed_keys(
    collected: &HashMap<(String, String), Collected>,
    input_tags: &[String],
    n: usize,
) -> Vec<(String, String)> {
    let refs: Vec<&Collected> = collected.values().collect();
    let ctx = build_score_ctx(&refs, input_tags);
    let mut scored: Vec<(f64, &(String, String))> = collected
        .iter()
        .map(|(key, c)| (score_one(c, &ctx, input_tags), key))
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(b.1))
    });
    scored.into_iter().take(n).map(|(_, key)| key.clone()).collect()
}

fn score_and_rank(
    items: Vec<Collected>,
    input_tags: &[String],
    max_results: usize,
) -> Vec<RankedNode> {
    if items.is_empty() { return Vec::new(); }

    let ctx = {
        let refs: Vec<&Collected> = items.iter().collect();
        build_score_ctx(&refs, input_tags)
    };

    let mut scored: Vec<RankedNode> = items.into_iter().map(|c| {
        let score = score_one(&c, &ctx, input_tags);
        let node = GraphNode {
            id: c.id,
            collection: c.collection,
            content_preview: content_preview(&c.content, 200),
            node_type: c.node_type,
            depth: 0,
        };
        RankedNode { node, score, source: c.source }
    }).collect();

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            // Deterministic order among exact-score ties (behavior-neutral
            // otherwise; HashMap drain order is not reproducible).
            .then_with(|| {
                (a.node.collection.as_str(), a.node.id.as_str())
                    .cmp(&(b.node.collection.as_str(), b.node.id.as_str()))
            })
    });
    scored.truncate(max_results);
    scored
}

#[cfg(test)]
mod step_query_body_tests {
    //! FIX-4 body-shape guards (no DB mock in this crate — the windowed-
    //! retrieval contract is enforced at the builder level). Step 1's former
    //! per-tag body builder is gone: tag matching is in-memory and guarded
    //! by `step1_tag_tests` below.
    use super::{session_edge_query_body, session_entries_query_body};
    use serde_json::json;

    #[test]
    fn session_body_recency_sorted_limit_50() {
        let body = session_entries_query_body("main");
        assert_eq!(body["filter"]["session"], json!("main"));
        assert_eq!(body["sort"], json!([{"created_at": "desc"}]));
        assert_eq!(body["limit"], json!(50));
    }

    #[test]
    fn edge_body_excludes_entry_targets_server_side() {
        let body = session_edge_query_body("entry-1");
        assert_eq!(
            body["filter"]["target_collection"],
            json!({ "$ne": "memory.entries" })
        );
    }

    #[test]
    fn edge_body_ranked_and_limited_50() {
        let body = session_edge_query_body("entry-1");
        assert_eq!(body["filter"]["source_id"], json!("entry-1"));
        assert_eq!(body["filter"]["edge_type"], json!("same_session"));
        assert_eq!(
            body["sort"],
            json!([{"weight": "desc"}, {"created_at": "desc"}])
        );
        assert_eq!(body["limit"], json!(50));
    }
}

#[cfg(test)]
mod step1_tag_tests {
    use super::super::node_store::sort_created_desc;
    use super::{step1_tag_hits, STEP1_PER_TAG_CAP};
    use serde_json::json;

    #[test]
    fn step1_in_memory_selects_newest_20_matching_per_tag() {
        // 25 tagged docs (+1 untagged decoy) — the newest 20 must win, in
        // recency order, mirroring the old `created_at desc, limit 20` body.
        let mut docs: Vec<serde_json::Value> = (0..25)
            .map(|i| json!({
                "_id": format!("d{i:02}"),
                "tags": ["kg"],
                "created_at": format!("2026-06-{:02}T00:00:00Z", i + 1),
            }))
            .collect();
        docs.push(json!({"_id": "decoy", "tags": ["other"], "created_at": "2026-06-30T00:00:00Z"}));
        sort_created_desc(&mut docs);

        let hits = step1_tag_hits(&docs, "kg");
        assert_eq!(hits.len(), STEP1_PER_TAG_CAP);
        assert_eq!(hits[0]["_id"], json!("d24"), "newest first");
        assert_eq!(hits[19]["_id"], json!("d05"), "oldest 5 pruned by the cap");
        assert!(hits.iter().all(|d| d["_id"] != json!("decoy")));
    }
}

#[cfg(test)]
mod scoring_tests {
    //! The shared scoring core drives final ranking AND seed selection —
    //! these lock the documented formula and the deterministic seed order.
    use super::*;

    fn item(
        id: &str,
        tags: &[&str],
        created_at: &str,
        access_count: u64,
        confidence: f64,
        source: &str,
    ) -> Collected {
        Collected {
            collection: "memory.semantic".into(),
            id: id.into(),
            content: format!("content {id}"),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            created_at: created_at.into(),
            access_count,
            confidence,
            node_type: NodeType::Semantic { category: SemanticCategory::Fact },
            source: source.into(),
        }
    }

    #[test]
    fn score_items_matches_documented_formula_and_multipliers() {
        let input_tags = vec!["kg".to_string()];
        let a = item("a", &["kg"], "2026-07-04T00:00:00Z", 4, 1.0, "direct_query");
        let b = item("b", &[], "2026-07-01T00:00:00Z", 2, 0.5, "graph_expansion");
        let refs = vec![&a, &b];
        let ctx = build_score_ctx(&refs, &input_tags);

        // a: tags 1/1*0.4 + recency 1.0*0.3 + access 4/4*0.2 + conf 1.0*0.1 = 1.0, direct x1.0
        assert!((score_one(&a, &ctx, &input_tags) - 1.0).abs() < 1e-9);
        // b: 0 + 0 + (2/4)*0.2 + 0.5*0.1 = 0.15, graph_expansion x0.5 = 0.075
        assert!((score_one(&b, &ctx, &input_tags) - 0.075).abs() < 1e-9);

        // Source multipliers on otherwise-identical items: 1.0 / 0.75 / 0.5.
        let d = item("d", &["kg"], "2026-07-04T00:00:00Z", 4, 1.0, "direct_query");
        let s = item("s", &["kg"], "2026-07-04T00:00:00Z", 4, 1.0, "session_based");
        let g = item("g", &["kg"], "2026-07-04T00:00:00Z", 4, 1.0, "graph_expansion");
        let refs = vec![&d, &s, &g, &b];
        let ctx = build_score_ctx(&refs, &input_tags);
        let ds = score_one(&d, &ctx, &input_tags);
        let ss = score_one(&s, &ctx, &input_tags);
        let gs = score_one(&g, &ctx, &input_tags);
        assert!((ss / ds - 0.75).abs() < 1e-9);
        assert!((gs / ds - 0.5).abs() < 1e-9);
    }

    #[test]
    fn seed_keys_are_top_scored_not_hash_order() {
        let mut collected = HashMap::new();
        let winner = item("top", &["kg"], "2026-07-04T00:00:00Z", 9, 1.0, "direct_query");
        let mid = item("mid", &["kg"], "2026-07-02T00:00:00Z", 3, 0.9, "session_based");
        let low = item("low", &[], "2026-07-01T00:00:00Z", 0, 0.5, "graph_expansion");
        for c in [&winner, &mid, &low] {
            collected.insert((c.collection.clone(), c.id.clone()), c.clone());
        }
        let seeds = seed_keys(&collected, &["kg".to_string()], 2);
        assert_eq!(seeds.len(), 2);
        assert_eq!(seeds[0].1, "top");
        assert_eq!(seeds[1].1, "mid");
    }

    #[test]
    fn seed_keys_tie_break_deterministic_by_key() {
        let mut collected = HashMap::new();
        for id in ["zz", "aa", "mm"] {
            let c = item(id, &["kg"], "2026-07-04T00:00:00Z", 1, 1.0, "direct_query");
            collected.insert((c.collection.clone(), c.id.clone()), c.clone());
        }
        let seeds = seed_keys(&collected, &["kg".to_string()], 3);
        let ids: Vec<&str> = seeds.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(ids, vec!["aa", "mm", "zz"], "exact ties order by key, not hash");
    }
}
