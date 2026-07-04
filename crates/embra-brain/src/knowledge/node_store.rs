//! Per-call in-memory node prefetch for retrieval and traversal.
//!
//! The KG hot paths (retrieval Steps 1–4, traversal node loads) used to issue
//! one HTTP point read per document — hundreds to thousands per call. The
//! node collections are small relative to the edge set (353 semantic + 12
//! procedural docs vs ~99k edges at the time of the 2026-07-04 fix), so one
//! windowed fetch per collection plus an in-memory join wins past ~10
//! lookups. A `NodeStore` lives for ONE retrieval/traversal call — staleness
//! is bounded by call duration (single-writer reality makes that moot);
//! `get_or_fetch` falls back to a point read for docs outside the windows.

use std::collections::HashMap;

use crate::db::{WardsonDbClient, MEMORY_FETCH_WINDOW};

use super::types::{content_preview, GraphNode, NodeType, SemanticCategory};

/// In-memory `(collection, _id) → doc` join table for one KG call.
pub(crate) struct NodeStore {
    docs: HashMap<(String, String), serde_json::Value>,
}

impl NodeStore {
    pub(crate) fn new() -> Self {
        Self { docs: HashMap::new() }
    }

    /// Windowed prefetch of the promoted-node collections (semantic +
    /// procedural). A fetch error degrades that collection to point-read
    /// fallback + warn — the same tolerance class as the per-query error
    /// handling this replaces. Window saturation is warned by `fetch_recent`
    /// itself and surfaced by `system_status`'s parity check.
    pub(crate) async fn prefetch_promoted(db: &WardsonDbClient) -> Self {
        let mut store = Self::new();
        for coll in ["memory.semantic", "memory.procedural"] {
            match db.fetch_recent(coll, MEMORY_FETCH_WINDOW).await {
                Ok(docs) => store.insert_docs(coll, docs),
                Err(e) => tracing::warn!(
                    target: "kg::node_store",
                    collection = coll,
                    "prefetch failed — falling back to point reads: {}",
                    e
                ),
            }
        }
        store
    }

    /// Bulk-index fetched docs by `(collection, _id)`. Docs without a string
    /// `_id` are skipped (they can't be addressed by any lookup path).
    pub(crate) fn insert_docs(&mut self, collection: &str, docs: Vec<serde_json::Value>) {
        for doc in docs {
            let Some(id) = doc.get("_id").and_then(|v| v.as_str()).map(str::to_string) else {
                continue;
            };
            self.docs.insert((collection.to_string(), id), doc);
        }
    }

    pub(crate) fn get(&self, collection: &str, id: &str) -> Option<&serde_json::Value> {
        self.docs.get(&(collection.to_string(), id.to_string()))
    }

    /// Map hit → clone; miss → point-read fallback, cached on success so a
    /// doc is fetched at most once per call.
    pub(crate) async fn get_or_fetch(
        &mut self,
        db: &WardsonDbClient,
        collection: &str,
        id: &str,
    ) -> Option<serde_json::Value> {
        if let Some(doc) = self.get(collection, id) {
            return Some(doc.clone());
        }
        let doc = db.read(collection, id).await.ok()?;
        self.docs.insert((collection.to_string(), id.to_string()), doc.clone());
        Some(doc)
    }
}

/// Mirror of WardSONDB `$contains` array membership (its `values_equal`):
/// exact, CASE-SENSITIVE string equality against each element. Query tags
/// are lowercased by the callers while stored tags are as-typed — so only
/// lowercase-stored tags match, exactly as the server behaves today. Do NOT
/// widen this to case-insensitive: that silently changes result sets (the
/// insensitive comparison in scoring, `eq_ignore_ascii_case` tag_relevance,
/// is a different signal and stays as it is).
pub(crate) fn doc_tag_contains(doc: &serde_json::Value, tag: &str) -> bool {
    doc.get("tags")
        .and_then(|v| v.as_array())
        .is_some_and(|arr| arr.iter().any(|t| t.as_str() == Some(tag)))
}

