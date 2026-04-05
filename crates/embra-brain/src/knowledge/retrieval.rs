//! Context-aware retrieval with graph expansion.
//!
//! Multi-signal ranking:
//!   score = tag_relevance*0.4 + recency*0.3 + access_frequency*0.2 + confidence*0.1

use anyhow::Result;
use chrono::DateTime;
use serde_json::json;
use std::collections::HashMap;

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::traversal::traverse;
use super::types::{content_preview, GraphNode, NodeType, RankedNode, SemanticCategory};

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

    // Step 1: Direct tag query on semantic + procedural.
    for tag in tags {
        if tag.is_empty() { continue; }
        for coll in &["memory.semantic", "memory.procedural"] {
            let filter = json!({
                "filter": { "tags": { "$contains": tag } },
                "limit": 20,
            });
            if let Ok(docs) = db.query(coll, &filter).await {
                for doc in docs {
                    insert_collected(&mut collected, &doc, coll, "direct_query");
                }
            }
        }
    }

    // Step 2: Session-based — find edges from current-session entries.
    let session_filter = json!({
        "filter": { "session": session_name },
        "limit": 50,
    });
    if let Ok(entries) = db.query("memory.entries", &session_filter).await {
        let session_ids: Vec<String> = entries
            .iter()
            .filter_map(|d| d.get("_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();
        for entry_id in session_ids.iter().take(20) {
            let edge_filter = json!({
                "filter": {
                    "source_id": entry_id,
                    "edge_type": "same_session",
                },
                "limit": 20,
            });
            if let Ok(edges) = db.query("memory.edges", &edge_filter).await {
                for edge in edges {
                    let Some(target_coll) = edge.get("target_collection").and_then(|v| v.as_str()) else { continue; };
                    let Some(target_id) = edge.get("target_id").and_then(|v| v.as_str()) else { continue; };
                    if target_coll == "memory.entries" { continue; }
                    if let Ok(doc) = db.read(target_coll, target_id).await {
                        insert_collected(&mut collected, &doc, target_coll, "session_based");
                    }
                }
            }
        }
    }

    // Step 3: Content-based substring match on memory.entries.
    if !query_text.is_empty() {
        let q_lower = query_text.to_lowercase();
        let all_entries = db.query("memory.entries", &json!({ "filter": {}, "limit": 500 })).await.unwrap_or_default();
        for doc in all_entries {
            let content = doc.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if !content.to_lowercase().contains(&q_lower) { continue; }
            // If promoted, include the promoted node instead.
            if let Some(promoted) = doc.get("promoted_to") {
                if !promoted.is_null() {
                    let Some(coll) = promoted.get("collection").and_then(|v| v.as_str()) else { continue; };
                    let Some(id) = promoted.get("id").and_then(|v| v.as_str()) else { continue; };
                    if let Ok(pdoc) = db.read(coll, id).await {
                        insert_collected(&mut collected, &pdoc, coll, "direct_query");
                    }
                    continue;
                }
            }
            insert_collected(&mut collected, &doc, "memory.entries", "direct_query");
        }
    }

    // Step 4: Graph expansion — top 10 unique seeds, traverse depth 2.
    let seeds: Vec<(String, String)> = collected.keys().take(10).cloned().collect();
    for (coll, id) in seeds {
        if let Ok(tr) = traverse(db, &id, &coll, 2, None, None, config).await {
            for node in tr.nodes.iter().skip(1) {
                let key = (node.collection.clone(), node.id.clone());
                if collected.contains_key(&key) { continue; }
                // Load full doc for scoring fields.
                if let Ok(doc) = db.read(&node.collection, &node.id).await {
                    insert_collected(&mut collected, &doc, &node.collection, "graph_expansion");
                }
            }
        }
    }

    // Step 5: Score and rank.
    let ranked = score_and_rank(collected.into_values().collect(), tags, max_results);
    Ok(ranked)
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

fn score_and_rank(
    items: Vec<Collected>,
    input_tags: &[String],
    max_results: usize,
) -> Vec<RankedNode> {
    if items.is_empty() { return Vec::new(); }

    let input_tag_count = input_tags.len().max(1) as f64;

    // Normalize recency: oldest=0.0, newest=1.0.
    let timestamps: Vec<i64> = items.iter()
        .filter_map(|c| DateTime::parse_from_rfc3339(&c.created_at).ok())
        .map(|d| d.timestamp())
        .collect();
    let (ts_min, ts_max) = match (timestamps.iter().min().copied(), timestamps.iter().max().copied()) {
        (Some(a), Some(b)) if a != b => (a, b),
        _ => (0, 1),
    };
    let ts_range = (ts_max - ts_min).max(1) as f64;

    let max_access = items.iter().map(|c| c.access_count).max().unwrap_or(1).max(1) as f64;

    let mut scored: Vec<RankedNode> = items.into_iter().map(|c| {
        let matching_tags = c.tags.iter()
            .filter(|t| input_tags.iter().any(|it| it.eq_ignore_ascii_case(t)))
            .count() as f64;
        let tag_relevance = (matching_tags / input_tag_count).min(1.0);

        let recency = DateTime::parse_from_rfc3339(&c.created_at).ok()
            .map(|d| (d.timestamp() - ts_min) as f64 / ts_range)
            .unwrap_or(0.0);

        let access_frequency = (c.access_count as f64) / max_access;

        let score = tag_relevance * 0.4 + recency * 0.3 + access_frequency * 0.2 + c.confidence * 0.1;

        let node = GraphNode {
            id: c.id,
            collection: c.collection,
            content_preview: content_preview(&c.content, 200),
            node_type: c.node_type,
            depth: 0,
        };
        RankedNode { node, score, source: c.source }
    }).collect();

    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(max_results);
    scored
}
