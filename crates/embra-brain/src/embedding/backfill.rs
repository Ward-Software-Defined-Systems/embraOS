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
use super::local::INFERENCE_SLOTS;
use super::write::embed_text;
use super::{cache, encode_vector, EmbeddingProvider};

/// Documents embedded per progress report.
const PROGRESS_EVERY: usize = 100;

#[derive(Debug, Default, Clone, Copy)]
pub struct BackfillReport {
    pub embedded: usize,
    pub failed: usize,
    pub skipped_empty: usize,
    pub already_current: usize,
}

/// Fields the coverage survey fetches. An INCLUSION list, and it MUST cover
/// every field `is_current` reads: a projected document simply lacks any
/// field not named here, so a missing one makes `is_current` return false
/// for every node and the operator sees `0/N embedded` forever — which is
/// exactly the defect this shipped with (the list held only `embedding_model`
/// while `is_current` also required `embedding`). Pinned by
/// `survey_projection_covers_every_field_is_current_reads`.
const SURVEY_FIELDS: [&str; 2] = ["embedding", "embedding_model"];

/// How much of each embedded collection carries a current vector.
#[derive(Debug, Default, Clone)]
pub struct Coverage {
    /// `(collection, embedded, total)` in `EMBEDDED_COLLECTIONS` order.
    pub per_collection: Vec<(&'static str, usize, usize)>,
}

impl Coverage {
    pub fn embedded(&self) -> usize {
        self.per_collection.iter().map(|c| c.1).sum()
    }
    pub fn total(&self) -> usize {
        self.per_collection.iter().map(|c| c.2).sum()
    }
    pub fn needed(&self) -> usize {
        self.total().saturating_sub(self.embedded())
    }
}

/// Truthful coverage, read from disk. Reports what IS embedded; callers that
/// want to re-embed everything (`--force`) decide that themselves.
pub async fn survey(db: &WardsonDbClient, model: &str) -> Coverage {
    let mut cov = Coverage::default();
    for coll in EMBEDDED_COLLECTIONS {
        let docs = db
            .fetch_recent_with_fields(coll, crate::db::MEMORY_FETCH_WINDOW, Some(&SURVEY_FIELDS))
            .await
            .unwrap_or_default();
        let embedded = docs.iter().filter(|d| is_current(d, model)).count();
        cov.per_collection.push((coll, embedded, docs.len()));
    }
    cov
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
    run_with(db, provider.as_ref(), force, progress).await
}

/// `run` on the provider given, so a test can hand it one of its own.
pub(crate) async fn run_with<F, Fut>(
    db: &WardsonDbClient,
    provider: &dyn EmbeddingProvider,
    force: bool,
    progress: F,
) -> Result<BackfillReport, String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let model = provider.model_id().to_string();
    let mut report = BackfillReport::default();

