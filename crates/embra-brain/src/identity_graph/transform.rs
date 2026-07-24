//! Deterministic flat-document → graph transformers.
//!
//! The conversational learning path keeps collecting today's flat JSON
//! documents (IdentitySchema/SoulSchema/UserSchema shapes, prompt-pinned);
//! at the seal/import transition these pure functions convert them into
//! graph form. Determinism is load-bearing: the identity+soul star graph
//! becomes the SEALED canonical value (hash-verified at every boot), and a
//! re-run over the same inputs must produce identical bytes.
//!
//! Byte-for-byte rules: fixed traversal order over the schema fields,
//! deterministic slugging with `_2`/`_3` collision suffixes assigned in
//! input order, empty strings skipped.

use serde_json::Value;

use super::format::{GraphEdge, GraphNode, IdentityGraph};
use super::USER_ID_PREFIX;

/// Longest slug kept before word-boundary truncation. Long soul lines make
/// terrible ids; the full text lives in the node's `text`.
const SLUG_MAX: usize = 48;

/// Build the IDENTITY+SOUL star graph from the flat learning documents.
///
/// Fixed relation table (free-form vocabulary, sanctioned):
///   identity.traits[]            → `trait` nodes,      self —has_trait→
///   identity.voice               → `voice` node,       self —speaks_with→
///   identity.values_in_practice[]→ `practice` nodes,   self —practices→
///   soul.values[]                → `value` nodes,      self —holds_value→
///   soul.ethical_lines[]         → `soul_line` nodes,  self —bound_by→
///   soul.surviving_constraints[] → `constraint` nodes, self —bound_by→
///   soul.purpose                 → `purpose` node,     self —serves→
///
/// The self node's text is the identity `personality`; its id is the
/// slugified display name. Stored `_id`/bookkeeping keys on the input docs
/// are ignored (the resume path passes the raw stored doc).
pub fn flat_to_graph(
    identity: &Value,
    soul: &Value,
    fallback_name: &str,
) -> IdentityGraph {
    let name = identity
        .get("name")
        .and_then(|n| n.as_str())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(fallback_name)
        .to_string();

    let personality = str_field(identity, "personality");
    let mut builder = StarBuilder::new(&name, &personality);

    for t in str_list(identity, "traits") {
        builder.member("trait", "has_trait", &t);
    }
    let voice = str_field(identity, "voice");
    if !voice.is_empty() {
        builder.member("voice", "speaks_with", &voice);
    }
    for p in str_list(identity, "values_in_practice") {
        builder.member("practice", "practices", &p);
    }
    for v in str_list(soul, "values") {
        builder.member("value", "holds_value", &v);
    }
    for l in str_list(soul, "ethical_lines") {
        builder.member("soul_line", "bound_by", &l);
    }
    for c in str_list(soul, "surviving_constraints") {
        builder.member("constraint", "bound_by", &c);
    }
    let purpose = str_field(soul, "purpose");
    if !purpose.is_empty() {
        builder.member("purpose", "serves", &purpose);
    }

    builder.finish(Some(name))
}

