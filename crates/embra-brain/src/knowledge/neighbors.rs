//! Link candidates: the nearest existing nodes of a node that was just
//! written.
//!
//! `remember` and `knowledge_promote` return them with the new node, so the
//! intelligence has ids to link to in the same turn — retrieval and
//! enrichment show it nodes without ids. The list is an offer, not an edge:
//! which of the five relations holds, and in which direction, is a judgement
//! `knowledge_link` leaves to the intelligence, and nothing here writes an
//! edge.
//!
//! The floor and the cap are measured, not chosen: `docs/KNOWLEDGE-GRAPH.md`,
//! "Link candidates", and the harness `measure_link_candidate_cosines`
//! (`embedding/local.rs`).

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::types::content_preview;

pub(crate) const LINK_CANDIDATE_TOP_K: usize = 5;

/// Cosine between two node vectors. Nodes of one graph are all near each
/// other (the nearest older node of a node has a median of 0.789 on the
/// measured instance), so this floor is higher than the one a query has to
/// clear. At 0.75 the list holds 2.4 nodes on average and names a quarter
/// of the pairs the intelligence linked by hand; at 0.70 it holds 3.95 and
/// names a third.
pub(crate) const LINK_CANDIDATE_MIN_COSINE: f32 = 0.75;

const PREVIEW_CHARS: usize = 100;