    for coll in EMBEDDED_COLLECTIONS {
        // Full documents here (unlike the retrieval hot path): the embeddable
        // text is exactly what we need, and this runs once, not per turn.
        let docs = db
            .fetch_recent(coll, crate::db::MEMORY_FETCH_WINDOW)
            .await
            .map_err(|e| format!("reading {coll} failed: {e}"))?;

        let pending: Vec<(&str, String)> = docs
            .iter()
            .filter_map(|doc| {
                let id = doc.get("_id").and_then(|v| v.as_str())?;
                if !force && is_current(doc, &model) {
                    report.already_current += 1;
                    return None;
                }
                let text = embed_text(doc, coll);
                if text.trim().is_empty() {
                    report.skipped_empty += 1;
                    return None;
                }
                Some((id, text))
            })
            .collect();

        // The provider runs `INFERENCE_SLOTS` forward passes at once; one
        // document awaited at a time left the second slot idle for the
        // whole run (#44). Each chunk embeds in parallel, in order, then
        // its vectors are stored one by one as before.
        for chunk in pending.chunks(INFERENCE_SLOTS) {
            let outcomes =
                futures::future::join_all(chunk.iter().map(|(_, text)| provider.embed_query_or_document(text))).await;
            for ((id, _), outcome) in chunk.iter().zip(outcomes) {
                let id: &str = id;
                match outcome {
                    Ok(vector) => {
                        let patch = serde_json::json!({
                            "embedding": encode_vector(&vector),
                            "embedding_model": model,
                            "embedding_updated_at": chrono::Utc::now().to_rfc3339(),
                        });
                        if let Err(e) = db.patch_document(coll, id, &patch).await {
                            tracing::warn!(target: "kg::embedding", "backfill store {coll}:{id}: {e}");
                            cache::record_failure(cache::FailureKind::Write, &format!("{coll}:{id}"), &e.to_string())
                                .await;
                            report.failed += 1;
                        } else {
                            cache::upsert(coll, id, vector, &model, false).await;
                            report.embedded += 1;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "kg::embedding", "backfill embed {coll}:{id}: {e}");
                        cache::record_failure(cache::FailureKind::Write, &format!("{coll}:{id}"), &e.to_string())
                            .await;
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
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A provider that counts how many forward passes are in flight at once
    /// and keeps the highest count seen.
    struct Counting {
        in_flight: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for Counting {
        async fn embed_query(&self, text: &str) -> Result<Vec<f32>, super::super::EmbeddingError> {
            self.embed_query_or_document(text).await
        }

        async fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, super::super::EmbeddingError> {
            use std::sync::atomic::Ordering;
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(texts.iter().map(|_| vec![0.5, 0.5]).collect())
        }

        fn model_id(&self) -> &str {
            "counting"
        }

        fn dimensions(&self) -> usize {
            2
        }
    }

    /// #44: the backfill embeds `INFERENCE_SLOTS` documents at once and
    /// stores every one of them; the order of the patches follows the
    /// documents.
    #[tokio::test]
    async fn the_backfill_fills_both_inference_slots_and_stores_each_vector() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let docs: Vec<serde_json::Value> =
            (1..=4).map(|i| json!({"_id": format!("n{i}"), "content": format!("node {i}")})).collect();
        Mock::given(method("POST"))
            .and(path("/memory.semantic/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": docs, "meta": {}})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.procedural/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": [], "meta": {}})))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": {}, "meta": {}})))
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());
        let provider = Counting { in_flight: Default::default(), peak: Default::default() };

        let report = run_with(&db, &provider, false, |_| async {}).await.expect("the backfill ran");
        assert_eq!((report.embedded, report.failed, report.skipped_empty), (4, 0, 0));
        assert_eq!(provider.peak.load(std::sync::atomic::Ordering::SeqCst), INFERENCE_SLOTS);
        let patched: Vec<String> = server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "PATCH")
            .map(|r| r.url.path().to_string())
            .collect();
        assert_eq!(
            patched,
            ["/memory.semantic/docs/n1", "/memory.semantic/docs/n2", "/memory.semantic/docs/n3", "/memory.semantic/docs/n4"]
        );
    }

    #[test]
    fn survey_projection_covers_every_field_is_current_reads() {
        // The bug class this guards: a reader that checks a field the
        // projection never fetches. Scan `is_current`'s own source.
        let src = include_str!("backfill.rs");
        let start = src.find("fn is_current(").expect("is_current present");
        let body = &src[start..src[start..].find("\n}\n").map(|i| start + i).unwrap()];
        let mut missing = Vec::new();
        let mut rest = body;
        while let Some(i) = rest.find(".get(\"") {
            rest = &rest[i + 6..];
            let end = rest.find('"').unwrap();
            let field = &rest[..end];
            if !SURVEY_FIELDS.contains(&field) && !missing.contains(&field) {
                missing.push(field);
            }
        }
        assert!(
            missing.is_empty(),
            "is_current reads {missing:?} but SURVEY_FIELDS does not fetch them — \
             every node would report as unembedded"
        );
    }

    #[test]
    fn a_projected_document_with_only_the_model_field_is_not_current() {
        // Exactly the shape the old projection returned: model present, vector
        // absent. It must NOT count as embedded — and SURVEY_FIELDS must
        // therefore fetch the vector too (guarded above).
        assert!(!is_current(&json!({"_id": "n", "embedding_model": "m"}), "m"));
    }

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
