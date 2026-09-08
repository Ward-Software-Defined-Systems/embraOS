//! Corpus backfill for `/embeddings backfill`.
//!
//! Operator-triggered, never automatic. It is a bounded local CPU job — no API
//! cost since inference runs in-OS — but a multi-minute foreground one
//! (measured 53 ms/document, ~2 minutes for a 2,300-node corpus), so it starts
//! when the operator says so and reports progress while it runs.
//!
//! Resumable by construction: the work set is re-derived on every run from
//! what is actually on disk, so an interrupted pass simply resumes.

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::cache::EMBEDDED_COLLECTIONS;
use super::write::embed_text;
use super::{cache, encode_vector};

/// Documents embedded per progress report.
const PROGRESS_EVERY: usize = 100;

#[derive(Debug, Default, Clone, Copy)]
pub struct BackfillReport {
    pub embedded: usize,
    pub failed: usize,
    pub skipped_empty: usize,
    pub already_current: usize,
}

/// What a backfill would do, without doing it: `(needing_work, total)`.
pub async fn survey(db: &WardsonDbClient, model: &str, force: bool) -> (usize, usize) {
    let mut needed = 0usize;
    let mut total = 0usize;
    for coll in EMBEDDED_COLLECTIONS {
        let docs = db
            .fetch_recent_with_fields(
                coll,
                crate::db::MEMORY_FETCH_WINDOW,
                Some(&["embedding_model"]),
            )
            .await
            .unwrap_or_default();
        total += docs.len();
        for d in &docs {
            if force || !is_current(d, model) {
                needed += 1;
            }
        }
    }
    (needed, total)
}

/// A document is current when it carries a vector produced by THIS model.
/// A model change invalidates every vector: the spaces are unrelated.
fn is_current(doc: &serde_json::Value, model: &str) -> bool {
    doc.get("embedding_model").and_then(|v| v.as_str()) == Some(model)
        && doc.get("embedding").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty())
}

/// Embed everything that needs it. `progress` is invoked with a human-readable
/// line every `PROGRESS_EVERY` documents so the operator sees movement.
pub async fn run<F, Fut>(
    db: &WardsonDbClient,
    cfg: &SystemConfig,
    force: bool,
    progress: F,
) -> Result<BackfillReport, String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let provider = super::provider(cfg)
        .await
        .ok_or_else(|| "no embedding model available — see /embeddings status".to_string())?;
    let model = provider.model_id().to_string();
    let mut report = BackfillReport::default();

    for coll in EMBEDDED_COLLECTIONS {
        // Full documents here (unlike the retrieval hot path): the embeddable
        // text is exactly what we need, and this runs once, not per turn.
        let docs = db
            .fetch_recent(coll, crate::db::MEMORY_FETCH_WINDOW)
            .await
            .map_err(|e| format!("reading {coll} failed: {e}"))?;

        for doc in &docs {
            let Some(id) = doc.get("_id").and_then(|v| v.as_str()) else { continue };
            if !force && is_current(doc, &model) {
                report.already_current += 1;
                continue;
            }
            let text = embed_text(doc, coll);
            if text.trim().is_empty() {
                report.skipped_empty += 1;
                continue;
            }
            match provider.embed_query_or_document(&text).await {
                Ok(vector) => {
                    let patch = serde_json::json!({
                        "embedding": encode_vector(&vector),
                        "embedding_model": model,
                        "embedding_updated_at": chrono::Utc::now().to_rfc3339(),
                    });
                    if let Err(e) = db.patch_document(coll, id, &patch).await {
                        tracing::warn!(target: "kg::embedding", "backfill store {coll}:{id}: {e}");
                        report.failed += 1;
                    } else {
                        cache::upsert(coll, id, vector, &model).await;
                        report.embedded += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "kg::embedding", "backfill embed {coll}:{id}: {e}");
                    report.failed += 1;
                }
            }
            let done = report.embedded + report.failed;
            if done > 0 && done % PROGRESS_EVERY == 0 {
                progress(format!(
                    "  … {} embedded, {} failed (in {coll})",
                    report.embedded, report.failed
                ))
                .await;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn currency_requires_both_a_vector_and_a_matching_model() {
        assert!(is_current(&json!({"embedding": "AAAA", "embedding_model": "m"}), "m"));
        // A different model's vector is not current: the spaces are unrelated.
        assert!(!is_current(&json!({"embedding": "AAAA", "embedding_model": "other"}), "m"));
        // A recorded model with no vector is not current either.
        assert!(!is_current(&json!({"embedding_model": "m"}), "m"));
        assert!(!is_current(&json!({"embedding": "", "embedding_model": "m"}), "m"));
        assert!(!is_current(&json!({}), "m"));
    }
}
