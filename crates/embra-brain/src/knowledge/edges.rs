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
    derive_edges_except(db, (new_doc_collection, new_doc_id), session, tags, created_at, config, None).await
}

/// `derive_edges` with one other document left out, both as
/// `(collection, id)`.
///
/// `remember` writes an entry and its node together and derives for both at
/// once. Each derivation would plan the entry↔node pair, and each checks
/// `edge_exists` before either has written its batch, so the pair's edges
/// would be written twice. The entry's derivation leaves its own node out;
/// the node's plans the pair, as it does when `knowledge_promote` promotes
/// an older entry.
pub async fn derive_edges_except(
    db: &WardsonDbClient,
    new_doc: (&str, &str),
    session: &str,
    tags: &[String],
    created_at: &str,
    config: &SystemConfig,
    except: Option<(&str, &str)>,
) -> Result<usize> {
    match derive_edges_inner(db, new_doc, session, tags, created_at, config, except).await {
        Ok(n) => Ok(n),
        Err(e) => {
            warn!("edge derivation failed for {}:{}: {}", new_doc.0, new_doc.1, e);
            Ok(0)
        }
    }
}

async fn derive_edges_inner(
    db: &WardsonDbClient,
    new_doc: (&str, &str),
    session: &str,
    tags: &[String],
    created_at: &str,
    config: &SystemConfig,
    except: Option<(&str, &str)>,
) -> Result<usize> {
    let (new_doc_collection, new_doc_id) = new_doc;
    // How many candidates one insert looks at, per collection and per kind,
    // newest first. It bounds the edges an insert creates; it is not a
    // search window. In a busy session it is full on most inserts, and
    // nothing is logged when it is.
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
        if skips(c, new_doc, except) { continue; }
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
            if skips(c, new_doc, except) { continue; }
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
            if skips(c, new_doc, except) { continue; }
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

    if edges_to_insert.is_empty() {
        return Ok(0);
    }

    // Duplicate prevention: drop the edges already stored. Two indexed
    // reads, one per direction, in place of one per planned edge (#35). An
    // existence that could not be determined is not an absence: nothing is
    // written (a duplicate would be permanent) and the failure is said
    // (#57).
    let others: Vec<&str> = {
        let mut ids: Vec<&str> = edges_to_insert
            .iter()
            .filter_map(|edge| {
                let source = edge.get("source_id").and_then(|v| v.as_str())?;
                let target = edge.get("target_id").and_then(|v| v.as_str())?;
                Some(if source == new_doc_id { target } else { source })
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let stored = match existing_edges(db, new_doc_id, &others, edges_to_insert.len()).await {
        Ok(stored) => stored,
        Err(e) => {
            warn!(
                "no edge written for {new_doc_collection}:{new_doc_id}: the stored edges could not be read: {e}"
            );
            return Ok(0);
        }
    };
    let new_edges: Vec<serde_json::Value> = edges_to_insert
        .into_iter()
        .filter(|edge| !stored.contains(&edge_key(edge)))
        .collect();

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

/// A candidate that gets no edge: the new document itself, and the one
/// document the caller left out. Both are `(collection, id)`.
fn skips(candidate: &Candidate, new_doc: (&str, &str), except: Option<(&str, &str)>) -> bool {
    let is = |doc: (&str, &str)| candidate.collection == doc.0 && candidate.id == doc.1;
    is(new_doc) || except.is_some_and(is)
}

fn doc_to_candidate(doc: &serde_json::Value, collection: &str) -> Option<Candidate> {
    let id = doc.get("_id").and_then(|v| v.as_str())?.to_string();
    let created_at = doc.get("created_at").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let tags = super::text::string_array(doc, "tags");
    Some(Candidate { id, collection: collection.to_string(), created_at, tags })
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

/// An edge's identity for the duplicate check: `(source_id, target_id,
/// edge_type)`, the three keys the one-per-edge read matched on.
fn edge_key(edge: &serde_json::Value) -> (String, String, String) {
    let field = |key: &str| edge.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string();
    (field("source_id"), field("target_id"), field("edge_type"))
}

/// The stored edges between `doc_id` and `others`, in either direction,
/// as `edge_key`s: one read per direction, `$in` over the ids on the other
/// end, both served by the `source_id` index (#35). The window is the
/// planned count plus one per arm: every planned edge may be stored once,
/// and a window that fills is an error, as a read that fails is — never an
/// absence, since the caller writes no edge on an unknown (#57).
async fn existing_edges(
    db: &WardsonDbClient,
    doc_id: &str,
    others: &[&str],
    planned: usize,
) -> Result<HashSet<(String, String, String)>> {
    let limit = planned + 1;
    let mut stored = HashSet::new();
    for (arm, filter) in [
        ("out", json!({ "source_id": doc_id, "target_id": { "$in": others } })),
        ("in", json!({ "source_id": { "$in": others }, "target_id": doc_id })),
    ] {
        let docs = db
            .query("memory.edges", &json!({ "filter": filter, "limit": limit }))
            .await?;
        if docs.len() >= limit {
            anyhow::bail!("the window of {limit} stored edges filled on the {arm} arm");
        }
        stored.extend(docs.iter().map(edge_key));
    }
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::{skips, Candidate};

    #[test]
    fn the_excepted_document_is_skipped_like_the_document_itself() {
        let candidate = |collection: &str, id: &str| Candidate {
            id: id.into(),
            collection: collection.into(),
            created_at: String::new(),
            tags: Vec::new(),
        };
        let entry = ("memory.entries", "e1");
        let node = Some(("memory.semantic", "n1"));
        assert!(skips(&candidate("memory.entries", "e1"), entry, None), "the document itself");
        assert!(skips(&candidate("memory.semantic", "n1"), entry, node), "the one left out");
        assert!(!skips(&candidate("memory.semantic", "n1"), entry, None), "nothing left out");
        assert!(!skips(&candidate("memory.semantic", "n2"), entry, node));
        // An id is only the same document in the same collection.
        assert!(!skips(&candidate("memory.procedural", "n1"), entry, node));
    }

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

#[cfg(test)]
mod derivation_tests {
    //! The caller of `edge_exists`: a check that fails writes no edge (#57).
    use super::derive_edges;
    use crate::db::WardsonDbClient;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Deserialized, not a struct literal: see `learning::phases` tests.
    fn config() -> crate::config::SystemConfig {
        serde_json::from_value(serde_json::json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
            "created_at": "", "version": "test", "api_provider": "anthropic"
        }))
        .expect("minimal config deserializes")
    }

    /// A candidate is found on every arm, the existence check fails, and no
    /// edge is written: the failure is said, never a duplicate written.
    #[tokio::test]
    async fn a_failed_existence_check_writes_no_edge() {
        let server = MockServer::start().await;
        let now = chrono::Utc::now().to_rfc3339();
        let candidate = serde_json::json!({"ok": true, "data": [{"_id": "c1", "created_at": now, "tags": ["t"], "session": "s1"}], "meta": {}});
        Mock::given(method("POST"))
            .and(path_regex(r"^/memory\.(entries|semantic|procedural)/query$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(candidate))
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/memory.edges/query")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let inserted = derive_edges(&db, "n1", "memory.semantic", "s1", &["t".to_string()], &now, &config()).await.unwrap();
        assert_eq!(inserted, 0);
        let requests = server.received_requests().await.unwrap_or_default();
        assert!(requests.iter().any(|r| r.url.path() == "/memory.edges/query"), "the existence check ran");
        assert!(!requests.iter().any(|r| r.url.path().starts_with("/memory.edges/docs")), "nothing was written");
    }

    /// A store that answers the existence reads the way the server does:
    /// the per-edge filter of the check this replaces, and the two `$in`
    /// arms of the prefetch, over the same stored edges. Both forms give
    /// the same answer on every stored set.
    struct EdgeStore(Vec<(&'static str, &'static str, &'static str)>);

    impl wiremock::Respond for EdgeStore {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let filter = &body["filter"];
            let limit = body["limit"].as_u64().unwrap_or(u64::MAX) as usize;
            let matches = |want: &serde_json::Value, have: &str| -> bool {
                match want {
                    serde_json::Value::String(s) => s == have,
                    v => v["$in"].as_array().is_some_and(|ids| ids.iter().any(|i| i == have)),
                }
            };
            let rows: Vec<serde_json::Value> = self
                .0
                .iter()
                .filter(|(s, t, e)| {
                    matches(&filter["source_id"], s)
                        && matches(&filter["target_id"], t)
                        && (filter["edge_type"].is_null() || filter["edge_type"] == *e)
                })
                .take(limit)
                .map(|(s, t, e)| serde_json::json!({"_id": format!("{s}-{t}-{e}"), "source_id": s, "target_id": t, "edge_type": e}))
                .collect();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "data": rows, "meta": {}}))
        }
    }

    /// The edges the derivation wrote, as `(source, target, type)`.
    async fn written(server: &MockServer) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        for r in server.received_requests().await.unwrap_or_default() {
            if r.method.as_str() != "POST" || !r.url.path().starts_with("/memory.edges/docs") {
                continue;
            }
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap_or_default();
            let docs = body.as_array().cloned().or_else(|| body["documents"].as_array().cloned()).unwrap_or_default();
            out.extend(docs.iter().map(super::edge_key));
        }
        out.sort();
        out
    }

    /// #35: the two `$in` reads dedupe exactly what the one-per-edge check
    /// did, in two round trips instead of one per planned edge. The stored
    /// set holds an edge on each arm and a direction, a type and a pair
    /// that are not stored, so every branch of the filter is exercised.
    #[tokio::test]
    async fn the_prefetch_dedupes_what_the_per_edge_check_did() {
        let now = chrono::Utc::now().to_rfc3339();
        let server = MockServer::start().await;
        // The candidates live in one collection; the other two hold nothing,
        // so each planned pair is planned once per type and direction.
        let candidates = serde_json::json!({"ok": true, "data": [
            {"_id": "c1", "created_at": now, "tags": ["t"], "session": "s1"},
            {"_id": "c2", "created_at": now, "tags": ["t"], "session": "s1"},
        ], "meta": {}});
        Mock::given(method("POST"))
            .and(path("/memory.semantic/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(candidates))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/memory\.(entries|procedural)/query$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "data": [], "meta": {}})))
            .mount(&server)
            .await;
        // Stored: both directions of n1↔c1 same_session, one direction of
        // n1→c2 temporal; c2 tag_overlap and the rest are not stored.
        let stored = vec![
            ("n1", "c1", "same_session"),
            ("c1", "n1", "same_session"),
            ("n1", "c2", "temporal"),
        ];
        Mock::given(method("POST")).and(path("/memory.edges/query")).respond_with(EdgeStore(stored.clone())).mount(&server).await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/memory\.edges/docs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "data": {"inserted": 9}, "meta": {}})))
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());

        derive_edges(&db, "n1", "memory.semantic", "s1", &["t".to_string()], &now, &config()).await.unwrap();

        // What the one-per-edge check would have written: every planned
        // edge whose key is not stored. Planned: both directions of
        // same_session, temporal and tag_overlap for c1 and c2.
        let mut expected: Vec<(String, String, String)> = Vec::new();
        for c in ["c1", "c2"] {
            for t in ["same_session", "temporal", "tag_overlap"] {
                for (s, d) in [("n1", c), (c, "n1")] {
                    if !stored.contains(&(s, d, t)) {
                        expected.push((s.to_string(), d.to_string(), t.to_string()));
                    }
                }
            }
        }
        expected.sort();
        assert_eq!(written(&server).await, expected);
        let reads = server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == "/memory.edges/query")
            .count();
        assert_eq!(reads, 2, "one read per direction, not one per planned edge");
    }

    /// A window that fills is no answer: nothing is written.
    #[tokio::test]
    async fn a_filled_prefetch_window_writes_no_edge() {
        let now = chrono::Utc::now().to_rfc3339();
        let server = MockServer::start().await;
        let candidate = serde_json::json!({"ok": true, "data": [{"_id": "c1", "created_at": now, "tags": ["t"], "session": "s1"}], "meta": {}});
        Mock::given(method("POST"))
            .and(path_regex(r"^/memory\.(entries|semantic|procedural)/query$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(candidate))
            .mount(&server)
            .await;
        // Every answer fills the window: `limit` rows, whatever is asked.
        struct Full;
        impl wiremock::Respond for Full {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
                let limit = body["limit"].as_u64().unwrap_or(1) as usize;
                let rows: Vec<serde_json::Value> = (0..limit)
                    .map(|i| serde_json::json!({"_id": format!("d{i}"), "source_id": "n1", "target_id": "c1", "edge_type": "same_session"}))
                    .collect();
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "data": rows, "meta": {}}))
            }
        }
        Mock::given(method("POST")).and(path("/memory.edges/query")).respond_with(Full).mount(&server).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let inserted = derive_edges(&db, "n1", "memory.semantic", "s1", &["t".to_string()], &now, &config()).await.unwrap();
        assert_eq!(inserted, 0);
        assert!(written(&server).await.is_empty(), "nothing was written");
    }
}

