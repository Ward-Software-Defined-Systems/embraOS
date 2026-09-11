//! Process-wide vector index for the promoted-node collections.
//!
//! Brute-force cosine over the whole corpus, deliberately: measured on
//! production (1,128 semantic docs) a full scan costs well under a
//! millisecond, against ~19 ms for the query embedding that precedes it. An
//! ANN index would optimize the cheapest step in the pipeline while adding a
//! WardSONDB fork divergence — the KG-02 spec's §9.2 is descoped for exactly
//! that reason, and the corpus would need to grow ~100x before it changes.
//!
//! Vectors live here rather than riding retrieval's document prefetch because
//! that prefetch window is `MEMORY_FETCH_WINDOW` (10,000): inline vectors
//! would put ~20 MB on the wire every single turn at the window's ceiling.
//! At 384-d the whole index is ~1.5 KB per node — 3.5 MB for a 2,300-node
//! corpus.

use std::collections::HashMap;

use tokio::sync::{OnceCell, RwLock};

use crate::db::WardsonDbClient;

use super::{decode_vector, EmbeddingProvider};

/// Collections carrying embeddings. `memory.entries` is deliberately absent
/// (KG-02 spec §4.3): high volume, noisy, and the entries that matter are
/// promoted — and embedded — as semantic or procedural nodes.
pub const EMBEDDED_COLLECTIONS: [&str; 2] = ["memory.semantic", "memory.procedural"];

/// Stored-vector fields. Nothing else is fetched: the index needs the vector
/// and the model that produced it, never the document body.
const VECTOR_FIELDS: [&str; 2] = ["embedding", "embedding_model"];

#[derive(Default)]
pub struct VectorIndex {
    vecs: HashMap<(String, String), Vec<f32>>,
    /// Model the loaded vectors were produced by. A change means every vector
    /// on disk is stale, so the index empties rather than mixing spaces.
    model: String,
    /// Per-collection document count observed at load. Divergence means
    /// documents were inserted or deleted outside this process (another boot,
    /// a seed reconcile, a TTL reap) and the index needs a reload.
    counts: HashMap<String, u64>,
    loaded: bool,
}

static INDEX: OnceCell<RwLock<VectorIndex>> = OnceCell::const_new();

async fn index() -> &'static RwLock<VectorIndex> {
    INDEX.get_or_init(|| async { RwLock::new(VectorIndex::default()) }).await
}

/// Current document counts for the embedded collections.
async fn live_counts(db: &WardsonDbClient) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    for coll in EMBEDDED_COLLECTIONS {
        if let Ok(n) = db.count(coll).await {
            out.insert(coll.to_string(), n);
        }
    }
    out
}

/// Load, or reload when the corpus changed underneath us. Cheap in the steady
/// state: two server-side counts, no document transfer.
pub async fn ensure_current(db: &WardsonDbClient, provider: &dyn EmbeddingProvider) {
    let counts = live_counts(db).await;
    {
        let idx = index().await.read().await;
        if idx.loaded && idx.model == provider.model_id() && idx.counts == counts {
            return;
        }
    }

    let dim = provider.dimensions();
    let model = provider.model_id().to_string();
    let mut vecs: HashMap<(String, String), Vec<f32>> = HashMap::new();
    let mut skipped_other_model = 0usize;
    let mut unembedded = 0usize;

    for coll in EMBEDDED_COLLECTIONS {
        let docs = db
            .fetch_recent_with_fields(coll, crate::db::MEMORY_FETCH_WINDOW, Some(&VECTOR_FIELDS))
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(target: "kg::embedding", "vector load of {coll} failed: {e}");
                Vec::new()
            });
        for doc in &docs {
            let Some(id) = doc.get("_id").and_then(|v| v.as_str()) else { continue };
            let Some(b64) = doc.get("embedding").and_then(|v| v.as_str()) else {
                unembedded += 1;
                continue;
            };
            // A vector from another model must never be scored against this
            // one — the spaces are unrelated. Backfill re-embeds these.
            if doc.get("embedding_model").and_then(|v| v.as_str()) != Some(model.as_str()) {
                skipped_other_model += 1;
                continue;
            }
            match decode_vector(b64, dim) {
                Some(v) => {
                    vecs.insert((coll.to_string(), id.to_string()), v);
                }
                None => skipped_other_model += 1,
            }
        }
    }

    let mut idx = index().await.write().await;
    tracing::info!(
        target: "kg::embedding",
        vectors = vecs.len(),
        unembedded,
        skipped_other_model,
        model = %model,
        "vector index loaded"
    );
    idx.vecs = vecs;
    idx.model = model;
    idx.counts = counts;
    idx.loaded = true;
}

