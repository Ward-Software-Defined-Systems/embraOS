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
use futures::stream::{self, StreamExt};
use serde_json::json;
use std::collections::{HashMap, HashSet};

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::node_store::{doc_tag_contains, sort_created_desc, NodeStore};
use super::text::content_tokens;
use super::traversal::{spawn_access_touches, traverse_multi};
use super::types::{content_preview, GraphNode, NodeType, RankedNode, SemanticCategory};

/// Step-1 window: newest matches admitted per tag per collection. Raised
/// 20 → 100 (2026-07-31): 20 was the old per-tag SERVER query's window;
/// matching has been in-memory since 2026-07-04, so the cap only bounds
/// candidate-set growth — scoring ranks and truncates regardless.
const STEP1_PER_TAG_CAP: usize = 100;

/// Step-2 walk width: how many of the newest session entries get their
/// same_session edge windows walked. Raised take(20) → the full
/// `session_entries_query_body` window (pinned together by
/// `step2_walk_cap_matches_entry_window_limit`).
const STEP2_ENTRY_WALK: usize = 50;

/// Bounded fan-out for the Step-2 edge-window fetches — HTTP pipelining
/// against the local DB, same rationale as traversal's `HOP_CONCURRENCY`.
const STEP2_WALK_CONCURRENCY: usize = 8;

/// Step-3 content-match admissions per collection, ordered
/// (match count desc, created_at desc, _id asc) — bounds candidate-set
/// growth; scoring ranks the survivors.
const STEP3_PER_COLLECTION_CAP: usize = 100;

/// Relevance-denominator cap (2026-07-31 scoring fix): tag_relevance and
/// content-match strength divide by `min(deduped query tokens, THIS)` — a
/// 25-word message no longer dilutes a 2-tag hit to ~0.04 of the 0.4
/// relevance budget.
const RELEVANCE_DENOM_CAP: usize = 8;

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
    /// Step-3 content-match strength in [0,1] (0.0 = no content match).
    /// Feeds the relevance signal as `max(tag_relevance, content_strength)`.
    content_strength: f64,
}

/// Pre-threshold, pre-truncation funnel counts — the observability seam
/// (2026-07-31): enrichment logs these so production journals can answer
/// "was retrieval comprehensive", not just show the surviving top-5.
#[derive(Debug, Clone, Copy, Default)]
pub struct RetrievalStats {
    pub candidates_total: usize,
    pub direct_query: usize,
    pub session_based: usize,
    pub graph_expansion: usize,
}

