//! Edge derivation engine — creates typed weighted edges in `memory.edges`
//! at write time for new documents in any memory collection.
//!
//! Called after any insert into memory.entries / memory.semantic / memory.procedural.
//! Best-effort: logs errors and returns Ok(0) on any failure — the memory
//! document is already saved before this runs.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::HashSet;
use tracing::warn;

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::types::EdgeType;

const MEMORY_COLLECTIONS: [&str; 3] = ["memory.entries", "memory.semantic", "memory.procedural"];

/// A candidate document for edge derivation.
#[derive(Clone)]
struct Candidate {
    id: String,
    collection: String,
    created_at: String,
    session: String,
    tags: Vec<String>,
}

/// Derive and bulk-insert auto edges for a newly-written node.
/// Returns the number of edge documents inserted (counts both directions).
pub async fn derive_edges(
    db: &WardsonDbClient,
    new_doc_id: &str,
    new_doc_collection: &str,
    session: &str,
    tags: &[String],
    created_at: &str,
    config: &SystemConfig,
) -> Result<usize> {
    match derive_edges_inner(db, new_doc_id, new_doc_collection, session, tags, created_at, config).await {
        Ok(n) => Ok(n),
        Err(e) => {
            warn!("edge derivation failed for {}:{}: {}", new_doc_collection, new_doc_id, e);
            Ok(0)
        }
    }
}