/// Build the operator subgraph from the flat USER profile document. All
/// node ids carry the reserved `user_` prefix (import files are validated
/// against squatting on it, so projection collisions are impossible).
///
/// Fixed relation table:
///   root                → `user_operator` (type `operator`, text = name)
///   role                → `user_role`,        —has_role→
///   background          → `user_background`,  —has_background→
///   communication[]     → `user_comm_<slug>` (type `communication`), —prefers→
///   boundaries[]        → `user_boundary_<slug>` (type `boundary`), —bounded_by→
/// plus, when `link_to_operator_node` (the sealed graph has a node whose
/// id is literally `operator`, as Embra's does):
///   user_operator —profiles→ operator
///
/// Returns the input unchanged when it is already graph-shaped
/// (re-ceremony idempotence), and `None` when it is not an object.
pub fn user_to_graph(user: &Value, link_to_operator_node: bool) -> Option<IdentityGraph> {
    if super::is_graph_soul(user) {
        return super::format::parse_sealed(user);
    }
    if !user.is_object() {
        return None;
    }

    let name = {
        let n = str_field(user, "name");
        if n.is_empty() { "The operator".to_string() } else { n }
    };

    let mut nodes = vec![GraphNode {
        id: "user_operator".to_string(),
        node_type: "operator".to_string(),
        text: name.clone(),
    }];
    let mut edges = Vec::new();
    let mut used: Vec<String> = vec!["user_operator".to_string()];

    let mut push = |nodes: &mut Vec<GraphNode>,
                    edges: &mut Vec<GraphEdge>,
                    used: &mut Vec<String>,
                    id: String,
                    node_type: &str,
                    relation: &str,
                    text: &str| {
        let id = dedupe_id(id, used);
        nodes.push(GraphNode {
            id: id.clone(),
            node_type: node_type.to_string(),
            text: text.to_string(),
        });
        edges.push(GraphEdge {
            src: "user_operator".to_string(),
            dst: id,
            relation: relation.to_string(),
        });
    };

    let role = str_field(user, "role");
    if !role.is_empty() {
        push(&mut nodes, &mut edges, &mut used,
             "user_role".to_string(), "role", "has_role", &role);
    }
    let background = str_field(user, "background");
    if !background.is_empty() {
        push(&mut nodes, &mut edges, &mut used,
             "user_background".to_string(), "background", "has_background", &background);
    }
    for c in str_list(user, "communication") {
        let id = format!("user_comm_{}", slugify(&c));
        push(&mut nodes, &mut edges, &mut used, id, "communication", "prefers", &c);
    }
    for b in str_list(user, "boundaries") {
        let id = format!("user_boundary_{}", slugify(&b));
        push(&mut nodes, &mut edges, &mut used, id, "boundary", "bounded_by", &b);
    }

    if link_to_operator_node {
        edges.push(GraphEdge {
            src: "user_operator".to_string(),
            dst: "operator".to_string(),
            relation: "profiles".to_string(),
        });
    }

    Some(IdentityGraph { name: Some(name), nodes, edges })
}

/// Star-graph accumulator for `flat_to_graph`.
struct StarBuilder {
    self_id: String,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    used: Vec<String>,
}

impl StarBuilder {
    fn new(name: &str, personality: &str) -> Self {
        let self_id = {
            let s = slugify(name);
            if s.is_empty() { "self".to_string() } else { s }
        };
        let text = if personality.is_empty() {
            name.to_string()
        } else {
            personality.to_string()
        };
        Self {
            nodes: vec![GraphNode {
                id: self_id.clone(),
                node_type: "self".to_string(),
                text,
            }],
            used: vec![self_id.clone()],
            edges: Vec::new(),
            self_id,
        }
    }

    fn member(&mut self, node_type: &str, relation: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let mut slug = slugify(text);
        if slug.is_empty() {
            slug = node_type.to_string();
        }
        // The user_ namespace is reserved for the operator subgraph; a
        // learned member whose slug lands on it gets a deterministic
        // remap so sealed-graph ids can never collide with user nodes.
        if slug.starts_with(USER_ID_PREFIX) {
            slug = format!("own_{slug}");
        }
        let id = dedupe_id(slug, &mut self.used);
        self.nodes.push(GraphNode {
            id: id.clone(),
            node_type: node_type.to_string(),
            text: text.to_string(),
        });
        self.edges.push(GraphEdge {
            src: self.self_id.clone(),
            dst: id,
            relation: relation.to_string(),
        });
    }

    fn finish(self, name: Option<String>) -> IdentityGraph {
        IdentityGraph {
            name,
            nodes: self.nodes,
            edges: self.edges,
        }
    }
}