pub async fn retrieve_relevant_knowledge(
    db: &WardsonDbClient,
    session_name: &str,
    tags: &[String],
    query_text: &str,
    max_results: usize,
    config: &SystemConfig,
) -> Result<(Vec<RankedNode>, RetrievalStats)> {
    let mut collected: HashMap<(String, String), Collected> = HashMap::new();
    let query_tokens = content_tokens(query_text);

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
                insert_collected(&mut collected, doc, coll, "direct_query", 0.0);
            }
        }
    }

    // Step 3a: content-token match over the SAME prefetched promoted-node
    // slices (2026-07-31 — before this, semantic/procedural CONTENT was
    // unsearchable anywhere: a promoted fact whose tags don't match was
    // invisible to direct retrieval). In-memory, so effectively free.
    if !query_tokens.is_empty() {
        for (coll, docs) in &prefetched {
            for (doc, strength) in
                step3_content_hits(docs, coll, &query_tokens, STEP3_PER_COLLECTION_CAP)
            {
                insert_collected(&mut collected, doc, coll, "direct_query", strength);
            }
        }
    }
    for (coll, docs) in prefetched {
        store.insert_docs(coll, docs);
    }

    // Step 2: Session-based — find edges from current-session entries.
    // Edge windows fetch with bounded concurrency (2026-07-31: the walk
    // widened take(20) → the full 50-entry window; 50 serial round trips
    // would cost real per-turn latency), then join the store sequentially
    // (it needs &mut).
    if let Ok(entries) = db.query("memory.entries", &session_entries_query_body(session_name)).await {
        let walk_ids: Vec<String> = entries
            .iter()
            .filter_map(|d| d.get("_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .take(STEP2_ENTRY_WALK)
            .collect();
        let edge_windows: Vec<Vec<serde_json::Value>> = stream::iter(walk_ids)
            .map(|entry_id| async move {
                db.query("memory.edges", &session_edge_query_body(&entry_id))
                    .await
                    .unwrap_or_default()
            })
            .buffered(STEP2_WALK_CONCURRENCY)
            .collect()
            .await;
        for edge in edge_windows.into_iter().flatten() {
            let Some(target_coll) = edge.get("target_collection").and_then(|v| v.as_str()) else { continue; };
            let Some(target_id) = edge.get("target_id").and_then(|v| v.as_str()) else { continue; };
            if target_coll == "memory.entries" { continue; }
            if let Some(doc) = store.get_or_fetch(db, target_coll, target_id).await {
                insert_collected(&mut collected, &doc, target_coll, "session_based", 0.0);
            }
        }
    }

    // Step 3b: content-token match on memory.entries (2026-07-31 — replaces
    // the whole-message substring, which required the ENTIRE user message to
    // appear verbatim inside an entry and so never fired on natural
    // messages). Recency window via fetch_recent (FIX-4).
    if !query_tokens.is_empty() {
        let all_entries = db
            .fetch_recent("memory.entries", crate::db::MEMORY_FETCH_WINDOW)
            .await
            .unwrap_or_default();
        for (doc, strength) in
            step3_content_hits(&all_entries, "memory.entries", &query_tokens, STEP3_PER_COLLECTION_CAP)
        {
            // The promoted target's content derives from the matched entry,
            // so the match strength rides the redirect.
            if let Some((pdoc, pcoll)) = redirect_if_promoted(&mut store, db, doc, "memory.entries").await {
                insert_collected(&mut collected, &pdoc, &pcoll, "direct_query", strength);
            } else {
                insert_collected(&mut collected, doc, "memory.entries", "direct_query", strength);
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
                        insert_collected(&mut collected, &pdoc, &pcoll, "graph_expansion", 0.0);
                    } else {
                        insert_collected(&mut collected, &doc, &node.collection, "graph_expansion", 0.0);
                    }
                }
            }
        }
    }

    // Funnel stats — pre-threshold, pre-truncation (the observability seam).
    let stats = funnel_stats(&collected);

    // Step 5: Score and rank; access-touch ONLY what is returned (the
    // 2026-07-04 semantics change: access_count = retrieval hits, not BFS
    // sweep wavefronts).
    let ranked = score_and_rank(collected.into_values().collect(), tags, max_results);
    spawn_access_touches(
        db.clone(),
        ranked.iter().map(|r| (r.node.collection.clone(), r.node.id.clone())).collect(),
    );
    Ok((ranked, stats))
}

