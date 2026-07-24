//! The `.graph.json` import format: parsing, validation, canonicalization.
//!
//! Grammar (authoring contract: `Imported_Intelligence/README.md`):
//! `{"_comment"?, "name"?, "nodes":[{id,type,text}...], "edges":[{src,dst,relation}...]}`
//! with `{"_comment": ...}` marker objects allowed anywhere in either array
//! (any object lacking `id`/`src` respectively is skipped). Node `type` and
//! edge `relation` vocabularies are per-intelligence — treated as opaque
//! strings end to end.
//!
//! `canonicalize` output is the value that gets SEALED (SHA-256 via
//! `learning::compute_soul_hash` → trustd re-verification at every boot).
//! Determinism is load-bearing: nodes sorted by id, edges by
//! (src, dst, relation), serialization via workspace serde_json (no
//! preserve_order → alphabetical object keys in every crate).

use std::collections::{BTreeMap, HashSet};

use serde_json::{json, Value};

use super::USER_ID_PREFIX;

#[derive(Debug, Clone, PartialEq)]
pub struct GraphNode {
    pub id: String,
    pub node_type: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphEdge {
    pub src: String,
    pub dst: String,
    pub relation: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IdentityGraph {
    /// Optional top-level display name. Absent in files that predate the
    /// `name` field — `display_name()` falls back to the self node's id.
    pub name: Option<String>,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// Operator-facing summary shown before the irreversible import confirm.
#[derive(Debug, Clone)]
pub struct ImportSummary {
    pub name: String,
    pub node_count: usize,
    pub edge_count: usize,
    /// node_type → count, deterministically ordered.
    pub type_histogram: BTreeMap<String, usize>,
}

/// Mirror of WardSONDB's `validate_custom_id` (engine/document.rs): the
/// projection uses graph node ids as document `_id`s, so anything the
/// server would reject must fail validation here, before sealing.
fn id_violation(id: &str) -> Option<String> {
    if id.is_empty() {
        return Some("empty id".to_string());
    }
    if id.len() > 512 {
        return Some(format!("id longer than 512 bytes ({} bytes)", id.len()));
    }
    if id.starts_with('_') {
        return Some(format!("id '{id}' starts with '_' (reserved)"));
    }
    if id.contains('\0') {
        return Some("id contains a NUL byte".to_string());
    }
    None
}

/// Parse and validate an import file. Errors are collected, not
/// first-fail, so the operator sees everything wrong with a candidate in
/// one report.
pub fn parse_import(raw: &str) -> Result<IdentityGraph, Vec<String>> {
    let value: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return Err(vec![format!("not valid JSON: {e}")]),
    };
    let mut errors = Vec::new();

    let name = match value.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.trim().is_empty() => {
            Some(s.trim().to_string())
        }
        Some(Value::String(_)) => None,
        Some(_) => {
            errors.push("top-level 'name' must be a string".to_string());
            None
        }
    };

    let mut nodes = Vec::new();
    match value.get("nodes").and_then(|n| n.as_array()) {
        None => errors.push("missing 'nodes' array".to_string()),
        Some(arr) => {
            for (i, entry) in arr.iter().enumerate() {
                // Marker objects (and anything without an `id` key) are
                // skipped — the `_comment` convention.
                let Some(obj) = entry.as_object() else { continue };
                if !obj.contains_key("id") {
                    continue;
                }
                let id = obj.get("id").and_then(|v| v.as_str());
                let node_type = obj.get("type").and_then(|v| v.as_str());
                let text = obj.get("text").and_then(|v| v.as_str());
                match (id, node_type, text) {
                    (Some(id), Some(t), Some(text)) => nodes.push(GraphNode {
                        id: id.to_string(),
                        node_type: t.to_string(),
                        text: text.to_string(),
                    }),
                    _ => errors.push(format!(
                        "nodes[{i}]: 'id', 'type', and 'text' must all be strings"
                    )),
                }
            }
        }
    }

    let mut edges = Vec::new();
    match value.get("edges").and_then(|e| e.as_array()) {
        None => errors.push("missing 'edges' array".to_string()),
        Some(arr) => {
            for (i, entry) in arr.iter().enumerate() {
                let Some(obj) = entry.as_object() else { continue };
                if !obj.contains_key("src") {
                    continue;
                }
                let src = obj.get("src").and_then(|v| v.as_str());
                let dst = obj.get("dst").and_then(|v| v.as_str());
                let relation = obj.get("relation").and_then(|v| v.as_str());
                match (src, dst, relation) {
                    (Some(src), Some(dst), Some(rel)) => edges.push(GraphEdge {
                        src: src.to_string(),
                        dst: dst.to_string(),
                        relation: rel.to_string(),
                    }),
                    _ => errors.push(format!(
                        "edges[{i}]: 'src', 'dst', and 'relation' must all be strings"
                    )),
                }
            }
        }
    }

    let graph = IdentityGraph { name, nodes, edges };
    errors.extend(validate(&graph));
    if errors.is_empty() {
        Ok(graph)
    } else {
        Err(errors)
    }
}

/// Structural validation of an import graph. Returns every violation.
fn validate(graph: &IdentityGraph) -> Vec<String> {
    let mut errors = Vec::new();

    // Rule 1: exactly one self node — the universal structural anchor.
    let self_nodes: Vec<&GraphNode> = graph
        .nodes
        .iter()
        .filter(|n| n.node_type == "self")
        .collect();
    match self_nodes.len() {
        1 => {}
        0 => errors.push("no node with type \"self\" (exactly one required)".to_string()),
        n => errors.push(format!("{n} nodes with type \"self\" (exactly one required)")),
    }

    // Rule 4: id validity (WardSONDB _id mirror + the reserved user_ prefix)
    // and node-id uniqueness (the projection would 409 on duplicates).
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for node in &graph.nodes {
        if let Some(v) = id_violation(&node.id) {
            errors.push(format!("node '{}': {v}", node.id));
        }
        if node.id.starts_with(USER_ID_PREFIX) {
            errors.push(format!(
                "node '{}': the '{USER_ID_PREFIX}' id prefix is reserved for the \
                 locally-generated operator subgraph",
                node.id
            ));
        }
        if !seen_ids.insert(node.id.as_str()) {
            errors.push(format!("duplicate node id '{}'", node.id));
        }
    }

    // Rule 2: no dangling references. Rule 3: no duplicate triples.
    let mut seen_triples: HashSet<(&str, &str, &str)> = HashSet::new();
    for edge in &graph.edges {
        if !seen_ids.contains(edge.src.as_str()) {
            errors.push(format!(
                "edge '{}' -> '{}': src references a missing node",
                edge.src, edge.dst
            ));
        }
        if !seen_ids.contains(edge.dst.as_str()) {
            errors.push(format!(
                "edge '{}' -> '{}': dst references a missing node",
                edge.src, edge.dst
            ));
        }
        if edge.relation.trim().is_empty() {
            errors.push(format!(
                "edge '{}' -> '{}': empty relation",
                edge.src, edge.dst
            ));
        }
        if !seen_triples.insert((&edge.src, &edge.dst, &edge.relation)) {
            errors.push(format!(
                "duplicate edge ({}, {}, {})",
                edge.src, edge.dst, edge.relation
            ));
        }
    }

    errors
}

impl IdentityGraph {
    /// The single `type == "self"` node. Guaranteed present on any graph
    /// that passed `parse_import`; defensive `Option` for sealed values
    /// read back from storage.
    pub fn self_node(&self) -> Option<&GraphNode> {
        self.nodes.iter().find(|n| n.node_type == "self")
    }

