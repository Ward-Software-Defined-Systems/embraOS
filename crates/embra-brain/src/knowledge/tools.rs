//! Knowledge graph tool implementations.
//!
//! Tag syntax uses `|` as a delimiter between compound parameters.

use serde_json::json;

use super::removal::{self, RemovalSummary};
use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::neighbors::{candidates_block, link_candidates};
use super::promotion::{node_of_entry, promote_to_procedural, promote_to_semantic};
use super::node_store::NodeStore;
use super::retrieval::{retrieve_relevant_knowledge, RetrievalStats};
use super::traversal::{spawn_access_touches, traverse_multi};
use super::types::{content_preview, EdgeType, NodeType, SemanticCategory, SUPERSESSION_KEY};

/// `knowledge_promote <entry_id> | <type> | <data>`
pub async fn knowledge_promote(
    params: &str,
    db: &WardsonDbClient,
    config: &SystemConfig,
) -> String {
    let parts: Vec<&str> = params.splitn(3, '|').map(|s| s.trim()).collect();
    if parts.len() < 3 {
        return "Error: usage knowledge_promote <entry_id> | <type> | <data>".into();
    }
    let entry_id = parts[0];
    let ptype = parts[1].to_lowercase();
    let data = parts[2];

    match ptype.as_str() {
        "semantic" => {
            let Some(category) = SemanticCategory::from_str(data) else {
                return format!("Error: Invalid category '{}'. Must be one of: fact, preference, decision, observation, pattern", data);
            };
            if let Some(answer) = answer_for_an_existing_node(db, entry_id, Some(&category)).await {
                return answer;
            }
            match promote_to_semantic(db, entry_id, category.clone(), config).await {
                Ok(new_id) => format!(
                    "Promoted entry {} to memory.semantic\nNew node ID: {}\nCategory: {}\n{}",
                    entry_id,
                    new_id,
                    category.as_str(),
                    candidates_block(&link_candidates(db, config, "memory.semantic", &new_id).await)
                ),
                Err(e) => format!("Error: {}", e),
            }
        }
        "procedural" => {
            if let Some(answer) = answer_for_an_existing_node(db, entry_id, None).await {
                return answer;
            }
            match promote_to_procedural(db, entry_id, data, config).await {
                Ok(new_id) => format!(
                    "Promoted entry {} to memory.procedural\nNew node ID: {}\n{}",
                    entry_id,
                    new_id,
                    candidates_block(&link_candidates(db, config, "memory.procedural", &new_id).await)
                ),
                Err(e) => format!("Error: {}", e),
            }
        }
        _ => "Error: Type must be 'semantic' or 'procedural'".into(),
    }
}

/// What a promotion means for an entry that already has a node.
#[derive(Debug, PartialEq)]
enum AlreadyPromoted {
    /// A semantic promotion that names another category: the node takes it.
    SetCategory { from: String, to: &'static str },
    /// A semantic promotion that names the category the node has.
    Nothing,
    Refused(String),
}

/// `remember` writes the node itself, so a `knowledge_promote` after it —
/// the habit of the two-step workflow — finds the entry promoted. A
/// semantic promotion then carries one thing the node may lack, the
/// category: it is set, and the old habit ends where it meant to. A
/// procedure cannot be laid over a node that exists.
fn already_promoted(
    semantic: Option<&SemanticCategory>,
    entry_id: &str,
    node: (&str, &str),
    node_doc: &serde_json::Value,
) -> AlreadyPromoted {
    let (collection, id) = node;
    match (collection == "memory.semantic", semantic) {
        (true, Some(category)) => {
            let current = node_doc.get("category").and_then(|v| v.as_str()).unwrap_or("");
            if current == category.as_str() {
                AlreadyPromoted::Nothing
            } else {
                AlreadyPromoted::SetCategory { from: current.to_string(), to: category.as_str() }
            }
        }
        (true, None) => AlreadyPromoted::Refused(format!(
            "Entry {} already has the semantic node {}:{}. A procedure is saved in one call: remember with procedure. To replace this node, remove it with knowledge_unlink_node first.",
            entry_id, collection, id
        )),
        (false, Some(_)) => AlreadyPromoted::Refused(format!(
            "Entry {} already has the procedural node {}:{}; a procedure has no category.",
            entry_id, collection, id
        )),
        (false, None) => AlreadyPromoted::Refused(format!(
            "Entry {} already has the procedural node {}:{}. Change it with knowledge_update.",
            entry_id, collection, id
        )),
    }
}

/// `None` when the entry has no node and the promotion goes ahead.
async fn answer_for_an_existing_node(
    db: &WardsonDbClient,
    entry_id: &str,
    semantic: Option<&SemanticCategory>,
) -> Option<String> {
    let (collection, id, node_doc) = match node_of_entry(db, entry_id).await {
        Ok(Some(node)) => node,
        Ok(None) => return None,
        Err(e) => return Some(format!("Error: {}", e)),
    };
    Some(match already_promoted(semantic, entry_id, (&collection, &id), &node_doc) {
        AlreadyPromoted::Refused(why) => format!("Error: {}", why),
        AlreadyPromoted::Nothing => format!(
            "Entry {} already has the semantic node {}:{} (category {}). Nothing to do: remember writes the node itself.",
            entry_id,
            collection,
            id,
            semantic.map(SemanticCategory::as_str).unwrap_or("")
        ),
        AlreadyPromoted::SetCategory { from, to } => {
            let patch = json!({ "category": to, "updated_at": chrono::Utc::now().to_rfc3339() });
            match db.patch_document(&collection, &id, &patch).await {
                Ok(()) => format!(
                    "Entry {} already has the semantic node {}:{}; its category is now {} (was {}).",
                    entry_id, collection, id, to, from
                ),
                Err(e) => format!(
                    "Error: Entry {} already has the semantic node {}:{}, and its category could not be set: {}",
                    entry_id, collection, id, e
                ),
            }
        }
    })
}

#[cfg(test)]
mod candidates_line_tests {
    use super::candidates_line;
    use super::RetrievalStats;

    /// #57: the line names the reads that failed, so an empty candidate
    /// set is told from an empty graph; without one it reads as before.
    #[test]
    fn candidates_line_names_the_reads_that_failed() {
        let stats = RetrievalStats { candidates_total: 3, direct_query: 2, session_based: 1, ..Default::default() };
        assert_eq!(
            candidates_line(&stats),
            "Candidates considered: 3 (direct 2, session 1, similarity 0 — pre-ranking)\n"
        );
        let degraded = RetrievalStats { failed_reads: vec!["memory.entries", "session edges"], ..Default::default() };
        assert_eq!(
            candidates_line(&degraded),
            "Candidates considered: 0 (direct 0, session 0, similarity 0 — pre-ranking) — reads that failed: memory.entries, session edges; the graph may hold more than this\n"
        );
    }
}

#[cfg(test)]
mod already_promoted_tests {
    use super::{already_promoted, AlreadyPromoted};
    use crate::knowledge::types::SemanticCategory;
    use serde_json::json;

    #[test]
    fn a_semantic_promote_on_a_semantic_node_sets_its_category() {
        // `remember` without a category, then the old `knowledge_promote`.
        let node = json!({"_id": "n1", "content": "x", "category": "observation"});
        assert_eq!(
            already_promoted(Some(&SemanticCategory::Decision), "e1", ("memory.semantic", "n1"), &node),
            AlreadyPromoted::SetCategory { from: "observation".into(), to: "decision" }
        );
        // The category the node has: nothing is written.
        assert_eq!(
            already_promoted(Some(&SemanticCategory::Observation), "e1", ("memory.semantic", "n1"), &node),
            AlreadyPromoted::Nothing
        );
    }

    #[test]
    fn a_procedural_promote_on_a_semantic_node_names_the_way_out() {
        let node = json!({"_id": "n1", "content": "x", "category": "fact"});
        let AlreadyPromoted::Refused(why) = already_promoted(None, "e1", ("memory.semantic", "n1"), &node) else {
            panic!("a procedure over a node is refused");
        };
        assert!(why.contains("memory.semantic:n1"), "{why}");
        assert!(why.contains("remember with procedure"), "{why}");
        assert!(why.contains("knowledge_unlink_node"), "{why}");
    }

    #[test]
    fn a_procedural_node_takes_neither_a_category_nor_a_second_procedure() {
        let node = json!({"_id": "p1", "title": "t"});
        let category = already_promoted(Some(&SemanticCategory::Fact), "e1", ("memory.procedural", "p1"), &node);
        assert!(matches!(&category, AlreadyPromoted::Refused(why) if why.contains("no category")), "{category:?}");
        let again = already_promoted(None, "e1", ("memory.procedural", "p1"), &node);
        assert!(matches!(&again, AlreadyPromoted::Refused(why) if why.contains("knowledge_update")), "{again:?}");
    }
}

#[cfg(test)]
mod write_link_tests {
    use super::write_link;
    use crate::db::WardsonDbClient;
    use crate::knowledge::types::EdgeType;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn data(v: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": v, "meta": {}}))
    }

    /// Two nodes `a` and `b`; the duplicate check answers `existing`.
    async fn server(existing: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        for id in ["a", "b"] {
            Mock::given(method("GET"))
                .and(path(format!("/memory.semantic/docs/{id}")))
                .respond_with(data(json!({"_id": id})))
                .mount(&server)
                .await;
        }
        Mock::given(method("POST")).and(path("/memory.edges/query")).respond_with(data(existing)).mount(&server).await;
        Mock::given(method("POST")).and(path("/memory.edges/docs")).respond_with(data(json!({"_id": "edge1"}))).mount(&server).await;
        server
    }

    fn edge_writes(requests: &[wiremock::Request]) -> usize {
        requests.iter().filter(|r| r.method.as_str() == "POST" && r.url.path() == "/memory.edges/docs").count()
    }

    /// The checks of `knowledge_link` hold for the brain's own links too: an
    /// end that is not there and an edge that exists already write nothing.
    #[tokio::test]
    async fn write_link_refuses_a_missing_end_and_a_duplicate() {
        let a = ("memory.semantic", "a");
        let b = ("memory.semantic", "b");

        let fresh = server(json!([])).await;
        let db = WardsonDbClient::from_url(&fresh.uri());
        let missing = write_link(&db, a, &EdgeType::RelatedTo, ("memory.semantic", "gone"), 0.5).await;
        assert_eq!(missing, Err("Error: Target memory.semantic:gone not found".to_string()));
        assert_eq!(write_link(&db, a, &EdgeType::RelatedTo, b, 0.5).await, Ok("edge1".to_string()));
        assert_eq!(edge_writes(&fresh.received_requests().await.unwrap_or_default()), 1);

        let linked = server(json!([{"_id": "edge1"}])).await;
        let db = WardsonDbClient::from_url(&linked.uri());
        let again = write_link(&db, a, &EdgeType::RelatedTo, b, 0.5).await;
        assert!(again.as_ref().is_err_and(|e| e.starts_with("Error: Edge already exists")), "{again:?}");
        assert_eq!(edge_writes(&linked.received_requests().await.unwrap_or_default()), 0);
    }

    fn written_edges(requests: &[wiremock::Request]) -> Vec<serde_json::Value> {
        requests
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/memory.edges/docs")
            .map(|r| serde_json::from_slice(&r.body).expect("an edge document"))
            .collect()
    }