#[cfg(test)]
mod existing_edges_tests {
    use super::existing_edges;
    use crate::db::WardsonDbClient;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn ask(answer: ResponseTemplate) -> anyhow::Result<std::collections::HashSet<(String, String, String)>> {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/memory.edges/query")).respond_with(answer).mount(&server).await;
        existing_edges(&WardsonDbClient::from_url(&server.uri()), "a", &["b"], 4).await
    }

    /// A failed existence read is an error, not "nothing stored" (#57).
    #[tokio::test]
    async fn an_unanswered_existence_check_is_an_error_not_an_absence() {
        assert!(ask(ResponseTemplate::new(500)).await.is_err());
        let none = ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "data": [], "meta": {}}));
        assert!(ask(none).await.unwrap().is_empty());
        let one = ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "data": [
            {"_id": "e1", "source_id": "a", "target_id": "b", "edge_type": "same_session"}
        ], "meta": {}}));
        let stored = ask(one).await.unwrap();
        assert!(stored.contains(&("a".to_string(), "b".to_string(), "same_session".to_string())));
        // The window is the planned count plus one; an answer that fills it is no answer.
        let full: Vec<serde_json::Value> = (0..5)
            .map(|i| serde_json::json!({"_id": format!("e{i}"), "source_id": "a", "target_id": "b", "edge_type": "same_session"}))
            .collect();
        let filled = ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "data": full, "meta": {}}));
        assert!(ask(filled).await.is_err());
    }
}