    /// Display name: the top-level `name` field when present, else the
    /// self node's id title-cased (`meridian` → `Meridian`,
    /// `pathfinder_prime` → `Pathfinder Prime`).
    pub fn display_name(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }
        let Some(self_node) = self.self_node() else {
            return String::new();
        };
        title_case_id(&self_node.id)
    }

    /// The canonical sealed value. Deterministic: nodes sorted by id,
    /// edges by (src, dst, relation); the resolved display name is always
    /// injected so the sealed doc is self-contained even when the source
    /// file omitted `name`. Object keys serialize alphabetically
    /// (workspace serde_json, no preserve_order) in both embra-brain and
    /// embra-trustd — the byte contract the seal hash rides on.
    pub fn canonicalize(&self, resolved_name: &str) -> Value {
        let mut nodes = self.nodes.clone();
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        let mut edges = self.edges.clone();
        edges.sort_by(|a, b| {
            (&a.src, &a.dst, &a.relation).cmp(&(&b.src, &b.dst, &b.relation))
        });
        json!({
            "format": super::FORMAT_GRAPH_V1,
            "name": resolved_name,
            "nodes": nodes
                .iter()
                .map(|n| json!({"id": n.id, "type": n.node_type, "text": n.text}))
                .collect::<Vec<_>>(),
            "edges": edges
                .iter()
                .map(|e| json!({"src": e.src, "dst": e.dst, "relation": e.relation}))
                .collect::<Vec<_>>(),
        })
    }

