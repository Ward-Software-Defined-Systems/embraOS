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
/// (KG-02 spec §4.3): an entry is promoted when it is written, and its text
/// is embedded once, on its semantic or procedural node.
pub const EMBEDDED_COLLECTIONS: [&str; 2] = ["memory.semantic", "memory.procedural"];

/// Stored-vector fields. Nothing else is fetched: the index needs the vector
/// and the model that produced it, never the document body.
const VECTOR_FIELDS: [&str; 2] = ["embedding", "embedding_model"];

/// What a failed embedding leaves behind for the status surfaces.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EmbeddingFailure {
    /// RFC 3339.
    pub at: String,
    /// `<collection>:<id>` for a document, `query` for a turn's query.
    pub subject: String,
    pub reason: String,
}

/// Embedding failures since boot, for `/embeddings` and `system_status`.
/// A failure never fails a write — the node is saved without a vector and
/// backfill retries it — and a query failure degrades the turn to lexical
/// retrieval; both were WARN or debug lines only, and a broken model or
/// missing weights shipped unembedded nodes with nothing on any status
/// surface (Embra#16, 2b).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EmbeddingFailures {
    pub write: u64,
    pub query: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<EmbeddingFailure>,
}

/// Which counter a failure moves.
#[derive(Debug, Clone, Copy)]
pub enum FailureKind {
    /// Embedding or storing a document's vector.
    Write,
    /// Embedding a retrieval query.
    Query,
}

/// A write-through that landed while a reload was in flight (#57).
enum Touch {
    Upsert { vector: Vec<f32>, new_document: bool },
    Remove,
}

/// What a reload fetched from disk, taken by `commit_reload`.
struct Loaded {
    vecs: HashMap<(String, String), Vec<f32>>,
    model: String,
    counts: HashMap<String, u64>,
}

#[derive(Default)]
pub struct VectorIndex {
    vecs: HashMap<(String, String), Vec<f32>>,
    /// Process memory, never reloaded: `ensure_current` assigns the other
    /// fields in place and leaves these.
    failures: EmbeddingFailures,
    /// Model the loaded vectors were produced by. A change means every vector
    /// on disk is stale, so the index empties rather than mixing spaces.
    model: String,
    /// Per-collection document count observed at load. Divergence means
    /// documents were inserted or deleted outside this process (another boot,
    /// a seed reconcile, a TTL reap) and the index needs a reload.
    counts: HashMap<String, u64>,
    loaded: bool,
    /// A reload is between its counts snapshot and its commit. The fetch
    /// runs with no lock held, so a write-through can land meanwhile; it is
    /// recorded in `touched` and merged at the commit, which replaced the
    /// map wholesale and dropped it before (#57). Outside a reload nothing
    /// is recorded.
    reloading: bool,
    touched: HashMap<(String, String), Touch>,
}

impl VectorIndex {
    /// A reload is starting: the write-throughs from here are recorded
    /// until `commit_reload`.
    fn begin_reload(&mut self) {
        self.reloading = true;
        self.touched.clear();
    }

    /// The write-through of `upsert`, on the index held.
    fn apply_upsert(&mut self, collection: &str, id: &str, vector: Vec<f32>, model: &str, new_document: bool) {
        // Only meaningful once loaded and only for the model in play;
        // otherwise the next `ensure_current` picks it up from disk anyway.
        if !(self.loaded && self.model == model) {
            return;
        }
        let key = (collection.to_string(), id.to_string());
        if self.reloading {
            self.touched.insert(key.clone(), Touch::Upsert { vector: vector.clone(), new_document });
        }
        self.vecs.insert(key, vector);
        if new_document {
            *self.counts.entry(collection.to_string()).or_insert(0) += 1;
        }
    }

