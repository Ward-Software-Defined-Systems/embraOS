//! Context-aware retrieval over the promoted knowledge collections.
//!
//! Multi-signal ranking:
//!   score = relevance*0.6 + recency*0.2 + access_frequency*0.2
//!
//! Since 2026-07-04 the pipeline joins against a per-call `NodeStore`
//! prefetch instead of issuing point reads: Step 1 tag-matches in memory and
//! Steps 2–3 resolve node docs from the store (point-read fallback for window
//! misses). Only the finally-returned nodes get access-touched.
//!
//! Graph expansion was DELETED 2026-09-08. Measured against a copy of
//! production (2,388 nodes / 408,046 edges): it cost 1,244 ms of a 1,301 ms
//! retrieval (96%) and 1,188 of 1,242 WardSONDB round-trips, and contributed
//! 0 of the top 5, 0 of the top 10 and 0 of the top 20 on every query
//! measured. It could not do better by construction — its candidates entered
//! with `content_strength = 0.0` and no query tags, so the 0.5 source
//! multiplier capped them at 0.300, exactly the enrichment threshold and below
//! every real direct hit. `traverse_multi` itself is untouched and still backs
//! the `knowledge_traverse` tool.

use anyhow::Result;
use chrono::DateTime;
use futures::stream::{self, StreamExt};
use serde_json::json;
use std::collections::{HashMap, HashSet};

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::idf::DocFreq;
use super::node_store::{doc_tag_contains, sort_created_desc, NodeStore};
use super::text::content_tokens;
use super::traversal::spawn_access_touches;
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

/// Fields the retrieval prefetch actually reads. This is an INCLUSION list,
/// and it is load-bearing: without it `fetch_recent` pulls every field, which
/// since KG-02 includes the base64 `embedding` — ~2 KB per document, up to
/// ~20 MB on the wire per turn at the `MEMORY_FETCH_WINDOW` ceiling, for data
/// retrieval never reads (similarity runs off the in-process vector index).
/// Pinned by `prefetch_projection_covers_every_field_retrieval_reads`.
const PREFETCH_FIELDS: [&str; 10] = [
    "_id",
    "content",
    "tags",
    "created_at",
    "access_count",
    "category",
    "title",
    "description",
    "promoted_to",
    "node_type",
];

/// Similarity candidates admitted per retrieval, before ranking.
const EMBEDDING_TOP_K: usize = 100;

/// Cosine floor for admission. Below this the neighbour is not "about" the
/// query in any useful sense and only dilutes the candidate set.
const EMBEDDING_MIN_SIMILARITY: f32 = 0.5;

/// Rescale a raw cosine onto the [0,1] scale the other relevance signals use.
///
/// Cosine and `tag_relevance` are NOT the same units, and feeding a raw cosine
/// into `relevance` conflates them. `tag_relevance` is a fraction of the query
/// matched, so 0.5 genuinely means "half"; a cosine of 0.5 means "barely
/// related at all" — it is the admission floor. Real cosines on this corpus
/// occupy a narrow high band (~0.45–0.85), so passing them through raw
/// compresses every similarity hit into a thin slice of the relevance budget
/// and hands the decision to recency — the exact defect the scoring wave was
/// fixing. Mapping [floor, 1.0] onto [0, 1] restores the discrimination.
fn similarity_strength(cosine: f32) -> f64 {
    let floor = EMBEDDING_MIN_SIMILARITY;
    (((cosine - floor) / (1.0 - floor)) as f64).clamp(0.0, 1.0)
}

/// Tag-relevance denominator cap (2026-07-31 scoring fix): tag_relevance
/// divides by `min(deduped query tokens, THIS)` — a 25-word message no longer
/// dilutes a 2-tag hit to ~0.04 of the relevance budget. Content strength uses
/// the IDF-weighted analogue (`idf::IDF_DENOM_CAP`, same number).
const RELEVANCE_DENOM_CAP: usize = 8;

/// Collected node prior to scoring.
#[derive(Clone)]
struct Collected {
    collection: String,
    id: String,
    content: String,
    tags: Vec<String>,
    created_at: String,
    access_count: u64,
    node_type: NodeType,
    source: String,
    /// Step-3 LEXICAL content-match strength in [0,1] (0.0 = no match).
    content_strength: f64,
    /// Rescaled cosine, when this node has a vector (Step 3c). Deliberately a
    /// separate channel from `content_strength`: see `score_one`.
    similarity: Option<f64>,
}