async fn derive_edges_inner(
    db: &WardsonDbClient,
    new_doc_id: &str,
    new_doc_collection: &str,
    session: &str,
    tags: &[String],
    created_at: &str,
    config: &SystemConfig,
) -> Result<usize> {
    let candidate_limit = config.kg_edge_candidate_limit as i64;
    let window_secs = config.kg_temporal_window_secs as i64;

    let new_ts = parse_ts(created_at);

    // Gather candidates across all 3 memory collections.
    let mut session_candidates: Vec<Candidate> = Vec::new();
    let mut temporal_candidates: Vec<Candidate> = Vec::new();
    let mut tag_candidates: Vec<Candidate> = Vec::new();

    for collection in &MEMORY_COLLECTIONS {
        // same-session: memory.entries uses "session", others use "source_session"
        let session_field = if *collection == "memory.entries" { "session" } else { "source_session" };
        let session_filter = json!({
            "filter": { session_field: session },
            "sort": [{ "created_at": "desc" }],
            "limit": candidate_limit,
        });
        if let Ok(docs) = db.query(collection, &session_filter).await {
            session_candidates.extend(docs.into_iter().filter_map(|d| doc_to_candidate(&d, collection)));
        }

        // temporal: +/- window
        if let Some(ts) = new_ts {
            let lo = (ts - chrono::Duration::seconds(window_secs)).to_rfc3339();
            let hi = (ts + chrono::Duration::seconds(window_secs)).to_rfc3339();
            let temporal_filter = json!({
                "filter": { "created_at": { "$gte": lo, "$lte": hi } },
                "sort": [{ "created_at": "desc" }],
                "limit": candidate_limit,
            });
            if let Ok(docs) = db.query(collection, &temporal_filter).await {
                temporal_candidates.extend(docs.into_iter().filter_map(|d| doc_to_candidate(&d, collection)));
            }
        }

        // tag-overlap: for each tag on the new doc
        for tag in tags {
            if tag.is_empty() { continue; }
            let tag_filter = json!({
                "filter": { "tags": { "$contains": tag } },
                "sort": [{ "created_at": "desc" }],
                "limit": candidate_limit,
            });
            if let Ok(docs) = db.query(collection, &tag_filter).await {
                tag_candidates.extend(docs.into_iter().filter_map(|d| doc_to_candidate(&d, collection)));
            }
        }
    }

    // Compute edges per type (each type gets independent dedup).
    let now = Utc::now().to_rfc3339();
    let mut edges_to_insert: Vec<serde_json::Value> = Vec::new();

    // same_session — weight = 1.0
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for c in &session_candidates {
        if is_self(&c.id, &c.collection, new_doc_id, new_doc_collection) { continue; }
        let key = (c.collection.clone(), c.id.clone());
        if !seen.insert(key) { continue; }
        push_bidirectional(
            &mut edges_to_insert,
            new_doc_id, new_doc_collection,
            &c.id, &c.collection,
            EdgeType::SameSession, 1.0,
            json!({ "session": session }),
            &now,
        );
    }

    // temporal — weight = 1.0 - (distance / window)
    seen.clear();
    if let Some(ts) = new_ts {
        for c in &temporal_candidates {
            if is_self(&c.id, &c.collection, new_doc_id, new_doc_collection) { continue; }
            let key = (c.collection.clone(), c.id.clone());
            if !seen.insert(key) { continue; }
            let Some(other_ts) = parse_ts(&c.created_at) else { continue; };
            let dist = (ts - other_ts).num_seconds().abs();
            if dist >= window_secs || window_secs == 0 { continue; }
            let weight = 1.0 - (dist as f64 / window_secs as f64);
            if weight <= 0.0 { continue; }
            push_bidirectional(
                &mut edges_to_insert,
                new_doc_id, new_doc_collection,
                &c.id, &c.collection,
                EdgeType::Temporal, weight,
                json!({ "distance_secs": dist, "window_secs": window_secs }),
                &now,
            );
        }
    }

    // tag_overlap — weight = Jaccard-like overlap
    seen.clear();
    if !tags.is_empty() {
        let new_set: HashSet<&str> = tags.iter().map(|s| s.as_str()).collect();
        for c in &tag_candidates {
            if is_self(&c.id, &c.collection, new_doc_id, new_doc_collection) { continue; }
            let key = (c.collection.clone(), c.id.clone());
            if !seen.insert(key) { continue; }
            if c.tags.is_empty() { continue; }
            let other_set: HashSet<&str> = c.tags.iter().map(|s| s.as_str()).collect();
            let overlap = new_set.intersection(&other_set).count();
            if overlap == 0 { continue; }
            let denom = new_set.len().max(other_set.len()) as f64;
            let weight = overlap as f64 / denom;
            push_bidirectional(
                &mut edges_to_insert,
                new_doc_id, new_doc_collection,
                &c.id, &c.collection,
                EdgeType::TagOverlap, weight,
                json!({ "overlap_count": overlap }),
                &now,
            );
        }
    }

    // Duplicate prevention: drop edges that already exist.
    let mut new_edges: Vec<serde_json::Value> = Vec::new();
    for edge in edges_to_insert {
        let src_id = edge.get("source_id").and_then(|v| v.as_str()).unwrap_or("");
        let tgt_id = edge.get("target_id").and_then(|v| v.as_str()).unwrap_or("");
        let etype = edge.get("edge_type").and_then(|v| v.as_str()).unwrap_or("");
        if edge_exists(db, src_id, tgt_id, etype).await {
            continue;
        }
        new_edges.push(edge);
    }

    if new_edges.is_empty() {
        return Ok(0);
    }

    match db.bulk_write("memory.edges", &new_edges).await {
        Ok(inserted) => Ok(inserted as usize),
        Err(e) => {
            warn!("bulk_write memory.edges failed: {}", e);
            Ok(0)
        }
    }
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

fn is_self(id: &str, coll: &str, self_id: &str, self_coll: &str) -> bool {
    id == self_id && coll == self_coll
}

fn doc_to_candidate(doc: &serde_json::Value, collection: &str) -> Option<Candidate> {
    let id = doc.get("_id").and_then(|v| v.as_str())?.to_string();
    let created_at = doc.get("created_at").and_then(|v| v.as_str()).unwrap_or("").to_string();
    // session field differs across collections
    let session = if collection == "memory.entries" {
        doc.get("session").and_then(|v| v.as_str()).unwrap_or("").to_string()
    } else {
        doc.get("source_session").and_then(|v| v.as_str()).unwrap_or("").to_string()
    };
    let tags = doc
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|t| t.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    Some(Candidate { id, collection: collection.to_string(), created_at, session, tags })
}

#[allow(clippy::too_many_arguments)]
fn push_bidirectional(
    out: &mut Vec<serde_json::Value>,
    a_id: &str, a_coll: &str,
    b_id: &str, b_coll: &str,
    edge_type: EdgeType, weight: f64,
    metadata: serde_json::Value,
    created_at: &str,
) {
    let etype = edge_type.as_str();
    out.push(json!({
        "source_id": a_id,
        "source_collection": a_coll,
        "target_id": b_id,
        "target_collection": b_coll,
        "edge_type": etype,
        "weight": weight,
        "metadata": metadata,
        "created_at": created_at,
    }));
    out.push(json!({
        "source_id": b_id,
        "source_collection": b_coll,
        "target_id": a_id,
        "target_collection": a_coll,
        "edge_type": etype,
        "weight": weight,
        "metadata": metadata,
        "created_at": created_at,
    }));
}

async fn edge_exists(db: &WardsonDbClient, source_id: &str, target_id: &str, edge_type: &str) -> bool {
    let filter = json!({
        "filter": {
            "source_id": source_id,
            "target_id": target_id,
            "edge_type": edge_type,
        },
        "limit": 1,
    });
    match db.query("memory.edges", &filter).await {
        Ok(docs) => !docs.is_empty(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    // Weight formulas — pure unit tests that don't need the DB.

    fn temporal_weight(distance_secs: i64, window_secs: i64) -> Option<f64> {
        if distance_secs >= window_secs || window_secs == 0 { return None; }
        let w = 1.0 - (distance_secs as f64 / window_secs as f64);
        if w <= 0.0 { None } else { Some(w) }
    }

    fn jaccard(a: &[&str], b: &[&str]) -> Option<f64> {
        if a.is_empty() || b.is_empty() { return None; }
        use std::collections::HashSet;
        let sa: HashSet<&&str> = a.iter().collect();
        let sb: HashSet<&&str> = b.iter().collect();
        let overlap = sa.intersection(&sb).count();
        if overlap == 0 { return None; }
        let denom = sa.len().max(sb.len()) as f64;
        Some(overlap as f64 / denom)
    }

    #[test]
    fn test_edge_weight_temporal() {
        assert_eq!(temporal_weight(0, 1800), Some(1.0));
        assert!((temporal_weight(900, 1800).unwrap() - 0.5).abs() < 1e-9);
        assert_eq!(temporal_weight(1800, 1800), None);
        assert_eq!(temporal_weight(2000, 1800), None);
    }

    #[test]
    fn test_edge_weight_tag_overlap() {
        // Formula: overlap / max(|a|, |b|). For [a,b,c] vs [b,c,d]: 2/max(3,3) = 2/3.
        let w = jaccard(&["a", "b", "c"], &["b", "c", "d"]).unwrap();
        assert!((w - 2.0 / 3.0).abs() < 1e-9, "expected 2/3, got {}", w);
        assert_eq!(jaccard(&[], &["a"]), None);
        assert_eq!(jaccard(&["x", "y"], &["x", "y"]), Some(1.0));
        assert_eq!(jaccard(&["a"], &["b"]), None);
        // Asymmetric sets: [a,b] vs [a,b,c,d] → 2/4 = 0.5
        let w2 = jaccard(&["a", "b"], &["a", "b", "c", "d"]).unwrap();
        assert!((w2 - 0.5).abs() < 1e-9, "expected 0.5, got {}", w2);
    }
}