    /// The marker makes a `supersedes` edge a declaration, and no other edge
    /// carries it.
    #[tokio::test]
    async fn a_supersedes_link_carries_the_marker_and_no_other_type_does() {
        let a = ("memory.semantic", "a");
        let b = ("memory.semantic", "b");
        let fresh = server(json!([])).await;
        let db = WardsonDbClient::from_url(&fresh.uri());
        assert_eq!(write_link(&db, a, &EdgeType::Supersedes, b, 1.0).await, Ok("edge1".to_string()));
        assert_eq!(write_link(&db, a, &EdgeType::RelatedTo, b, 0.5).await, Ok("edge1".to_string()));
        let edges = written_edges(&fresh.received_requests().await.unwrap_or_default());
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0]["edge_type"], "supersedes");
        assert_eq!(edges[0]["metadata"], json!({"supersession": true}));
        assert_eq!(edges[1]["edge_type"], "related_to");
        assert_eq!(edges[1]["metadata"], json!({}));
    }

    /// A declaration joins two knowledge nodes: an identity node is sealed
    /// and an entry is not a node. Nothing is read or written.
    #[tokio::test]
    async fn supersedes_is_refused_outside_semantic_and_procedural() {
        let fresh = server(json!([])).await;
        let db = WardsonDbClient::from_url(&fresh.uri());
        for (source, target) in [
            (("memory.semantic", "a"), ("identity.graph", "self")),
            (("memory.entries", "e1"), ("memory.semantic", "b")),
        ] {
            let refused = write_link(&db, source, &EdgeType::Supersedes, target, 1.0).await;
            assert!(refused.as_ref().is_err_and(|e| e.contains("supersedes joins two knowledge nodes")), "{refused:?}");
        }
        assert!(fresh.received_requests().await.unwrap_or_default().is_empty());
    }

    /// Two nodes that supersede each other would both be history and leave
    /// every check: the reverse of an existing declaration is refused.
    #[tokio::test]
    async fn supersedes_is_refused_when_the_reverse_exists() {
        let server = MockServer::start().await;
        for id in ["a", "b"] {
            Mock::given(method("GET"))
                .and(path(format!("/memory.semantic/docs/{id}")))
                .respond_with(data(json!({"_id": id})))
                .mount(&server)
                .await;
        }
        // b → a exists; a → b does not.
        Mock::given(method("POST"))
            .and(path("/memory.edges/query"))
            .and(body_partial_json(json!({"filter": {"source_id": "b", "target_id": "a"}, "limit": 1})))
            .respond_with(data(json!([{"_id": "edge0"}])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.edges/query"))
            .and(body_partial_json(json!({"filter": {"source_id": "a", "target_id": "b"}, "limit": 1})))
            .respond_with(data(json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/memory.edges/docs")).respond_with(data(json!({"_id": "edge1"}))).mount(&server).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let refused =
            write_link(&db, ("memory.semantic", "a"), &EdgeType::Supersedes, ("memory.semantic", "b"), 1.0).await;
        assert!(refused.as_ref().is_err_and(|e| e.contains("already supersedes")), "{refused:?}");
        assert_eq!(edge_writes(&server.received_requests().await.unwrap_or_default()), 0);
    }
}

/// `knowledge_link <source_coll>:<source_id> | <edge_type> | <target_coll>:<target_id> | <weight>`
pub async fn knowledge_link(params: &str, db: &WardsonDbClient) -> String {
    let parts: Vec<&str> = params.splitn(4, '|').map(|s| s.trim()).collect();
    if parts.len() < 4 {
        return "Error: usage knowledge_link <source_coll>:<source_id> | <edge_type> | <target_coll>:<target_id> | <weight>".into();
    }
    let Some((src_coll, src_id)) = parts[0].split_once(':') else {
        return "Error: source must be <collection>:<id>".into();
    };
    // The gate: the strict parser, then the brain-created check. The model
    // links with the six types it may create and cannot mint another.
    // `EdgeType::parse_lossy` is for reading — it also carries the free-form
    // relations of an identity graph — and never goes here.
    let Some(edge_type) = EdgeType::from_str(parts[1]) else {
        return format!("Error: Invalid edge type '{}'. Brain-created types: {}", parts[1], BRAIN_CREATED_TYPES);
    };
    if !edge_type.is_brain_created() {
        return format!("Error: Invalid edge type '{}'. Brain-created types: {}", parts[1], BRAIN_CREATED_TYPES);
    }
    let Some((tgt_coll, tgt_id)) = parts[2].split_once(':') else {
        return "Error: target must be <collection>:<id>".into();
    };
    if src_coll == tgt_coll && src_id == tgt_id {
        return "Error: Cannot create edge from a node to itself".into();
    }
    let weight: f64 = match parts[3].parse() {
        Ok(w) => w,
        Err(_) => return "Error: Weight must be > 0.0 and ≤ 1.0".into(),
    };
    if weight <= 0.0 || weight > 1.0 {
        return "Error: Weight must be > 0.0 and ≤ 1.0".into();
    }

    match write_link(db, (src_coll, src_id), &edge_type, (tgt_coll, tgt_id), weight).await {
        Ok(edge_id) => format!(
            "Created edge: {}:{} --[{} w={}]--> {}:{}\nEdge ID: {}",
            src_coll, src_id, edge_type.as_str(), weight, tgt_coll, tgt_id, edge_id
        ),
        Err(refusal) => refusal,
    }
}

/// The types `knowledge_link` creates, as its refusals name them.
const BRAIN_CREATED_TYPES: &str = "enables, contradicts, refines, depends_on, related_to, supersedes";

/// The collections a declared supersession may join: the knowledge nodes.
/// An identity node is sealed and an entry is not a node.
const SUPERSESSION_COLLECTIONS: [&str; 2] = ["memory.semantic", "memory.procedural"];

/// Write one edge `source --edge_type--> target` after the checks
/// `knowledge_link` makes once its arguments are parsed: both ends exist and
/// the edge is not there yet. Returns the edge's id, or the refusal in
/// `knowledge_link`'s words. The caller gives a brain-created type, a weight
/// in (0, 1] and two different ends; the brain's own links (the record of a
/// deleted session, migration v14) come through here too.
///
/// A `supersedes` edge is a declaration: it joins two knowledge nodes, is
/// refused when the target already supersedes the source (both would be
/// history and leave every check), and carries `SUPERSESSION_KEY`, which no
/// other edge does (`supersession.rs`).
pub(crate) async fn write_link(
    db: &WardsonDbClient,
    (src_coll, src_id): (&str, &str),
    edge_type: &EdgeType,
    (tgt_coll, tgt_id): (&str, &str),
    weight: f64,
) -> Result<String, String> {
    if src_coll == tgt_coll && src_id == tgt_id {
        return Err("Error: Cannot create edge from a node to itself".into());
    }
    let declares = *edge_type == EdgeType::Supersedes;
    if declares
        && !(SUPERSESSION_COLLECTIONS.contains(&src_coll) && SUPERSESSION_COLLECTIONS.contains(&tgt_coll))
    {
        return Err(format!(
            "Error: supersedes joins two knowledge nodes ({}), not {}:{} and {}:{}",
            SUPERSESSION_COLLECTIONS.join(" or "),
            src_coll, src_id, tgt_coll, tgt_id
        ));
    }
    // Both ends exist. Only a 404 is "not found"; a refused id or a failing
    // server is said as what it is (#4, #60).
    for (role, coll, id) in [("Source", src_coll, src_id), ("Target", tgt_coll, tgt_id)] {
        if let Err(e) = db.read(coll, id).await {
            return Err(if crate::db::error::is_not_found(&e) {
                format!("Error: {role} {coll}:{id} not found")
            } else {
                format!("Error: could not read the {} {coll}:{id}: {e}", role.to_ascii_lowercase())
            });
        }
    }

    // Duplicate check
    let dup_filter = json!({
        "filter": {
            "source_id": src_id,
            "target_id": tgt_id,
            "edge_type": edge_type.as_str(),
        },
        "limit": 1,
    });
    if let Ok(existing) = db.query("memory.edges", &dup_filter).await
        && !existing.is_empty()
    {
        return Err(format!(
            "Error: Edge already exists from {}:{} to {}:{} with type {}",
            src_coll, src_id, tgt_coll, tgt_id, edge_type.as_str()
        ));
    }
    if declares {
        let reverse = json!({
            "filter": {
                "source_id": tgt_id,
                "target_id": src_id,
                "edge_type": edge_type.as_str(),
            },
            "limit": 1,
        });
        match db.query("memory.edges", &reverse).await {
            Ok(existing) if !existing.is_empty() => {
                return Err(format!(
                    "Error: {}:{} already supersedes {}:{}; unlink that edge first (knowledge_unlink_edge) — a node cannot supersede what supersedes it",
                    tgt_coll, tgt_id, src_coll, src_id
                ));
            }
            Ok(_) => {}
            Err(e) => return Err(format!("Error: could not check for the reverse supersedes edge: {}", e)),
        }
    }

    let mut metadata = serde_json::Map::new();
    if declares {
        metadata.insert(SUPERSESSION_KEY.to_string(), serde_json::Value::Bool(true));
    }
    let edge_doc = json!({
        "source_id": src_id,
        "source_collection": src_coll,
        "target_id": tgt_id,
        "target_collection": tgt_coll,
        "edge_type": edge_type.as_str(),
        "weight": weight,
        "metadata": metadata,
        "created_at": chrono::Utc::now().to_rfc3339(),
    });
    db.write("memory.edges", &edge_doc)
        .await
        .map_err(|e| format!("Error: failed to create edge: {}", e))
}

/// Triple-form endpoint: `<collection>:<id>` → (collection, id); a bare
/// `<id>` (or `:<id>`) → ("", id). The collection half is display-only —
/// the delete filter has always matched by id + edge_type, so an omitted
/// collection means "any" (ids are unique across collections in practice:
/// server-issued UUIDv7 for memories, slugs for identity nodes).
fn split_endpoint(token: &str) -> (&str, &str) {
    match token.split_once(':') {
        Some((coll, id)) => (coll.trim(), id.trim()),
        None => ("", token),
    }
}

/// Render an endpoint collection for messages; empty (omitted) → "any".
fn display_coll(coll: &str) -> &str {
    if coll.is_empty() { "any" } else { coll }
}

/// `knowledge_unlink_edge <edge_id>` — delete a single edge by ID
/// `knowledge_unlink_edge <src>:<id> | <edge_type> | <tgt>:<id>` —
/// delete matching edges; endpoint collections are optional (a bare `<id>`
/// matches any collection — the filter keys on id + type). Symmetric edge
/// types (`same_session`/`temporal`/`tag_overlap`/`related_to`) are removed
/// bidirectionally; directional types (`enables`/`contradicts`/`refines`/
/// `depends_on`/`supersedes`/`derived_from` and free-form identity
/// relations) only remove the forward direction.
pub async fn knowledge_unlink_edge(params: &str, db: &WardsonDbClient) -> String {
    let trimmed = params.trim();
    if trimmed.is_empty() {
        return "Error: usage knowledge_unlink_edge <edge_id> or knowledge_unlink_edge <src_id> | <edge_type> | <tgt_id> (ids may be prefixed <collection>:)".into();
    }

    if trimmed.contains('|') {
        // Form 2: triple parse. Bidirectional only for symmetric edge
        // types (Embra_Debug #63 — was unconditionally bidirectional).
        let parts: Vec<&str> = trimmed.splitn(3, '|').map(|s| s.trim()).collect();
        if parts.len() < 3 {
            return "Error: usage knowledge_unlink_edge <src_id> | <edge_type> | <tgt_id> (ids may be prefixed <collection>:)".into();
        }
        let (src_coll, src_id) = split_endpoint(parts[0]);
        if src_id.is_empty() {
            return "Error: source must be <collection>:<id> or a bare <id>".into();
        }
        // parse_lossy: free-form identity relations are addressable for
        // deletion (they are directional → forward doc only). A deleted
        // identity-projection edge is restored by the next boot reconcile.
        let Some(edge_type) = EdgeType::parse_lossy(parts[1]) else {
            return format!("Error: Invalid edge type '{}'. Built-in types: same_session, temporal, tag_overlap, derived_from, {} — or any stored free-form relation", parts[1], BRAIN_CREATED_TYPES);
        };
        let (tgt_coll, tgt_id) = split_endpoint(parts[2]);
        if tgt_id.is_empty() {
            return "Error: target must be <collection>:<id> or a bare <id>".into();
        }
        let (src_coll, tgt_coll) = (display_coll(src_coll), display_coll(tgt_coll));

        let etype = edge_type.as_str();
        let symmetric = edge_type.is_symmetric();
        // Cold-path cascade delete: `$or` forces a WardSONDB full scan —
        // acceptable for an operator-invoked one-off. NEVER copy this shape
        // into a `query()` hot path; hot paths arm-split for the indexes
        // (traversal.rs, 2026-07-04).
        let filter = if symmetric {
            json!({
                "$or": [
                    {"source_id": src_id, "target_id": tgt_id, "edge_type": etype},
                    {"source_id": tgt_id, "target_id": src_id, "edge_type": etype}
                ]
            })
        } else {
            json!({
                "source_id": src_id,
                "target_id": tgt_id,
                "edge_type": etype,
            })
        };
        match db.delete_by_query("memory.edges", &filter).await {
            Ok(0) => format!(
                "Error: No edge found from {}:{} to {}:{} with type {}",
                src_coll, src_id, tgt_coll, tgt_id, etype
            ),
            Ok(1) => format!(
                "Removed 1 edge:\n  {}:{} --[{}]--> {}:{}",
                src_coll, src_id, etype, tgt_coll, tgt_id
            ),
            Ok(n) if symmetric => format!(
                "Removed {} edges (bidirectional):\n  {}:{} --[{}]--> {}:{}\n  {}:{} --[{}]--> {}:{}",
                n, src_coll, src_id, etype, tgt_coll, tgt_id,
                tgt_coll, tgt_id, etype, src_coll, src_id
            ),
            Ok(n) => format!(
                "Removed {} edges (forward, possible duplicates):\n  {}:{} --[{}]--> {}:{}",
                n, src_coll, src_id, etype, tgt_coll, tgt_id
            ),
            Err(e) => format!("Error: delete failed: {}", e),
        }
    } else {
        // Form 1: delete by edge ID
        let edge_id = trimmed;
        let edge_doc = match db.read("memory.edges", edge_id).await {
            Ok(doc) => doc,
            Err(_) => return format!("Error: Edge {} not found", edge_id),
        };
        let src_coll = edge_doc.get("source_collection").and_then(|v| v.as_str()).unwrap_or("?");
        let src_id = edge_doc.get("source_id").and_then(|v| v.as_str()).unwrap_or("?");
        let tgt_coll = edge_doc.get("target_collection").and_then(|v| v.as_str()).unwrap_or("?");
        let tgt_id = edge_doc.get("target_id").and_then(|v| v.as_str()).unwrap_or("?");
        let etype = edge_doc.get("edge_type").and_then(|v| v.as_str()).unwrap_or("?");
        match db.delete("memory.edges", edge_id).await {
            Ok(()) => format!(
                "Removed 1 edge:\n  {}:{} --[{}]--> {}:{}",
                src_coll, src_id, etype, tgt_coll, tgt_id
            ),
            Err(e) => format!("Error: delete failed: {}", e),
        }
    }
}

/// `knowledge_unlink_node <collection>:<id>` — delete a semantic or
/// procedural node and cascade-remove every edge referencing it (source or target).
///
/// Scoped to `memory.semantic` and `memory.procedural`. Use `forget` for
/// episodic `memory.entries` cleanup.
/// `knowledge_unlink_node`: `dry_run` and the directory of its removal
/// record are parameters, so a test runs it under a temporary one. The
/// edges are read whole first (`node_edges`); a real run writes the record,
/// clears the pointers of the entries the node came from, deletes the edges
/// by id, then the node.
pub async fn knowledge_unlink_node_at(params: &str, db: &WardsonDbClient, dry_run: bool, dumps_dir: &str) -> String {
    let trimmed = params.trim();
    if trimmed.is_empty() {
        return "Error: usage knowledge_unlink_node <collection>:<id>".into();
    }
    let Some((coll, id)) = trimmed.split_once(':') else {
        return "Error: target must be <collection>:<id>".into();
    };
    let coll = coll.trim();
    let id = id.trim();
    // Two collections, by name. `identity.graph` is not one of them: its
    // nodes are the projection of the sealed document, and what the model
    // may not change in the seal it may not remove here.
    if coll != "memory.semantic" && coll != "memory.procedural" {
        return format!(
            "Error: knowledge_unlink_node only operates on memory.semantic or memory.procedural (got '{}'). Use forget for memory.entries.",
            coll
        );
    }

    let node_doc = match db.read(coll, id).await {
        Ok(doc) => doc,
        Err(e) if crate::db::error::is_not_found(&e) => return format!("Error: Node {}:{} not found", coll, id),
        Err(e) => return format!("Error: could not read {}:{}: {}", coll, id, e),
    };

    let preview_src = node_doc
        .get("content")
        .or_else(|| node_doc.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("(no preview)");
    let preview = content_preview(preview_src, 80);

    // Every edge that names the node, whole: the record needs them, and the
    // reply splits them. A read that fails removes nothing.
    let edges = match removal::node_edges(db, coll, id).await {
        Ok(edges) => edges,
        Err(e) => return format!("Error: the edges of {coll}:{id} could not be read ({e}); nothing was removed."),
    };
    let summary = RemovalSummary::of(&edges).with_nodes(vec![format!("{coll}:{id}")]);
    // The entries the node was promoted from: the `derived_from` edges
    // from the node into `memory.entries`.
    let source_entries: Vec<String> = edges
        .iter()
        .filter(|e| {
            e.get("edge_type").and_then(|v| v.as_str()) == Some("derived_from")
                && e.get("source_id").and_then(|v| v.as_str()) == Some(id)
                && e.get("target_collection").and_then(|v| v.as_str()) == Some("memory.entries")
        })
        .filter_map(|e| e.get("target_id").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    if dry_run {
        return format!(
            "Would remove node {}:{} (\"{}\"), {} referencing edge(s), and clear promoted_to on {} source entry(ies){}{}",
            coll,
            id,
            preview,
            edges.len(),
            source_entries.len(),
            if source_entries.is_empty() { String::new() } else { format!(": {}", source_entries.join(", ")) },
            removal::removal_report(&summary, None, true)
        );
    }
    let source = json!({ "tool": "knowledge_unlink_node", "target": format!("{coll}:{id}") });
    let record = match removal::write_removal_record(dumps_dir, "knowledge_unlink_node", source, &[(coll, &node_doc)], &edges).await {
        Ok(path) => path,
        Err(e) => return format!("Error: the removal record could not be written ({e}); nothing was removed."),
    };

    // Clear promoted_to on the entries that point at this node. Done before
    // the edge deletes so a partial failure leaves the system retryable
    // rather than with a stale pointer to a missing node.
    let mut cleared_entries: Vec<String> = Vec::new();
    for entry_id in &source_entries {
        if db.patch_document("memory.entries", entry_id, &json!({"promoted_to": null})).await.is_ok() {
            cleared_entries.push(entry_id.clone());
        }
    }

    // One delete per edge, by id: what the record holds, and nothing it
    // does not (a `delete_by_query` would take an edge written since).
    let mut edge_count: u64 = 0;
    for edge in &edges {
        let Some(edge_id) = edge.get("_id").and_then(|v| v.as_str()) else { continue };
        if db.delete("memory.edges", edge_id).await.is_ok() {
            edge_count += 1;
        }
    }

    if let Err(e) = db.delete(coll, id).await {
        return format!(
            "Error: cleared {} source entry(ies) and removed {} referencing edge(s) but failed to delete node {}:{}: {} (the record is {})",
            cleared_entries.len(), edge_count, coll, id, e, record.display()
        );
    }
    // The node is gone — drop its vector, as the merge does for its loser.
    // Left in the index, the key could not resolve, took a search slot, and
    // the count divergence reloaded the whole index on the next retrieval
    // (Embra#16).
    crate::embedding::write::forget_node(coll, id).await;
    removal::log_removal("knowledge_unlink_node", &format!("{coll}:{id}"), &summary, &record);

    format!(
        "{}{}",
        unlink_summary(coll, id, &preview, edge_count, &cleared_entries),
        removal::removal_report(&summary, Some(&record), false)
    )
}

/// What `knowledge_unlink_node` reports. The entries it left behind are
/// named: each is unpromoted now, and `forget` is the call that removes one.
fn unlink_summary(coll: &str, id: &str, preview: &str, edge_count: u64, cleared_entries: &[String]) -> String {
    let named = if cleared_entries.is_empty() {
        String::new()
    } else {
        format!(": {}", cleared_entries.join(", "))
    };
    format!(
        "Removed node {}:{} (\"{}\"), {} referencing edge(s), cleared promoted_to on {} source entry(ies){}",
        coll,
        id,
        preview,
        edge_count,
        cleared_entries.len(),
        named
    )
}

#[cfg(test)]
mod unlink_summary_tests {
    use super::unlink_summary;

    #[test]
    fn unlink_node_names_the_entries_it_leaves() {
        assert_eq!(
            unlink_summary("memory.semantic", "n1", "the cert…", 12, &["e1".to_string()]),
            "Removed node memory.semantic:n1 (\"the cert…\"), 12 referencing edge(s), cleared promoted_to on 1 source entry(ies): e1"
        );
        // A seed node, or one whose entry is gone: the line is the old one.
        assert_eq!(
            unlink_summary("memory.semantic", "n1", "x", 0, &[]),
            "Removed node memory.semantic:n1 (\"x\"), 0 referencing edge(s), cleared promoted_to on 0 source entry(ies)"
        );
    }
}

#[cfg(test)]
mod unlink_tests {
    use super::*;
    use crate::knowledge::removal::tests::{record_lines, TempDir};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn data(v: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": v, "meta": {}}))
    }

    fn body(req: &Request) -> serde_json::Value {
        serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null)
    }

    /// Node `n1`, promoted from `e1`, with three edges: its provenance edge,
    /// an automatic one and a link the brain authored.
    async fn node_with_three_edges() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/memory.semantic/docs/n1"))
            .respond_with(data(json!({"_id": "n1", "content": "the cert refresh works", "category": "fact", "embedding": "AAAA"})))
            .mount(&server)
            .await;
        let out_edges = json!([
            {"_id": "d1", "edge_type": "derived_from", "weight": 1.0, "source_collection": "memory.semantic", "source_id": "n1", "target_collection": "memory.entries", "target_id": "e1"},
            {"_id": "a1", "edge_type": "tag_overlap", "weight": 0.4, "source_collection": "memory.semantic", "source_id": "n1", "target_collection": "memory.semantic", "target_id": "n2"},
        ]);
        let in_edges = json!([
            {"_id": "c1", "edge_type": "refines", "weight": 0.9, "source_collection": "memory.semantic", "source_id": "n3", "target_collection": "memory.semantic", "target_id": "n1"},
        ]);
        Mock::given(method("POST"))
            .and(path("/memory.edges/query"))
            .and(|req: &Request| body(req)["filter"]["source_id"] == "n1")
            .respond_with(data(out_edges))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.edges/query"))
            .and(|req: &Request| body(req)["filter"]["target_id"] == "n1")
            .respond_with(data(in_edges))
            .mount(&server)
            .await;
        Mock::given(method("PATCH")).and(path("/memory.entries/docs/e1")).respond_with(data(json!({}))).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(data(json!({}))).mount(&server).await;
        server
    }

    /// Every request that writes, in order.
    async fn writes_seen(server: &MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "DELETE" || r.method.as_str() == "PATCH")
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect()
    }

    #[tokio::test]
    async fn unlink_writes_the_record_then_clears_the_pointer_and_deletes_by_id() {
        let server = node_with_three_edges().await;
        let db = WardsonDbClient::from_url(&server.uri());
        let dir = TempDir::new("unlink");
        let out = knowledge_unlink_node_at("memory.semantic:n1", &db, false, dir.path()).await;
        assert!(
            out.starts_with("Removed node memory.semantic:n1 (\"the cert refresh works\"), 3 referencing edge(s), cleared promoted_to on 1 source entry(ies): e1\n  edges removed: 3 (auto 1, curated 1, other 1)\n  auto: tag_overlap=1\n  curated: refines=1\n    c1 memory.semantic:n3 → memory.semantic:n1 refines 0.90\n  other: derived_from=1\n  nodes removed: memory.semantic:n1\n  record: "),
            "{out}"
        );
        assert_eq!(
            writes_seen(&server).await,
            [
                "PATCH /memory.entries/docs/e1",
                "DELETE /memory.edges/docs/d1",
                "DELETE /memory.edges/docs/a1",
                "DELETE /memory.edges/docs/c1",
                "DELETE /memory.semantic/docs/n1",
            ]
        );
        let record = std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().path()).next().expect("one record");
        let lines = record_lines(&record);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0]["source"], json!({"tool": "knowledge_unlink_node", "target": "memory.semantic:n1"}));
        assert_eq!(lines[0]["collections"], json!(["memory.semantic", "memory.edges"]));
        assert_eq!(lines[1]["data"]["content"], "the cert refresh works", "the node's payload is in the record");
        assert_eq!(lines[4]["_id"], "c1");
    }

    #[tokio::test]
    async fn unlink_dry_run_reads_and_writes_nothing() {
        let server = node_with_three_edges().await;
        let db = WardsonDbClient::from_url(&server.uri());
        let dir = TempDir::new("unlink-dry");
        let out = knowledge_unlink_node_at("memory.semantic:n1", &db, true, dir.path()).await;
        assert_eq!(
            out,
            "Would remove node memory.semantic:n1 (\"the cert refresh works\"), 3 referencing edge(s), and clear promoted_to on 1 source entry(ies): e1\n  edges to remove: 3 (auto 1, curated 1, other 1)\n  auto: tag_overlap=1\n  curated: refines=1\n    c1 memory.semantic:n3 → memory.semantic:n1 refines 0.90\n  other: derived_from=1\n  nodes to remove: memory.semantic:n1\n  record: not written (dry_run)"
        );
        assert!(writes_seen(&server).await.is_empty());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn an_unlink_whose_record_cannot_be_written_removes_nothing() {
        let server = node_with_three_edges().await;
        let db = WardsonDbClient::from_url(&server.uri());
        let dir = TempDir::new("unlink-blocked");
        let blocked = dir.0.join("not-a-directory");
        std::fs::write(&blocked, b"x").unwrap();
        let out = knowledge_unlink_node_at("memory.semantic:n1", &db, false, blocked.to_str().unwrap()).await;
        assert!(out.starts_with("Error: the removal record could not be written ("), "{out}");
        assert!(out.ends_with("nothing was removed."), "{out}");
        assert!(writes_seen(&server).await.is_empty());
    }

    #[tokio::test]
    async fn an_unlink_whose_edges_cannot_be_read_removes_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/memory.semantic/docs/n1"))
            .respond_with(data(json!({"_id": "n1", "content": "x"})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.edges/query"))
            .respond_with(ResponseTemplate::new(500).set_body_string("timeout"))
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());
        let dir = TempDir::new("unlink-unread");
        let out = knowledge_unlink_node_at("memory.semantic:n1", &db, false, dir.path()).await;
        assert!(out.starts_with("Error: the edges of memory.semantic:n1 could not be read ("), "{out}");
        assert!(writes_seen(&server).await.is_empty());
    }
}

/// The fields `knowledge_update` never lets the model write: the identity
/// and provenance of a node (`_id`, where it came from, when), the counters
/// the read path keeps, and the embedding fields, which are derived from
/// the node's own text (a forged vector would poison every similarity
/// search; the write path and `/embeddings backfill` maintain them). The
/// guard and its test read this one list.
const IMMUTABLE: &[&str] = &[
    "_id",
    "source_entry_id",
    "source_session",
    "created_at",
    "access_count",
    "last_accessed",
    "updated_at",
    "embedding",
    "embedding_model",
    "embedding_updated_at",
];