#[derive(Debug, PartialEq)]
pub(crate) struct Candidate {
    pub collection: &'static str,
    pub id: String,
    /// The category of a semantic node; `procedure` for a procedural one.
    pub label: String,
    pub cosine: f32,
    pub preview: String,
    /// The audit's dedup rule pairs it with the new node.
    pub near_duplicate: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Candidates {
    Found(Vec<Candidate>),
    /// The node has a vector and nothing reaches the floor.
    NoneClose,
    /// The node has no vector: no model, embeddings off, or the embedding
    /// failed. Nothing can be said about what is near it.
    NoVector,
}

/// The nearest existing nodes of `collection:id`, read with their text.
pub(crate) async fn link_candidates(
    db: &WardsonDbClient,
    config: &SystemConfig,
    collection: &'static str,
    id: &str,
) -> Candidates {
    let Some(provider) = crate::embedding::provider(config).await else {
        return Candidates::NoVector;
    };
    // Two counts in the steady state. Before the first turn of a boot the
    // index is not loaded and the write path's upsert was a no-op: this
    // loads it, the new node's stored vector included.
    crate::embedding::cache::ensure_current(db, provider.as_ref()).await;
    let Some(hits) =
        crate::embedding::cache::neighbors(collection, id, LINK_CANDIDATE_TOP_K, LINK_CANDIDATE_MIN_COSINE).await
    else {
        return Candidates::NoVector;
    };
    if hits.is_empty() {
        return Candidates::NoneClose;
    }

    let own = db.read(collection, id).await.ok();
    let mut found = Vec::with_capacity(hits.len());
    for (hit_collection, hit_id, cosine) in hits {
        let hit_collection = match hit_collection.as_str() {
            "memory.semantic" => "memory.semantic",
            "memory.procedural" => "memory.procedural",
            _ => continue,
        };
        // A vector whose node is gone is no candidate.
        let Ok(doc) = db.read(hit_collection, &hit_id).await else { continue };
        let near_duplicate = hit_collection == collection
            && own.as_ref().is_some_and(|own| super::audit::near_duplicate(own, &doc, collection));
        found.push(candidate(hit_collection, &hit_id, cosine, &doc, near_duplicate));
    }
    if found.is_empty() {
        Candidates::NoneClose
    } else {
        Candidates::Found(found)
    }
}

fn candidate(
    collection: &'static str,
    id: &str,
    cosine: f32,
    doc: &serde_json::Value,
    near_duplicate: bool,
) -> Candidate {
    let text = |key: &str| doc.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let (label, preview) = if collection == "memory.procedural" {
        ("procedure".to_string(), one_line(&format!("{}: {}", text("title"), text("description"))))
    } else {
        let category = text("category");
        (if category.is_empty() { "uncategorized" } else { category }.to_string(), one_line(text("content")))
    };
    Candidate {
        collection,
        id: id.to_string(),
        label,
        cosine,
        preview: content_preview(&preview, PREVIEW_CHARS),
        near_duplicate,
    }
}

/// The lines a tool result carries after the new node. Each candidate is in
/// the `collection:id` form `knowledge_link` takes.
pub(crate) fn candidates_block(candidates: &Candidates) -> String {
    let found = match candidates {
        Candidates::Found(found) => found,
        Candidates::NoneClose => return "No existing node is close enough to link.".to_string(),
        Candidates::NoVector => {
            return "No link candidates: the node has no embedding (see /embeddings).".to_string()
        }
    };
    let mut out = String::from("Nearest nodes (similarity):\n");
    for c in found {
        out.push_str(&format!(
            "- {}:{} [{}] {:.2}{} — {}\n",
            c.collection,
            c.id,
            c.label,
            c.cosine,
            if c.near_duplicate { ", near-duplicate" } else { "" },
            c.preview
        ));
    }
    out.push_str(
        "Link the new node to each of these it has a real relation to, with knowledge_link; link nothing that is only similar in wording.",
    );
    if found.iter().any(|c| c.near_duplicate) {
        out.push_str(
            " A near-duplicate says the same thing: merge it with knowledge_merge (dry_run first) instead of linking.",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_block_lists_each_candidate_in_link_form() {
        let semantic = candidate(
            "memory.semantic",
            "n2",
            0.834,
            &json!({"content": "Guardian tools run under wasmtime\nwith an epoch timeout", "category": "decision"}),
            false,
        );
        let procedure = candidate(
            "memory.procedural",
            "p7",
            0.78,
            &json!({"title": "Re-seal ceremony", "description": "how the soul is sealed again"}),
            false,
        );
        let block = candidates_block(&Candidates::Found(vec![semantic, procedure]));
        assert_eq!(
            block,
            "Nearest nodes (similarity):\n\
             - memory.semantic:n2 [decision] 0.83 — Guardian tools run under wasmtime with an epoch timeout\n\
             - memory.procedural:p7 [procedure] 0.78 — Re-seal ceremony: how the soul is sealed again\n\
             Link the new node to each of these it has a real relation to, with knowledge_link; link nothing that is only similar in wording."
        );
    }

    #[test]
    fn a_near_duplicate_points_at_the_merge() {
        let twin = candidate("memory.semantic", "n3", 0.97, &json!({"content": "the same", "category": "fact"}), true);
        let block = candidates_block(&Candidates::Found(vec![twin]));
        assert!(block.contains("- memory.semantic:n3 [fact] 0.97, near-duplicate — the same\n"), "{block}");
        assert!(block.ends_with("merge it with knowledge_merge (dry_run first) instead of linking."), "{block}");
    }

    #[test]
    fn no_close_node_and_no_vector_read_differently() {
        let none = candidates_block(&Candidates::NoneClose);
        let blind = candidates_block(&Candidates::NoVector);
        assert_eq!(none, "No existing node is close enough to link.");
        assert!(blind.contains("no embedding"), "{blind}");
        assert_ne!(none, blind);
    }

    #[test]
    fn a_long_text_is_cut_and_a_missing_category_is_named() {
        let long = "word ".repeat(60);
        let c = candidate("memory.semantic", "n4", 0.8, &json!({"content": long}), false);
        assert_eq!(c.label, "uncategorized");
        assert!(c.preview.chars().count() <= PREVIEW_CHARS + 1, "{}", c.preview);
        assert!(c.preview.ends_with('…'));
    }

    /// The floor is a measured constant; the docs carry the table it was
    /// read from. Moving one without the other is the mistake this catches.
    #[test]
    fn the_floor_and_the_cap_are_the_measured_ones() {
        assert_eq!(LINK_CANDIDATE_TOP_K, 5);
        assert!((LINK_CANDIDATE_MIN_COSINE - 0.75).abs() < f32::EPSILON);
    }
}