/// Pre-threshold, pre-truncation funnel counts — the observability seam
/// (2026-07-31): enrichment logs these so production journals can answer
/// "was retrieval comprehensive", not just show the surviving top-5.
#[derive(Debug, Clone, Copy, Default)]
pub struct RetrievalStats {
    pub candidates_total: usize,
    pub direct_query: usize,
    pub session_based: usize,
    /// Unknown-source bucket. Held at its historical name because it is `pub`
    /// and enrichment logs it as `candidates_other`; since graph expansion was
    /// deleted (2026-09-08) nothing populates it and it reads 0.
    pub graph_expansion: usize,
    /// Candidates admitted by similarity search that no lexical step had
    /// already found. Counted at the step because cosine hits carry the
    /// `direct_query` label and `funnel_stats` cannot tell them apart.
    pub embedding: usize,
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
            .fetch_recent_with_fields(coll, crate::db::MEMORY_FETCH_WINDOW, Some(&PREFETCH_FIELDS))
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("retrieval prefetch of {} failed: {}", coll, e);
                Vec::new()
            });
        prefetched.push((coll, docs));
    }

    // The entries window is fetched HERE rather than at its consuming step
    // (3b) so document frequency spans all three collections: 3a and 3b
    // strengths are compared against each other during ranking, so they must
    // share one IDF corpus or the weighting is inconsistent between them.
    let all_entries: Vec<serde_json::Value> = if query_tokens.is_empty() {
        Vec::new()
    } else {
        db.fetch_recent_with_fields(
            "memory.entries",
            crate::db::MEMORY_FETCH_WINDOW,
            Some(&PREFETCH_FIELDS),
        )
        .await
        .unwrap_or_default()
    };

    // Per-query document frequency (2026-09-08). Counts only the query's own
    // tokens, so this is a handful of counters regardless of corpus size.
    let doc_freq = DocFreq::build(
        &query_tokens,
        prefetched
            .iter()
            .flat_map(|(coll, docs)| {
                docs.iter().map(move |d| content_tokens(&doc_match_text(d, coll)))
            })
            .chain(
                all_entries
                    .iter()
                    .map(|d| content_tokens(&doc_match_text(d, "memory.entries"))),
            ),
    );

    // Step 1: Direct tag query on semantic + procedural — in memory over the
    // prefetch, mirroring the old per-tag `$contains` server queries exactly
    // (case-sensitive membership, newest 100 per tag per collection).
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
            for (doc, strength) in step3_content_hits(
                docs, coll, &query_tokens, &doc_freq, STEP3_PER_COLLECTION_CAP,
            ) {
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
    // messages), over the window fetched above.
    if !query_tokens.is_empty() {
        for (doc, strength) in step3_content_hits(
            &all_entries, "memory.entries", &query_tokens, &doc_freq, STEP3_PER_COLLECTION_CAP,
        ) {
            // The promoted target's content derives from the matched entry,
            // so the match strength rides the redirect.
            if let Some((pdoc, pcoll)) = redirect_if_promoted(&mut store, db, doc, "memory.entries").await {
                insert_collected(&mut collected, &pdoc, &pcoll, "direct_query", strength);
            } else {
                insert_collected(&mut collected, doc, "memory.entries", "direct_query", strength);
            }
        }
    }

    // Step 3c: semantic similarity (KG-02). Takes the slot graph expansion
    // vacated, and fills the role expansion was meant to fill but could not —
    // finding nodes that ARE about the query under different words. Wholly
    // optional: no model, no embeddings, or any error degrades to the lexical
    // result above, silently and by design.
    let mut embedding_candidates = 0usize;
    if !query_text.trim().is_empty()
        && let Some(provider) = crate::embedding::provider(config).await
    {
        {
            crate::embedding::cache::ensure_current(db, provider.as_ref()).await;
            match provider.embed_query(query_text).await {
                Ok(qv) => {
                    let hits = crate::embedding::cache::search(
                        &qv,
                        EMBEDDING_TOP_K,
                        EMBEDDING_MIN_SIMILARITY,
                    )
                    .await;
                    for (coll, id, score) in hits {
                        if let Some(doc) = store.get_or_fetch(db, &coll, &id).await {
                            let before = collected.len();
                            // Source stays `direct_query`: a cosine hit IS a
                            // direct match on meaning, and it must not take the
                            // 0.5 fallback multiplier that made graph expansion
                            // structurally incapable of reaching the top-5.
                            insert_collected(
                                &mut collected,
                                &doc,
                                &coll,
                                "direct_query",
                                similarity_strength(score),
                            );
                            if collected.len() > before {
                                embedding_candidates += 1;
                            }
                        }
                    }

                    // Every candidate the lexical steps found also gets its
                    // cosine, so the semantic signal corrects lexical noise
                    // instead of merely competing with it inside the top-K.
                    let keys: Vec<(String, String)> = collected.keys().cloned().collect();
                    let scored = crate::embedding::cache::score_keys(&qv, &keys).await;
                    for (key, cos) in scored {
                        if let Some(c) = collected.get_mut(&key) {
                            c.similarity = Some(similarity_strength(cos));
                        }
                    }
                }
                Err(e) => tracing::debug!(target: "kg::embedding", "query embedding failed: {e}"),
            }
        }
    }

    // Funnel stats — pre-threshold, pre-truncation (the observability seam).
    let mut stats = funnel_stats(&collected);
    stats.embedding = embedding_candidates;

    // Score and rank; access-touch ONLY what is returned (the 2026-07-04
    // semantics change: access_count = retrieval hits, not BFS sweeps).
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

/// Step-3 admission rule (pure), IDF-weighted since 2026-09-08.
///
/// Admission: a doc must share >= 2 distinct content tokens with the query —
/// or >= 1 when the query itself has <= 2 tokens — AND at least one matched
/// token must be a non-stopword. The stopword clause is the fix for
/// "What is the plan for the code review?" admitting documents on `for,the,
/// what` alone while `plan` and `review` contributed nothing.
///
/// Strength = `sum(idf of matched) / sum(idf of the top-8 query tokens)`,
/// clamped to [0,1]; it feeds the relevance signal via
/// `max(tag_relevance, content_strength)`.
fn content_match(
    doc_tokens: &HashSet<String>,
    query_tokens: &HashSet<String>,
    doc_freq: &DocFreq,
) -> Option<(usize, f64)> {
    if query_tokens.is_empty() {
        return None;
    }
    let matched: Vec<&String> = query_tokens
        .iter()
        .filter(|t| doc_tokens.contains(*t))
        .collect();
    let required = if query_tokens.len() <= 2 { 1 } else { 2 };
    if matched.len() < required {
        return None;
    }
    // Stopwords add strength but never carry admission on their own.
    if !matched.iter().any(|t| !doc_freq.is_stopword(t)) {
        return None;
    }
    let numerator: f64 = matched.iter().map(|t| doc_freq.idf(t)).sum();
    let strength = (numerator / doc_freq.denominator(query_tokens)).clamp(0.0, 1.0);
    Some((matched.len(), strength))
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
    doc_freq: &DocFreq,
    cap: usize,
) -> Vec<(&'a serde_json::Value, f64)> {
    let mut hits: Vec<(usize, &str, &str, &'a serde_json::Value, f64)> = docs
        .iter()
        .filter_map(|doc| {
            let (matched, strength) = content_match(
                &content_tokens(&doc_match_text(doc, collection)),
                query_tokens,
                doc_freq,
            )?;
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

    // `confidence` is deliberately NOT read here (2026-09-08). It was a
    // ranking term until the same measurement wave found it information-free:
    // 0.9 for un-annotated semantic docs and a synthesized 1.0 for the other
    // three collections, i.e. a constant ~0.095 offset every candidate
    // received. The stored document field is untouched.
    let (content, node_type) = match collection {
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
        crate::identity_graph::IDENTITY_COLLECTION => {
            // Identity nodes no longer reach retrieval at all: graph expansion
            // was their only entry point and it is gone (2026-09-08). That is
            // correct — the full sealed graph already rides the system prompt
            // via `operational_mode_graph`, and identity docs carry `tags: []`
            // so they could never produce a tag hit. The arm stays to mirror
            // `node_store::graph_node_from_doc`'s classification.
            let node_type = doc.get("node_type").and_then(|v| v.as_str())
                .unwrap_or("").to_string();
            (
                doc.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                NodeType::Identity { node_type },
            )
        }
        _ => (
            doc.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            NodeType::Episodic,
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
        node_type,
        source: source.to_string(),
        content_strength: content_strength.clamp(0.0, 1.0),
        similarity: None,
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
// `score_one` + `ScoreCtx` are the one scoring core. Weights were retuned
// 2026-09-08 against a production copy, which found relevance carrying only
// 17–36% of the top-5 score while recency routinely hit 1.0 and won.

/// Set-normalization context for the multi-signal score.
struct ScoreCtx {
    /// Tag-relevance denominator: `min(deduped query tokens,
    /// RELEVANCE_DENOM_CAP)` (2026-07-31 fix — was the raw, non-deduped,
    /// stopword-inclusive token count, which diluted a 2-tag hit on a 25-word
    /// message to ~0.04 of the relevance budget).
    tag_denom: f64,
    ts_min: i64,
    ts_range: f64,
    /// <2 distinct parseable timestamps in the candidate set — min-max
    /// normalization carries no ordering information.
    degenerate_recency: bool,
    max_access: f64,
    /// No candidate has been accessed more than once — the access signal
    /// carries no ordering information, same shape as `degenerate_recency`.
    degenerate_access: bool,
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
    let max_access = items.iter().map(|c| c.access_count).max().unwrap_or(0);
    ScoreCtx {
        tag_denom: input_tags.len().clamp(1, RELEVANCE_DENOM_CAP) as f64,
        ts_min,
        ts_range: (ts_max - ts_min).max(1) as f64,
        degenerate_recency,
        max_access: max_access.max(1) as f64,
        degenerate_access: max_access <= 1,
    }
}

fn score_one(c: &Collected, ctx: &ScoreCtx, input_tags: &[String]) -> f64 {
    let matching_tags = c.tags.iter()
        .filter(|t| input_tags.iter().any(|it| it.eq_ignore_ascii_case(t)))
        .count() as f64;
    let tag_relevance = (matching_tags / ctx.tag_denom).min(1.0);
    // Relevance takes the best available evidence, but the two content signals
    // are NOT interchangeable. Lexical overlap is a proxy for aboutness and a
    // biased one — a long document shares more query tokens by sheer length.
    // Cosine measures aboutness directly. So where a node has a vector, its
    // similarity REPLACES the lexical score rather than competing with it via
    // max(); lexical only carries nodes that have no vector (episodic entries,
    // anything not yet backfilled). Measured on production: under max(), a
    // long unrelated node scored lexical 0.687 and beat the node that
    // literally answered the question at cosine-derived 0.529.
    // Tags are kept in the max: they are operator- or model-authored and
    // high-precision, not a proxy for anything.
    let content_signal = c.similarity.unwrap_or(c.content_strength);
    let relevance = tag_relevance.max(content_signal.clamp(0.0, 1.0));

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

    // LOG-scaled since 2026-09-08. Linear `count / max` normalization let one
    // heavily-accessed node flatten every other candidate to ~0.000, so the
    // 0.2 weight was dead across the whole production corpus. Degenerate sets
    // (nothing accessed more than once) score 0.0: absent ordering is not a
    // full mark.
    let access_frequency = if ctx.degenerate_access {
        0.0
    } else {
        ((c.access_count as f64) + 1.0).ln() / (ctx.max_access + 1.0).ln()
    };

    let base = relevance * 0.6 + recency * 0.2 + access_frequency * 0.2;
    // Source-quality multiplier separates direct matches from weaker ones.
    // The `graph_expansion` arm is retained for legibility: nothing produces
    // that label since the step was deleted, and it shares the 0.5 fallback.
    let source_mult = match c.source.as_str() {
        "direct_query" => 1.0,
        "session_based" => 0.75,
        "graph_expansion" => 0.5,
        _ => 0.5,
    };
    base * source_mult
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
mod prefetch_projection_tests {
    //! The prefetch projection is an INCLUSION list, so a field added to the
    //! readers but not to `PREFETCH_FIELDS` degrades silently — the document
    //! arrives without it and every `.unwrap_or` default takes over. This
    //! scans the module's own source for the node fields it reads and fails
    //! if any is missing from the list.
    use super::PREFETCH_FIELDS;

    #[test]
    fn prefetch_projection_covers_every_field_retrieval_reads() {
        let src = include_str!("retrieval.rs");
        // Node-document fields read anywhere in this module. Edge and
        // sub-object keys are excluded: they never come from the prefetch.
        const NOT_FROM_PREFETCH: &[&str] = &[
            "collection",       // promoted_to sub-object + edge docs
            "id",               // promoted_to sub-object
            "target_collection", // edge docs
            "target_id",        // edge docs
            "embedding",        // vector index only, deliberately NOT prefetched
            "embedding_model",
        ];
        let mut missing: Vec<&str> = Vec::new();
        let mut rest = src;
        while let Some(i) = rest.find(".get(\"") {
            rest = &rest[i + 6..];
            let Some(end) = rest.find('"') else { break };
            let field = &rest[..end];
            if NOT_FROM_PREFETCH.contains(&field) {
                continue;
            }
            if !PREFETCH_FIELDS.contains(&field) && !missing.contains(&field) {
                missing.push(field);
            }
        }
        assert!(
            missing.is_empty(),
            "fields read by retrieval but absent from PREFETCH_FIELDS: {missing:?} \
             — add them, or the prefetch returns documents without them"
        );
    }

    #[test]
    fn projection_excludes_the_embedding_vector() {
        // ~2 KB per document that retrieval never reads; similarity runs off
        // the in-process index. If this ever lands in the projection, every
        // turn pays for it.
        assert!(!PREFETCH_FIELDS.contains(&"embedding"));
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
    //! The shared scoring core drives the final ranking — these lock the
    //! documented formula (`relevance*0.6 + recency*0.2 + access*0.2`, with
    //! access log-scaled) and the source multipliers. If one of these fails,
    //! the module doc comment at the top of this file is now wrong too: they
    //! are a matched pair.
    use super::*;

    fn item(
        id: &str,
        tags: &[&str],
        created_at: &str,
        access_count: u64,
        source: &str,
    ) -> Collected {
        Collected {
            collection: "memory.semantic".into(),
            id: id.into(),
            content: format!("content {id}"),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            created_at: created_at.into(),
            access_count,
            node_type: NodeType::Semantic { category: SemanticCategory::Fact },
            source: source.into(),
            content_strength: 0.0,
            similarity: None,
        }
    }

    #[test]
    fn score_items_matches_documented_formula_and_multipliers() {
        let input_tags = vec!["kg".to_string()];
        let a = item("a", &["kg"], "2026-07-04T00:00:00Z", 4, "direct_query");
        let b = item("b", &[], "2026-07-01T00:00:00Z", 2, "session_based");
        let refs = vec![&a, &b];
        let ctx = build_score_ctx(&refs, &input_tags);

        // a: relevance 1/1*0.5 + recency 1.0*0.3 + access ln(5)/ln(5)*0.2 = 1.0
        assert!((score_one(&a, &ctx, &input_tags) - 1.0).abs() < 1e-9);
        // b: 0 + 0 + (ln(3)/ln(5))*0.2 = 0.13657..., session_based x0.75
        let expected_b = (3.0f64.ln() / 5.0f64.ln()) * 0.2 * 0.75;
        assert!((score_one(&b, &ctx, &input_tags) - expected_b).abs() < 1e-9);

        // Source multipliers on otherwise-identical items: 1.0 / 0.75 / 0.5.
        let d = item("d", &["kg"], "2026-07-04T00:00:00Z", 4, "direct_query");
        let s = item("s", &["kg"], "2026-07-04T00:00:00Z", 4, "session_based");
        let u = item("u", &["kg"], "2026-07-04T00:00:00Z", 4, "no_such_source");
        let refs = vec![&d, &s, &u, &b];
        let ctx = build_score_ctx(&refs, &input_tags);
        let ds = score_one(&d, &ctx, &input_tags);
        let ss = score_one(&s, &ctx, &input_tags);
        let us = score_one(&u, &ctx, &input_tags);
        assert!((ss / ds - 0.75).abs() < 1e-9);
        assert!((us / ds - 0.5).abs() < 1e-9, "unknown sources take the 0.5 fallback");
    }

    #[test]
    fn confidence_is_not_a_ranking_term() {
        // Two identical candidates from collections that used to carry
        // different confidences (0.9 semantic vs a synthesized 1.0 elsewhere)
        // must now score identically — the term was a constant offset, not a
        // signal (2026-09-08).
        let input_tags = vec!["kg".to_string()];
        let mut sem = item("sem", &["kg"], "2026-07-04T00:00:00Z", 3, "direct_query");
        sem.collection = "memory.semantic".into();
        let mut epi = item("epi", &["kg"], "2026-07-04T00:00:00Z", 3, "direct_query");
        epi.collection = "memory.entries".into();
        epi.node_type = NodeType::Episodic;
        let older = item("o", &[], "2026-07-01T00:00:00Z", 0, "direct_query");
        let refs = vec![&sem, &epi, &older];
        let ctx = build_score_ctx(&refs, &input_tags);
        assert!(
            (score_one(&sem, &ctx, &input_tags) - score_one(&epi, &ctx, &input_tags)).abs() < 1e-9
        );
    }

    #[test]
    fn access_frequency_is_log_scaled_not_linear() {
        // The production defect: one hub with a huge access_count flattened
        // every other candidate to ~0.000 under linear `count / max`.
        let input_tags: Vec<String> = vec![];
        let hub = item("hub", &[], "2026-07-04T00:00:00Z", 4000, "direct_query");
        let mid = item("mid", &[], "2026-07-04T00:00:00Z", 20, "direct_query");
        let refs = vec![&hub, &mid];
        let ctx = build_score_ctx(&refs, &input_tags);
        // Degenerate recency (one distinct timestamp) contributes 0.5*0.2 to both.
        let mid_access = (score_one(&mid, &ctx, &input_tags) - 0.10) / 0.2;
        let linear = 20.0 / 4000.0;
        assert!(
            (mid_access - (21.0f64.ln() / 4001.0f64.ln())).abs() < 1e-9,
            "must be ln(1+n)/ln(1+max)"
        );
        assert!(
            mid_access > linear * 10.0,
            "log scaling must rescue the mid node from ~0 (linear would be {linear})"
        );
    }

    #[test]
    fn degenerate_access_scores_zero_not_full_marks() {
        // Nothing accessed more than once: the signal carries no ordering, so
        // it contributes nothing rather than a free 0.2 for every candidate.
        let input_tags: Vec<String> = vec![];
        let a = item("a", &[], "2026-07-04T00:00:00Z", 1, "direct_query");
        let b = item("b", &[], "2026-07-01T00:00:00Z", 0, "direct_query");
        let refs = vec![&a, &b];
        let ctx = build_score_ctx(&refs, &input_tags);
        assert!(ctx.degenerate_access);
        // a: 0 relevance + recency 1.0*0.2 + access 0.0 = 0.2
        assert!((score_one(&a, &ctx, &input_tags) - 0.2).abs() < 1e-9);
    }

    #[test]
    fn relevance_takes_max_of_tag_and_content_strength() {
        let input_tags = vec!["kg".to_string()];
        // No tag match, but a strong content match: relevance = 0.75, not 0.
        let mut c = item("c", &[], "2026-07-04T00:00:00Z", 0, "direct_query");
        c.content_strength = 0.75;
        let plain = item("p", &[], "2026-07-01T00:00:00Z", 0, "direct_query");
        let refs = vec![&c, &plain];
        let ctx = build_score_ctx(&refs, &input_tags);
        // c: relevance 0.75*0.6 + recency 1.0*0.2 + access 0 = 0.65
        assert!((score_one(&c, &ctx, &input_tags) - 0.65).abs() < 1e-9);
        // A tag match stronger than the content strength wins the max.
        let mut t = item("t", &["kg"], "2026-07-04T00:00:00Z", 0, "direct_query");
        t.content_strength = 0.25;
        let refs = vec![&t, &plain];
        let ctx = build_score_ctx(&refs, &input_tags);
        // tag_relevance 1/1 = 1.0 > 0.25 → relevance 1.0.
        assert!((score_one(&t, &ctx, &input_tags) - (0.6 + 0.2)).abs() < 1e-9);
    }

    #[test]
    fn tag_denominator_is_deduped_count_capped_at_8() {
        // 12 (already-deduped) query tokens: denominator caps at 8, so a
        // 4-tag hit scores 0.5 of the relevance budget — not 4/12.
        let input_tags: Vec<String> = (0..12).map(|i| format!("tag{i}")).collect();
        let a = item("a", &["tag0", "tag1", "tag2", "tag3"], "2026-07-04T00:00:00Z", 0, "direct_query");
        let old = item("o", &[], "2026-07-01T00:00:00Z", 0, "direct_query");
        let refs = vec![&a, &old];
        let ctx = build_score_ctx(&refs, &input_tags);
        // relevance (4/8)*0.6 = 0.30 + recency 0.2 = 0.50
        assert!((score_one(&a, &ctx, &input_tags) - 0.50).abs() < 1e-9);
    }

    #[test]
    fn degenerate_recency_is_neutral_half_unparseable_zero() {
        let input_tags: Vec<String> = vec![];
        // All candidates share one timestamp — the old code fed RAW epoch
        // seconds (~1.8e9) through; now recency is a neutral 0.5.
        let a = item("a", &[], "2026-07-04T00:00:00Z", 0, "direct_query");
        let b = item("b", &[], "2026-07-04T00:00:00Z", 0, "direct_query");
        let refs = vec![&a, &b];
        let ctx = build_score_ctx(&refs, &input_tags);
        // 0 + 0.5*0.2 + 0 = 0.10 — sane against the 0.3 threshold scale.
        assert!((score_one(&a, &ctx, &input_tags) - 0.10).abs() < 1e-9);

        // Unparseable created_at is NOT neutral — missing data scores 0.
        let bad = item("bad", &[], "not-a-date", 0, "direct_query");
        let refs = vec![&bad, &a];
        let ctx = build_score_ctx(&refs, &input_tags);
        assert!((score_one(&bad, &ctx, &input_tags) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn a_relevance_free_recent_node_cannot_clear_the_enrichment_threshold() {
        // The reweights' operator-visible consequence. Originally a
        // maximally-recent node with ZERO relevance scored 0.3*1.0 +
        // 0.1*confidence = 0.40 and was injected on recency alone. After the
        // scoring wave it sat at exactly 0.30, the threshold itself; with
        // relevance raised to 0.6 it lands at 0.20 — safely below
        // `enrichment::SCORE_THRESHOLD`, which is the point.
        let input_tags: Vec<String> = vec![];
        let newest = item("new", &[], "2026-07-04T00:00:00Z", 0, "direct_query");
        let oldest = item("old", &[], "2026-07-01T00:00:00Z", 0, "direct_query");
        let refs = vec![&newest, &oldest];
        let ctx = build_score_ctx(&refs, &input_tags);
        let s = score_one(&newest, &ctx, &input_tags);
        assert!((s - 0.20).abs() < 1e-9, "got {s}");
        assert!(s < 0.3, "must not clear the enrichment threshold on recency alone");
    }

    #[test]
    fn similarity_replaces_the_lexical_signal_rather_than_competing_with_it() {
        // Measured on production: a long unrelated node scored lexical 0.687
        // and beat the node that literally answered the question, whose
        // cosine-derived strength was 0.529. Lexical overlap is a
        // length-biased proxy for aboutness; cosine measures it directly, so
        // where a vector exists it REPLACES the lexical score.
        let input_tags: Vec<String> = vec![];
        let mut noisy = item("noisy", &[], "2026-07-04T00:00:00Z", 0, "direct_query");
        noisy.content_strength = 0.687;
        noisy.similarity = Some(0.10); // the embedding says: not really related
        let mut answer = item("answer", &[], "2026-07-04T00:00:00Z", 0, "direct_query");
        answer.content_strength = 0.0;
        answer.similarity = Some(0.529);
        let refs = vec![&noisy, &answer];
        let ctx = build_score_ctx(&refs, &input_tags);
        assert!(
            score_one(&answer, &ctx, &input_tags) > score_one(&noisy, &ctx, &input_tags),
            "the semantic signal must override lexical length bias"
        );
        // Without a vector the lexical score still carries the node.
        let mut lexical_only = item("lex", &[], "2026-07-04T00:00:00Z", 0, "direct_query");
        lexical_only.content_strength = 0.8;
        let refs = vec![&lexical_only, &answer];
        let ctx = build_score_ctx(&refs, &input_tags);
        assert!((score_one(&lexical_only, &ctx, &input_tags) - (0.8 * 0.6 + 0.5 * 0.2)).abs() < 1e-9);
    }

    #[test]
    fn tags_still_win_over_a_weak_similarity_score() {
        // Tags are operator- or model-authored and high-precision — they are
        // not a proxy for anything, so they stay in the max().
        let input_tags = vec!["kg".to_string()];
        let mut tagged = item("t", &["kg"], "2026-07-01T00:00:00Z", 0, "direct_query");
        tagged.similarity = Some(0.05);
        let other = item("o", &[], "2026-07-04T00:00:00Z", 0, "direct_query");
        let refs = vec![&tagged, &other];
        let ctx = build_score_ctx(&refs, &input_tags);
        // relevance = max(tag 1.0, similarity 0.05) = 1.0 → 0.6, recency 0.
        assert!((score_one(&tagged, &ctx, &input_tags) - 0.6).abs() < 1e-9);
    }

    #[test]
    fn retrieval_stats_count_sources_pre_truncation() {
        let mut collected = HashMap::new();
        for (id, source) in [
            ("d1", "direct_query"),
            ("d2", "direct_query"),
            ("s1", "session_based"),
            ("u1", "some_unknown_source"),
        ] {
            let c = item(id, &[], "2026-07-04T00:00:00Z", 0, source);
            collected.insert((c.collection.clone(), c.id.clone()), c.clone());
        }
        let stats = funnel_stats(&collected);
        assert_eq!(stats.candidates_total, 4);
        assert_eq!(stats.direct_query, 2);
        assert_eq!(stats.session_based, 1);
        // The historical `graph_expansion` field is now the unknown-source
        // bucket; nothing in retrieval produces that label any more.
        assert_eq!(stats.graph_expansion, 1);
    }
}

#[cfg(test)]
mod content_match_tests {
    use super::super::text::content_tokens;
    use super::*;

    /// A corpus large enough for the stopword gate to be live, in which every
    /// token of interest is rare. Tests that want a stopword add it here.
    fn corpus_where(common: &[&str]) -> Vec<HashSet<String>> {
        (0..100)
            .map(|i| {
                let mut d: HashSet<String> = HashSet::new();
                d.insert(format!("filler{i}"));
                if i < 60 {
                    for c in common {
                        d.insert((*c).to_string());
                    }
                }
                d
            })
            .collect()
    }

    fn df_for(query: &HashSet<String>, common: &[&str]) -> DocFreq {
        DocFreq::build(query, corpus_where(common))
    }

    #[test]
    fn content_match_requires_two_tokens_or_one_for_short_queries() {
        let doc = content_tokens("the cert refresh works after manual generation");
        // 3+-token query: one shared token is not enough…
        let q3 = content_tokens("cert broken tomorrow");
        assert!(content_match(&doc, &q3, &df_for(&q3, &[])).is_none());
        // …two are.
        let q3b = content_tokens("cert refresh broken");
        assert!(content_match(&doc, &q3b, &df_for(&q3b, &[])).is_some());
        // ≤2-token query: a single shared token admits.
        let q1 = content_tokens("cert");
        assert!(content_match(&doc, &q1, &df_for(&q1, &[])).is_some());
        // Empty query never matches.
        let q0 = content_tokens("");
        assert!(content_match(&doc, &q0, &df_for(&q0, &[])).is_none());
    }

    #[test]
    fn stopword_only_matches_are_refused() {
        // The production failure: "What is the plan for the code review?"
        // admitted documents on `for,the,what` alone. Two stopword hits now
        // clear the count but fail admission.
        let doc = content_tokens("the void sessions were archived for the record");
        let query = content_tokens("what the plan for code review");
        let df = df_for(&query, &["the", "for", "what"]);
        assert!(df.is_stopword("the") && df.is_stopword("for"));
        assert!(
            content_match(&doc, &query, &df).is_none(),
            "stopwords alone must not admit"
        );

        // One rare token alongside them does admit.
        let real = content_tokens("the review plan for the code");
        assert!(content_match(&real, &query, &df).is_some());
    }

    #[test]
    fn rare_tokens_outweigh_common_ones_in_strength() {
        let query = content_tokens("the review");
        let df = df_for(&query, &["the"]);
        let common_only = content_tokens("the filler0 padding");
        let rare_only = content_tokens("review filler0 padding");
        // Both admit under the <=2-token rule; "the" is a stopword so it
        // cannot admit alone — use a 1-token comparison via strength instead.
        let (_, rare_strength) = content_match(&rare_only, &query, &df).unwrap();
        assert!(
            content_match(&common_only, &query, &df).is_none(),
            "a lone stopword match is refused"
        );
        // And the rare term carries most of the available weight.
        assert!(rare_strength > 0.5, "got {rare_strength}");
    }

    #[test]
    fn content_match_strength_is_idf_weighted_and_clamped() {
        let doc = content_tokens("alpha beta gamma delta epsilon zeta eta theta iota kappa");
        // 4-token query, 3 matched, all equally rare → 3 of 4 IDF weights.
        let q = content_tokens("alpha beta gamma missing");
        let df = df_for(&q, &[]);
        let (matched, strength) = content_match(&doc, &q, &df).unwrap();
        assert_eq!(matched, 3);
        let expected = (df.idf("alpha") + df.idf("beta") + df.idf("gamma")) / df.denominator(&q);
        assert!((strength - expected).abs() < 1e-9);

        // 10-token query, 9 matched → the denominator caps at 8 of them, so
        // the ratio saturates and clamps to 1.0.
        let q10 = content_tokens("alpha beta gamma delta epsilon zeta eta theta iota missing");
        let df10 = df_for(&q10, &[]);
        let (matched, strength) = content_match(&doc, &q10, &df10).unwrap();
        assert_eq!(matched, 9);
        assert!((strength - 1.0).abs() < 1e-9);
    }

    #[test]
    fn step3_hits_capped_ordered_matchcount_recency_id() {
        let query = content_tokens("cert refresh trustd");
        let df = df_for(&query, &[]);
        let mk = |id: &str, content: &str, created: &str| {
            serde_json::json!({"_id": id, "content": content, "created_at": created})
        };
        let docs = vec![
            mk("weak-old", "cert refresh notes", "2026-06-01T00:00:00Z"),
            mk("strong", "cert refresh trustd pipeline", "2026-06-02T00:00:00Z"),
            mk("weak-new", "cert refresh other", "2026-06-03T00:00:00Z"),
            mk("miss", "unrelated content entirely", "2026-06-04T00:00:00Z"),
        ];
        let hits = step3_content_hits(&docs, "memory.semantic", &query, &df, 2);
        let ids: Vec<&str> = hits.iter().map(|(d, _)| d["_id"].as_str().unwrap()).collect();
        // match-count desc first (strong=3), then recency desc among the
        // 2-token matches (weak-new beats weak-old), capped at 2.
        assert_eq!(ids, vec!["strong", "weak-new"]);
        assert!((hits[0].1 - 1.0).abs() < 1e-9, "3/3 matched saturates the denominator");
    }

    #[test]
    fn step3_procedural_matches_title_and_description() {
        let query = content_tokens("cert refresh procedure");
        let df = df_for(&query, &[]);
        let doc = serde_json::json!({
            "_id": "p1",
            "title": "Cert refresh procedure",
            "description": "regenerate via trustd then restart embra-web",
            "created_at": "2026-06-01T00:00:00Z",
        });
        let docs = vec![doc];
        let hits = step3_content_hits(&docs, "memory.procedural", &query, &df, 10);
        assert_eq!(hits.len(), 1, "title text must be matchable for procedural nodes");
        // Same doc under memory.semantic matches nothing — no `content` field.
        let hits = step3_content_hits(&docs, "memory.semantic", &query, &df, 10);
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
        insert_collected(&mut out, &doc, "memory.semantic", "session_based", 0.8);
        let key = ("memory.semantic".to_string(), "n1".to_string());
        let c = &out[&key];
        assert_eq!(c.source, "direct_query", "first write wins the source");
        assert!((c.content_strength - 0.8).abs() < 1e-9, "strength max-merges");
        // A weaker later strength never downgrades.
        insert_collected(&mut out, &doc, "memory.semantic", "session_based", 0.2);
        assert!((out[&key].content_strength - 0.8).abs() < 1e-9);
    }
}


