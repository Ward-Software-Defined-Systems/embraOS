//! Embed-at-write-time (KG-02 spec §6.1).
//!
//! The contract that matters: **an embedding failure never fails the write.**
//! The document is saved first, then embedded, then patched. A model that is
//! absent, disabled, or erroring leaves `embedding` unset and the node fully
//! usable through lexical retrieval; `/embeddings backfill` collects it later.
//! Nothing in the knowledge write path may propagate an embedding error.

use serde_json::json;

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::{cache, encode_vector};

/// The text an embedding represents, per collection.
///
/// Semantic nodes embed `content`. Procedural nodes embed title, description,
/// preconditions and step actions together (spec §4.2) — a procedure's title
/// alone is too thin to retrieve on, and its steps carry the vocabulary an
/// operator would actually search for.
pub fn embed_text(doc: &serde_json::Value, collection: &str) -> String {
    if collection != "memory.procedural" {
        return doc
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    for key in ["title", "description"] {
        if let Some(s) = doc.get(key).and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) {
            parts.push(s.to_string());
        }
    }
    if let Some(pre) = doc.get("preconditions").and_then(|v| v.as_array()) {
        for p in pre.iter().filter_map(|v| v.as_str()) {
            if !p.trim().is_empty() {
                parts.push(p.to_string());
            }
        }
    }
    if let Some(steps) = doc.get("steps").and_then(|v| v.as_array()) {
        for s in steps {
            if let Some(a) = s.get("action").and_then(|v| v.as_str()).filter(|a| !a.trim().is_empty()) {
                parts.push(a.to_string());
            }
        }
    }
    parts.join("\n")
}

/// Embed one node and patch the three additive fields onto it.
///
/// Best-effort by construction: every failure path logs and returns. Callers
/// invoke this AFTER the document is durably written.
pub async fn embed_node(
    db: &WardsonDbClient,
    cfg: &SystemConfig,
    collection: &str,
    id: &str,
    doc: &serde_json::Value,
) {
    let Some(provider) = super::provider(cfg).await else { return };
    let text = embed_text(doc, collection);
    if text.trim().is_empty() {
        return;
    }
    let vector = match provider.embed_query_or_document(&text).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "kg::embedding",
                "embedding {collection}:{id} failed (node saved, backfill will retry): {e}"
            );
            return;
        }
    };
    let model = provider.model_id().to_string();
    let patch = json!({
        "embedding": encode_vector(&vector),
        "embedding_model": model,
        "embedding_updated_at": chrono::Utc::now().to_rfc3339(),
    });
    if let Err(e) = db.patch_document(collection, id, &patch).await {
        tracing::warn!(target: "kg::embedding", "storing embedding for {collection}:{id} failed: {e}");
        return;
    }
    cache::upsert(collection, id, vector, &model).await;
}

/// Drop a node's vector from the index. Called where nodes are deleted.
pub async fn forget_node(collection: &str, id: &str) {
    cache::remove(collection, id).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_embeds_content_only() {
        let doc = json!({"content": "the fact", "title": "ignored"});
        assert_eq!(embed_text(&doc, "memory.semantic"), "the fact");
    }

    #[test]
    fn procedural_embeds_title_description_preconditions_and_step_actions() {
        let doc = json!({
            "title": "Rotate the cert",
            "description": "when trustd complains",
            "preconditions": ["trustd is running", ""],
            "steps": [
                {"order": 1, "action": "stop embra-web", "notes": "not embedded"},
                {"order": 2, "action": "regenerate"},
                {"order": 3}
            ],
        });
        let t = embed_text(&doc, "memory.procedural");
        assert_eq!(
            t,
            "Rotate the cert\nwhen trustd complains\ntrustd is running\nstop embra-web\nregenerate"
        );
        // Step notes are deliberately excluded — they are commentary, and
        // including them dilutes the procedure's own vocabulary.
        assert!(!t.contains("not embedded"));
        // Empty preconditions and action-less steps contribute nothing.
        assert!(!t.contains("\n\n"));
    }

    #[test]
    fn missing_fields_degrade_to_empty_not_panic() {
        assert_eq!(embed_text(&json!({}), "memory.semantic"), "");
        assert_eq!(embed_text(&json!({}), "memory.procedural"), "");
        assert_eq!(embed_text(&json!({"steps": "not an array"}), "memory.procedural"), "");
    }
}