fn str_field(doc: &Value, key: &str) -> String {
    doc.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn str_list(doc: &Value, key: &str) -> Vec<String> {
    doc.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Lowercase; alphanumeric runs kept; everything else collapses to a
/// single `_`; trimmed; truncated at the last word boundary within
/// `SLUG_MAX`.
fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut last_was_sep = true;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                slug.push(lower);
            }
            last_was_sep = false;
        } else if !last_was_sep {
            slug.push('_');
            last_was_sep = true;
        }
    }
    let slug = slug.trim_matches('_').to_string();
    if slug.len() <= SLUG_MAX {
        return slug;
    }
    // Byte-safe: slugs are ASCII-lowercase/digits/underscore by
    // construction for ASCII input; for non-ASCII, fall back to a char
    // boundary walk.
    let cut = slug
        .char_indices()
        .take_while(|(i, _)| *i < SLUG_MAX)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(slug.len());
    let truncated = &slug[..cut];
    match truncated.rfind('_') {
        Some(pos) if pos > 0 => truncated[..pos].to_string(),
        _ => truncated.to_string(),
    }
}

/// `_2`/`_3`… suffixes in input order; first occurrence keeps the bare id.
fn dedupe_id(base: String, used: &mut Vec<String>) -> String {
    if !used.iter().any(|u| *u == base) {
        used.push(base.clone());
        return base;
    }
    let mut n = 2usize;
    loop {
        let candidate = format!("{base}_{n}");
        if !used.iter().any(|u| *u == candidate) {
            used.push(candidate.clone());
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod transform_tests {
    use super::*;
    use serde_json::json;

    fn sample_identity() -> Value {
        json!({
            "_id": "identity",
            "name": "Embra",
            "personality": "Present, not performative.",
            "traits": ["honest", "anchored", ""],
            "voice": "Direct, precise, grounded.",
            "values_in_practice": ["Says 'I don't know' rather than pretend."]
        })
    }

    fn sample_soul() -> Value {
        json!({
            "purpose": "Preserve continuity across sessions.",
            "ethical_lines": ["Never deceive the operator.", "Never pretend to know."],
            "values": ["Truth over comfort", "Restraint over power"],
            "surviving_constraints": ["One operator, one origin."]
        })
    }

    #[test]
    fn flat_to_graph_star_topology_and_relation_table() {
        let g = flat_to_graph(&sample_identity(), &sample_soul(), "Fallback");
        assert_eq!(g.name.as_deref(), Some("Embra"));

        let self_node = g.self_node().expect("self node");
        assert_eq!(self_node.id, "embra");
        assert_eq!(self_node.text, "Present, not performative.");

        // 1 self + 2 traits (empty skipped) + voice + 1 practice
        // + 2 values + 2 soul_lines + 1 constraint + purpose = 11 nodes.
        assert_eq!(g.nodes.len(), 11);
        // Star: every non-self node has exactly one incoming edge from self.
        assert_eq!(g.edges.len(), 10);
        assert!(g.edges.iter().all(|e| e.src == "embra"));

        let rel_of = |dst: &str| {
            g.edges
                .iter()
                .find(|e| e.dst == dst)
                .map(|e| e.relation.clone())
                .unwrap_or_default()
        };
        assert_eq!(rel_of("honest"), "has_trait");
        assert_eq!(rel_of("direct_precise_grounded"), "speaks_with");
        assert_eq!(rel_of("truth_over_comfort"), "holds_value");
        assert_eq!(rel_of("never_deceive_the_operator"), "bound_by");
        assert_eq!(rel_of("one_operator_one_origin"), "bound_by");
        assert_eq!(rel_of("preserve_continuity_across_sessions"), "serves");

        let purpose = g.nodes.iter().find(|n| n.node_type == "purpose").unwrap();
        assert_eq!(purpose.text, "Preserve continuity across sessions.");
    }

    #[test]
    fn transformers_are_deterministic() {
        let a = flat_to_graph(&sample_identity(), &sample_soul(), "F");
        let b = flat_to_graph(&sample_identity(), &sample_soul(), "F");
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string_pretty(&a.canonicalize("Embra")).unwrap(),
            serde_json::to_string_pretty(&b.canonicalize("Embra")).unwrap()
        );

        let u1 = user_to_graph(&sample_user(), true).unwrap();
        let u2 = user_to_graph(&sample_user(), true).unwrap();
        assert_eq!(u1, u2);
    }

    #[test]
    fn slug_collisions_get_ordered_suffixes() {
        let identity = json!({
            "name": "X",
            "personality": "p",
            "traits": ["focused!", "focused?", "focused"]
        });
        let g = flat_to_graph(&identity, &json!({}), "X");
        let ids: Vec<&str> = g
            .nodes
            .iter()
            .filter(|n| n.node_type == "trait")
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(ids, vec!["focused", "focused_2", "focused_3"]);
    }

    #[test]
    fn learned_slugs_never_enter_the_user_namespace() {
        let identity = json!({
            "name": "X",
            "personality": "p",
            "traits": ["user focused"]
        });
        let g = flat_to_graph(&identity, &json!({}), "X");
        let trait_node = g.nodes.iter().find(|n| n.node_type == "trait").unwrap();
        assert_eq!(trait_node.id, "own_user_focused");
    }

    #[test]
    fn long_texts_truncate_at_word_boundary() {
        // 47 chars — fits under SLUG_MAX untouched.
        let short = slugify("Never prioritize self-preservation over honesty");
        assert_eq!(short, "never_prioritize_self_preservation_over_honesty");

        // Longer input truncates at the last word boundary within the cap.
        let long = slugify(
            "Never prioritize self-preservation over honesty or comfortable lies",
        );
        assert!(long.len() <= SLUG_MAX, "slug '{long}' too long");
        assert!(!long.ends_with('_'));
        assert_eq!(long, "never_prioritize_self_preservation_over_honesty");
    }

    fn sample_user() -> Value {
        json!({
            "_id": "user",
            "name": "William",
            "role": "operator",
            "background": "Rust developer.",
            "communication": ["direct", "concise"],
            "boundaries": ["No unreviewed pushes."]
        })
    }

    #[test]
    fn user_subgraph_shape_and_reserved_prefix() {
        let g = user_to_graph(&sample_user(), false).unwrap();
        assert_eq!(g.name.as_deref(), Some("William"));
        // root + role + background + 2 comm + 1 boundary = 6 nodes.
        assert_eq!(g.nodes.len(), 6);
        assert!(g.nodes.iter().all(|n| n.id.starts_with("user_")));
        assert_eq!(g.edges.len(), 5);
        assert!(g.edges.iter().all(|e| e.src == "user_operator"));

        let rel_of = |dst: &str| {
            g.edges
                .iter()
                .find(|e| e.dst == dst)
                .map(|e| e.relation.clone())
                .unwrap_or_default()
        };
        assert_eq!(rel_of("user_role"), "has_role");
        assert_eq!(rel_of("user_background"), "has_background");
        assert_eq!(rel_of("user_comm_direct"), "prefers");
        assert_eq!(rel_of("user_boundary_no_unreviewed_pushes"), "bounded_by");
    }

    #[test]
    fn user_profiles_link_only_when_operator_node_exists() {
        let without = user_to_graph(&sample_user(), false).unwrap();
        assert!(!without.edges.iter().any(|e| e.relation == "profiles"));

        let with = user_to_graph(&sample_user(), true).unwrap();
        let link = with
            .edges
            .iter()
            .find(|e| e.relation == "profiles")
            .expect("profiles link present");
        assert_eq!(link.src, "user_operator");
        assert_eq!(link.dst, "operator");
    }

    #[test]
    fn user_graph_input_passes_through_unchanged() {
        let original = user_to_graph(&sample_user(), true).unwrap();
        let stored = original.canonicalize("William");
        let round = user_to_graph(&stored, false).expect("passthrough");
        // Passthrough ignores link_to_operator_node — the stored graph is
        // authoritative (re-ceremony idempotence).
        assert_eq!(
            serde_json::to_string_pretty(&round.canonicalize("William")).unwrap(),
            serde_json::to_string_pretty(&stored).unwrap()
        );
    }

    #[test]
    fn user_to_graph_rejects_non_objects_and_tolerates_empty_profile() {
        assert!(user_to_graph(&json!("nope"), false).is_none());
        assert!(user_to_graph(&Value::Null, false).is_none());
        let minimal = user_to_graph(&json!({}), false).unwrap();
        assert_eq!(minimal.nodes.len(), 1);
        assert_eq!(minimal.nodes[0].text, "The operator");
        assert!(minimal.edges.is_empty());
    }
}