/// `knowledge_update <collection>:<id> | <json_patch>` — update a semantic
/// or procedural node in place while preserving every referencing edge.
///
/// JSON patch is an object containing only the fields to change. Immutable fields
/// (`_id`, `source_entry_id`, `source_session`, `created_at`, `access_count`,
/// `last_accessed`, `updated_at`) are rejected. `updated_at` is auto-refreshed.
/// A non-null `superseded_by` is rejected too: supersession is a `supersedes`
/// edge (`knowledge_link`). A null clears a leftover field.
///
/// Edges referencing the node by id are preserved automatically — `memory.edges`
/// stores references by id, not content, so a PATCH on the node doc leaves every
/// edge intact.
///
/// Auto-derived edges (`tag_overlap`, `temporal`) are NOT re-derived. If a tag
/// change makes specific edges stale, follow up with `knowledge_unlink_edge`.
pub async fn knowledge_update(params: &str, db: &WardsonDbClient, config: &SystemConfig) -> String {
    let trimmed = params.trim();
    if trimmed.is_empty() {
        return "knowledge_update rejected (missing arguments). Usage: knowledge_update <collection>:<id> | <json_patch>".into();
    }
    let Some((target, patch_str)) = trimmed.split_once('|') else {
        return "knowledge_update rejected (missing `|` separator). Usage: knowledge_update <collection>:<id> | <json_patch>".into();
    };
    let Some((coll, id)) = target.trim().split_once(':') else {
        return "knowledge_update rejected (target must be <collection>:<id>)".into();
    };
    let coll = coll.trim();
    let id = id.trim();
    if id.is_empty() {
        return "knowledge_update rejected (missing id after `:`)".into();
    }

    // Two collections, by name. `identity.graph` is not one of them: its
    // nodes are the projection of the sealed document. The projection adds
    // what is missing and never patches what exists, so a change made here
    // would stay and disagree with the seal.
    if coll != "memory.semantic" && coll != "memory.procedural" {
        return format!(
            "knowledge_update rejected (collection '{}' not supported — only memory.semantic or memory.procedural). An entry is corrected through its node: update the node it was promoted to. Only an entry that has no node is replaced with forget + remember.",
            coll
        );
    }

    let mut patch: serde_json::Value = match serde_json::from_str(patch_str.trim()) {
        Ok(v) => v,
        Err(e) => return format!("knowledge_update rejected (invalid JSON patch: {})", e),
    };
    let obj = match patch.as_object_mut() {
        Some(o) => o,
        None => return "knowledge_update rejected (JSON patch must be an object)".into(),
    };
    if obj.is_empty() {
        return "knowledge_update rejected (JSON patch is empty — nothing to update)".into();
    }

    for field in IMMUTABLE {
        if obj.contains_key(*field) {
            return format!(
                "knowledge_update rejected (field '{}' is immutable)",
                field
            );
        }
    }
    // Supersession is an edge, not a field: the field was a convention that
    // nothing read and no read surface showed. A null still passes, so a
    // field migration v14 could not convert can be cleared.
    if obj.get("superseded_by").is_some_and(|v| !v.is_null()) {
        return "knowledge_update rejected (superseded_by is not a field: link the newer node to this one with knowledge_link, edge_type supersedes)".into();
    }

    let existing = match db.read(coll, id).await {
        Ok(doc) => doc,
        Err(e) if crate::db::error::is_not_found(&e) => {
            return format!("knowledge_update rejected (node {}:{} not found)", coll, id);
        }
        Err(e) => return format!("knowledge_update rejected (could not read node {}:{}: {})", coll, id, e),
    };

    let changed_fields: Vec<String> = obj.keys().cloned().collect();
    obj.insert(
        "updated_at".to_string(),
        json!(chrono::Utc::now().to_rfc3339()),
    );

    if let Err(e) = db.patch_document(coll, id, &patch).await {
        return format!("knowledge_update failed: {}", e);
    }

    // Re-embed when the node's own embeddable text changed. Editing tags
    // must not pay for an inference pass; editing `content` must, or the
    // vector silently describes the pre-edit node.
    if changed_fields
        .iter()
        .any(|f| matches!(f.as_str(), "content" | "title" | "description" | "preconditions" | "steps"))
        && let Ok(updated) = db.read(coll, id).await
    {
        crate::embedding::write::embed_node(db, config, coll, id, &updated, false).await;
    }

    let preview_src = existing
        .get("content")
        .or_else(|| existing.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("(no preview)");
    let preview = content_preview(preview_src, 60);

    format!(
        "Updated {}:{} (\"{}\") — {} field(s): {}",
        coll,
        id,
        preview,
        changed_fields.len(),
        changed_fields.join(", ")
    )
}

/// `knowledge_traverse <collection>:<id> [depth [edge_types] [min_weight]]`
pub async fn knowledge_traverse(
    params: &str,
    db: &WardsonDbClient,
    config: &SystemConfig,
) -> String {
    // Tokenize by whitespace, first token = collection:id
    let mut toks = params.split_whitespace();
    let Some(start) = toks.next() else {
        return "Error: usage knowledge_traverse <collection>:<id> [depth [edge_types] [min_weight]]".into();
    };
    let Some((start_coll, start_id)) = start.split_once(':') else {
        return "Error: start must be <collection>:<id>".into();
    };

    // Validate start node exists — distinguishes "not found" from "no edges"
    if db.read(start_coll, start_id).await.is_err() {
        return format!("Error: Node {}:{} not found", start_coll, start_id);
    }

    let depth: u32 = toks.next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(config.kg_max_traversal_depth);

    // parse_lossy: operators can filter on free-form identity relations
    // (e.g. `has_trait`) — the built-in types still parse to their variants.
    let edge_types: Option<Vec<EdgeType>> = toks.next().and_then(|s| {
        let parsed: Vec<EdgeType> = s.split(',')
            .filter_map(EdgeType::parse_lossy)
            .collect();
        if parsed.is_empty() { None } else { Some(parsed) }
    });

    let min_weight: Option<f64> = toks.next().and_then(|s| s.parse().ok());

    // Prefetch the promoted-node collections (7ms for the whole set) so the
    // BFS resolves node docs in memory; entries hit the point-read fallback.
    let mut store = NodeStore::prefetch_promoted(db).await;
    let start = [(start_coll.to_string(), start_id.to_string())];
    let result = match traverse_multi(db, &start, depth, edge_types, min_weight, config, &mut store).await {
        Ok(r) => r,
        Err(e) => return format!("Error: traversal failed: {}", e),
    };

    // Format output. One read of the declarations marks every superseded
    // node the traversal prints, however many there are.
    let superseders = super::supersession::superseders(db).await;
    let mark = |collection: &str, id: &str| {
        super::supersession::superseded_note(super::supersession::successor_of(&superseders, collection, id))
    };
    let mut out = String::new();
    out.push_str(&format!("Traversal from {}:{}\n", start_coll, start_id));
    if let Some(start_node) = result.nodes.iter().find(|n| n.depth == 0) {
        let kind = match &start_node.node_type {
            NodeType::Episodic => "episodic".to_string(),
            NodeType::Semantic { category } => format!("semantic/{}", category.as_str()),
            NodeType::Procedural { title } => format!("procedural: {}", title),
            NodeType::Identity { node_type } => format!("identity/{}", node_type),
        };
        out.push_str(&format!(
            "Starting node: \"{}\" ({}){}\n\n",
            start_node.content_preview,
            kind,
            mark(&start_node.collection, &start_node.id)
        ));
    }

    // Group edges by depth of the target node
    let mut max_seen = 0u32;
    for d in 1..=result.depth_reached {
        let at_depth: Vec<_> = result.nodes.iter().filter(|n| n.depth == d).collect();
        if at_depth.is_empty() { continue; }
        max_seen = max_seen.max(d);
        out.push_str(&format!("Depth {} ({} nodes):\n", d, at_depth.len()));
        for node in at_depth {
            // Find the edge that leads to this node
            let edge = result.edges.iter()
                .find(|e| e.target_id == node.id && e.target_collection == node.collection);
            let (etype, w) = edge
                .map(|e| (e.edge_type.as_str(), e.weight))
                .unwrap_or(("?", 0.0));
            out.push_str(&format!(
                "  → {}:{} via {} (w={:.2}){}\n    \"{}\"\n",
                node.collection, node.id, etype, w, mark(&node.collection, &node.id), node.content_preview
            ));
        }
        out.push('\n');
    }

    // Edge type distribution
    let mut type_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for e in &result.edges {
        *type_counts.entry(e.edge_type.as_str()).or_insert(0) += 1;
    }
    let dist: Vec<String> = type_counts.iter().map(|(t, c)| format!("{}={}", t, c)).collect();
    out.push_str(&format!(
        "Summary: {} nodes visited, max depth {}, edges: {}",
        result.nodes_visited, max_seen, if dist.is_empty() { "none".into() } else { dist.join(", ") }
    ));
    if result.truncated {
        out.push_str("\n[!] traversal truncated: node budget reached (kg_traversal_node_budget)");
    }
    // Access-touch the RETURNED node set (2026-07-04 returned-only
    // semantics) in one background task — BFS visiting no longer touches.
    spawn_access_touches(
        db.clone(),
        result.nodes.iter().map(|n| (n.collection.clone(), n.id.clone())).collect(),
    );
    let _ = max_seen;
    out
}

/// What retrieval considered before ranking, so "few results" is told from
/// "few candidates", and a read that failed from an empty graph (#57).
pub(crate) fn candidates_line(stats: &RetrievalStats) -> String {
    let mut line = format!(
        "Candidates considered: {} (direct {}, session {}, similarity {} — pre-ranking)",
        stats.candidates_total, stats.direct_query, stats.session_based, stats.embedding
    );
    if !stats.failed_reads.is_empty() {
        line.push_str(&format!(
            " — reads that failed: {}; the graph may hold more than this",
            stats.failed_reads.join(", ")
        ));
    }
    line.push('\n');
    line
}

/// `knowledge_query <query_text> [| <max_results> [| <categories_csv>]]`
pub async fn knowledge_query(
    params: &str,
    db: &WardsonDbClient,
    session_name: &str,
    config: &SystemConfig,
) -> String {
    // Pipe-delimited: query_text | max_results | categories_csv
    let parts: Vec<&str> = params.splitn(3, '|').map(|s| s.trim()).collect();
    let query_text = parts.first().copied().unwrap_or("").trim();
    if query_text.is_empty() {
        return "Error: usage knowledge_query <query_text> [| <max_results> [| <categories_csv>]]".into();
    }

    let max_results: usize = parts
        .get(1)
        .and_then(|s| if s.is_empty() { None } else { s.parse::<usize>().ok() })
        .map(|n| n.clamp(1, 100))
        .unwrap_or(20);

    // Retrieve more than max_results before category filtering so filtering
    // doesn't starve the output. Cap at 100 internally.
    let retrieve_n = if parts.get(2).map(|s| !s.is_empty()).unwrap_or(false) {
        (max_results * 3).clamp(20, 100)
    } else {
        max_results
    };

    let category_filter: Option<Vec<SemanticCategory>> = parts.get(2).and_then(|csv| {
        if csv.is_empty() { return None; }
        let cats: Vec<SemanticCategory> = csv.split(',')
            .filter_map(|c| SemanticCategory::from_str(c.trim()))
            .collect();
        if cats.is_empty() { None } else { Some(cats) }
    });

    // Derive tags from the query text — shared rule (knowledge/text.rs):
    // punctuation-trimmed, deduped, same as auto-enrichment's derivation.
    let query_tags: Vec<String> = super::text::query_tag_tokens(query_text);

    let (mut results, stats) = match retrieve_relevant_knowledge(
        db, session_name, &query_tags, query_text, &[], retrieve_n, config
    ).await {
        Ok(rs) => rs,
        Err(e) => return format!("Error: retrieval failed: {}", e),
    };

    // Apply category filter (only affects semantic nodes; episodic/procedural pass through)
    if let Some(ref allowed) = category_filter {
        results.retain(|r| match &r.node.node_type {
            NodeType::Semantic { category } => allowed.iter().any(|c| c == category),
            _ => true,
        });
    }

    // Truncate to max_results after filtering
    results.truncate(max_results);

    // Count by source. The `other` bucket has had no producer since graph
    // expansion was deleted (2026-09-08); it is kept so an unrecognized
    // source label shows up in the header instead of vanishing.
    let mut direct = 0usize;
    let mut session = 0usize;
    let mut other = 0usize;
    for r in &results {
        match r.source.as_str() {
            "direct_query" => direct += 1,
            "session_based" => session += 1,
            _ => other += 1,
        }
    }

    // The header counts the RETURNED set; this line reports what retrieval
    // actually considered before ranking, so "few results" can be told apart
    // from "few candidates", and a read that failed from an empty graph.
    let candidates_line = candidates_line(&stats);

    if results.is_empty() {
        return format!(
            "Knowledge query: \"{}\" (0 results)\n{}",
            query_text, candidates_line
        );
    }

    let mut out = format!(
        "Knowledge query: \"{}\" ({} results — direct: {}, session: {}, other: {})\n",
        query_text, results.len(), direct, session, other
    );
    out.push_str(&candidates_line);
    if direct == 0 {
        out.push_str("[No direct matches — these are session-adjacent results]\n");
    }
    out.push('\n');
    let superseders = super::supersession::superseders(db).await;
    for (i, r) in results.iter().enumerate() {
        let note = super::supersession::superseded_note(super::supersession::successor_of(
            &superseders,
            &r.node.collection,
            &r.node.id,
        ));
        out.push_str(&query_result_line(i + 1, r, &note));
    }
    out
}

/// One result of `knowledge_query`. The node is named as `collection:id`,
/// the form `knowledge_traverse` prints and the id tools take
/// (`knowledge_update`, `knowledge_link`, `knowledge_merge`,
/// `knowledge_traverse`, `forget`), so a node found here is acted on
/// without a second lookup through `recall`. Enrichment's
/// `retrieved_context_block` names a node the same way.
fn query_result_line(index: usize, r: &super::types::RankedNode, superseded_note: &str) -> String {
    format!(
        "{}. [{}:{}] {} (score: {:.2}){}\n   Source: {}\n\n",
        index, r.node.collection, r.node.id, r.node.content_preview, r.score, superseded_note, r.source
    )
}

/// `knowledge_graph_stats` — windowless. Totals come from server-side
/// `count_only` and distributions from aggregate `$group`, so the report is
/// exact at ANY collection size. (The old version pulled every document
/// through fixed 10k/100k windows — ~91k full edge docs per call at
/// production scale — and went silently partial once a collection outgrew
/// its window.) Node counts include `identity.graph` (the sealed-graph
/// projection) so density's denominator covers every node kind whose edges
/// sit in the numerator, and the edge section carries a provenance split
/// via a `metadata.origin` group (KG review 2026-07-26, A2 + A4).
pub async fn knowledge_graph_stats(db: &WardsonDbClient) -> String {
    let mut out = String::from("Knowledge Graph Statistics:\n\n");

    // Semantic count + category distribution
    let sem_total = db.count("memory.semantic").await.unwrap_or(0);
    out.push_str(&format!("memory.semantic: {} nodes\n", sem_total));
    let cat_counts = group_counts(
        &db.aggregate("memory.semantic", &group_by_pipeline("category")).await.unwrap_or_default(),
    );
    if !cat_counts.is_empty() {
        let cats: Vec<String> = ["fact", "preference", "decision", "observation", "pattern"]
            .iter()
            .map(|k| format!("{}={}", k, cat_counts.get(*k).copied().unwrap_or(0)))
            .collect();
        out.push_str(&format!("  Categories: {}\n", cats.join(", ")));
    }
    out.push_str(&edited_nodes_line(db, "memory.semantic", sem_total).await);
    // Seeded-node counts render only when a pack is loaded — legacy output
    // stays byte-stable.
    let sem_seeded = db
        .count_filtered("memory.semantic", &seeded_nodes_filter())
        .await
        .unwrap_or(0);
    if sem_seeded > 0 {
        out.push_str(&format!("  Seeded (knowledge_seed): {}\n", sem_seeded));
        out.push_str(&edited_seed_nodes_line(db, "memory.semantic").await);
    }
    out.push('\n');

    // Procedural count
    let proc_total = db.count("memory.procedural").await.unwrap_or(0);
    out.push_str(&format!("memory.procedural: {} nodes\n", proc_total));
    out.push_str(&edited_nodes_line(db, "memory.procedural", proc_total).await);
    let proc_seeded = db
        .count_filtered("memory.procedural", &seeded_nodes_filter())
        .await
        .unwrap_or(0);
    if proc_seeded > 0 {
        out.push_str(&format!("  Seeded (knowledge_seed): {}\n", proc_seeded));
        out.push_str(&edited_seed_nodes_line(db, "memory.procedural").await);
    }
    out.push('\n');

    // Identity nodes — the sealed-graph projection (schema v13). Counted so
    // the density denominator covers every node kind whose edges are in the
    // numerator; zero on legacy flat-soul instances (collection exists,
    // projection doesn't run).
    let ident_total = db
        .count(crate::identity_graph::IDENTITY_COLLECTION)
        .await
        .unwrap_or(0);
    out.push_str(&format!("identity.graph: {} nodes\n\n", ident_total));

    // Edge count + type distribution
    let edge_total = db.count("memory.edges").await.unwrap_or(0);
    out.push_str(&format!("memory.edges: {} edges\n", edge_total));
    let etype_counts = group_counts(
        &db.aggregate("memory.edges", &group_by_pipeline("edge_type")).await.unwrap_or_default(),
    );
    if !etype_counts.is_empty() {
        let mut pairs: Vec<(String, u64)> =
            etype_counts.iter().map(|(t, c)| (t.clone(), *c)).collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let summary: Vec<String> = pairs.iter().map(|(t, c)| format!("{}={}", t, c)).collect();
        out.push_str(&format!("  Types: {}\n", summary.join(", ")));
    }
    let origin_counts = group_counts(
        &db.aggregate("memory.edges", &group_by_pipeline("metadata.origin"))
            .await
            .unwrap_or_default(),
    );
    out.push_str(&provenance_summary(&etype_counts, &origin_counts, edge_total));
    out.push('\n');

    // Entry promotion stats — promoted counted server-side via the filter
    // form of the is-promoted predicate (live non-null promoted_to pointer).
    let entries_total = db.count("memory.entries").await.unwrap_or(0);
    let promoted = db
        .count_filtered("memory.entries", &promoted_entries_filter())
        .await
        .unwrap_or(0);
    out.push_str(&format!(
        "memory.entries: {} total, {} promoted, {} unpromoted\n\n",
        entries_total,
        promoted,
        entries_total.saturating_sub(promoted)
    ));

    // Graph density
    let node_total = sem_total + proc_total + ident_total + entries_total;
    if node_total > 0 {
        let density = edge_total as f64 / node_total as f64;
        out.push_str(&format!("Graph density: {:.1} edges/node\n", density));
    }

    // Orphan edges (an endpoint that no longer exists), so nobody has to
    // call knowledge_sweep_orphans to know. Every edge is covered: the check
    // reads the endpoint indexes, not a window of edge documents.
    let orphans = orphan_line(&find_orphan_edges(db, STATS_ORPHAN_LIST_CAP).await);
    if !orphans.is_empty() {
        out.push_str(&orphans);
        out.push('\n');
    }
    // The promotion nodes no entry points at (#57), checked the same way.
    out.push_str(&pointerless_line(&find_pointerless_nodes(db).await));

    out
}

/// Orphan edges the stats report counts one by one. Past it the line says
/// "at least".
const STATS_ORPHAN_LIST_CAP: usize = 10_000;

/// The stats report's orphan line. Empty for a graph without edges; a check
/// that could not be read says so and gives no number.
fn orphan_line(scan: &Result<OrphanScan, String>) -> String {
    match scan {
        Ok(scan) if scan.scanned == 0 => String::new(),
        Ok(scan) => format!(
            "Orphan edges: {}{} of {} scanned{}",
            if scan.more { "at least " } else { "" },
            scan.orphans.len(),
            scan.scanned,
            if scan.orphans.is_empty() { "" } else { " (run knowledge_sweep_orphans to clean up)" }
        ),
        Err(e) => format!("Orphan edges: not checked ({e})"),
    }
}

/// Aggregate pipeline: count documents grouped by `field` (dot-paths reach
/// nested fields, e.g. `metadata.origin`). Output row count is bounded by
/// the field's cardinality — category = 5, but edge_type is UNBOUNDED since
/// `EdgeType::Other` carries free-form imported identity relations (19
/// distinct types after the first production import). Fine today because
/// the aggregate has no result window — revisit if one is ever added.
fn group_by_pipeline(field: &str) -> serde_json::Value {
    json!([{ "$group": { "_id": field, "count": { "$count": {} } } }])
}

/// Parse aggregate `$group` rows (`{"_id": <key>, "count": N}`) into a map.
/// Rows with non-string keys or missing counts are skipped.
fn group_counts(rows: &[serde_json::Value]) -> std::collections::HashMap<String, u64> {
    rows.iter()
        .filter_map(|r| {
            let key = r.get("_id").and_then(|v| v.as_str())?;
            let count = r.get("count").and_then(|v| v.as_u64())?;
            Some((key.to_string(), count))
        })
        .collect()
}

/// Server-side form of the is-promoted predicate: a live (non-null)
/// `promoted_to` pointer. WardSONDB's `$ne` skips explicit-null AND
/// missing-field docs — exactly matching `tools::entry_is_promoted`.
fn promoted_entries_filter() -> serde_json::Value {
    json!({ "promoted_to": { "$ne": null } })
}

/// Origin-only filter counting seed-pack nodes across all packs (the
/// per-pack fast-paths in knowledge/seed.rs add the `pack` key).
fn seeded_nodes_filter() -> serde_json::Value {
    json!({ "origin": super::seed::ORIGIN_SEED })
}

/// The seed nodes of `collection` that carry an operator edit — the ones a
/// changed pack will never revise. One windowed read; the line says when
/// the window filled, so a count under it is never taken as exact.
async fn edited_seed_nodes_line(db: &WardsonDbClient, collection: &str) -> String {
    let docs = db
        .query(collection, &super::seed::edited_seed_nodes_query_body())
        .await
        .unwrap_or_default();
    let saturated = crate::db::client::window_saturated(docs.len(), super::seed::EDITED_SEED_WINDOW);
    edited_seed_line(&super::seed::edited_seed_ids(&docs), saturated)
}

/// The rendered line: the ids, or `none`; and the window when it filled.
/// It names seed nodes only, and says so: read on its own, an unscoped
/// "Edited here: none" claimed that nothing in the graph was edited.
fn edited_seed_line(ids: &[String], saturated: bool) -> String {
    let mut line = String::from("  Seed nodes edited here (updated_at past created_at): ");
    if ids.is_empty() {
        line.push_str("none");
    } else {
        line.push_str(&ids.join(", "));
    }
    if saturated {
        line.push_str(&format!(
            " (the first {} seed nodes read)",
            super::seed::EDITED_SEED_WINDOW
        ));
    }
    line.push('\n');
    line
}

/// The nodes of `collection` edited in place: `updated_at` past
/// `created_at`, the predicate the seed reconcile uses
/// (`seed::operator_edited`); seeding and promotion write the two equal.
/// WardSONDB cannot compare two fields in a filter, so this is one
/// projected read under `MEMORY_FETCH_WINDOW`. The line says when the
/// window filled, and a read that failed says so rather than counting none.
async fn edited_nodes_line(db: &WardsonDbClient, collection: &str, total: u64) -> String {
    match db
        .fetch_recent_with_fields(collection, crate::db::MEMORY_FETCH_WINDOW, Some(&["created_at", "updated_at"]))
        .await
    {
        Ok(docs) => {
            let saturated = crate::db::client::window_saturated(docs.len(), crate::db::MEMORY_FETCH_WINDOW);
            edited_nodes_text(edited_in_place(&docs), docs.len(), total, saturated)
        }
        Err(e) => format!("  Edited in place (updated_at past created_at): not read ({e})\n"),
    }
}

/// How many of `docs` were edited in place.
fn edited_in_place(docs: &[serde_json::Value]) -> usize {
    docs.iter().filter(|d| super::seed::operator_edited(d)).count()
}

/// The rendered line: the count out of the collection, or out of the
/// window read when it filled.
fn edited_nodes_text(edited: usize, read: usize, total: u64, saturated: bool) -> String {
    if saturated {
        format!("  Edited in place (updated_at past created_at): {edited} of the newest {read} read\n")
    } else {
        format!("  Edited in place (updated_at past created_at): {edited} of {total}\n")
    }
}

/// Render the edge provenance split (KG review 2026-07-26, A4):
/// identity-projection edges (`metadata.origin` = `identity_import` |
/// `user_profile`) share type buckets with brain-authored `knowledge_link`
/// edges, so the type distribution alone cannot tell them apart. The
/// brain-authored count is exact arithmetic, not an estimate: free-form
/// (non-built-in) type names can only come from the projection
/// (`knowledge_link`'s strict gate never mints them), so
///   projected-in-builtin-buckets = origin-labeled − free-form-bucket sum
///   authored = brain-created-bucket sum − projected-in-builtin-buckets.
/// The origin line renders only when a projection exists (labeled > 0); a
/// failed or empty origin aggregate degrades to authored = brain-created
/// buckets, which is exactly right on legacy instances with no projection.
fn provenance_summary(
    etype_counts: &std::collections::HashMap<String, u64>,
    origin_counts: &std::collections::HashMap<String, u64>,
    edge_total: u64,
) -> String {
    let identity_import = origin_counts.get("identity_import").copied().unwrap_or(0);
    let user_profile = origin_counts.get("user_profile").copied().unwrap_or(0);
    let knowledge_seed = origin_counts.get("knowledge_seed").copied().unwrap_or(0);
    // Every origin-labeled bucket MUST join `labeled` — omitting one
    // inflates `unlabeled` AND under-subtracts from the authored count.
    let labeled = identity_import + user_profile + knowledge_seed;

    let mut builtin_brain_sum: u64 = 0;
    let mut other_sum: u64 = 0;
    for (name, count) in etype_counts {
        match EdgeType::from_str(name) {
            Some(t) if t.is_brain_created() => builtin_brain_sum += count,
            Some(_) => {} // auto-derived + derived_from: never authored, never mixed
            None => other_sum += count, // free-form relation names — projection-only
        }
    }
    let projected_in_builtin = labeled.saturating_sub(other_sum);
    let authored = builtin_brain_sum.saturating_sub(projected_in_builtin);

    let mut out = String::new();
    if labeled > 0 {
        out.push_str(&format!(
            "  Provenance: identity_import={}, user_profile={}, knowledge_seed={}, unlabeled={}\n",
            identity_import,
            user_profile,
            knowledge_seed,
            edge_total.saturating_sub(labeled)
        ));
    }
    out.push_str(&format!("  Brain-authored (knowledge_link): {}\n", authored));
    out
}

#[cfg(test)]
mod gate_tests {
    //! What the three write tools refuse before they touch the database.
    //! The client points nowhere: a gate that let a call through would
    //! answer "not found", not the refusal asserted here.
    use super::{knowledge_link, knowledge_unlink_node_at, knowledge_update, KG_DUMPS_DIR};
    use crate::config::SystemConfig;
    use crate::db::WardsonDbClient;

    fn nowhere() -> WardsonDbClient {
        WardsonDbClient::from_url("http://127.0.0.1:1")
    }

    // Deserialized, not a struct literal: see `learning::phases` tests.
    fn config() -> SystemConfig {
        serde_json::from_value(serde_json::json!({
            "name": "Embra",
            "api_key": "k",
            "timezone": "UTC",
            "deployment_mode": "phase1",
            "created_at": "",
            "version": "test"
        }))
        .expect("minimal config deserializes")
    }

    #[tokio::test]
    async fn an_identity_node_is_neither_updated_nor_removed() {
        let db = nowhere();
        let out = knowledge_update(r#"identity.graph:self | {"content":"x"}"#, &db, &config()).await;
        assert!(out.contains("not supported"), "{out}");
        let out = knowledge_unlink_node_at("identity.graph:self", &db, false, KG_DUMPS_DIR).await;
        assert!(out.contains("only operates on"), "{out}");
    }

    #[tokio::test]
    async fn the_model_cannot_write_any_immutable_field() {
        let db = nowhere();
        assert_eq!(super::IMMUTABLE.len(), 10, "the list the guard reads");
        for field in super::IMMUTABLE {
            let params = format!(r#"memory.semantic:n1 | {{"{field}":"x"}}"#);
            let out = knowledge_update(&params, &db, &config()).await;
            assert!(out.contains(&format!("'{field}' is immutable")), "{field}: {out}");
        }
    }

    /// The two input guards of `knowledge_link` run before any read: a
    /// self-loop and a weight outside (0, 1] are refused in their own
    /// words, and a valid link clears both and reaches the database stage
    /// (here an address that answers nothing, so no edge is created).
    #[tokio::test]
    async fn knowledge_link_refuses_a_self_loop_and_a_weight_outside_the_range() {
        let db = nowhere();
        let out = knowledge_link("memory.semantic:a | related_to | memory.semantic:a | 0.5", &db).await;
        assert!(out.contains("node to itself"), "{out}");
        for w in ["0", "1.5", "-0.1"] {
            let out = knowledge_link(&format!("memory.semantic:a | related_to | memory.semantic:b | {w}"), &db).await;
            assert!(out.contains("Weight must be"), "{w}: {out}");
        }
        let out = knowledge_link("memory.semantic:a | related_to | memory.semantic:b | 0.5", &db).await;
        assert!(
            !out.contains("node to itself") && !out.contains("Weight must be") && !out.starts_with("Created edge"),
            "{out}"
        );
    }

    /// Supersession is an edge now: a value for the old field is refused
    /// with the edge named, and a null (which clears the key) is let through
    /// to the database, here an address that answers nothing.
    #[tokio::test]
    async fn the_old_supersession_field_is_refused_and_can_be_cleared() {
        let db = nowhere();
        for value in [r#""01a0e8be""#, r#"{"collection":"memory.semantic","id":"n2"}"#] {
            let params = format!(r#"memory.semantic:n1 | {{"superseded_by":{value}}}"#);
            let out = knowledge_update(&params, &db, &config()).await;
            assert!(out.starts_with("knowledge_update rejected (superseded_by is not a field"), "{out}");
            assert!(out.contains("edge_type supersedes"), "{out}");
        }
        let cleared = knowledge_update(r#"memory.semantic:n1 | {"superseded_by":null}"#, &db, &config()).await;
        assert!(!cleared.contains("superseded_by is not a field"), "{cleared}");
    }

    #[tokio::test]
    async fn the_model_cannot_mint_an_edge_type() {
        let db = nowhere();
        // A free-form relation, and two types the strict parser knows and the
        // brain does not create.
        for edge_type in ["mentors", "same_session", "derived_from"] {
            let params = format!("memory.semantic:a | {edge_type} | memory.semantic:b | 0.5");
            let out = knowledge_link(&params, &db).await;
            assert!(out.starts_with("Error: Invalid edge type"), "{edge_type}: {out}");
        }
    }
}

#[cfg(test)]
mod windowless_stats_tests {
    //! Shape guards for the windowless graph_stats / paginated orphan scan
    //! (no DB mock in this crate — contracts are enforced at the builder
    //! level, same pattern as the FIX-1..8 query-body tests).
    use super::{
        group_by_pipeline, group_counts, next_scan_page, scan_page_query_body,
        promoted_entries_filter, provenance_summary, ScanPage,
    };
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn edited_seed_line_names_the_ids_or_none() {
        assert_eq!(
            super::edited_seed_line(&[], false),
            "  Seed nodes edited here (updated_at past created_at): none\n"
        );
        let ids = ["seed_kg_density".to_string(), "seed_kg_link_tool".to_string()];
        assert_eq!(
            super::edited_seed_line(&ids, false),
            "  Seed nodes edited here (updated_at past created_at): seed_kg_density, seed_kg_link_tool\n"
        );
        // A full window is said, never passed off as the whole list.
        assert!(super::edited_seed_line(&ids, true).contains("seed nodes read)"));
    }

    /// Every node counts, seed or not: an edit is an `updated_at` past
    /// `created_at`, and a node without the fields was never edited.
    #[test]
    fn nodes_edited_in_place_are_those_with_a_later_updated_at() {
        let docs = [
            serde_json::json!({"_id": "a", "created_at": "2026-09-18T22:23:35Z", "updated_at": "2026-10-11T15:04:11Z"}),
            serde_json::json!({"_id": "b", "created_at": "2026-09-18T22:23:35Z", "updated_at": "2026-09-18T22:23:35Z"}),
            serde_json::json!({"_id": "seed_kg_x", "created_at": "2026-07-31T00:00:00Z", "updated_at": "2026-10-01T00:00:00Z"}),
            serde_json::json!({"_id": "c"}),
        ];
        assert_eq!(super::edited_in_place(&docs), 2);
    }

    /// The count is out of the whole collection, or out of the window read
    /// when it filled; a full window is never passed off as the whole.
    #[test]
    fn the_edited_in_place_line_counts_the_collection_or_the_window() {
        assert_eq!(
            super::edited_nodes_text(238, 1328, 1328, false),
            "  Edited in place (updated_at past created_at): 238 of 1328\n"
        );
        assert_eq!(
            super::edited_nodes_text(4100, 10_000, 12_500, true),
            "  Edited in place (updated_at past created_at): 4100 of the newest 10000 read\n"
        );
    }

    #[test]
    fn group_pipeline_counts_by_field() {
        let p = group_by_pipeline("edge_type");
        assert_eq!(
            p,
            json!([{ "$group": { "_id": "edge_type", "count": { "$count": {} } } }])
        );
        // Dot-paths pass through verbatim — WardSONDB's extract_group_key
        // resolves them (the A4 provenance split depends on this).
        let p = group_by_pipeline("metadata.origin");
        assert_eq!(
            p,
            json!([{ "$group": { "_id": "metadata.origin", "count": { "$count": {} } } }])
        );
    }

    fn counts(pairs: &[(&str, u64)]) -> HashMap<String, u64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn provenance_review_numbers_yield_exact_authored_split() {
        // The 2026-07-26 KG review's live distribution: 765 edges sit in
        // built-in-name buckets, 26 in free-form identity buckets, 361
        // projection edges total — so 335 projected edges hide in built-in
        // buckets and exactly 430 are brain-authored (the review could only
        // estimate this; the origin aggregate makes it a measurement).
        let etypes = counts(&[
            ("same_session", 154_172),
            ("tag_overlap", 87_523),
            ("temporal", 41_904),
            ("derived_from", 771),
            ("related_to", 329),
            ("refines", 191),
            ("enables", 164),
            ("depends_on", 44),
            ("contradicts", 37),
            ("has_trait", 8),
            ("holds_value", 7),
            ("prefers", 3),
            ("serves", 2),
            ("bounded_by", 1),
            ("derives_form_from", 1),
            ("has_background", 1),
            ("has_role", 1),
            ("loyal_to", 1),
            ("profiles", 1),
        ]);
        let origins = counts(&[("identity_import", 354), ("user_profile", 7)]);
        let s = provenance_summary(&etypes, &origins, 285_161);
        assert!(s.contains(
            "Provenance: identity_import=354, user_profile=7, knowledge_seed=0, unlabeled=284800"
        ));
        assert!(s.contains("Brain-authored (knowledge_link): 430"));
    }

    #[test]
    fn provenance_seed_bucket_keeps_authored_exact() {
        // Seed edges may use built-in OR free-form relations; the exact
        // brain-authored arithmetic holds either way BECAUSE the seed
        // bucket joins `labeled`: built-in seed edges subtract out via
        // projected_in_builtin, free-form ones land in other_sum.
        let etypes = counts(&[
            ("related_to", 5),   // 2 authored + 3 seed (built-in relation)
            ("grounds", 4),      // seed free-form relation
        ]);
        let origins = counts(&[("knowledge_seed", 7)]);
        let s = provenance_summary(&etypes, &origins, 9);
        assert!(s.contains("knowledge_seed=7"));
        assert!(s.contains("unlabeled=2"));
        assert!(s.contains("Brain-authored (knowledge_link): 2"));
    }

    #[test]
    fn seeded_filter_is_origin_only() {
        assert_eq!(
            super::seeded_nodes_filter(),
            json!({ "origin": "knowledge_seed" })
        );
    }

    #[test]
    fn provenance_no_projection_hides_origin_line_authored_is_brain_buckets() {
        // Legacy instance (or failed/empty origin aggregate): no origin
        // labels means nothing projected — every built-in brain bucket is
        // genuinely authored, and the origin line would be noise.
        let etypes = counts(&[("same_session", 100), ("related_to", 5), ("enables", 2)]);
        let s = provenance_summary(&etypes, &HashMap::new(), 107);
        assert!(!s.contains("identity_import"));
        assert!(s.contains("Brain-authored (knowledge_link): 7"));
    }

    #[test]
    fn provenance_saturates_on_inconsistent_inputs() {
        // More origin-labeled edges than any bucket accounts for (drifted
        // live instance mid-scan): both subtractions saturate instead of
        // wrapping.
        let etypes = counts(&[("related_to", 3)]);
        let origins = counts(&[("identity_import", 50)]);
        let s = provenance_summary(&etypes, &origins, 10);
        assert!(s.contains("unlabeled=0"));
        assert!(s.contains("Brain-authored (knowledge_link): 0"));
    }

    #[test]
    fn group_counts_parses_rows_and_skips_malformed() {
        let rows = vec![
            json!({"_id": "same_session", "count": 42}),
            json!({"_id": "temporal", "count": 7}),
            json!({"_id": null, "count": 3}),        // non-string key: skipped
            json!({"_id": "tag_overlap"}),            // missing count: skipped
        ];
        let m = group_counts(&rows);
        assert_eq!(m.get("same_session"), Some(&42));
        assert_eq!(m.get("temporal"), Some(&7));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn promoted_filter_is_ne_null() {
        // $ne null matches live pointer objects only — explicit-null and
        // missing-field docs both read as unpromoted, matching
        // entry_is_promoted.
        assert_eq!(
            promoted_entries_filter(),
            json!({ "promoted_to": { "$ne": null } })
        );
    }

    #[test]
    fn scan_page_body_paginates_key_order_no_sort() {
        let body = scan_page_query_body(&json!({}), 20_000, &ScanPage::Offset(40_000));
        assert_eq!(body["limit"], json!(20_000));
        assert_eq!(body["offset"], json!(40_000));
        // Deliberately unsorted (doctrine exception): exhaustive pagination
        // rides stable key order; a sort would re-sort the whole collection
        // per page.
        assert!(body.get("sort").is_none());
        // Offset mode never carries a cursor (server rejects the combination).
        assert!(body.get("cursor").is_none());
    }

    #[test]
    fn cursor_page_body_has_cursor_no_offset_no_sort() {
        let body = scan_page_query_body(&json!({}), 20_000, &ScanPage::Cursor("tok123".into()));
        assert_eq!(body["limit"], json!(20_000));
        assert_eq!(body["cursor"], json!("tok123"));
        // cursor+offset is a 400 on cursor-capable servers; the modes are
        // mutually exclusive by construction.
        assert!(body.get("offset").is_none());
        assert!(body.get("sort").is_none());
    }

    #[test]
    fn scan_follows_server_cursor_when_offered() {
        // From either mode, a string next_cursor wins.
        let meta = json!({ "next_cursor": "abc" });
        match next_scan_page(&meta, &ScanPage::Offset(0), 20_000, 20_000) {
            Some(ScanPage::Cursor(t)) => assert_eq!(t, "abc"),
            other => panic!("expected cursor page, got {:?}", other.map(|p| p.label())),
        }
        match next_scan_page(&meta, &ScanPage::Cursor("prev".into()), 5, 20_000) {
            Some(ScanPage::Cursor(t)) => assert_eq!(t, "abc"),
            other => panic!("expected cursor page, got {:?}", other.map(|p| p.label())),
        }
    }

    #[test]
    fn scan_without_cursor_tiles_offsets_until_partial_page() {
        // Pre-cursor server: full page advances the offset by the returned
        // count (legacy behavior, byte-identical bodies)…
        let meta = json!({});
        match next_scan_page(&meta, &ScanPage::Offset(40_000), 20_000, 20_000) {
            Some(ScanPage::Offset(o)) => assert_eq!(o, 60_000),
            other => panic!("expected offset page, got {:?}", other.map(|p| p.label())),
        }
        // …and a partial page ends the scan.
        assert!(next_scan_page(&meta, &ScanPage::Offset(60_000), 12, 20_000).is_none());
    }

    #[test]
    fn cursor_mode_ends_exactly_when_server_withholds_cursor() {
        // A cursor-capable server omits next_cursor exactly at end-of-matches
        // — even on a full page. Never fall back to offset mid-walk.
        let meta = json!({});
        assert!(next_scan_page(&meta, &ScanPage::Cursor("prev".into()), 20_000, 20_000).is_none());
    }

    #[test]
    fn non_string_next_cursor_is_treated_as_absent() {
        for bad in [json!({ "next_cursor": null }), json!({ "next_cursor": 7 })] {
            assert!(next_scan_page(&bad, &ScanPage::Cursor("prev".into()), 5, 20_000).is_none());
            match next_scan_page(&bad, &ScanPage::Offset(0), 20_000, 20_000) {
                Some(ScanPage::Offset(o)) => assert_eq!(o, 20_000),
                other => panic!("expected offset page, got {:?}", other.map(|p| p.label())),
            }
        }
    }
}

/// Page size when the orphan check reads the edges of a missing node — well
/// under WardSONDB's `--max-query-limit` (100k) and bounds per-page memory.
pub(crate) const ORPHAN_SCAN_PAGE: usize = 20_000;

/// Position of one exhaustive-scan page: classic offset tiling, or a server
/// resume cursor. Page 1 is always `Offset(0)`; the mode for every later
/// page is decided by the server per `next_scan_page`.
pub(crate) enum ScanPage {
    Offset(usize),
    Cursor(String),
}

impl ScanPage {
    /// Human label for error messages ("failed at ...").
    pub(crate) fn label(&self) -> String {
        match self {
            ScanPage::Offset(offset) => format!("offset {}", offset),
            ScanPage::Cursor(_) => "cursor page".to_string(),
        }
    }
}

/// One page of an exhaustive maintenance scan. Deliberately UNSORTED — a
/// doctrine exception: the pagination rides the stable UUIDv7 key order
/// (the storage scan order). This is full-coverage maintenance, not a
/// relevance window (the invariant's target), and a sort would re-sort the
/// whole matched set on every page.
///
/// Offset mode tiles `offset`/`limit` (applied post-filter in every
/// WardSONDB executor path, so a constant filter tiles without skips or
/// duplicates) — but each page re-skips from key zero, O(n²) server work
/// across a full scan. Cursor mode resumes at the server-issued
/// `meta.next_cursor` token instead (O(n) total); the server rejects
/// `cursor` combined with `offset`, so the two are mutually exclusive here
/// by construction.
pub(crate) fn scan_page_query_body(
    filter: &serde_json::Value,
    page_limit: usize,
    page: &ScanPage,
) -> serde_json::Value {
    match page {
        ScanPage::Offset(offset) => {
            json!({ "filter": filter, "limit": page_limit, "offset": offset })
        }
        ScanPage::Cursor(token) => {
            json!({ "filter": filter, "limit": page_limit, "cursor": token })
        }
    }
}

/// Decide the next page (or `None` when the scan is complete), adapting to
/// the server build:
/// - `meta.next_cursor` present → cursor mode. WardSONDB builds with cursor
///   pagination emit it on no-sort full scans exactly when more matches
///   remain, so its absence after a cursor page is an exact end-of-scan.
/// - No cursor, offset mode → pre-cursor server (or an index-served plan,
///   which never bootstraps a no-sort cursor): keep tiling offsets until a
///   partial page, byte-identical to the legacy behavior.
pub(crate) fn next_scan_page(
    meta: &serde_json::Value,
    prev: &ScanPage,
    page_len: usize,
    page_limit: usize,
) -> Option<ScanPage> {
    if let Some(token) = meta.get("next_cursor").and_then(|v| v.as_str()) {
        return Some(ScanPage::Cursor(token.to_string()));
    }
    match prev {
        ScanPage::Cursor(_) => None,
        ScanPage::Offset(offset) => {
            (page_len >= page_limit).then(|| ScanPage::Offset(offset + page_len))
        }
    }
}

// ── orphan edges ─────────────────────────────────────────────────────────

/// The collections an edge endpoint can live in: every dumpable collection
/// except the edges. Guard `an_endpoint_can_live_in_every_node_collection`.
const ENDPOINT_COLLECTIONS: [&str; 4] = [
    "memory.entries",
    "memory.semantic",
    "memory.procedural",
    crate::identity_graph::IDENTITY_COLLECTION,
];

/// How many edges name one id, on each side.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct EndpointUse {
    as_source: u64,
    as_target: u64,
}

/// What the orphan check found.
#[derive(Debug, Default, PartialEq)]
struct OrphanScan {
    /// Edges covered: all of them.
    scanned: u64,
    /// Endpoint ids that no node collection holds.
    dangling: Vec<String>,
    /// The edges that name one of them, up to the caller's cap: the whole
    /// documents, for the record the sweep writes before it deletes.
    orphans: Vec<serde_json::Value>,
    /// The cap was reached and more such edges exist.
    more: bool,
}

impl OrphanScan {
    /// The ids of the listed edges, in the order they were read.
    fn orphan_ids(&self) -> Vec<&str> {
        self.orphans.iter().filter_map(|e| e.get("_id").and_then(|v| v.as_str())).collect()
    }
}

/// The two collections a promotion writes a node into.
const PROMOTION_COLLECTIONS: [&str; 2] = ["memory.semantic", "memory.procedural"];

/// A promotion node no entry's pointer names, with the entry it names as
/// its source.
#[derive(Debug, Clone, PartialEq)]
struct PointerlessNode {
    collection: &'static str,
    id: String,
    entry_id: String,
}

/// What the pointerless-node check found (#57): the promotion nodes no
/// entry's pointer names. A promotion cut between its node write and its
/// pointer patch leaves one; so did the `forget` of before 2026-10-03,
/// which removed an entry and left its node. The node is knowledge either
/// way, and the sweep heals or adopts it (`classify_pointerless`).
#[derive(Debug, Default, PartialEq)]
struct PointerlessScan {
    /// Promotion nodes checked: every node of the two collections that
    /// carries `source_entry_id`.
    checked: usize,
    /// Each one no pointer names.
    nodes: Vec<PointerlessNode>,
}

/// The promotion nodes no entry points at. The pointed-at ids come from one
/// `$group` over `promoted_to.id`, which covers every entry; the nodes from
/// one projected window per collection, refused when the window fills: a
/// node past it would go unchecked, and a list that may be short must never
/// drive a delete.
async fn find_pointerless_nodes(db: &WardsonDbClient) -> Result<PointerlessScan, String> {
    let rows = db
        .aggregate("memory.entries", &group_by_pipeline("promoted_to.id"))
        .await
        .map_err(|e| format!("grouping memory.entries by promoted_to.id failed: {e}"))?;
    let pointed: std::collections::HashSet<String> = group_counts(&rows).into_keys().collect();
    let mut scan = PointerlessScan::default();
    for collection in PROMOTION_COLLECTIONS {
        let docs = db
            .fetch_recent_with_fields(
                collection,
                crate::db::MEMORY_FETCH_WINDOW,
                Some(&["_id", "source_entry_id"]),
            )
            .await
            .map_err(|e| format!("reading the nodes of {collection} failed: {e}"))?;
        if docs.len() >= crate::db::MEMORY_FETCH_WINDOW {
            return Err(format!(
                "the {collection} window of {} is full; a node past it would go unchecked",
                crate::db::MEMORY_FETCH_WINDOW
            ));
        }
        for doc in &docs {
            let Some(entry_id) = doc.get("source_entry_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
                continue;
            };
            let Some(id) = doc.get("_id").and_then(|v| v.as_str()) else { continue };
            scan.checked += 1;
            if !pointed.contains(id) {
                scan.nodes.push(PointerlessNode { collection, id: id.to_string(), entry_id: entry_id.to_string() });
            }
        }
    }
    Ok(scan)
}

/// Ids the stats report lists for the pointerless-node check before
/// "and N more".
const STATS_POINTERLESS_IDS_SHOWN: usize = 10;

/// `a, b, c and N more`: at most `cap` of `ids`, which number `total`.
fn listed_ids<'a>(ids: impl IntoIterator<Item = &'a str>, total: usize, cap: usize) -> String {
    let shown: Vec<&str> = ids.into_iter().take(cap).collect();
    let mut out = shown.join(", ");
    if total > shown.len() {
        out.push_str(&format!(" and {} more", total - shown.len()));
    }
    out
}

/// The stats report's line for the pointerless-node check: the nodes are
/// named, and the line says what the sweep does with them.
fn pointerless_line(scan: &Result<PointerlessScan, String>) -> String {
    match scan {
        Ok(scan) if scan.nodes.is_empty() => {
            format!("Promotion nodes without their entry's pointer: 0 of {} checked", scan.checked)
        }
        Ok(scan) => {
            let names: Vec<String> = scan.nodes.iter().map(|n| format!("{}:{}", n.collection, n.id)).collect();
            format!(
                "Promotion nodes without their entry's pointer: {} of {} checked: {} (knowledge_sweep_orphans heals or adopts them; nothing is deleted)",
                scan.nodes.len(),
                scan.checked,
                listed_ids(names.iter().map(String::as_str), names.len(), STATS_POINTERLESS_IDS_SHOWN)
            )
        }
        Err(e) => format!("Promotion nodes without their entry's pointer: not checked ({e})"),
    }
}

/// Every id an edge names, with how often on each side: the keys of two
/// `$group` aggregates. `source_id` and `target_id` each carry a
/// single-field index, so the server answers from the index and reads no
/// edge document; without the index it scans, with the same answer. Either
/// way every edge is covered, at any collection size.
async fn edge_endpoints(
    db: &WardsonDbClient,
) -> Result<std::collections::BTreeMap<String, EndpointUse>, String> {
    let mut endpoints: std::collections::BTreeMap<String, EndpointUse> = Default::default();
    for field in ["source_id", "target_id"] {
        let rows = db
            .aggregate("memory.edges", &group_by_pipeline(field))
            .await
            .map_err(|e| format!("grouping memory.edges by {field} failed: {e}"))?;
        for (id, count) in group_counts(&rows) {
            let entry = endpoints.entry(id).or_default();
            if field == "source_id" {
                entry.as_source = count;
            } else {
                entry.as_target = count;
            }
        }
    }
    Ok(endpoints)
}

/// The ids of every node: one `$group` by `_id` per node collection. An
/// aggregate has no result window, so one request lists a collection of any
/// size.
///
/// The sweep deletes by this list, so a list that may be short is an error
/// and never an answer: a request that fails, and a list with fewer ids
/// than the collection counted just before it. A collection the server does
/// not have holds nothing.
async fn node_ids(db: &WardsonDbClient) -> Result<std::collections::HashSet<String>, String> {
    let mut ids: std::collections::HashSet<String> = Default::default();
    for collection in ENDPOINT_COLLECTIONS {
        let counted = match db.count(collection).await {
            Ok(n) => n,
            Err(e) if crate::db::error::is_not_found(&e) => continue,
            Err(e) => return Err(format!("counting {collection} failed: {e}")),
        };
        let rows = db
            .aggregate(collection, &group_by_pipeline("_id"))
            .await
            .map_err(|e| format!("listing the nodes of {collection} failed: {e}"))?;
        let listed = group_counts(&rows);
        if (listed.len() as u64) < counted {
            return Err(format!(
                "the node list of {collection} is incomplete: {} of {counted}",
                listed.len()
            ));
        }
        ids.extend(listed.into_keys());
    }
    Ok(ids)
}

/// The edges that name one of `dangling`, at most `cap` of them, and
/// whether more exist. One indexed read per id and side, paged like every
/// maintenance scan; an edge whose two ends both dangle is listed once.
async fn edges_naming(
    db: &WardsonDbClient,
    endpoints: &std::collections::BTreeMap<String, EndpointUse>,
    dangling: &[String],
    cap: usize,
) -> Result<(Vec<serde_json::Value>, bool), String> {
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut orphans: Vec<serde_json::Value> = Vec::new();
    for id in dangling {
        let used = endpoints.get(id).copied().unwrap_or_default();
        for (field, count) in [("source_id", used.as_source), ("target_id", used.as_target)] {
            if count == 0 {
                continue;
            }
            let filter = json!({ field: id });
            let mut page = ScanPage::Offset(0);
            loop {
                let body = scan_page_query_body(&filter, ORPHAN_SCAN_PAGE, &page);
                let (edges, meta) = db
                    .query_with_meta("memory.edges", &body)
                    .await
                    .map_err(|e| format!("reading the edges of {id} failed: {e}"))?;
                let page_len = edges.len();
                for edge in edges {
                    let Some(edge_id) = edge.get("_id").and_then(|v| v.as_str()) else { continue };
                    if !seen.insert(edge_id.to_string()) {
                        continue;
                    }
                    if orphans.len() == cap {
                        return Ok((orphans, true));
                    }
                    orphans.push(edge);
                }
                match next_scan_page(&meta, &page, page_len, ORPHAN_SCAN_PAGE) {
                    Some(next) => page = next,
                    None => break,
                }
            }
        }
    }
    Ok((orphans, false))
}

/// The orphan check: the edges whose source or target no node collection
/// holds, at most `cap` listed.
///
/// It covers every edge. The check it replaces read edge documents page by
/// page from the oldest, and the stats report stopped it at 100,000: what a
/// merge left at the newest end was never seen (Embra#10). Endpoints are
/// compared by id; an id is unique across the node collections.
async fn find_orphan_edges(db: &WardsonDbClient, cap: usize) -> Result<OrphanScan, String> {
    // The edges first, the nodes second: an edge is written after its two
    // nodes, so a node that an edge of the first reading names is in the
    // second unless it was deleted. The other order would report the edges
    // of a memory saved in between.
    let endpoints = edge_endpoints(db).await?;
    let scanned = endpoints.values().map(|u| u.as_source).sum();
    let nodes = node_ids(db).await?;
    let dangling: Vec<String> =
        endpoints.keys().filter(|id| !nodes.contains(*id)).cloned().collect();
    let (orphans, more) = edges_naming(db, &endpoints, &dangling, cap).await?;
    Ok(OrphanScan { scanned, dangling, orphans, more })
}

/// Remove the edges whose endpoints no longer resolve: what a delete left
/// that did not take the node's edges with it, or an edge written while its
/// node was being removed. `limit` is the most one call removes; the check
/// itself always covers every edge.
/// The most orphan edges one call removes. The default is 10,000; the
/// documents of the edges are held for the reply.
const SWEEP_LIMIT_MAX: usize = 100_000;

/// Ids and nodes listed in a sweep reply before "and N more".
const SWEEP_IDS_LISTED: usize = 100;

/// What the sweep does with a pointerless node. The node is knowledge: it is
/// never deleted for a missing pointer (2026-10-30, Embra #30/#32: the pass
/// of #57 deleted sixteen nodes whose entries the old `forget` had removed,
/// and 5,495 edges with them, 57 of them links the brain had authored).
#[derive(Debug, Clone, PartialEq)]
enum PointerlessAction {
    /// The entry exists and has no live node: its pointer is written at this
    /// node and the promotion finished.
    Heal { missing: super::promotion::Unfinished },
    /// The entry exists and points at another node: a pair for
    /// `knowledge_merge`; nothing is written.
    Duplicate { collection: String, id: String },
    /// The entry is gone: one is written back under its id, pointing at the
    /// node, and the promotion finished.
    Adopt { missing: super::promotion::Unfinished },
    /// The node or its entry could not be read; nothing is done to it.
    Unclassified(String),
}

/// One pointerless node, classified, with what the real run did to it.
#[derive(Debug, Clone, PartialEq)]
struct PointerlessCase {
    node: PointerlessNode,
    action: PointerlessAction,
    /// The node document, read at classification, for the writes.
    doc: Option<serde_json::Value>,
    /// The real run's result; `None` in a dry run and for an action that
    /// writes nothing.
    outcome: Option<Result<(), String>>,
}

impl PointerlessCase {
    /// The reply's line for the node: the same in both modes, with
    /// `: failed (…)` appended when the real run's writes failed.
    fn line(&self) -> String {
        let node = format!("{}:{}", self.node.collection, self.node.id);
        let entry = &self.node.entry_id;
        let mut line = match &self.action {
            PointerlessAction::Heal { missing } => {
                format!("{node} → heal (entry {entry} is unpromoted): pointer{}", missing.listed())
            }
            PointerlessAction::Adopt { missing } => {
                format!("{node} → adopt (entry {entry} is missing): entry {entry}, pointer{}", missing.listed())
            }
            PointerlessAction::Duplicate { collection, id } => format!(
                "{node} → duplicate of {collection}:{id} (entry {entry} points at it): knowledge_merge source={node} target={collection}:{id}"
            ),
            PointerlessAction::Unclassified(why) => format!("{node} → not classified ({why})"),
        };
        if let Some(Err(why)) = &self.outcome {
            line.push_str(&format!(": failed ({why})"));
        }
        line
    }
}

/// The counts a sweep reply prints for its pointerless nodes.
#[derive(Debug, Default, PartialEq)]
struct PointerlessTally {
    to_heal: usize,
    to_adopt: usize,
    duplicates: usize,
    healed: usize,
    adopted: usize,
}

impl PointerlessTally {
    fn of(cases: &[PointerlessCase]) -> Self {
        let mut tally = Self::default();
        for case in cases {
            let done = matches!(case.outcome, Some(Ok(())));
            match case.action {
                PointerlessAction::Heal { .. } => {
                    tally.to_heal += 1;
                    if done {
                        tally.healed += 1;
                    }
                }
                PointerlessAction::Adopt { .. } => {
                    tally.to_adopt += 1;
                    if done {
                        tally.adopted += 1;
                    }
                }
                PointerlessAction::Duplicate { .. } => tally.duplicates += 1,
                PointerlessAction::Unclassified(_) => {}
            }
        }
        tally
    }
}

/// Classify a pointerless node by its entry (`promotion::entry_state`) and
/// what its promotion still lacks (`promotion::unfinished`). Reads only.
async fn classify_pointerless(db: &WardsonDbClient, node: PointerlessNode) -> PointerlessCase {
    use super::promotion::{entry_state, unfinished, EntryState};
    let doc = match db.read(node.collection, &node.id).await {
        Ok(doc) => doc,
        Err(e) => {
            let action = PointerlessAction::Unclassified(format!("the node could not be read: {e}"));
            return PointerlessCase { node, action, doc: None, outcome: None };
        }
    };
    let action = match entry_state(db, &node.entry_id).await {
        Ok(EntryState::Unpromoted) => match unfinished(db, node.collection, &node.id, &node.entry_id, &doc).await {
            Ok(missing) => PointerlessAction::Heal { missing },
            Err(e) => PointerlessAction::Unclassified(e.to_string()),
        },
        Ok(EntryState::Missing) => match unfinished(db, node.collection, &node.id, &node.entry_id, &doc).await {
            Ok(missing) => PointerlessAction::Adopt { missing },
            Err(e) => PointerlessAction::Unclassified(e.to_string()),
        },
        Ok(EntryState::PointsAt { collection, id }) => PointerlessAction::Duplicate { collection, id },
        Err(e) => PointerlessAction::Unclassified(e.to_string()),
    };
    PointerlessCase { node, action, doc: Some(doc), outcome: None }
}

/// The real run's writes for a classified node: the pointer and the rest of
/// the promotion (heal), or the entry written back and the rest of the
/// promotion (adopt). A duplicate and an unclassified node are left alone.
async fn resolve_pointerless(db: &WardsonDbClient, config: &SystemConfig, case: &mut PointerlessCase) {
    let Some(doc) = case.doc.as_ref() else { return };
    let (collection, node_id, entry_id) = (case.node.collection, case.node.id.as_str(), case.node.entry_id.as_str());
    let result = match &case.action {
        PointerlessAction::Heal { missing } => {
            match db.patch_document("memory.entries", entry_id, &super::promotion::pointer_patch(collection, node_id)).await {
                Ok(()) => {
                    super::promotion::finish_promotion(db, config, collection, node_id, entry_id, doc, *missing).await;
                    Ok(())
                }
                Err(e) => Err(format!("the pointer could not be written on entry {entry_id}: {e}")),
            }
        }
        PointerlessAction::Adopt { missing } => adopt_entry(db, config, collection, node_id, entry_id, doc, *missing).await,
        PointerlessAction::Duplicate { .. } | PointerlessAction::Unclassified(_) => return,
    };
    case.outcome = Some(result);
}

/// Write the entry back for a node whose entry is gone, under the id the
/// node names when it is free. A taken id (409) gets a fresh one, and the
/// node's `source_entry_id` follows it, so the pointer, the `derived_from`
/// edge and the provenance agree.
async fn adopt_entry(
    db: &WardsonDbClient,
    config: &SystemConfig,
    collection: &'static str,
    node_id: &str,
    entry_id: &str,
    doc: &serde_json::Value,
    missing: super::promotion::Unfinished,
) -> Result<(), String> {
    use crate::tools::memory::adopted_entry_doc;
    let text = adopted_text(collection, doc);
    let tags = super::text::string_array(doc, "tags");
    let session = doc.get("source_session").and_then(|v| v.as_str()).unwrap_or("");
    let created_at = doc
        .get("created_at")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let node = (collection, node_id);
    let written = match db.write("memory.entries", &adopted_entry_doc(Some(entry_id), &text, &tags, session, &created_at, node)).await {
        Ok(id) => id,
        Err(e) if crate::db::error::is_conflict(&e) => {
            let fresh = db
                .write("memory.entries", &adopted_entry_doc(None, &text, &tags, session, &created_at, node))
                .await
                .map_err(|e| format!("entry {entry_id} is taken and a fresh one could not be written: {e}"))?;
            db.patch_document(collection, node_id, &json!({ "source_entry_id": fresh }))
                .await
                .map_err(|e| format!("entry {fresh} written, but the node's source_entry_id could not follow it: {e}"))?;
            fresh
        }
        Err(e) => return Err(format!("the entry could not be written: {e}")),
    };
    super::promotion::finish_promotion(db, config, collection, node_id, &written, doc, missing).await;
    Ok(())
}

/// The text an adopted entry carries: a semantic node's content; a
/// procedure's title and description, the form `recall` prints.
fn adopted_text(collection: &str, doc: &serde_json::Value) -> String {
    let field = |key: &str| doc.get(key).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if collection == "memory.procedural" {
        let (title, description) = (field("title"), field("description"));
        if description.is_empty() { title } else { format!("{title}: {description}") }
    } else {
        field("content")
    }
}

pub async fn knowledge_sweep_orphans(
    db: &WardsonDbClient,
    config: &SystemConfig,
    dry_run: bool,
    limit: usize,
) -> String {
    knowledge_sweep_orphans_at(db, config, dry_run, limit, KG_DUMPS_DIR).await
}

/// `knowledge_sweep_orphans` with the directory of its removal record as a
/// parameter, so a test runs it under a temporary one.
pub(crate) async fn knowledge_sweep_orphans_at(
    db: &WardsonDbClient,
    config: &SystemConfig,
    dry_run: bool,
    limit: usize,
    dumps_dir: &str,
) -> String {
    let limit = limit.clamp(1, SWEEP_LIMIT_MAX);
    // The nodes first. A pointerless node is healed or adopted and never
    // deleted, so nothing here changes what the edge check that follows
    // sees: the two modes scan the same graph, and their replies differ in
    // what was written, not in what was found.
    let pointerless = find_pointerless_nodes(db).await;
    let mut cases: Vec<PointerlessCase> = Vec::new();
    if let Ok(found) = &pointerless {
        for node in &found.nodes {
            cases.push(classify_pointerless(db, node.clone()).await);
        }
        if !dry_run {
            for case in &mut cases {
                resolve_pointerless(db, config, case).await;
            }
        }
    }
    let scan = match find_orphan_edges(db, limit).await {
        Ok(scan) => scan,
        Err(e) => {
            let tally = PointerlessTally::of(&cases);
            let nodes = if tally.healed + tally.adopted == 0 {
                "Nothing was deleted.".to_string()
            } else {
                format!(
                    "{} pointerless nodes were healed and {} adopted; no edge was deleted.",
                    tally.healed, tally.adopted
                )
            };
            return format!("knowledge_sweep_orphans: the check could not be completed ({e}). {nodes}");
        }
    };

    // The record first, then one delete per edge, by id. A `delete_by_query`
    // reads the whole collection whatever its filter — about a second per
    // call at 430,000 edges — so the old chunks of a hundred ids cost more
    // than a hundred deletes do, and the cost grew with the graph, not with
    // the orphans.
    let summary = RemovalSummary::of(&scan.orphans);
    let mut record: Option<std::path::PathBuf> = None;
    let mut deleted: u64 = 0;
    if !dry_run && !scan.orphans.is_empty() {
        let source = json!({ "tool": "knowledge_sweep_orphans", "target": serde_json::Value::Null, "dangling": scan.dangling });
        match removal::write_removal_record(dumps_dir, "knowledge_sweep_orphans", source, &[], &scan.orphans).await {
            Ok(path) => record = Some(path),
            Err(e) => {
                let tally = PointerlessTally::of(&cases);
                return format!(
                    "knowledge_sweep_orphans: the removal record could not be written ({e}); no edge was deleted ({} healed, {} adopted).",
                    tally.healed, tally.adopted
                );
            }
        }
        for id in scan.orphan_ids() {
            if db.delete("memory.edges", id).await.is_ok() {
                deleted += 1;
            }
        }
        if let Some(path) = &record {
            removal::log_removal("knowledge_sweep_orphans", "memory.edges", &summary, path);
        }
    }
    sweep_report(&scan, dry_run, deleted, limit, &pointerless, &cases, (&summary, record.as_deref()))
}

/// The sweep's reply. Its first four lines keep the shape they always had;
/// the missing-node ids, the pointerless nodes with what is done to each,
/// their counts, and the removal block (when there are orphan edges) come
/// before the limit note. A dry run prints the same lines with its counts
/// of writes at zero and no record.
fn sweep_report(
    scan: &OrphanScan,
    dry_run: bool,
    deleted: u64,
    limit: usize,
    pointerless: &Result<PointerlessScan, String>,
    cases: &[PointerlessCase],
    removal: (&RemovalSummary, Option<&std::path::Path>),
) -> String {
    let (summary, record) = removal;
    let mut out = format!(
        "knowledge_sweep_orphans{}:\n  scanned: {}\n  orphan_count: {}\n  deleted: {}",
        if dry_run { " (dry_run)" } else { "" },
        scan.scanned,
        scan.orphans.len(),
        deleted
    );
    if !scan.dangling.is_empty() {
        out.push_str(&format!(
            "\n  missing_nodes: {}\n  missing_node_ids: {}",
            scan.dangling.len(),
            listed_ids(scan.dangling.iter().map(String::as_str), scan.dangling.len(), SWEEP_IDS_LISTED)
        ));
    }
    match pointerless {
        Ok(found) => {
            out.push_str(&format!("\n  pointerless_nodes: {}", found.nodes.len()));
            for case in cases.iter().take(SWEEP_IDS_LISTED) {
                out.push_str("\n    ");
                out.push_str(&case.line());
            }
            if cases.len() > SWEEP_IDS_LISTED {
                out.push_str(&format!("\n    and {} more", cases.len() - SWEEP_IDS_LISTED));
            }
            let tally = PointerlessTally::of(cases);
            out.push_str(&format!(
                "\n  to_heal: {}\n  to_adopt: {}\n  duplicates: {}\n  healed: {}\n  adopted: {}",
                tally.to_heal, tally.to_adopt, tally.duplicates, tally.healed, tally.adopted
            ));
        }
        Err(e) => out.push_str(&format!("\n  pointerless_nodes: not checked ({e})")),
    }
    if !scan.orphans.is_empty() {
        out.push_str(&removal::removal_report(summary, record, dry_run));
    }
    if scan.more {
        out.push_str(&format!(
            "\n  more orphan edges exist than the limit of {limit}: run it again"
        ));
    }
    out
}

// ── knowledge_dump — JSONL export of the knowledge graph ──

/// Dump directory. Inside the tool-layer workspace jail (`/embra/workspace`,
/// bind-mounted from DATA at boot) but outside `repos/` — dumps are exports,
/// not repository content. Full-path const per repo precedent; do not widen
/// engineering's `WORKSPACE_ROOT` visibility for this.
pub(crate) const KG_DUMPS_DIR: &str = "/embra/workspace/KG_DUMPS";

/// Page size for the exhaustive dump scan — same bound rationale as
/// `ORPHAN_SCAN_PAGE` (well under the server's 100k `--max-query-limit`,
/// bounds per-page memory).
const DUMP_SCAN_PAGE: usize = 20_000;

/// The dumpable collections as `(short_name, wardsondb_collection)` in
/// canonical dump order: nodes first (entries, semantic, procedural,
/// identity), then edges. Short names are the tool-facing vocabulary.
const DUMP_COLLECTIONS: &[(&str, &str)] = &[
    ("entries", "memory.entries"),
    ("semantic", "memory.semantic"),
    ("procedural", "memory.procedural"),
    ("identity", crate::identity_graph::IDENTITY_COLLECTION),
    ("edges", "memory.edges"),
];

/// One page of the exhaustive dump scan (see `scan_page_query_body` for the
/// no-sort doctrine exception and the offset/cursor mode contract).
fn dump_page_query_body(
    filter: &serde_json::Value,
    page_limit: usize,
    page: &ScanPage,
) -> serde_json::Value {
    scan_page_query_body(filter, page_limit, page)
}

/// Edge-type restriction for the edges pass.
fn dump_edge_type_filter(edge_types: &[String]) -> serde_json::Value {
    json!({ "edge_type": { "$in": edge_types } })
}

/// Map tool-facing short names to `memory.*` collections in canonical dump
/// order regardless of input order. `None` selects all five; unknown names
/// and an empty selection are errors.
fn resolve_dump_collections(requested: Option<&[String]>) -> Result<Vec<&'static str>, String> {
    let Some(requested) = requested else {
        return Ok(DUMP_COLLECTIONS.iter().map(|(_, full)| *full).collect());
    };
    if requested.is_empty() {
        return Err(
            "collections is empty — omit it for a full dump, or pick from: entries, semantic, procedural, identity, edges"
                .into(),
        );
    }
    for name in requested {
        if !DUMP_COLLECTIONS
            .iter()
            .any(|(short, _)| *short == name.as_str())
        {
            return Err(format!(
                "Unknown collection '{}'. Valid: entries, semantic, procedural, identity, edges",
                name
            ));
        }
    }
    Ok(DUMP_COLLECTIONS
        .iter()
        .filter(|(short, _)| requested.iter().any(|r| r.as_str() == *short))
        .map(|(_, full)| *full)
        .collect())
}

/// Validate the edge-type restriction. Any non-empty token is dumpable —
/// the built-ins plus free-form identity-projection relations (the
/// `$in` filter matches stored strings verbatim); an empty list or an
/// empty/whitespace token is an error.
fn validate_dump_edge_types(edge_types: &[String]) -> Result<(), String> {
    if edge_types.is_empty() {
        return Err("edge_types is empty — omit it to dump all edge types".into());
    }
    for t in edge_types {
        if t.trim().is_empty() {
            return Err("edge_types contains an empty entry".into());
        }
    }
    Ok(())
}

/// First line of every dump — provenance for the file itself. Consumers
/// keyed on `type` (e.g. the kg_scan guardian example) skip it as an
/// unknown record type.
pub(crate) fn dump_meta_record(
    collections: &[&str],
    edge_types: Option<&[String]>,
    include_payload: bool,
    source: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut meta = json!({
        "type": "meta",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "collections": collections,
        "edge_types": edge_types,
        "include_payload": include_payload,
    });
    if let Some(source) = source {
        meta["source"] = source.clone();
    }
    meta
}

/// A node line: `type`/`_id`/`collection` lifted top-level for scanners; the
/// full stored doc rides under `data` unless the dump is slim.
pub(crate) fn dump_node_record(
    doc: &serde_json::Value,
    collection: &str,
    include_payload: bool,
) -> serde_json::Value {
    let mut rec = json!({
        "type": "node",
        "_id": doc.get("_id").cloned().unwrap_or(serde_json::Value::Null),
        "collection": collection,
    });
    if include_payload {
        rec["data"] = doc.clone();
    }
    rec
}

/// An edge line: the stored doc's fields spread at top level (they already
/// carry source/target/edge_type/weight — see the `knowledge_link` write
/// shape) plus the `type` discriminator, which wins over any stored `type`
/// field.
pub(crate) fn dump_edge_record(doc: &serde_json::Value) -> serde_json::Value {
    let mut rec = doc.clone();
    if let Some(obj) = rec.as_object_mut() {
        obj.insert("type".to_string(), json!("edge"));
    }
    rec
}

/// Per-collection outcome of one dump pass.
struct DumpCollectionStat {
    collection: &'static str,
    written: usize,
    /// Authoritative server-side count for the same filter (parity signal;
    /// `None` when the count call itself failed).
    server_count: Option<u64>,
}

/// Write the meta line plus every selected collection into `writer`,
/// tiling each collection exhaustively via unsorted key-order pagination.
/// Returns per-collection stats and the byte total. Any query or write
/// error aborts the whole dump (the caller removes the partial file).
async fn write_dump_contents(
    db: &WardsonDbClient,
    writer: &mut tokio::io::BufWriter<tokio::fs::File>,
    selected: &[&'static str],
    edge_types: Option<&[String]>,
    include_payload: bool,
) -> Result<(Vec<DumpCollectionStat>, u64), String> {
    use tokio::io::AsyncWriteExt;

    let mut bytes: u64 = 0;
    let meta_line = format!(
        "{}\n",
        dump_meta_record(selected, edge_types, include_payload, None)
    );
    writer
        .write_all(meta_line.as_bytes())
        .await
        .map_err(|e| format!("write failed: {}", e))?;
    bytes += meta_line.len() as u64;

    let mut stats = Vec::new();
    for coll in selected {
        let is_edges = *coll == "memory.edges";
        let filter = if is_edges && let Some(types) = edge_types {
            dump_edge_type_filter(types)
        } else {
            json!({})
        };

        let mut written = 0usize;
        let mut page = ScanPage::Offset(0);
        loop {
            let (docs, meta) = db
                .query_with_meta(coll, &dump_page_query_body(&filter, DUMP_SCAN_PAGE, &page))
                .await
                .map_err(|e| format!("query {} failed at {}: {}", coll, page.label(), e))?;
            if docs.is_empty() {
                break;
            }
            let page_len = docs.len();
            for doc in &docs {
                let rec = if is_edges {
                    dump_edge_record(doc)
                } else {
                    dump_node_record(doc, coll, include_payload)
                };
                let line = format!("{}\n", rec);
                writer
                    .write_all(line.as_bytes())
                    .await
                    .map_err(|e| format!("write failed: {}", e))?;
                bytes += line.len() as u64;
                written += 1;
            }
            match next_scan_page(&meta, &page, page_len, DUMP_SCAN_PAGE) {
                Some(next) => page = next,
                None => break, // collection exhausted (exact in cursor mode)
            }
        }

        // Parity: authoritative server-side count for the same filter. A soft
        // signal only — a live instance can legitimately drift between the
        // scan and the count.
        let server_count = if is_edges && edge_types.is_some() {
            db.count_filtered(coll, &filter).await.ok()
        } else {
            db.count(coll).await.ok()
        };
        stats.push(DumpCollectionStat {
            collection: coll,
            written,
            server_count,
        });
    }

    writer
        .flush()
        .await
        .map_err(|e| format!("flush failed: {}", e))?;
    Ok((stats, bytes))
}

/// Export the knowledge graph as a JSONL dump under `KG_DUMPS_DIR`. On any
/// query or write error the partial file is removed — the format has a
/// header but no trailer, so a partial dump would otherwise be
/// indistinguishable from a complete one.
pub async fn run_knowledge_dump(
    db: &WardsonDbClient,
    collections: Option<Vec<String>>,
    edge_types: Option<Vec<String>>,
    include_payload: bool,
) -> Result<String, String> {
    let started = std::time::Instant::now();

    let selected = resolve_dump_collections(collections.as_deref())?;
    if let Some(types) = edge_types.as_deref() {
        validate_dump_edge_types(types)?;
        if !selected.contains(&"memory.edges") {
            return Err(
                "edge_types was provided but 'edges' is not among the dumped collections".into(),
            );
        }
    }

    tokio::fs::create_dir_all(KG_DUMPS_DIR)
        .await
        .map_err(|e| format!("Failed to create {}: {}", KG_DUMPS_DIR, e))?;
    let path = format!(
        "{}/kg-dump-{}.jsonl",
        KG_DUMPS_DIR,
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    );
    let file = tokio::fs::File::create(&path)
        .await
        .map_err(|e| format!("Failed to create {}: {}", path, e))?;
    let mut writer = tokio::io::BufWriter::new(file);

    match write_dump_contents(
        db,
        &mut writer,
        &selected,
        edge_types.as_deref(),
        include_payload,
    )
    .await
    {
        Ok((stats, bytes)) => {
            let mut out = format!("knowledge_dump → {}\n", path);
            for s in &stats {
                let parity = match s.server_count {
                    Some(n) if n == s.written as u64 => format!("server count {}", n),
                    Some(n) => format!("server count {} — mismatch, likely concurrent writes", n),
                    None => "server count unavailable".to_string(),
                };
                out.push_str(&format!(
                    "  {}: {} written ({})\n",
                    s.collection, s.written, parity
                ));
            }
            if let Some(types) = edge_types.as_deref() {
                out.push_str(&format!("  edge filter: {}\n", types.join(", ")));
            }
            if !include_payload {
                out.push_str("  mode: slim (node payloads omitted)\n");
            }
            let total: usize = stats.iter().map(|s| s.written).sum();
            out.push_str(&format!(
                "  total: {} records (+1 meta line), {} bytes, {:.2}s",
                total,
                bytes,
                started.elapsed().as_secs_f64()
            ));
            Ok(out)
        }
        Err(e) => {
            drop(writer);
            let _ = tokio::fs::remove_file(&path).await;
            Err(format!("Dump aborted ({}); partial file removed", e))
        }
    }
}

#[cfg(test)]
mod dump_shape_tests {
    //! Shape guards for the dump builders — contracts enforced at the
    //! builder level (no DB mock in this crate), same pattern as
    //! `windowless_stats_tests`.
    use super::{
        dump_edge_record, dump_edge_type_filter, dump_meta_record, dump_node_record,
        dump_page_query_body, resolve_dump_collections, validate_dump_edge_types, ScanPage,
    };
    use serde_json::json;

    #[test]
    fn dump_page_body_paginates_key_order_no_sort() {
        // Same doctrine exception as the orphan scan: exhaustive pagination
        // rides stable key order; a sort key must never appear.
        let empty = json!({});
        let body = dump_page_query_body(&empty, 20_000, &ScanPage::Offset(40_000));
        assert_eq!(body["limit"], json!(20_000));
        assert_eq!(body["offset"], json!(40_000));
        assert_eq!(body["filter"], json!({}));
        assert!(body.get("sort").is_none());
        assert!(body.get("cursor").is_none());

        let filtered = dump_edge_type_filter(&["enables".to_string()]);
        let body = dump_page_query_body(&filtered, 20_000, &ScanPage::Offset(0));
        assert_eq!(body["filter"], json!({"edge_type": {"$in": ["enables"]}}));
        assert!(body.get("sort").is_none());

        // Cursor mode keeps the same filter and drops offset (mutually
        // exclusive server-side); still no sort.
        let body = dump_page_query_body(&filtered, 20_000, &ScanPage::Cursor("tok".into()));
        assert_eq!(body["filter"], json!({"edge_type": {"$in": ["enables"]}}));
        assert_eq!(body["cursor"], json!("tok"));
        assert!(body.get("offset").is_none());
        assert!(body.get("sort").is_none());
    }

    #[test]
    fn dump_edge_type_filter_builds_dollar_in() {
        let f = dump_edge_type_filter(&["enables".to_string(), "refines".to_string()]);
        assert_eq!(f, json!({"edge_type": {"$in": ["enables", "refines"]}}));
    }

    #[test]
    fn dump_collections_default_and_canonical_order() {
        let all = resolve_dump_collections(None).unwrap();
        assert_eq!(
            all,
            vec![
                "memory.entries",
                "memory.semantic",
                "memory.procedural",
                "identity.graph",
                "memory.edges"
            ]
        );
        // Selections are re-ordered canonically (nodes before edges), not
        // echoed in input order.
        let sel = vec!["edges".to_string(), "identity".to_string(), "semantic".to_string()];
        let some = resolve_dump_collections(Some(&sel)).unwrap();
        assert_eq!(some, vec!["memory.semantic", "identity.graph", "memory.edges"]);
    }

    #[test]
    fn dump_collections_reject_unknown_and_empty() {
        let bogus = vec!["bogus".to_string()];
        let err = resolve_dump_collections(Some(&bogus)).unwrap_err();
        assert!(err.contains("entries"), "should list valid names: {err}");
        let empty: Vec<String> = vec![];
        assert!(resolve_dump_collections(Some(&empty)).is_err());
    }

    #[test]
    fn dump_edge_types_accept_free_form_reject_empty() {
        // Free-form identity relations are dumpable (the $in filter
        // matches stored strings verbatim) alongside the built-in nine.
        let mixed = vec!["enables".to_string(), "has_trait".to_string()];
        assert!(validate_dump_edge_types(&mixed).is_ok());
        let good = vec!["same_session".to_string(), "depends_on".to_string()];
        assert!(validate_dump_edge_types(&good).is_ok());
        // Empty list and empty/whitespace tokens are still errors.
        assert!(validate_dump_edge_types(&[]).is_err());
        let blank = vec!["enables".to_string(), "  ".to_string()];
        assert!(validate_dump_edge_types(&blank).is_err());
    }

    #[test]
    fn dump_node_record_full_includes_data_slim_omits() {
        let doc = json!({"_id": "abc", "content": "x", "tags": ["t"]});
        let full = dump_node_record(&doc, "memory.semantic", true);
        assert_eq!(full["type"], "node");
        assert_eq!(full["_id"], "abc");
        assert_eq!(full["collection"], "memory.semantic");
        assert_eq!(full["data"], doc);
        let slim = dump_node_record(&doc, "memory.semantic", false);
        assert_eq!(slim["_id"], "abc");
        assert!(slim.get("data").is_none());
    }

    #[test]
    fn dump_edge_record_spreads_fields_and_sets_type() {
        let doc = json!({
            "_id": "e1",
            "source_id": "a",
            "source_collection": "memory.semantic",
            "target_id": "b",
            "target_collection": "memory.procedural",
            "edge_type": "enables",
            "weight": 0.9,
            "metadata": {},
            "created_at": "t"
        });
        let rec = dump_edge_record(&doc);
        assert_eq!(rec["type"], "edge");
        for key in [
            "_id",
            "source_id",
            "source_collection",
            "target_id",
            "target_collection",
            "edge_type",
            "weight",
            "metadata",
            "created_at",
        ] {
            assert_eq!(rec[key], doc[key], "{key} must spread top-level");
        }
    }

    #[test]
    fn dump_meta_record_shape() {
        let m = dump_meta_record(&["memory.edges"], None, true, None);
        assert_eq!(m["type"], "meta");
        assert!(m.get("source").is_none(), "the live dump's meta line is what it always was");
        assert!(m["generated_at"].as_str().is_some());
        assert_eq!(m["collections"], json!(["memory.edges"]));
        assert!(m["edge_types"].is_null());
        assert_eq!(m["include_payload"], json!(true));

        let types = vec!["enables".to_string()];
        let m2 = dump_meta_record(&["memory.edges"], Some(&types), false, None);
        assert_eq!(m2["edge_types"], json!(["enables"]));
        assert_eq!(m2["include_payload"], json!(false));
        let source = json!({"tool": "forget", "target": "memory.entries:e1"});
        let m3 = dump_meta_record(&["memory.entries", "memory.edges"], None, true, Some(&source));
        assert_eq!(m3["source"], source);
        assert_eq!(m3["collections"], json!(["memory.entries", "memory.edges"]));
    }
}

// ── Native tool-use registrations (NATIVE-TOOLS-01) ──

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tools::registry::DispatchContext;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum KnowledgePromoteKind {
    Semantic,
    Procedural,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_promote",
    is_side_effectful = true,
    description = "Give a node to a memory entry that has none (recall with unpromoted_only=true lists them): one saved before remember wrote nodes, one whose promotion failed, or one whose node was removed. Never needed after remember, which writes the node itself. For kind=semantic, data is one of: fact, preference, decision, observation, pattern; when the entry already has a semantic node, its category is set to data. For kind=procedural, data is a JSON object describing the procedure (title, description, preconditions, steps, outcomes); an entry that already has a node is refused."
)]
pub struct KnowledgePromoteArgs {
    pub entry_id: String,
    pub kind: KnowledgePromoteKind,
    /// For semantic: a category string. For procedural: a JSON procedure object.
    pub data: String,
}

impl KnowledgePromoteArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let kind_str = match self.kind {
            KnowledgePromoteKind::Semantic => "semantic",
            KnowledgePromoteKind::Procedural => "procedural",
        };
        let param = format!("{} | {} | {}", self.entry_id, kind_str, self.data);
        Ok(knowledge_promote(&param, ctx.db, ctx.config).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_link",
    is_side_effectful = true,
    description = "Create a directed, weighted, typed edge between two knowledge nodes. edge_type: enables | contradicts | refines | depends_on | related_to | supersedes. weight is 0.0-1.0 indicating confidence. Use enables when the source is a prerequisite for the target; contradicts when they conflict; refines when the source is more specific than the target; depends_on when the source requires the target to hold; related_to for same-scope, non-hierarchical association; supersedes when the source replaces the target, which stays as history (both must be semantic or procedural nodes; every read then marks the target superseded and knowledge_audit leaves it out)."
)]
pub struct KnowledgeLinkArgs {
    pub source_collection: String,
    pub source_id: String,
    pub edge_type: String,
    pub target_collection: String,
    pub target_id: String,
    pub weight: f64,
}

impl KnowledgeLinkArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!(
            "{}:{} | {} | {}:{} | {}",
            self.source_collection,
            self.source_id,
            self.edge_type,
            self.target_collection,
            self.target_id,
            self.weight
        );
        Ok(knowledge_link(&param, ctx.db).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_unlink_edge",
    is_side_effectful = true,
    description = "Delete edges. Provide either edge_id (removes one edge by its document id) OR the triple — source_id + edge_type + target_id; source_collection/target_collection are optional (matching is by id and type). Symmetric edge types (same_session, temporal, tag_overlap, related_to) are removed bidirectionally; directional types (enables, contradicts, refines, depends_on, supersedes, derived_from — and free-form identity relations) remove only the forward direction. edge_id takes precedence when both are provided."
)]
pub struct KnowledgeUnlinkEdgeArgs {
    /// Specific edge document id. When set, the triple fields are ignored.
    #[serde(default)]
    pub edge_id: Option<String>,
    /// Triple form: source collection. Optional — when omitted, the edge
    /// matches by source_id + edge_type + target_id alone.
    #[serde(default)]
    pub source_collection: Option<String>,
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub edge_type: Option<String>,
    #[serde(default)]
    pub target_collection: Option<String>,
    #[serde(default)]
    pub target_id: Option<String>,
}

impl KnowledgeUnlinkEdgeArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        // Legacy impl takes either "<edge_id>" OR
        // "<src>:<id> | <edge_type> | <tgt>:<id>" (bare ids allowed —
        // collections are optional, the filter keys on id + type).
        // Reconstruct whichever form the caller provided.
        fn endpoint(coll: Option<&str>, id: &str) -> String {
            match coll.map(str::trim).filter(|c| !c.is_empty()) {
                Some(c) => format!("{}:{}", c, id),
                None => id.to_string(),
            }
        }
        let param = if let Some(eid) = self.edge_id.filter(|s| !s.is_empty()) {
            eid
        } else {
            match (
                self.source_id.as_deref(),
                self.edge_type.as_deref(),
                self.target_id.as_deref(),
            ) {
                (Some(si), Some(et), Some(ti))
                    if !si.is_empty() && !et.is_empty() && !ti.is_empty() =>
                {
                    format!(
                        "{} | {} | {}",
                        endpoint(self.source_collection.as_deref(), si),
                        et,
                        endpoint(self.target_collection.as_deref(), ti)
                    )
                }
                _ => {
                    return Ok(
                        "knowledge_unlink_edge rejected (missing arguments). Provide edge_id OR the triple: source_id + edge_type + target_id (collections optional — matching is by id and type)."
                            .into(),
                    );
                }
            }
        };
        Ok(knowledge_unlink_edge(&param, ctx.db).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_unlink_node",
    is_side_effectful = true,
    description = "Delete a semantic or procedural node and every edge referencing it. The entry it was promoted from stays, without a node, and the result names it. Before anything is deleted the node and its edges are written to a record under /embra/workspace/KG_DUMPS (a dump file), and the reply lists the removed edges by type with every brain-authored link named. dry_run=true shows the same without writing. To remove the whole memory, entry included, use forget."
)]
pub struct KnowledgeUnlinkNodeArgs {
    pub collection: String,
    pub id: String,
    /// Show what the call would remove, the brain-authored links named;
    /// nothing is written.
    #[serde(default)]
    pub dry_run: bool,
}

impl KnowledgeUnlinkNodeArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{}:{}", self.collection, self.id);
        Ok(knowledge_unlink_node_at(&param, ctx.db, self.dry_run, KG_DUMPS_DIR).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_update",
    is_side_effectful = true,
    description = "Update fields on a semantic or procedural node in place while preserving all referencing edges. Immutable fields (provenance, timestamps, access counters) are rejected. patch_json is a JSON object of the fields to patch. Prefer this over unlink + re-promote when node identity and provenance should stay intact. If you substantially change tags, auto-derived tag_overlap edges may be stale — clean specific ones with knowledge_unlink_edge."
)]
pub struct KnowledgeUpdateArgs {
    pub collection: String,
    pub id: String,
    /// JSON object describing the partial patch, serialized as a string.
    pub patch_json: String,
}

impl KnowledgeUpdateArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{}:{} | {}", self.collection, self.id, self.patch_json);
        Ok(knowledge_update(&param, ctx.db, ctx.config).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_traverse",
    description = "BFS-explore the knowledge graph starting from <collection>:<id>. depth bounds expansion (ceiling 5). edge_types optionally restricts by type (CSV). min_weight optionally filters edges below the threshold (0.0-1.0)."
)]
pub struct KnowledgeTraverseArgs {
    pub start_collection: String,
    pub start_id: String,
    #[serde(default)]
    pub depth: Option<u32>,
    /// CSV of edge types to include (same_session, temporal, tag_overlap,
    /// derived_from, enables, contradicts, refines, depends_on, related_to,
    /// supersedes).
    #[serde(default)]
    pub edge_types: Option<String>,
    #[serde(default)]
    pub min_weight: Option<f64>,
}