/// Server sort mirror for `created_at desc`: WardSONDB orders missing <
/// present, so under `desc` docs missing the field sort LAST. `created_at`
/// is always written via `to_rfc3339()` (UTC), so lexicographic order is
/// chronological. Stable, preserving fetch order among exact ties.
pub(crate) fn sort_created_desc(docs: &mut [serde_json::Value]) {
    docs.sort_by(|a, b| {
        let ka = a.get("created_at").and_then(|v| v.as_str());
        let kb = b.get("created_at").and_then(|v| v.as_str());
        match (ka, kb) {
            (Some(x), Some(y)) => y.cmp(x),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
}

/// Build a `GraphNode` from a full stored doc (pure; extracted from the old
/// `traversal::load_graph_node` so store-backed callers share one mapping).
pub(crate) fn graph_node_from_doc(
    doc: &serde_json::Value,
    collection: &str,
    id: &str,
    depth: u32,
) -> GraphNode {
    let (preview_source, node_type) = match collection {
        "memory.entries" => (
            doc.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            NodeType::Episodic,
        ),
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
        _ => (String::new(), NodeType::Episodic),
    };
    GraphNode {
        id: id.to_string(),
        collection: collection.to_string(),
        content_preview: content_preview(&preview_source, 200),
        node_type,
        depth,
    }
}

#[cfg(test)]
mod node_store_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn store_indexes_by_collection_and_id_skips_docs_without_id() {
        let mut store = NodeStore::new();
        store.insert_docs(
            "memory.semantic",
            vec![
                json!({"_id": "a", "content": "one"}),
                json!({"content": "no id — skipped"}),
                json!({"_id": 42, "content": "non-string id — skipped"}),
            ],
        );
        assert!(store.get("memory.semantic", "a").is_some());
        assert!(store.get("memory.semantic", "42").is_none());
        // Same id in a different collection is a different node.
        assert!(store.get("memory.procedural", "a").is_none());
    }

    #[test]
    fn doc_tag_contains_is_case_sensitive_exact_membership() {
        let doc = json!({"tags": ["kg", "Performance", "rust"]});
        // Exact match hits; the server's $contains is values_equal — no
        // case folding, no substring semantics.
        assert!(doc_tag_contains(&doc, "kg"));
        assert!(!doc_tag_contains(&doc, "performance"));
        assert!(!doc_tag_contains(&doc, "k"));
        // Missing / non-array tags never match.
        assert!(!doc_tag_contains(&json!({"content": "x"}), "kg"));
        assert!(!doc_tag_contains(&json!({"tags": "kg"}), "kg"));
    }

    #[test]
    fn sort_created_desc_places_missing_created_at_last() {
        let mut docs = vec![
            json!({"_id": "old", "created_at": "2026-01-01T00:00:00Z"}),
            json!({"_id": "none"}),
            json!({"_id": "new", "created_at": "2026-07-04T00:00:00Z"}),
        ];
        sort_created_desc(&mut docs);
        let order: Vec<&str> = docs.iter().map(|d| d["_id"].as_str().unwrap()).collect();
        assert_eq!(order, vec!["new", "old", "none"]);
    }

    #[test]
    fn graph_node_from_doc_maps_entries_semantic_procedural() {
        let e = graph_node_from_doc(&json!({"content": "an episode"}), "memory.entries", "e1", 1);
        assert!(matches!(e.node_type, NodeType::Episodic));
        assert_eq!(e.content_preview, "an episode");
        assert_eq!(e.depth, 1);

        let s = graph_node_from_doc(
            &json!({"content": "a fact", "category": "decision"}),
            "memory.semantic",
            "s1",
            2,
        );
        match s.node_type {
            NodeType::Semantic { category } => assert_eq!(category.as_str(), "decision"),
            other => panic!("expected semantic node, got {:?}", other),
        }

        let p = graph_node_from_doc(
            &json!({"title": "deploy", "description": "how to deploy"}),
            "memory.procedural",
            "p1",
            0,
        );
        match p.node_type {
            NodeType::Procedural { title } => assert_eq!(title, "deploy"),
            other => panic!("expected procedural node, got {:?}", other),
        }
        assert_eq!(p.content_preview, "how to deploy");

        // Unknown collections degrade to Episodic with an empty preview.
        let u = graph_node_from_doc(&json!({"x": 1}), "memory.unknown", "u1", 3);
        assert!(matches!(u.node_type, NodeType::Episodic));
        assert_eq!(u.content_preview, "");
    }
}