    /// Operator-facing pre-confirm summary.
    pub fn summary(&self) -> ImportSummary {
        let mut type_histogram: BTreeMap<String, usize> = BTreeMap::new();
        for node in &self.nodes {
            *type_histogram.entry(node.node_type.clone()).or_default() += 1;
        }
        ImportSummary {
            name: self.display_name(),
            node_count: self.nodes.len(),
            edge_count: self.edges.len(),
            type_histogram,
        }
    }
}

/// Read a canonical sealed graph value back into an `IdentityGraph`
/// (renderer + projection). Returns `None` unless the value carries the
/// graph.v1 marker shape.
pub fn parse_sealed(inner: &Value) -> Option<IdentityGraph> {
    if !super::is_graph_soul(inner) {
        return None;
    }
    let name = inner
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|n| !n.is_empty())
        .map(|n| n.to_string());
    let nodes = inner
        .get("nodes")?
        .as_array()?
        .iter()
        .filter_map(|n| {
            Some(GraphNode {
                id: n.get("id")?.as_str()?.to_string(),
                node_type: n.get("type")?.as_str()?.to_string(),
                text: n.get("text")?.as_str()?.to_string(),
            })
        })
        .collect();
    let edges = inner
        .get("edges")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    Some(GraphEdge {
                        src: e.get("src")?.as_str()?.to_string(),
                        dst: e.get("dst")?.as_str()?.to_string(),
                        relation: e.get("relation")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(IdentityGraph { name, nodes, edges })
}

/// `meridian` → `Meridian`, `pathfinder_prime` → `Pathfinder Prime`.
fn title_case_id(id: &str) -> String {
    id.split('_')
        .filter(|seg| !seg.is_empty())
        .map(|seg| {
            let mut chars = seg.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + chars.as_str()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod format_tests {
    use super::*;

    const EMBRA_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../Imported_Intelligence/Embra_IDENTITY-SOUL.graph.json"
    ));
    const MERIDIAN_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../Imported_Intelligence/Meridian_IDENTITY-SOUL.graph.json"
    ));

    #[test]
    fn both_fixtures_parse_clean() {
        let embra = parse_import(EMBRA_FIXTURE).expect("Embra fixture valid");
        assert_eq!(embra.nodes.len(), 100);
        assert_eq!(embra.edges.len(), 354);
        assert_eq!(embra.self_node().unwrap().id, "embra");
        assert_eq!(embra.display_name(), "Embra");

        let meridian = parse_import(MERIDIAN_FIXTURE).expect("Meridian fixture valid");
        assert_eq!(meridian.nodes.len(), 100);
        assert_eq!(meridian.edges.len(), 349);
        assert_eq!(meridian.self_node().unwrap().id, "meridian");
        // Neither fixture has a top-level name yet — the title-cased
        // self-id fallback is the active path.
        assert_eq!(meridian.display_name(), "Meridian");
        // Free-form vocabulary: Meridian's relation set is large and alien.
        let relations: std::collections::HashSet<&str> =
            meridian.edges.iter().map(|e| e.relation.as_str()).collect();
        assert!(relations.len() > 40, "expected 54 distinct relations");
    }

    #[test]
    fn comment_markers_are_skipped_everywhere() {
        let g = parse_import(
            r#"{
                "_comment": "top",
                "nodes": [
                    {"_comment": "section"},
                    {"id": "me", "type": "self", "text": "I."},
                    {},
                    {"id": "v", "type": "value", "text": "V."}
                ],
                "edges": [
                    {"_comment": "wiring"},
                    {"src": "me", "dst": "v", "relation": "holds_value"}
                ]
            }"#,
        )
        .expect("markers skipped");
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.edges.len(), 1);
    }

    #[test]
    fn validation_rules_each_trip() {
        // No self node.
        let e = parse_import(
            r#"{"nodes":[{"id":"a","type":"value","text":"t"}],"edges":[]}"#,
        )
        .unwrap_err();
        assert!(e.iter().any(|m| m.contains("no node with type \"self\"")));

        // Two self nodes.
        let e = parse_import(
            r#"{"nodes":[
                {"id":"a","type":"self","text":"t"},
                {"id":"b","type":"self","text":"t"}
            ],"edges":[]}"#,
        )
        .unwrap_err();
        assert!(e.iter().any(|m| m.contains("2 nodes with type \"self\"")));

        // Dangling edge, duplicate triple, duplicate node id, reserved
        // prefix, leading underscore, empty relation — all in one report.
        let e = parse_import(
            r#"{"nodes":[
                {"id":"me","type":"self","text":"t"},
                {"id":"me","type":"value","text":"t"},
                {"id":"user_x","type":"value","text":"t"},
                {"id":"_bad","type":"value","text":"t"}
            ],"edges":[
                {"src":"me","dst":"ghost","relation":"r"},
                {"src":"me","dst":"user_x","relation":"r"},
                {"src":"me","dst":"user_x","relation":"r"},
                {"src":"me","dst":"_bad","relation":"  "}
            ]}"#,
        )
        .unwrap_err();
        assert!(e.iter().any(|m| m.contains("duplicate node id 'me'")));
        assert!(e.iter().any(|m| m.contains("reserved")));
        assert!(e.iter().any(|m| m.contains("starts with '_'")));
        assert!(e.iter().any(|m| m.contains("missing node")));
        assert!(e.iter().any(|m| m.contains("duplicate edge")));
        assert!(e.iter().any(|m| m.contains("empty relation")));

        // Malformed node (id present but wrong types) errors rather than
        // being silently skipped.
        let e = parse_import(r#"{"nodes":[{"id":5}],"edges":[]}"#).unwrap_err();
        assert!(e.iter().any(|m| m.contains("nodes[0]")));

        // Not JSON at all.
        assert!(parse_import("not json").is_err());
    }

    #[test]
    fn canonicalize_is_deterministic_and_order_insensitive() {
        let g = parse_import(MERIDIAN_FIXTURE).unwrap();
        let canonical = g.canonicalize("Meridian");

        // Shuffled input (reverse both arrays) produces identical bytes.
        let mut shuffled = g.clone();
        shuffled.nodes.reverse();
        shuffled.edges.reverse();
        assert_eq!(
            serde_json::to_string_pretty(&canonical).unwrap(),
            serde_json::to_string_pretty(&shuffled.canonicalize("Meridian")).unwrap()
        );

        // Round-trip idempotence: parse_sealed(canonical) re-canonicalizes
        // to the same bytes.
        let reparsed = parse_sealed(&canonical).expect("canonical parses");
        assert_eq!(
            serde_json::to_string_pretty(&canonical).unwrap(),
            serde_json::to_string_pretty(&reparsed.canonicalize("Meridian")).unwrap()
        );

        // The canonical value is a graph soul; the name is injected.
        assert!(crate::identity_graph::is_graph_soul(&canonical));
        assert_eq!(canonical["name"], "Meridian");
    }

    /// The cross-crate byte-determinism lock: this hash is exactly what
    /// `seal_soul` writes to /embra/state/soul.sha256 and what
    /// embra-trustd recomputes at every boot for a Meridian import. If it
    /// moves, canonicalization changed and EVERY previously sealed graph
    /// instance would fail boot verification — fix the change, never the
    /// pinned hash.
    #[test]
    fn canonical_meridian_seal_hash_is_frozen() {
        let g = parse_import(MERIDIAN_FIXTURE).unwrap();
        let canonical = g.canonicalize(&g.display_name());
        let hash = crate::learning::compute_soul_hash(&canonical).unwrap();
        assert_eq!(
            hash,
            "ba0cebe174ca2029c776425ec31ba41301394fb1266a664b7c941c00d72c7031",
            "canonicalization bytes moved — see doc-comment"
        );
    }

    #[test]
    fn title_case_fallback() {
        assert_eq!(title_case_id("meridian"), "Meridian");
        assert_eq!(title_case_id("pathfinder_prime"), "Pathfinder Prime");
        assert_eq!(title_case_id("x"), "X");
    }

    #[test]
    fn summary_histogram_is_deterministic() {
        let g = parse_import(EMBRA_FIXTURE).unwrap();
        let s = g.summary();
        assert_eq!(s.name, "Embra");
        assert_eq!(s.node_count, 100);
        assert_eq!(s.edge_count, 354);
        assert_eq!(s.type_histogram.get("self"), Some(&1));
        assert_eq!(s.type_histogram.get("soul_line"), Some(&3));
        // BTreeMap ordering: first key is alphabetically first type.
        let first = s.type_histogram.keys().next().unwrap();
        assert_eq!(first, "anti_pattern");
    }

    #[test]
    fn explicit_name_field_wins_over_fallback() {
        let g = parse_import(
            r#"{"name":"Custom Name","nodes":[{"id":"me","type":"self","text":"t"}],"edges":[]}"#,
        )
        .unwrap();
        assert_eq!(g.display_name(), "Custom Name");
    }
}