impl KnowledgeTraverseArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let mut parts = vec![format!("{}:{}", self.start_collection, self.start_id)];
        if let Some(d) = self.depth {
            parts.push(d.to_string());
            if let Some(t) = self.edge_types {
                parts.push(t);
                if let Some(w) = self.min_weight {
                    parts.push(w.to_string());
                }
            }
        }
        let param = parts.join(" ");
        Ok(knowledge_traverse(&param, ctx.db, ctx.config).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_query",
    description = "Find relevant knowledge by multi-signal retrieval: tag, content and similarity matches plus the current session's context, ranked by relevance, recency and access frequency. max_results defaults to 20, capped at 100. categories is a CSV of semantic categories to filter by (fact, preference, decision, observation, pattern). Call this before answering questions where prior context would help."
)]
pub struct KnowledgeQueryArgs {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<u32>,
    #[serde(default)]
    pub categories: Option<String>,
}

impl KnowledgeQueryArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let mut parts = vec![self.query];
        match (self.max_results, self.categories) {
            (Some(m), Some(c)) => {
                parts.push(m.to_string());
                parts.push(c);
            }
            (Some(m), None) => parts.push(m.to_string()),
            (None, Some(c)) => {
                parts.push(String::new()); // empty max_results slot
                parts.push(c);
            }
            (None, None) => {}
        }
        let param = parts.join(" | ");
        Ok(knowledge_query(&param, ctx.db, ctx.session_name, ctx.config).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_graph_stats",
    description = "Return summary statistics of the knowledge graph: node counts by collection, edge counts by type, total density, the orphan-edge count and the promotion nodes no entry's pointer names, listed."
)]
pub struct KnowledgeGraphStatsArgs {}

impl KnowledgeGraphStatsArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(knowledge_graph_stats(ctx.db).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_sweep_orphans",
    is_side_effectful = true,
    description = "Scan memory.edges and remove edges whose source or target doc no longer resolves. The promotion nodes no entry's pointer names are healed (the entry re-pointed at the node) or adopted (an entry written back for the node), never deleted; a node whose entry already has another node is reported as a pair for knowledge_merge. Orphans accumulate from pre-cascade forget calls and direct deletes that bypassed knowledge_unlink_node. Before any edge is deleted the edges are written to a record under /embra/workspace/KG_DUMPS (a dump file), and the reply lists them by type with every brain-authored link named. dry_run=true prints the same lines without writing; it describes the graph at call time, so take it right before the real run."
)]
pub struct KnowledgeSweepOrphansArgs {
    /// Preview: the orphan edges, and each pointerless node with what the
    /// real run would do to it; nothing is written.
    #[serde(default)]
    pub dry_run: bool,
    /// Most orphan edges removed in one call; clamped to [1, 100000].
    /// Every edge is checked on every call, whatever the limit. When more
    /// orphan edges exist than the limit, the result says so.
    #[serde(default = "default_sweep_limit")]
    pub limit: usize,
}

fn default_sweep_limit() -> usize {
    10_000
}

impl KnowledgeSweepOrphansArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(knowledge_sweep_orphans(ctx.db, ctx.config, self.dry_run, self.limit).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_dump",
    is_side_effectful = true,
    description = "Export the knowledge graph as a JSONL dump file under /embra/workspace/KG_DUMPS (first line: meta header; then node lines from memory.entries/semantic/procedural/identity.graph and edge lines from memory.edges). collections optionally restricts to: entries, semantic, procedural, identity, edges (default all five). edge_types optionally filters the edge pass by type. include_payload=false omits node payloads — slim dumps for structural scanning, sized for guardian_call's 2 MiB data_file bridge. Returns the file path plus per-collection written-vs-counted parity, byte total, and elapsed time."
)]
pub struct KnowledgeDumpArgs {
    /// Short collection names: entries | semantic | procedural | identity |
    /// edges. Omit for all five.
    #[serde(default)]
    pub collections: Option<Vec<String>>,
    /// Edge types to include (same_session, temporal, tag_overlap,
    /// derived_from, enables, contradicts, refines, depends_on, related_to,
    /// supersedes — or any stored free-form identity relation). Omit for all. Requires
    /// 'edges' among the dumped collections.
    #[serde(default)]
    pub edge_types: Option<Vec<String>>,
    /// When false, node lines omit the `data` payload (slim structural dump).
    #[serde(default = "default_include_payload")]
    pub include_payload: bool,
}

fn default_include_payload() -> bool {
    true
}

impl KnowledgeDumpArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        run_knowledge_dump(ctx.db, self.collections, self.edge_types, self.include_payload)
            .await
            .map_err(DispatchError::Handler)
    }
}

#[cfg(test)]
mod native_args_tests {
    use super::*;

    #[test]
    fn unlink_node_dry_run_defaults_false() {
        let a: KnowledgeUnlinkNodeArgs = serde_json::from_str(r#"{"collection":"memory.semantic","id":"n1"}"#).unwrap();
        assert!(!a.dry_run);
        let b: KnowledgeUnlinkNodeArgs =
            serde_json::from_str(r#"{"collection":"memory.semantic","id":"n1","dry_run":true}"#).unwrap();
        assert!(b.dry_run);
    }

    #[test]
    fn knowledge_promote_kind_deserializes() {
        let a: KnowledgePromoteArgs = serde_json::from_value(serde_json::json!({
            "entry_id": "abc", "kind": "semantic", "data": "fact"
        }))
        .unwrap();
        assert!(matches!(a.kind, KnowledgePromoteKind::Semantic));

        let b: KnowledgePromoteArgs = serde_json::from_value(serde_json::json!({
            "entry_id": "abc", "kind": "procedural", "data": "{\"steps\": []}"
        }))
        .unwrap();
        assert!(matches!(b.kind, KnowledgePromoteKind::Procedural));
    }

    #[test]
    fn knowledge_unlink_edge_accepts_edge_id_only() {
        let a: KnowledgeUnlinkEdgeArgs =
            serde_json::from_value(serde_json::json!({"edge_id": "edge123"})).unwrap();
        assert_eq!(a.edge_id.as_deref(), Some("edge123"));
        assert!(a.source_collection.is_none());
    }

    #[test]
    fn knowledge_unlink_edge_accepts_triple_only() {
        let a: KnowledgeUnlinkEdgeArgs = serde_json::from_value(serde_json::json!({
            "source_collection": "memory.semantic",
            "source_id": "a",
            "edge_type": "refines",
            "target_collection": "memory.semantic",
            "target_id": "b"
        }))
        .unwrap();
        assert!(a.edge_id.is_none());
        assert_eq!(a.source_id.as_deref(), Some("a"));
        assert_eq!(a.target_id.as_deref(), Some("b"));
    }

    #[test]
    fn knowledge_unlink_edge_accepts_triple_without_collections() {
        // The 3-field triple: collections are optional and display-only —
        // the delete filter has always matched by id + type. Requiring them
        // produced spurious "missing arguments" rejections.
        let a: KnowledgeUnlinkEdgeArgs = serde_json::from_value(serde_json::json!({
            "source_id": "a",
            "edge_type": "refines",
            "target_id": "b"
        }))
        .unwrap();
        assert!(a.edge_id.is_none());
        assert!(a.source_collection.is_none());
        assert!(a.target_collection.is_none());
        assert_eq!(a.source_id.as_deref(), Some("a"));
        assert_eq!(a.edge_type.as_deref(), Some("refines"));
        assert_eq!(a.target_id.as_deref(), Some("b"));
    }

    #[test]
    fn unlink_endpoint_split_and_display() {
        use super::{display_coll, split_endpoint};
        assert_eq!(split_endpoint("memory.semantic:abc"), ("memory.semantic", "abc"));
        assert_eq!(split_endpoint("abc"), ("", "abc"));
        assert_eq!(split_endpoint(":abc"), ("", "abc"));
        assert_eq!(split_endpoint(""), ("", ""));
        assert_eq!(display_coll(""), "any");
        assert_eq!(display_coll("memory.semantic"), "memory.semantic");
    }

    #[test]
    fn knowledge_unlink_edge_schema_has_no_top_level_oneof() {
        // Regression guard for the Anthropic "input_schema does not support
        // oneOf, allOf, or anyOf at the top level" rejection. schemars must
        // render KnowledgeUnlinkEdgeArgs as a plain object schema.
        let schema = schemars::schema_for!(KnowledgeUnlinkEdgeArgs);
        let v = serde_json::to_value(&schema).unwrap();
        assert!(
            v.get("oneOf").is_none(),
            "top-level oneOf present: {}",
            v
        );
        assert!(
            v.get("allOf").is_none(),
            "top-level allOf present: {}",
            v
        );
        assert!(
            v.get("anyOf").is_none(),
            "top-level anyOf present: {}",
            v
        );
        assert_eq!(v["type"], "object", "schema should be a plain object");
    }

    #[test]
    fn knowledge_traverse_optional_fields() {
        let a: KnowledgeTraverseArgs = serde_json::from_value(serde_json::json!({
            "start_collection": "memory.semantic", "start_id": "x"
        }))
        .unwrap();
        assert!(a.depth.is_none());
        assert!(a.edge_types.is_none());
        assert!(a.min_weight.is_none());
    }

    #[test]
    fn knowledge_tools_register() {
        let names: Vec<&'static str> = inventory::iter::<crate::tools::registry::ToolDescriptor>()
            .map(|d| d.name)
            .collect();
        for expected in [
            "knowledge_promote",
            "knowledge_link",
            "knowledge_unlink_edge",
            "knowledge_unlink_node",
            "knowledge_update",
            "knowledge_traverse",
            "knowledge_query",
            "knowledge_graph_stats",
            "knowledge_sweep_orphans",
            "knowledge_dump",
            "knowledge_audit",
            "knowledge_merge",
        ] {
            assert!(names.contains(&expected), "{} not registered", expected);
        }
    }

    #[test]
    fn sweep_orphans_defaults() {
        let a: KnowledgeSweepOrphansArgs =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!a.dry_run);
        assert_eq!(a.limit, 10_000);
    }

    #[test]
    fn sweep_orphans_explicit_dry_run_and_limit() {
        let a: KnowledgeSweepOrphansArgs = serde_json::from_value(serde_json::json!({
            "dry_run": true,
            "limit": 500,
        }))
        .unwrap();
        assert!(a.dry_run);
        assert_eq!(a.limit, 500);
    }

    #[test]
    fn knowledge_dump_args_defaults() {
        // A job scheduled without arguments dispatches json!({}), so the
        // defaults are what such a cron job does: full dump, all
        // collections, all edge types.
        let a: KnowledgeDumpArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(a.collections.is_none());
        assert!(a.edge_types.is_none());
        assert!(a.include_payload);
    }

    #[test]
    fn knowledge_dump_schema_is_plain_object() {
        let schema = schemars::schema_for!(KnowledgeDumpArgs);
        let v = serde_json::to_value(&schema).unwrap();
        assert!(v.get("oneOf").is_none());
        assert!(v.get("allOf").is_none());
        assert!(v.get("anyOf").is_none());
        assert_eq!(v["type"], "object");
    }

    #[test]
    fn sweep_orphans_schema_is_plain_object() {
        // Regression guard (same shape as the knowledge_unlink_edge guard) —
        // the universal brain::tests variant also covers this, but keeping an
        // inline copy lets this module stay self-testing.
        let schema = schemars::schema_for!(KnowledgeSweepOrphansArgs);
        let v = serde_json::to_value(&schema).unwrap();
        assert!(v.get("oneOf").is_none());
        assert!(v.get("allOf").is_none());
        assert!(v.get("anyOf").is_none());
        assert_eq!(v["type"], "object");
    }
}

/// Embra#10: the orphan check covers every edge, and it deletes nothing it
/// could not look up.
#[cfg(test)]
mod orphan_tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use crate::knowledge::removal::tests::TempDir;

    fn scan(orphans: &[&str], dangling: &[&str], more: bool) -> OrphanScan {
        OrphanScan {
            scanned: 433_546,
            dangling: dangling.iter().map(|s| s.to_string()).collect(),
            orphans: orphans.iter().map(|s| orphan_edge(s, "same_session", "n1", "gone")).collect(),
            more,
        }
    }

    fn orphan_edge(id: &str, kind: &str, src: &str, tgt: &str) -> serde_json::Value {
        json!({
            "_id": id, "edge_type": kind, "weight": 0.5,
            "source_collection": "memory.semantic", "source_id": src,
            "target_collection": "memory.semantic", "target_id": tgt,
        })
    }

    fn no_removal() -> RemovalSummary {
        RemovalSummary::default()
    }

    #[test]
    fn the_stats_line_counts_against_every_edge() {
        assert_eq!(orphan_line(&Ok(scan(&[], &[], false))), "Orphan edges: 0 of 433546 scanned");
        assert_eq!(
            orphan_line(&Ok(scan(&["e1", "e2"], &["gone"], false))),
            "Orphan edges: 2 of 433546 scanned (run knowledge_sweep_orphans to clean up)"
        );
        assert_eq!(
            orphan_line(&Ok(scan(&["e1", "e2"], &["gone"], true))),
            "Orphan edges: at least 2 of 433546 scanned (run knowledge_sweep_orphans to clean up)"
        );
    }

    #[test]
    fn a_check_that_could_not_be_read_gives_no_number() {
        assert_eq!(
            orphan_line(&Err("looking up edge endpoints in memory.entries failed: timeout".into())),
            "Orphan edges: not checked (looking up edge endpoints in memory.entries failed: timeout)"
        );
        // A graph without edges has no orphan line, as before.
        assert_eq!(orphan_line(&Ok(OrphanScan::default())), "");
    }

    #[test]
    fn the_sweep_reply_keeps_its_lines_and_adds_what_is_missing() {
        assert_eq!(
            sweep_report(&scan(&[], &[], false), true, 0, 10_000, &Ok(PointerlessScan::default()), &[], (&no_removal(), None)),
            "knowledge_sweep_orphans (dry_run):\n  scanned: 433546\n  orphan_count: 0\n  deleted: 0\n  pointerless_nodes: 0\n  to_heal: 0\n  to_adopt: 0\n  duplicates: 0\n  healed: 0\n  adopted: 0"
        );
        assert_eq!(
            sweep_report(&scan(&["e1", "e2"], &["gone"], false), false, 2, 10_000, &Ok(PointerlessScan::default()), &[], (&no_removal(), None)),
            "knowledge_sweep_orphans:\n  scanned: 433546\n  orphan_count: 2\n  deleted: 2\n  missing_nodes: 1\n  missing_node_ids: gone\n  pointerless_nodes: 0\n  to_heal: 0\n  to_adopt: 0\n  duplicates: 0\n  healed: 0\n  adopted: 0\n  edges removed: 0 (auto 0, curated 0, other 0)\n  record: not written (nothing to remove)"
        );
        // With orphan edges the removal block follows the counts: the split,
        // the curated edges named, the record.
        let two = scan(&["e1", "e2"], &["gone"], false);
        let summary = RemovalSummary::of(&two.orphans);
        let with_record = sweep_report(&two, false, 2, 10_000, &Ok(PointerlessScan::default()), &[], (&summary, Some(std::path::Path::new("/embra/workspace/KG_DUMPS/removed-knowledge_sweep_orphans-x.jsonl"))));
        assert!(with_record.ends_with("\n  adopted: 0\n  edges removed: 2 (auto 2, curated 0, other 0)\n  auto: same_session=2\n  record: /embra/workspace/KG_DUMPS/removed-knowledge_sweep_orphans-x.jsonl"), "{with_record}");
        let capped = sweep_report(&scan(&["e1", "e2"], &["gone"], true), false, 2, 2, &Ok(PointerlessScan::default()), &[], (&no_removal(), None));
        assert!(capped.ends_with("\n  more orphan edges exist than the limit of 2: run it again"), "{capped}");
        // Long lists are cut, and say by how much.
        let many: Vec<String> = (0..103).map(|i| format!("gone{i}")).collect();
        let dangling: Vec<&str> = many.iter().map(String::as_str).collect();
        let cut = sweep_report(&scan(&[], &dangling, false), true, 0, 10_000, &Ok(PointerlessScan::default()), &[], (&no_removal(), None));
        assert!(cut.contains("\n  missing_nodes: 103\n  missing_node_ids: gone0, gone1, "), "{cut}");
        assert!(cut.contains(", gone99 and 3 more\n  pointerless_nodes: 0"), "{cut}");
        assert_eq!(listed_ids(["a", "b"], 2, 10), "a, b");
        assert_eq!(listed_ids(["a", "b", "c"], 3, 2), "a, b and 1 more");
    }

    /// A node collection added to the dump is one an edge can point into.
    #[test]
    fn an_endpoint_can_live_in_every_node_collection() {
        let nodes: Vec<&str> = DUMP_COLLECTIONS
            .iter()
            .map(|(_, collection)| *collection)
            .filter(|collection| *collection != "memory.edges")
            .collect();
        assert_eq!(nodes, ENDPOINT_COLLECTIONS);
    }

    // ── against a stub server ────────────────────────────────────────────

    fn data(v: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": v, "meta": {}}))
    }