    /// The write-through of `remove`, on the index held.
    fn apply_remove(&mut self, collection: &str, id: &str) {
        let key = (collection.to_string(), id.to_string());
        if self.reloading && (self.vecs.contains_key(&key) || self.touched.contains_key(&key)) {
            self.touched.insert(key.clone(), Touch::Remove);
        }
        if self.vecs.remove(&key).is_some()
            && let Some(c) = self.counts.get_mut(collection)
        {
            *c = c.saturating_sub(1);
        }
    }

    /// Take what the reload fetched, with the write-throughs of its window
    /// on top: a vector embedded meanwhile stays searchable, a node forgotten
    /// meanwhile stays gone, and the counts move with them. A document
    /// created before the counts snapshot and embedded after it is counted
    /// once more than the snapshot holds; the next `ensure_current` reads the
    /// divergence and reloads, as it does for any count that moved outside
    /// this process.
    fn commit_reload(&mut self, loaded: Loaded) {
        let Loaded { mut vecs, model, mut counts } = loaded;
        if self.loaded && self.model == model {
            for (key, touch) in self.touched.drain() {
                match touch {
                    Touch::Upsert { vector, new_document } => {
                        vecs.insert(key.clone(), vector);
                        if new_document {
                            *counts.entry(key.0).or_insert(0) += 1;
                        }
                    }
                    Touch::Remove => {
                        if vecs.remove(&key).is_some()
                            && let Some(c) = counts.get_mut(&key.0)
                        {
                            *c = c.saturating_sub(1);
                        }
                    }
                }
            }
        }
        self.touched.clear();
        self.reloading = false;
        self.vecs = vecs;
        self.model = model;
        self.counts = counts;
        self.loaded = true;
    }
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
    // The fetch below runs with no lock held; the write-throughs that land
    // meanwhile are recorded from here and merged at the commit (#57).
    index().await.write().await.begin_reload();

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

    tracing::info!(
        target: "kg::embedding",
        vectors = vecs.len(),
        unembedded,
        skipped_other_model,
        model = %model,
        "vector index loaded"
    );
    index().await.write().await.commit_reload(Loaded { vecs, model, counts });
}

/// Cosine search over the whole index. Returns `(collection, id, score)`
/// above `min_similarity`, best first, capped at `top_k`.
pub async fn search(query: &[f32], top_k: usize, min_similarity: f32) -> Vec<(String, String, f32)> {
    let idx = index().await.read().await;
    scan(&idx, query, top_k, min_similarity, None)
}

/// The one cosine pass behind `search` and `neighbors`: every vector of the
/// index against `query`, those at or above `min_similarity` kept, `exclude`
/// (a node's own key) left out, ranked best first under `top_k`. Brute
/// force by decision (`embedding/mod.rs`).
fn scan(
    idx: &VectorIndex,
    query: &[f32],
    top_k: usize,
    min_similarity: f32,
    exclude: Option<&(String, String)>,
) -> Vec<(String, String, f32)> {
    let hits: Vec<(String, String, f32)> = idx
        .vecs
        .iter()
        .filter(|(key, _)| exclude != Some(*key))
        .filter_map(|((coll, id), v)| {
            let s = super::cosine(query, v);
            (s >= min_similarity).then(|| (coll.clone(), id.clone(), s))
        })
        .collect();
    ranked(hits, top_k)
}

/// Best first, capped at `top_k`.
fn ranked(mut hits: Vec<(String, String, f32)>, top_k: usize) -> Vec<(String, String, f32)> {
    hits.sort_by(|a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            // Deterministic among exact ties, as everywhere else in the KG.
            .then_with(|| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())))
    });
    hits.truncate(top_k);
    hits
}

/// The nearest nodes of one node: its own vector against the rest of the
/// index, itself left out, ranked as `search` ranks. No inference — the
/// vector is the one the write path just stored. `None` when the node has no
/// vector here: it was not embedded, or the index is not loaded.
pub async fn neighbors(
    collection: &str,
    id: &str,
    top_k: usize,
    min_similarity: f32,
) -> Option<Vec<(String, String, f32)>> {
    let idx = index().await.read().await;
    let key = (collection.to_string(), id.to_string());
    let own = idx.vecs.get(&key)?;
    Some(scan(&idx, own, top_k, min_similarity, Some(&key)))
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
    index().await.write().await.apply_upsert(collection, id, vector, model, new_document);
}