/// One pass over the candidate map, counting by source label.
fn funnel_stats(collected: &HashMap<(String, String), Collected>) -> RetrievalStats {
    let mut stats = RetrievalStats {
        candidates_total: collected.len(),
        ..Default::default()
    };
    for c in collected.values() {
        match c.source.as_str() {
            "direct_query" => stats.direct_query += 1,
            "session_based" => stats.session_based += 1,
            _ => stats.graph_expansion += 1,
        }
    }
    stats
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

/// Step-3 admission rule (pure): a doc matches when it shares ≥ 2 distinct
/// content tokens with the query — or ≥ 1 when the query itself has ≤ 2
/// tokens. Strength = `matched / min(query_token_count, RELEVANCE_DENOM_CAP)`
/// clamped to [0,1]; it feeds the relevance signal via
/// `max(tag_relevance, content_strength)`.
fn content_match(
    doc_tokens: &HashSet<String>,
    query_tokens: &HashSet<String>,
) -> Option<(usize, f64)> {
    if query_tokens.is_empty() {
        return None;
    }
    let matched = query_tokens.iter().filter(|t| doc_tokens.contains(*t)).count();
    let required = if query_tokens.len() <= 2 { 1 } else { 2 };
    if matched < required {
        return None;
    }
    let denom = query_tokens.len().clamp(1, RELEVANCE_DENOM_CAP) as f64;
    Some((matched, (matched as f64 / denom).clamp(0.0, 1.0)))
}

/// The matchable text per collection: semantic/entries use `content`;
/// procedural nodes match on `title` + `description`.
fn doc_match_text(doc: &serde_json::Value, collection: &str) -> String {
    if collection == "memory.procedural" {
        let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let desc = doc.get("description").and_then(|v| v.as_str()).unwrap_or("");
        format!("{title}\n{desc}")
    } else {
        doc.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string()
    }
}

/// Top `cap` content-matching docs of one collection, ordered
/// (match count desc, created_at desc, _id asc) — deterministic regardless
/// of input order. Returns each doc with its match strength.
fn step3_content_hits<'a>(
    docs: &'a [serde_json::Value],
    collection: &str,
    query_tokens: &HashSet<String>,
    cap: usize,
) -> Vec<(&'a serde_json::Value, f64)> {
    let mut hits: Vec<(usize, &str, &str, &'a serde_json::Value, f64)> = docs
        .iter()
        .filter_map(|doc| {
            let (matched, strength) =
                content_match(&content_tokens(&doc_match_text(doc, collection)), query_tokens)?;
            Some((
                matched,
                doc.get("created_at").and_then(|v| v.as_str()).unwrap_or(""),
                doc.get("_id").and_then(|v| v.as_str()).unwrap_or(""),
                doc,
                strength,
            ))
        })
        .collect();
    hits.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(a.1))
            .then_with(|| a.2.cmp(b.2))
    });
    hits.truncate(cap);
    hits.into_iter().map(|(_, _, _, doc, strength)| (doc, strength)).collect()
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
    content_strength: f64,
) {
    let Some(id) = doc.get("_id").and_then(|v| v.as_str()).map(|s| s.to_string()) else { return; };
    let key = (collection.to_string(), id.clone());
    if let Some(existing) = out.get_mut(&key) {
        // First-write-wins for everything EXCEPT the content-match
        // strength, which max-merges: a Step-1 tag hit later re-matched by
        // content keeps its source/fields but gains the strength signal.
        if content_strength > existing.content_strength {
            existing.content_strength = content_strength;
        }
        return;
    }

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
        crate::identity_graph::IDENTITY_COLLECTION => {
            // Identity nodes reach here only via Step-4 graph expansion
            // (they are deliberately absent from the bulk prefetch — the
            // full sealed graph already rides the system prompt).
            let node_type = doc.get("node_type").and_then(|v| v.as_str())
                .unwrap_or("").to_string();
            (
                doc.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                NodeType::Identity { node_type },
                1.0,
            )
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
        content_strength: content_strength.clamp(0.0, 1.0),
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
    /// Relevance denominator: `min(deduped query tokens, RELEVANCE_DENOM_CAP)`
    /// (2026-07-31 fix — was the raw, non-deduped, stopword-inclusive token
    /// count, which diluted a 2-tag hit on a 25-word message to ~0.04 of
    /// the 0.4 relevance budget).
    tag_denom: f64,
    ts_min: i64,
    ts_range: f64,
    /// <2 distinct parseable timestamps in the candidate set — min-max
    /// normalization carries no ordering information.
    degenerate_recency: bool,
    max_access: f64,
}

fn build_score_ctx(items: &[&Collected], input_tags: &[String]) -> ScoreCtx {
    // Normalize recency: oldest=0.0, newest=1.0.
    let timestamps: Vec<i64> = items.iter()
        .filter_map(|c| DateTime::parse_from_rfc3339(&c.created_at).ok())
        .map(|d| d.timestamp())
        .collect();
    let (ts_min, ts_max, degenerate_recency) =
        match (timestamps.iter().min().copied(), timestamps.iter().max().copied()) {
            (Some(a), Some(b)) if a != b => (a, b, false),
            _ => (0, 1, true),
        };
    ScoreCtx {
        tag_denom: input_tags.len().clamp(1, RELEVANCE_DENOM_CAP) as f64,
        ts_min,
        ts_range: (ts_max - ts_min).max(1) as f64,
        degenerate_recency,
        max_access: items.iter().map(|c| c.access_count).max().unwrap_or(1).max(1) as f64,
    }
}

fn score_one(c: &Collected, ctx: &ScoreCtx, input_tags: &[String]) -> f64 {
    let matching_tags = c.tags.iter()
        .filter(|t| input_tags.iter().any(|it| it.eq_ignore_ascii_case(t)))
        .count() as f64;
    let tag_relevance = (matching_tags / ctx.tag_denom).min(1.0);
    // Relevance = the stronger of the two direct-match signals (2026-07-31):
    // untagged-but-relevant content becomes scoreable above the enrichment
    // threshold instead of riding on recency alone.
    let relevance = tag_relevance.max(c.content_strength.clamp(0.0, 1.0));

    // Degenerate sets (2026-07-31 fix): with <2 distinct timestamps the
    // signal carries no ordering — neutral 0.5 keeps absolute comparisons
    // against the enrichment threshold consistent (the old fallback fed the
    // RAW epoch seconds through, ~1.8e9, destroying the score scale — and a
    // freshly-seeded instance is exactly the all-one-timestamp case).
    // Missing/unparseable created_at stays 0.0: absent data is not neutral.
    let recency = match DateTime::parse_from_rfc3339(&c.created_at) {
        Ok(d) if !ctx.degenerate_recency => {
            (((d.timestamp() - ctx.ts_min) as f64) / ctx.ts_range).clamp(0.0, 1.0)
        }
        Ok(_) => 0.5,
        Err(_) => 0.0,
    };

    let access_frequency = (c.access_count as f64) / ctx.max_access;

    let base = relevance * 0.4 + recency * 0.3 + access_frequency * 0.2 + c.confidence * 0.1;
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
    fn step2_walk_cap_matches_entry_window_limit() {
        // The walk width and the entries window are one number now — a
        // drifted raise of either alone silently narrows or wastes the walk.
        let body = session_entries_query_body("main");
        assert_eq!(body["limit"], json!(super::STEP2_ENTRY_WALK));
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
    fn step1_in_memory_selects_newest_100_matching_per_tag() {
        // 105 tagged docs (+1 untagged decoy) — the newest 100 must win, in
        // recency order (cap raised 20 → 100 in the 2026-07-31 scale wave;
        // matching is in-memory, the cap only bounds candidate growth).
        let mut docs: Vec<serde_json::Value> = (0..105)
            .map(|i| json!({
                "_id": format!("d{i:03}"),
                "tags": ["kg"],
                "created_at": format!("2026-06-01T{:02}:{:02}:00Z", i / 60, i % 60),
            }))
            .collect();
        docs.push(json!({"_id": "decoy", "tags": ["other"], "created_at": "2026-06-02T00:00:00Z"}));
        sort_created_desc(&mut docs);

        let hits = step1_tag_hits(&docs, "kg");
        assert_eq!(hits.len(), STEP1_PER_TAG_CAP);
        assert_eq!(hits[0]["_id"], json!("d104"), "newest first");
        assert_eq!(hits[99]["_id"], json!("d005"), "oldest 5 pruned by the cap");
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
            content_strength: 0.0,
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

    #[test]
    fn relevance_takes_max_of_tag_and_content_strength() {
        let input_tags = vec!["kg".to_string()];
        // No tag match, but a strong content match: relevance = 0.75, not 0.
        let mut c = item("c", &[], "2026-07-04T00:00:00Z", 0, 0.0, "direct_query");
        c.content_strength = 0.75;
        let plain = item("p", &[], "2026-07-01T00:00:00Z", 0, 0.0, "direct_query");
        let refs = vec![&c, &plain];
        let ctx = build_score_ctx(&refs, &input_tags);
        // c: relevance 0.75*0.4 + recency 1.0*0.3 + 0 + 0 = 0.6 (direct ×1.0)
        assert!((score_one(&c, &ctx, &input_tags) - 0.6).abs() < 1e-9);
        // A tag match stronger than the content strength wins the max.
        let mut t = item("t", &["kg"], "2026-07-04T00:00:00Z", 0, 0.0, "direct_query");
        t.content_strength = 0.25;
        let refs = vec![&t, &plain];
        let ctx = build_score_ctx(&refs, &input_tags);
        // tag_relevance 1/1 = 1.0 > 0.25 → relevance 1.0.
        assert!((score_one(&t, &ctx, &input_tags) - (0.4 + 0.3)).abs() < 1e-9);
    }

    #[test]
    fn tag_denominator_is_deduped_count_capped_at_8() {
        // 12 (already-deduped) query tokens: denominator caps at 8, so a
        // 4-tag hit scores 0.5 of the relevance budget — not 4/12.
        let input_tags: Vec<String> = (0..12).map(|i| format!("tag{i}")).collect();
        let a = item("a", &["tag0", "tag1", "tag2", "tag3"], "2026-07-04T00:00:00Z", 0, 0.0, "direct_query");
        let old = item("o", &[], "2026-07-01T00:00:00Z", 0, 0.0, "direct_query");
        let refs = vec![&a, &old];
        let ctx = build_score_ctx(&refs, &input_tags);
        // relevance (4/8)*0.4 = 0.2 + recency 0.3 = 0.5
        assert!((score_one(&a, &ctx, &input_tags) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn degenerate_recency_is_neutral_half_unparseable_zero() {
        let input_tags: Vec<String> = vec![];
        // All candidates share one timestamp — the old code fed RAW epoch
        // seconds (~1.8e9) through; now recency is a neutral 0.5.
        let a = item("a", &[], "2026-07-04T00:00:00Z", 0, 0.0, "direct_query");
        let b = item("b", &[], "2026-07-04T00:00:00Z", 0, 0.0, "direct_query");
        let refs = vec![&a, &b];
        let ctx = build_score_ctx(&refs, &input_tags);
        // 0 + 0.5*0.3 + 0 + 0 = 0.15 — sane against the 0.3 threshold scale.
        assert!((score_one(&a, &ctx, &input_tags) - 0.15).abs() < 1e-9);

        // Unparseable created_at is NOT neutral — missing data scores 0.
        let bad = item("bad", &[], "not-a-date", 0, 0.0, "direct_query");
        let refs = vec![&bad, &a];
        let ctx = build_score_ctx(&refs, &input_tags);
        assert!((score_one(&bad, &ctx, &input_tags) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn retrieval_stats_count_sources_pre_truncation() {
        let mut collected = HashMap::new();
        for (id, source) in [
            ("d1", "direct_query"),
            ("d2", "direct_query"),
            ("s1", "session_based"),
            ("g1", "graph_expansion"),
        ] {
            let c = item(id, &[], "2026-07-04T00:00:00Z", 0, 0.9, source);
            collected.insert((c.collection.clone(), c.id.clone()), c.clone());
        }
        let stats = funnel_stats(&collected);
        assert_eq!(stats.candidates_total, 4);
        assert_eq!(stats.direct_query, 2);
        assert_eq!(stats.session_based, 1);
        assert_eq!(stats.graph_expansion, 1);
    }
}

#[cfg(test)]
mod content_match_tests {
    use super::super::text::content_tokens;
    use super::*;

    #[test]
    fn content_match_requires_two_tokens_or_one_for_short_queries() {
        let doc = content_tokens("the cert refresh works after manual generation");
        // 3+-token query: one shared token is not enough…
        let q3 = content_tokens("cert broken tomorrow");
        assert!(content_match(&doc, &q3).is_none());
        // …two are.
        let q3b = content_tokens("cert refresh broken");
        assert!(content_match(&doc, &q3b).is_some());
        // ≤2-token query: a single shared token admits.
        let q1 = content_tokens("cert");
        assert!(content_match(&doc, &q1).is_some());
        // Empty query never matches.
        assert!(content_match(&doc, &content_tokens("")).is_none());
    }

    #[test]
    fn content_match_strength_is_matched_over_min_qcount_8_clamped() {
        let doc = content_tokens("alpha beta gamma delta epsilon zeta eta theta iota kappa");
        // 4-token query, 3 matched → 3/4.
        let q = content_tokens("alpha beta gamma missing");
        let (matched, strength) = content_match(&doc, &q).unwrap();
        assert_eq!(matched, 3);
        assert!((strength - 0.75).abs() < 1e-9);
        // 10-token query, 9 matched → denominator caps at 8 → clamped to 1.0.
        let q10 = content_tokens("alpha beta gamma delta epsilon zeta eta theta iota missing");
        let (matched, strength) = content_match(&doc, &q10).unwrap();
        assert_eq!(matched, 9);
        assert!((strength - 1.0).abs() < 1e-9);
    }

    #[test]
    fn step3_hits_capped_ordered_matchcount_recency_id() {
        let query = content_tokens("cert refresh trustd");
        let mk = |id: &str, content: &str, created: &str| {
            serde_json::json!({"_id": id, "content": content, "created_at": created})
        };
        let docs = vec![
            mk("weak-old", "cert refresh notes", "2026-06-01T00:00:00Z"),
            mk("strong", "cert refresh trustd pipeline", "2026-06-02T00:00:00Z"),
            mk("weak-new", "cert refresh other", "2026-06-03T00:00:00Z"),
            mk("miss", "unrelated content entirely", "2026-06-04T00:00:00Z"),
        ];
        let hits = step3_content_hits(&docs, "memory.semantic", &query, 2);
        let ids: Vec<&str> = hits.iter().map(|(d, _)| d["_id"].as_str().unwrap()).collect();
        // match-count desc first (strong=3), then recency desc among the
        // 2-token matches (weak-new beats weak-old), capped at 2.
        assert_eq!(ids, vec!["strong", "weak-new"]);
        assert!((hits[0].1 - 1.0).abs() < 1e-9, "3/3 matched");
    }

    #[test]
    fn step3_procedural_matches_title_and_description() {
        let query = content_tokens("cert refresh procedure");
        let doc = serde_json::json!({
            "_id": "p1",
            "title": "Cert refresh procedure",
            "description": "regenerate via trustd then restart embra-web",
            "created_at": "2026-06-01T00:00:00Z",
        });
        let docs = vec![doc];
        let hits = step3_content_hits(&docs, "memory.procedural", &query, 10);
        assert_eq!(hits.len(), 1, "title text must be matchable for procedural nodes");
        // Same doc under memory.semantic matches nothing — no `content` field.
        let hits = step3_content_hits(&docs, "memory.semantic", &query, 10);
        assert!(hits.is_empty());
    }

    #[test]
    fn insert_collected_max_merges_strength_keeps_first_source() {
        let mut out = HashMap::new();
        let doc = serde_json::json!({
            "_id": "n1", "content": "x", "category": "fact",
            "tags": ["kg"], "created_at": "2026-06-01T00:00:00Z",
        });
        insert_collected(&mut out, &doc, "memory.semantic", "direct_query", 0.0);
        insert_collected(&mut out, &doc, "memory.semantic", "graph_expansion", 0.8);
        let key = ("memory.semantic".to_string(), "n1".to_string());
        let c = &out[&key];
        assert_eq!(c.source, "direct_query", "first write wins the source");
        assert!((c.content_strength - 0.8).abs() < 1e-9, "strength max-merges");
        // A weaker later strength never downgrades.
        insert_collected(&mut out, &doc, "memory.semantic", "session_based", 0.2);
        assert!((out[&key].content_strength - 0.8).abs() < 1e-9);
    }
}