    fn body(req: &Request) -> serde_json::Value {
        serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null)
    }

    /// The edge collection as the two groups see it: `(id, edges as source,
    /// edges as target)`.
    async fn edges_grouped(server: &MockServer, rows: &[(&str, u64, u64)]) {
        for (field, pick) in [("source_id", 0usize), ("target_id", 1usize)] {
            let groups: Vec<serde_json::Value> = rows
                .iter()
                .map(|(id, s, t)| json!({"_id": id, "count": if pick == 0 { *s } else { *t }}))
                .filter(|row| row["count"] != 0)
                .collect();
            Mock::given(method("POST"))
                .and(path("/memory.edges/aggregate"))
                .and(move |req: &Request| body(req)["pipeline"][0]["$group"]["_id"] == field)
                .respond_with(data(json!(groups)))
                .mount(server)
                .await;
        }
    }

    /// A node collection that holds `ids`: its count, and its ids grouped.
    async fn collection_holds(server: &MockServer, collection: &str, ids: &[&str]) {
        Mock::given(method("POST"))
            .and(path(format!("/{collection}/query")))
            .respond_with(data(json!({"count": ids.len()})))
            .mount(server)
            .await;
        let rows: Vec<serde_json::Value> = ids.iter().map(|id| json!({"_id": id, "count": 1})).collect();
        Mock::given(method("POST"))
            .and(path(format!("/{collection}/aggregate")))
            .respond_with(data(json!(rows)))
            .mount(server)
            .await;
    }

    /// The edges that name `id` on one side, as whole documents of the
    /// given types; the other end is `n1`.
    async fn arm_typed(server: &MockServer, field: &'static str, id: &'static str, edges: &[(&str, &str)]) {
        let docs: Vec<serde_json::Value> = edges
            .iter()
            .map(|(e, kind)| {
                let (src, tgt) = if field == "source_id" { (id, "n1") } else { ("n1", id) };
                orphan_edge(e, kind, src, tgt)
            })
            .collect();
        Mock::given(method("POST"))
            .and(path("/memory.edges/query"))
            .and(move |req: &Request| body(req)["filter"][field] == id)
            .respond_with(data(json!(docs)))
            .mount(server)
            .await;
    }

    /// The edges that name `id` on one side, all automatic.
    async fn arm(server: &MockServer, field: &'static str, id: &'static str, edge_ids: &[&str]) {
        let typed: Vec<(&str, &str)> = edge_ids.iter().map(|e| (*e, "same_session")).collect();
        arm_typed(server, field, id, &typed).await;
    }

    /// The paths the server was asked to delete, in order.
    async fn deletes_seen(server: &MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "DELETE" || r.url.path().ends_with("_delete_by_query"))
            .map(|r| r.url.path().to_string())
            .collect()
    }

    fn test_config() -> SystemConfig {
        serde_json::from_value(json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
            "created_at": "", "version": "test", "embedding_enabled": false
        }))
        .expect("minimal config deserializes")
    }

    /// Every request that writes, in order: "METHOD /path" with its body.
    /// Queries and aggregates are POSTs too; they end in `/query` or
    /// `/aggregate` and are left out.
    async fn writes_seen(server: &MockServer) -> Vec<(String, serde_json::Value)> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| {
                r.method.as_str() != "GET" && !r.url.path().ends_with("/query") && !r.url.path().ends_with("/aggregate")
            })
            .map(|r| (format!("{} {}", r.method, r.url.path()), body(r)))
            .collect()
    }

    /// A graph without edges, where the promotion node `n1` is pointed at,
    /// the semantic nodes of `pointerless` (`(id, entry id)`) are not, and
    /// `s1` is no promotion node. No `derived_from` edge joins any node to
    /// its entry; a node, an entry, a pointer write and an entry write are
    /// mounted by each test.
    async fn graph_with_pointerless_nodes(pointerless: &[(&str, &str)]) -> MockServer {
        let server = MockServer::start().await;
        edges_grouped(&server, &[]).await;
        for collection in ENDPOINT_COLLECTIONS {
            collection_holds(&server, collection, &[]).await;
        }
        Mock::given(method("POST"))
            .and(path("/memory.entries/aggregate"))
            .and(|req: &Request| body(req)["pipeline"][0]["$group"]["_id"] == "promoted_to.id")
            .respond_with(data(json!([{"_id": "n1", "count": 1}, {"_id": null, "count": 1}])))
            .with_priority(1)
            .mount(&server)
            .await;
        let mut window = vec![json!({"_id": "n1", "source_entry_id": "e1"}), json!({"_id": "s1"})];
        for (id, entry) in pointerless {
            window.push(json!({"_id": id, "source_entry_id": entry}));
        }
        Mock::given(method("POST"))
            .and(path("/memory.semantic/query"))
            .and(|req: &Request| body(req)["fields"].is_array())
            .respond_with(data(json!(window)))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.procedural/query"))
            .and(|req: &Request| body(req)["fields"].is_array())
            .respond_with(data(json!([])))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.edges/query"))
            .and(|req: &Request| body(req)["filter"]["edge_type"] == "derived_from")
            .respond_with(data(json!([])))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.edges/docs"))
            .respond_with(data(json!({"_id": "edge1"})))
            .mount(&server)
            .await;
        Mock::given(method("DELETE")).respond_with(data(json!({}))).mount(&server).await;
        server
    }

    fn semantic_node(id: &str, entry: &str, content: &str, embedded: bool) -> serde_json::Value {
        let mut doc = json!({
            "_id": id, "content": content, "tags": ["t"], "category": "decision",
            "source_entry_id": entry, "source_session": "old", "created_at": "2026-06-01T00:00:00Z",
        });
        if embedded {
            doc["embedding"] = json!("AAAA");
            doc["embedding_model"] = json!("m");
        }
        doc
    }

    async fn graph_with_one_missing_node() -> MockServer {
        let server = MockServer::start().await;
        edges_grouped(&server, &[("e1", 2, 2), ("n1", 2, 1), ("gone", 1, 2)]).await;
        collection_holds(&server, "memory.entries", &["e1"]).await;
        collection_holds(&server, "memory.semantic", &["n1"]).await;
        collection_holds(&server, "memory.procedural", &[]).await;
        collection_holds(&server, "identity.graph", &[]).await;
        arm_typed(&server, "source_id", "gone", &[("x1", "same_session")]).await;
        arm_typed(&server, "target_id", "gone", &[("x2", "related_to"), ("x3", "tag_overlap")]).await;
        Mock::given(method("DELETE")).respond_with(data(json!({}))).mount(&server).await;
        server
    }

    #[tokio::test]
    async fn the_check_reads_the_endpoints_of_every_edge_and_no_window_of_them() {
        let server = graph_with_one_missing_node().await;
        let db = WardsonDbClient::from_url(&server.uri());
        let found = find_orphan_edges(&db, 10_000).await.expect("the check completes");
        assert_eq!((found.scanned, found.dangling.as_slice(), found.orphan_ids(), found.more), (5, ["gone".to_string()].as_slice(), vec!["x1", "x2", "x3"], false));
        assert_eq!(found.orphans[1]["edge_type"], "related_to", "the documents are kept whole for the record");
        // No page of edge documents is read: every edge query names the one
        // missing id, on one side.
        let requests = server.received_requests().await.unwrap_or_default();
        let edge_queries: Vec<serde_json::Value> = requests
            .iter()
            .filter(|r| r.url.path() == "/memory.edges/query")
            .map(|r| body(r)["filter"].clone())
            .collect();
        assert_eq!(edge_queries, [json!({"source_id": "gone"}), json!({"target_id": "gone"})]);
        // The edges are read before the nodes.
        let order: Vec<&str> = requests
            .iter()
            .filter(|r| r.url.path().ends_with("/aggregate"))
            .map(|r| r.url.path())
            .collect();
        assert_eq!(
            order,
            [
                "/memory.edges/aggregate",
                "/memory.edges/aggregate",
                "/memory.entries/aggregate",
                "/memory.semantic/aggregate",
                "/memory.procedural/aggregate",
                "/identity.graph/aggregate",
            ]
        );
    }

    /// The record is written before the first delete, in the dump format,
    /// and the reply splits what went: the auto edges counted, the curated
    /// one named with its ends.
    #[tokio::test]
    async fn the_sweep_removes_what_the_check_lists() {
        let server = graph_with_one_missing_node().await;
        let db = WardsonDbClient::from_url(&server.uri());
        let dir = TempDir::new("sweep");
        let out = knowledge_sweep_orphans_at(&db, &test_config(), false, 10_000, dir.path()).await;
        // The node check reads the count mock as its window and says so.
        assert!(
            out.starts_with("knowledge_sweep_orphans:\n  scanned: 5\n  orphan_count: 3\n  deleted: 3\n  missing_nodes: 1\n  missing_node_ids: gone\n  pointerless_nodes: not checked ("),
            "{out}"
        );
        assert!(
            out.contains("\n  edges removed: 3 (auto 2, curated 1, other 0)\n  auto: same_session=1, tag_overlap=1\n  curated: related_to=1\n    x2 memory.semantic:n1 → memory.semantic:gone related_to 0.50\n  record: "),
            "{out}"
        );
        assert_eq!(
            deletes_seen(&server).await,
            ["/memory.edges/docs/x1", "/memory.edges/docs/x2", "/memory.edges/docs/x3"]
        );
        let record = std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().path()).next().expect("one record");
        assert!(out.ends_with(&format!("\n  record: {}", record.display())), "{out}");
        let lines = crate::knowledge::removal::tests::record_lines(&record);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["source"], json!({"tool": "knowledge_sweep_orphans", "target": null, "dangling": ["gone"]}));
        assert_eq!(lines[0]["collections"], json!(["memory.edges"]));
        let ids: Vec<&str> = lines[1..].iter().map(|l| l["_id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["x1", "x2", "x3"]);
        assert!(lines[1..].iter().all(|l| l["type"] == "edge"));
    }

    /// The record gates the deletes: when it cannot be written, no edge goes.
    #[tokio::test]
    async fn a_record_that_cannot_be_written_removes_nothing() {
        let server = graph_with_one_missing_node().await;
        let db = WardsonDbClient::from_url(&server.uri());
        let dir = TempDir::new("sweep-blocked");
        let blocked = dir.0.join("not-a-directory");
        std::fs::write(&blocked, b"x").unwrap();
        let out = knowledge_sweep_orphans_at(&db, &test_config(), false, 10_000, blocked.to_str().unwrap()).await;
        assert!(out.starts_with("knowledge_sweep_orphans: the removal record could not be written ("), "{out}");
        assert!(out.ends_with("no edge was deleted (0 healed, 0 adopted)."), "{out}");
        assert!(deletes_seen(&server).await.is_empty());
    }

    /// A promotion node no entry points at is reported by the stats and
    /// healed or adopted by the sweep, never deleted (2026-10-30); a node
    /// without a source entry (a seed node, a node the model wrote) is not a
    /// promotion node. `n2`'s entry exists and is unpromoted: the pointer
    /// goes to it. `n3`'s entry is gone: one is written back under its id.
    #[tokio::test]
    async fn a_promotion_node_no_entry_points_at_is_healed_or_adopted_and_never_deleted() {
        let server = graph_with_pointerless_nodes(&[("n2", "e2"), ("n3", "e3")]).await;
        Mock::given(method("GET"))
            .and(path("/memory.entries/docs/e2"))
            .respond_with(data(json!({"_id": "e2", "content": "two", "tags": [], "session": "ops", "promoted_to": null})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/memory.semantic/docs/n2"))
            .respond_with(data(semantic_node("n2", "e2", "two", false)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/memory.semantic/docs/n3"))
            .respond_with(data(semantic_node("n3", "e3", "three", true)))
            .mount(&server)
            .await;
        Mock::given(method("PATCH")).and(path("/memory.entries/docs/e2")).respond_with(data(json!({}))).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/memory.entries/docs"))
            .respond_with(data(json!({"_id": "e3"})))
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());
        let config = test_config();

        let found = find_pointerless_nodes(&db).await.expect("the check completes");
        assert_eq!(found.checked, 3);
        assert_eq!(
            found.nodes,
            [
                PointerlessNode { collection: "memory.semantic", id: "n2".into(), entry_id: "e2".into() },
                PointerlessNode { collection: "memory.semantic", id: "n3".into(), entry_id: "e3".into() },
            ]
        );
        assert_eq!(
            pointerless_line(&Ok(found)),
            "Promotion nodes without their entry's pointer: 2 of 3 checked: memory.semantic:n2, memory.semantic:n3 (knowledge_sweep_orphans heals or adopts them; nothing is deleted)"
        );

        let lines = "\n  pointerless_nodes: 2\
            \n    memory.semantic:n2 → heal (entry e2 is unpromoted): pointer, vector, derived_from\
            \n    memory.semantic:n3 → adopt (entry e3 is missing): entry e3, pointer, derived_from\
            \n  to_heal: 1\n  to_adopt: 1\n  duplicates: 0";
        let out = knowledge_sweep_orphans(&db, &config, true, 10_000).await;
        assert!(out.contains(&format!("{lines}\n  healed: 0\n  adopted: 0")), "{out}");
        assert!(writes_seen(&server).await.is_empty(), "a dry run writes nothing");

        let out = knowledge_sweep_orphans(&db, &config, false, 10_000).await;
        assert!(out.contains(&format!("{lines}\n  healed: 1\n  adopted: 1")), "{out}");
        assert!(deletes_seen(&server).await.is_empty(), "nothing is deleted: {out}");
        let writes = writes_seen(&server).await;
        let order: Vec<&str> = writes.iter().map(|w| w.0.as_str()).collect();
        assert_eq!(
            order,
            ["PATCH /memory.entries/docs/e2", "POST /memory.edges/docs", "POST /memory.entries/docs", "POST /memory.edges/docs"]
        );
        assert_eq!(writes[0].1, json!({"promoted_to": {"collection": "memory.semantic", "id": "n2"}}));
        let edge = &writes[1].1;
        assert_eq!(
            (edge["source_id"].as_str(), edge["target_id"].as_str(), edge["edge_type"].as_str()),
            (Some("n2"), Some("e2"), Some("derived_from"))
        );
        assert_eq!(
            writes[2].1,
            json!({
                "_id": "e3", "content": "three", "tags": ["t"], "session": "old",
                "promoted_to": {"collection": "memory.semantic", "id": "n3"}, "created_at": "2026-06-01T00:00:00Z",
            })
        );
        let edge = &writes[3].1;
        assert_eq!((edge["source_id"].as_str(), edge["target_id"].as_str()), (Some("n3"), Some("e3")));
        assert_eq!(edge["metadata"], json!({"promotion_type": "semantic", "category": "decision"}));
    }

    /// An adoption whose id is taken (the entry came back under it between
    /// the scan and the write, or the id is refused) takes a fresh id, and
    /// the node's provenance follows it.
    #[tokio::test]
    async fn an_adoption_whose_id_is_taken_uses_a_fresh_id_and_repoints_the_node() {
        let server = graph_with_pointerless_nodes(&[("n3", "e3")]).await;
        Mock::given(method("GET"))
            .and(path("/memory.semantic/docs/n3"))
            .respond_with(data(semantic_node("n3", "e3", "three", true)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.entries/docs"))
            .and(|req: &Request| body(req)["_id"] == "e3")
            .respond_with(ResponseTemplate::new(409).set_body_string("DOCUMENT_CONFLICT"))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.entries/docs"))
            .respond_with(data(json!({"_id": "fresh"})))
            .mount(&server)
            .await;
        Mock::given(method("PATCH")).and(path("/memory.semantic/docs/n3")).respond_with(data(json!({}))).mount(&server).await;
        let db = WardsonDbClient::from_url(&server.uri());

        let out = knowledge_sweep_orphans(&db, &test_config(), false, 10_000).await;
        assert!(out.contains("\n    memory.semantic:n3 → adopt (entry e3 is missing): entry e3, pointer, derived_from\n"), "{out}");
        assert!(out.contains("\n  adopted: 1"), "{out}");
        let writes = writes_seen(&server).await;
        let order: Vec<&str> = writes.iter().map(|w| w.0.as_str()).collect();
        assert_eq!(
            order,
            ["POST /memory.entries/docs", "POST /memory.entries/docs", "PATCH /memory.semantic/docs/n3", "POST /memory.edges/docs"]
        );
        assert_eq!(writes[0].1["_id"], "e3");
        assert!(writes[1].1.get("_id").is_none(), "the second write lets the server mint the id");
        assert_eq!(writes[2].1, json!({"source_entry_id": "fresh"}));
        assert_eq!(writes[3].1["target_id"], "fresh");
        assert!(deletes_seen(&server).await.is_empty());
    }

    /// A pointerless node whose entry already has another node is the
    /// superseded first attempt of a promotion retried: a pair for the
    /// merge, which carries its links over. The sweep names it and writes
    /// nothing.
    #[tokio::test]
    async fn a_pointerless_node_whose_entry_has_another_node_is_a_duplicate_and_nothing_is_written() {
        let server = graph_with_pointerless_nodes(&[("n2", "e2")]).await;
        Mock::given(method("GET"))
            .and(path("/memory.entries/docs/e2"))
            .respond_with(data(json!({
                "_id": "e2", "content": "two", "tags": [], "session": "ops",
                "promoted_to": {"collection": "memory.semantic", "id": "n9"},
            })))
            .mount(&server)
            .await;
        for id in ["n2", "n9"] {
            Mock::given(method("GET"))
                .and(path(format!("/memory.semantic/docs/{id}")))
                .respond_with(data(semantic_node(id, "e2", "two", true)))
                .mount(&server)
                .await;
        }
        let db = WardsonDbClient::from_url(&server.uri());

        let out = knowledge_sweep_orphans(&db, &test_config(), false, 10_000).await;
        assert!(
            out.contains("\n    memory.semantic:n2 → duplicate of memory.semantic:n9 (entry e2 points at it): knowledge_merge source=memory.semantic:n2 target=memory.semantic:n9\n  to_heal: 0\n  to_adopt: 0\n  duplicates: 1\n"),
            "{out}"
        );
        assert!(writes_seen(&server).await.is_empty(), "nothing is written for a duplicate");
    }

    /// An entry that cannot be read says nothing about its node: the node
    /// is listed as not classified, and nothing is written for it.
    #[tokio::test]
    async fn an_entry_that_cannot_be_read_leaves_its_node_unclassified() {
        let server = graph_with_pointerless_nodes(&[("n2", "e2")]).await;
        Mock::given(method("GET"))
            .and(path("/memory.entries/docs/e2"))
            .respond_with(ResponseTemplate::new(500).set_body_string("timeout"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/memory.semantic/docs/n2"))
            .respond_with(data(semantic_node("n2", "e2", "two", false)))
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());

        let out = knowledge_sweep_orphans(&db, &test_config(), false, 10_000).await;
        assert!(out.contains("\n    memory.semantic:n2 → not classified (Entry e2 could not be read: "), "{out}");
        assert!(out.contains("\n  to_heal: 0\n  to_adopt: 0\n  duplicates: 0\n  healed: 0\n  adopted: 0"), "{out}");
        assert!(writes_seen(&server).await.is_empty());
    }

    /// The stats report names the pointerless nodes, up to a cap, and says
    /// what the sweep does with them.
    #[test]
    fn the_stats_line_lists_the_pointerless_ids_and_promises_no_deletion() {
        let nodes: Vec<PointerlessNode> = (0..12)
            .map(|i| PointerlessNode { collection: "memory.semantic", id: format!("n{i}"), entry_id: format!("e{i}") })
            .collect();
        let line = pointerless_line(&Ok(PointerlessScan { checked: 40, nodes }));
        assert!(line.starts_with("Promotion nodes without their entry's pointer: 12 of 40 checked: memory.semantic:n0, "), "{line}");
        assert!(line.ends_with(", memory.semantic:n9 and 2 more (knowledge_sweep_orphans heals or adopts them; nothing is deleted)"), "{line}");
        assert!(!line.contains("clean up"));
        assert_eq!(
            pointerless_line(&Ok(PointerlessScan { checked: 40, nodes: vec![] })),
            "Promotion nodes without their entry's pointer: 0 of 40 checked"
        );
    }

    #[tokio::test]
    async fn a_dry_run_deletes_nothing() {
        let server = graph_with_one_missing_node().await;
        let db = WardsonDbClient::from_url(&server.uri());
        let dir = TempDir::new("sweep-dry");
        let out = knowledge_sweep_orphans_at(&db, &test_config(), true, 10_000, dir.path()).await;
        assert!(out.starts_with("knowledge_sweep_orphans (dry_run):\n  scanned: 5\n  orphan_count: 3\n  deleted: 0"), "{out}");
        assert!(out.contains("\n  edges to remove: 3 (auto 2, curated 1, other 0)\n"), "{out}");
        assert!(out.ends_with("\n  record: not written (dry_run)"), "{out}");
        assert!(deletes_seen(&server).await.is_empty());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0, "a dry run writes no record");
    }

    #[tokio::test]
    async fn the_limit_is_what_one_call_removes_and_the_reply_says_more_exist() {
        let server = graph_with_one_missing_node().await;
        let db = WardsonDbClient::from_url(&server.uri());
        let found = find_orphan_edges(&db, 2).await.expect("the check completes");
        assert_eq!(found.orphan_ids(), ["x1", "x2"]);
        assert!(found.more);
        // The check itself covered every edge.
        assert_eq!(found.scanned, 5);
        let out = knowledge_sweep_orphans(&db, &test_config(), true, 2).await;
        assert!(out.ends_with("more orphan edges exist than the limit of 2: run it again"), "{out}");
    }

    #[tokio::test]
    async fn an_edge_with_two_missing_ends_is_listed_once() {
        let server = MockServer::start().await;
        edges_grouped(&server, &[("gone_a", 1, 0), ("gone_b", 0, 1)]).await;
        for collection in ENDPOINT_COLLECTIONS {
            collection_holds(&server, collection, &[]).await;
        }
        arm(&server, "source_id", "gone_a", &["x1"]).await;
        arm(&server, "target_id", "gone_b", &["x1"]).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let found = find_orphan_edges(&db, 10_000).await.expect("the check completes");
        assert_eq!(found.dangling, ["gone_a", "gone_b"]);
        assert_eq!(found.orphan_ids(), ["x1"]);
        assert!(!found.more);
    }

    /// The check it replaces read a failed lookup as "no such node", and the
    /// sweep then deleted every edge of the page.
    #[tokio::test]
    async fn a_node_list_that_could_not_be_read_is_an_error_and_never_a_missing_node() {
        let server = MockServer::start().await;
        edges_grouped(&server, &[("e1", 2, 2), ("n1", 2, 2)]).await;
        Mock::given(method("POST"))
            .and(path("/memory.entries/query"))
            .respond_with(data(json!({"count": 1})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.entries/aggregate"))
            .respond_with(ResponseTemplate::new(500).set_body_string("query timed out"))
            .mount(&server)
            .await;
        for collection in ["memory.semantic", "memory.procedural", "identity.graph"] {
            collection_holds(&server, collection, &["n1"]).await;
        }
        let db = WardsonDbClient::from_url(&server.uri());
        let err = find_orphan_edges(&db, 10_000).await.expect_err("an unread collection is no answer");
        assert!(err.starts_with("listing the nodes of memory.entries failed"), "{err}");

        let out = knowledge_sweep_orphans(&db, &test_config(), false, 10_000).await;
        assert!(out.starts_with("knowledge_sweep_orphans: the check could not be completed ("), "{out}");
        assert!(out.ends_with("Nothing was deleted."), "{out}");
        assert!(deletes_seen(&server).await.is_empty());
        assert!(orphan_line(&Err(err)).starts_with("Orphan edges: not checked ("));
    }

    /// A list shorter than the collection would make its other nodes look
    /// deleted.
    #[tokio::test]
    async fn a_node_list_shorter_than_the_collection_is_refused() {
        let server = MockServer::start().await;
        edges_grouped(&server, &[("e1", 1, 1), ("e2", 1, 1)]).await;
        Mock::given(method("POST"))
            .and(path("/memory.entries/query"))
            .respond_with(data(json!({"count": 2})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory.entries/aggregate"))
            .respond_with(data(json!([{"_id": "e1", "count": 1}])))
            .mount(&server)
            .await;
        for collection in ["memory.semantic", "memory.procedural", "identity.graph"] {
            collection_holds(&server, collection, &[]).await;
        }
        let db = WardsonDbClient::from_url(&server.uri());
        let err = find_orphan_edges(&db, 10_000).await.expect_err("a short list is no answer");
        assert_eq!(err, "the node list of memory.entries is incomplete: 1 of 2");
        let out = knowledge_sweep_orphans(&db, &test_config(), false, 10_000).await;
        assert!(out.ends_with("Nothing was deleted."), "{out}");
        assert!(deletes_seen(&server).await.is_empty());
    }

    #[tokio::test]
    async fn a_collection_the_server_does_not_have_holds_nothing() {
        let server = MockServer::start().await;
        edges_grouped(&server, &[("e1", 1, 1), ("old_identity", 1, 1)]).await;
        collection_holds(&server, "memory.entries", &["e1"]).await;
        collection_holds(&server, "memory.semantic", &[]).await;
        collection_holds(&server, "memory.procedural", &[]).await;
        Mock::given(method("POST"))
            .and(path("/identity.graph/query"))
            .respond_with(ResponseTemplate::new(404).set_body_string("collection not found"))
            .mount(&server)
            .await;
        arm(&server, "source_id", "old_identity", &["x1"]).await;
        arm(&server, "target_id", "old_identity", &["x2"]).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let found = find_orphan_edges(&db, 10_000).await.expect("a missing collection is an answer");
        assert_eq!(found.dangling, ["old_identity"]);
        assert_eq!(found.orphan_ids(), ["x1", "x2"]);
    }

    #[test]
    fn the_limit_is_described_as_what_a_call_removes() {
        let schema = serde_json::to_value(schemars::schema_for!(KnowledgeSweepOrphansArgs)).unwrap();
        let limit = schema["properties"]["limit"]["description"].as_str().unwrap();
        assert!(limit.starts_with("Most orphan edges removed in one call"), "{limit}");
        assert!(limit.contains("Every edge is checked on every call"), "{limit}");
    }
}

#[cfg(test)]
mod query_result_line_tests {
    use super::query_result_line;
    use crate::knowledge::types::{GraphNode, NodeType, RankedNode};

    /// A query result names its node as `collection:id`, the form the id
    /// tools take, so the model acts on a node it found without a second
    /// lookup; the superseded note follows the score as before.
    #[test]
    fn a_query_result_names_its_node_as_collection_and_id() {
        let r = RankedNode {
            node: GraphNode {
                id: "n1".to_string(),
                collection: "memory.semantic".to_string(),
                content_preview: "the clone lives in repos/".to_string(),
                node_type: NodeType::Episodic,
                depth: 0,
            },
            score: 0.62,
            source: "direct_query".to_string(),
            relevance: 0.5,
        };
        assert_eq!(
            query_result_line(1, &r, ""),
            "1. [memory.semantic:n1] the clone lives in repos/ (score: 0.62)\n   Source: direct_query\n\n"
        );
        assert_eq!(
            query_result_line(2, &r, " — superseded by memory.semantic:n2"),
            "2. [memory.semantic:n1] the clone lives in repos/ (score: 0.62) — superseded by memory.semantic:n2\n   Source: direct_query\n\n"
        );
    }
}