/// Count a failed embedding and keep it as the last one seen.
pub async fn record_failure(kind: FailureKind, subject: &str, reason: &str) {
    let mut idx = index().await.write().await;
    match kind {
        FailureKind::Write => idx.failures.write += 1,
        FailureKind::Query => idx.failures.query += 1,
    }
    idx.failures.last = Some(EmbeddingFailure {
        at: chrono::Utc::now().to_rfc3339(),
        subject: subject.to_string(),
        reason: reason.to_string(),
    });
}

/// The failures since boot. No loader and no database: this reads process
/// memory, so a status surface can show it before the index has loaded.
pub async fn failures() -> EmbeddingFailures {
    index().await.read().await.failures.clone()
}

/// Drop a vector whose node is gone (merge loser, deletion).
pub async fn remove(collection: &str, id: &str) {
    index().await.write().await.apply_remove(collection, id);
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

    /// The index is one process-wide static; every test that writes it
    /// holds this lock so two of them never interleave.
    static INDEX_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// #57: a reload fetches with no lock held; a vector embedded and a node
    /// forgotten in that window survive the commit, and the counts move
    /// with them. Without a write in the window the fetch is taken as it
    /// is, and nothing is recorded outside a reload.
    #[test]
    fn an_upsert_during_a_reload_survives_the_reload() {
        let key = |id: &str| ("memory.semantic".to_string(), id.to_string());
        let fetched = || Loaded {
            vecs: [(key("a"), vec![1.0]), (key("b"), vec![1.0])].into_iter().collect(),
            model: "m".into(),
            counts: [("memory.semantic".to_string(), 2u64)].into_iter().collect(),
        };
        let mut idx = VectorIndex { loaded: true, model: "m".into(), ..Default::default() };
        idx.commit_reload(fetched());

        idx.begin_reload();
        idx.apply_upsert("memory.semantic", "z", vec![0.5], "m", true);
        idx.apply_remove("memory.semantic", "b");
        idx.commit_reload(fetched());
        assert_eq!(idx.vecs.get(&key("z")), Some(&vec![0.5]));
        assert!(idx.vecs.contains_key(&key("a")));
        assert!(!idx.vecs.contains_key(&key("b")));
        assert_eq!(idx.counts["memory.semantic"], 2, "two fetched, one embedded, one forgotten");
        assert!(!idx.reloading && idx.touched.is_empty());

        idx.begin_reload();
        idx.commit_reload(fetched());
        assert_eq!(idx.vecs.len(), 2);
        assert_eq!(idx.counts["memory.semantic"], 2);

        idx.apply_upsert("memory.semantic", "q", vec![0.1], "m", true);
        assert!(idx.touched.is_empty(), "nothing is recorded outside a reload");
        assert_eq!(idx.counts["memory.semantic"], 3);
    }

    /// `knowledge_unlink_node` and the merge forget the vector of a node they
    /// deleted: the key is gone and the collection's count moves down with
    /// it, so the next `ensure_current` sees no divergence to reload over.
    #[tokio::test]
    async fn forgetting_a_node_drops_its_vector_and_moves_the_collection_count_down() {
        let _guard = INDEX_TEST_LOCK.lock().await;
        {
            let mut idx = index().await.write().await;
            idx.loaded = true;
            idx.model = "m".into();
            idx.vecs.insert(("unlink-test".into(), "gone".into()), vec![0.0, 0.0, 1.0]);
            idx.counts.insert("unlink-test".into(), 1);
        }
        let key = ("unlink-test".to_string(), "gone".to_string());
        let q = [0.0f32, 0.0, 1.0];
        assert_eq!(score_keys(&q, std::slice::from_ref(&key)).await.len(), 1);

        remove("unlink-test", "gone").await;
        assert!(score_keys(&q, std::slice::from_ref(&key)).await.is_empty());
        assert_eq!(index().await.read().await.counts.get("unlink-test").copied(), Some(0));

        // Forgetting a node twice is harmless: nothing to drop, nothing to count.
        remove("unlink-test", "gone").await;
        assert_eq!(index().await.read().await.counts.get("unlink-test").copied(), Some(0));

        let mut idx = index().await.write().await;
        idx.counts.remove("unlink-test");
        idx.loaded = false;
        idx.model.clear();
    }

    /// Each failure moves its counter once and becomes the last one shown.
    /// Deltas, because the counters are process-wide.
    #[tokio::test]
    async fn a_recorded_failure_counts_once_and_is_the_last_one_shown() {
        let _guard = INDEX_TEST_LOCK.lock().await;
        let before = failures().await;
        record_failure(FailureKind::Write, "memory.semantic:n1", "model not loaded").await;
        let after = failures().await;
        assert_eq!(after.write - before.write, 1);
        assert_eq!(after.query, before.query);
        let last = after.last.expect("the failure is kept");
        assert_eq!(last.subject, "memory.semantic:n1");
        assert_eq!(last.reason, "model not loaded");
        assert!(chrono::DateTime::parse_from_rfc3339(&last.at).is_ok(), "{}", last.at);
    }

    /// A query that could not be embedded is a different remedy from a
    /// document that could not: the two are counted apart.
    #[tokio::test]
    async fn write_and_query_failures_are_counted_apart() {
        let _guard = INDEX_TEST_LOCK.lock().await;
        let before = failures().await;
        record_failure(FailureKind::Query, "query", "inference failed").await;
        record_failure(FailureKind::Query, "query", "inference failed").await;
        let after = failures().await;
        assert_eq!(after.query - before.query, 2);
        assert_eq!(after.write, before.write);
        assert_eq!(after.last.map(|l| l.subject), Some("query".to_string()));
    }

    #[tokio::test]
    async fn search_ranks_by_cosine_and_respects_threshold_and_cap() {
        let _guard = INDEX_TEST_LOCK.lock().await;
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

    #[tokio::test]
    async fn neighbors_leave_the_node_itself_out_and_rank_like_search() {
        let _guard = INDEX_TEST_LOCK.lock().await;
        {
            let mut idx = index().await.write().await;
            idx.loaded = true;
            idx.model = "m".into();
            idx.vecs.insert(("c".into(), "new".into()), vec![1.0, 0.0]);
            idx.vecs.insert(("c".into(), "twin".into()), vec![1.0, 0.0]);
            idx.vecs.insert(("c".into(), "half".into()), vec![0.6, 0.8]);
            idx.vecs.insert(("d".into(), "orth".into()), vec![0.0, 1.0]);
        }

        let near = neighbors("c", "new", 10, 0.5).await.expect("the node has a vector");
        let ids: Vec<&str> = near.iter().map(|(_, id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["twin", "half"], "itself is left out; orthogonal is below the floor");
        assert!((near[0].2 - 1.0).abs() < 1e-6);
        assert!((near[1].2 - 0.6).abs() < 1e-6);
        // The cap applies after ranking.
        assert_eq!(neighbors("c", "new", 1, 0.0).await.expect("a vector")[0].1, "twin");
        // A node with a vector and nothing near it: an empty list, not `None`.
        assert_eq!(neighbors("d", "orth", 10, 0.9).await, Some(Vec::new()));

        let mut idx = index().await.write().await;
        idx.vecs.clear();
        idx.loaded = false;
        idx.model.clear();
    }

    #[tokio::test]
    async fn a_node_without_a_vector_has_no_neighbors() {
        let _guard = INDEX_TEST_LOCK.lock().await;
        assert_eq!(neighbors("c", "never-embedded", 10, 0.0).await, None);
    }
}