/// Cosine search over the whole index. Returns `(collection, id, score)`
/// above `min_similarity`, best first, capped at `top_k`.
pub async fn search(query: &[f32], top_k: usize, min_similarity: f32) -> Vec<(String, String, f32)> {
    let idx = index().await.read().await;
    let mut hits: Vec<(String, String, f32)> = idx
        .vecs
        .iter()
        .filter_map(|((coll, id), v)| {
            let s = super::cosine(query, v);
            (s >= min_similarity).then(|| (coll.clone(), id.clone(), s))
        })
        .collect();
    hits.sort_by(|a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            // Deterministic among exact ties, as everywhere else in the KG.
            .then_with(|| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())))
    });
    hits.truncate(top_k);
    hits
}

/// Write-through from the embed-on-write paths, so a freshly embedded node is
/// searchable on the very next turn without waiting for a count divergence.
///
/// `new_document` says whether the node was just CREATED (promotion, seed
/// insert) or merely re-embedded (backfill, `knowledge_update`, merge). The
/// index tracks per-collection document counts to detect out-of-band change,
/// and only a new document moves that count; bumping it on a re-embed — the
/// original behaviour — desynced the count after every backfill and forced a
/// full reload on the next turn.
pub async fn upsert(collection: &str, id: &str, vector: Vec<f32>, model: &str, new_document: bool) {
    let mut idx = index().await.write().await;
    // Only meaningful once loaded and only for the model in play; otherwise
    // the next `ensure_current` picks it up from disk anyway.
    if idx.loaded && idx.model == model {
        idx.vecs.insert((collection.to_string(), id.to_string()), vector);
        if new_document {
            *idx.counts.entry(collection.to_string()).or_insert(0) += 1;
        }
    }
}

/// Drop a vector whose node is gone (merge loser, deletion).
pub async fn remove(collection: &str, id: &str) {
    let mut idx = index().await.write().await;
    if idx.vecs.remove(&(collection.to_string(), id.to_string())).is_some()
        && let Some(c) = idx.counts.get_mut(collection)
    {
        *c = c.saturating_sub(1);
    }
}

/// Cosine for a specific set of candidates, for those that have a vector.
/// Cheap — a few hundred 384-wide dot products — and it lets the caller score
/// candidates that lexical steps found, not only the ones similarity search
/// surfaced on its own.
pub async fn score_keys(
    query: &[f32],
    keys: &[(String, String)],
) -> HashMap<(String, String), f32> {
    let idx = index().await.read().await;
    keys.iter()
        .filter_map(|k| idx.vecs.get(k).map(|v| (k.clone(), super::cosine(query, v))))
        .collect()
}

/// `(vectors, model)` for operator-facing status — AFTER bringing the index
/// current. The index loads lazily on first retrieval, so a status read taken
/// before any turn on a fresh boot would otherwise honestly report an empty
/// index as "0 vectors" and look like a lost backfill. Taking the loader here
/// makes that misreport impossible to reintroduce.
pub async fn stats(db: &WardsonDbClient, provider: &dyn EmbeddingProvider) -> (usize, String) {
    ensure_current(db, provider).await;
    let idx = index().await.read().await;
    (idx.vecs.len(), idx.model.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn search_ranks_by_cosine_and_respects_threshold_and_cap() {
        {
            let mut idx = index().await.write().await;
            idx.loaded = true;
            idx.model = "m".into();
            // Unit vectors in 2-D so the cosines are exact and obvious.
            idx.vecs.insert(("c".into(), "same".into()), vec![1.0, 0.0]);
            idx.vecs.insert(("c".into(), "half".into()), vec![0.6, 0.8]);
            idx.vecs.insert(("c".into(), "orth".into()), vec![0.0, 1.0]);
        }
        let q = vec![1.0f32, 0.0];

        let hits = search(&q, 10, 0.5).await;
        let ids: Vec<&str> = hits.iter().map(|(_, id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["same", "half"], "orthogonal is below the threshold");
        assert!((hits[0].2 - 1.0).abs() < 1e-6);
        assert!((hits[1].2 - 0.6).abs() < 1e-6);

        // top_k truncates after ranking, not before.
        let hits = search(&q, 1, 0.0).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "same");

        // A threshold above every score returns nothing rather than a best guess.
        assert!(search(&q, 10, 0.99999).await.len() == 1);
        assert!(search(&q, 10, 1.5).await.is_empty());

        let mut idx = index().await.write().await;
        idx.vecs.clear();
        idx.loaded = false;
        idx.model.clear();
    }
}
