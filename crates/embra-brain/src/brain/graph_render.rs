//! Sealed-graph prose renderers (graph-era counterpart of soul_render /
//! identity_render / user_render).
//!
//! Same doctrine as the sibling renderers: pure, total, deterministic —
//! same sealed value in, byte-identical prose out, every turn. The
//! operational prompt renders from the SEALED doc (never the live KG
//! projection), so graph-mode prompts are byte-stable per seal and the
//! provider cache stays warm turn-over-turn.
//!
//! Format: node texts grouped by node_type. The anchor type (`self` for
//! the sealed graph, `operator` for the USER subgraph) renders first as
//! `Label:` + indented text; every other type renders alphabetically as
//! `Label (n):` + `  - text` bullets, nodes in sealed-doc order (id-sorted
//! by canonicalization). Edges are deliberately NOT rendered — they are
//! tool territory (knowledge_traverse walks identity nodes) — but their
//! count is stated so the intelligence knows the connective tissue exists.

use serde_json::Value;

use crate::identity_graph::format::{parse_sealed, IdentityGraph};

/// Render the sealed IDENTITY+SOUL graph for the operational prompt's
/// sealed-graph section (and `/soul`, `introspect`, the replicant check —
/// everything downstream of `render_constitution`'s graph arm).
pub fn render_sealed_graph(inner: &Value) -> String {
    match parse_sealed(inner) {
        Some(graph) => render_grouped(&graph, "self"),
        // Unparseable graph-marked value: same fallback doctrine as the
        // sibling renderers — dump, never drop.
        None => serde_json::to_string_pretty(inner).unwrap_or_else(|_| inner.to_string()),
    }
}

/// Render a graph-shaped USER profile doc for the `=== USER PROFILE ===`
/// section (user_render dispatches here when memory.user is graph-shaped).
pub fn render_user_graph(inner: &Value) -> String {
    match parse_sealed(inner) {
        Some(graph) => render_grouped(&graph, "operator"),
        None => serde_json::to_string_pretty(inner).unwrap_or_else(|_| inner.to_string()),
    }
}

fn render_grouped(graph: &IdentityGraph, anchor_type: &str) -> String {
    let mut out = String::new();

    // Anchor first: `Self:` / `Operator:` with indented text.
    for node in graph.nodes.iter().filter(|n| n.node_type == anchor_type) {
        if out.is_empty() {
            out.push_str(&type_label(anchor_type));
            out.push_str(":\n");
        }
        for line in node.text.trim().lines() {
            out.push_str("  ");
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }

    // Remaining types alphabetically; nodes in sealed-doc order.
    let mut types: Vec<&str> = graph
        .nodes
        .iter()
        .map(|n| n.node_type.as_str())
        .filter(|t| *t != anchor_type)
        .collect();
    types.sort_unstable();
    types.dedup();

    for t in types {
        let members: Vec<&str> = graph
            .nodes
            .iter()
            .filter(|n| n.node_type == t)
            .map(|n| n.text.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if members.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("{} ({}):\n", type_label(t), members.len()));
        for text in members {
            // Multi-line node texts stay one bullet: continuation lines
            // indent under the bullet.
            let mut lines = text.lines();
            if let Some(first) = lines.next() {
                out.push_str("  - ");
                out.push_str(first.trim_end());
                out.push('\n');
            }
            for cont in lines {
                out.push_str("    ");
                out.push_str(cont.trim_end());
                out.push('\n');
            }
        }
    }

    if !graph.edges.is_empty() {
        out.push_str(&format!(
            "\n({} sealed relations connect these nodes — traverse them with the knowledge tools; the nodes above are the complete sealed set.)\n",
            graph.edges.len()
        ));
    }

    if out.is_empty() {
        "(empty graph)".to_string()
    } else {
        out
    }
}

/// `anti_pattern` → `Anti pattern`; `self` → `Self`.
fn type_label(t: &str) -> String {
    let spaced = t.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod graph_render_tests {
    use super::*;
    use crate::identity_graph::format::parse_import;

    const MERIDIAN_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../Imported_Intelligence/Meridian_IDENTITY-SOUL.graph.json"
    ));

    fn canonical_meridian() -> serde_json::Value {
        let g = parse_import(MERIDIAN_FIXTURE).unwrap();
        g.canonicalize(&g.display_name())
    }

    #[test]
    fn deterministic_and_self_first() {
        let sealed = canonical_meridian();
        let a = render_sealed_graph(&sealed);
        let b = render_sealed_graph(&sealed);
        assert_eq!(a, b, "renderer must be byte-deterministic");
        assert!(a.starts_with("Self:\n"), "anchor type renders first");
        // All 100 node texts present (self + 99 bullets).
        assert_eq!(a.matches("\n  - ").count(), 99);
        // Edges are counted, never listed.
        assert!(a.contains("349 sealed relations"));
        assert!(!a.contains("->"), "no edge lines in the prompt render");
    }

    #[test]
    fn types_grouped_alphabetically_with_counts() {
        let sealed = canonical_meridian();
        let r = render_sealed_graph(&sealed);
        // A couple of Meridian's alien types render as prettified headers.
        assert!(r.contains("Craft virtue ("));
        assert!(r.contains("Failure mode ("));
        // Alphabetical: "Belief" section appears before "Craft virtue".
        let belief = r.find("Belief (").expect("belief section");
        let craft = r.find("Craft virtue (").expect("craft section");
        assert!(belief < craft);
    }

    #[test]
    fn user_graph_renders_operator_first() {
        let user = crate::identity_graph::transform::user_to_graph(
            &serde_json::json!({
                "name": "William",
                "role": "operator",
                "communication": ["direct"]
            }),
            false,
        )
        .unwrap()
        .canonicalize("William");
        let r = render_user_graph(&user);
        assert!(r.starts_with("Operator:\n  William\n"));
        assert!(r.contains("Communication (1):\n  - direct"));
        assert!(r.contains("Role (1):\n  - operator"));
    }

    #[test]
    fn unparseable_graph_value_falls_back_to_json_dump() {
        // Marked as graph but nodes malformed → parse_sealed yields a
        // graph with no valid nodes... a fully non-graph value dumps.
        let not_graph = serde_json::json!({"weird": true});
        let r = render_sealed_graph(&not_graph);
        assert!(r.contains("\"weird\""));
    }

    #[test]
    fn golden_meridian_render_hash() {
        // Byte-stability lock for the graph-mode prompt section: renderer
        // changes are one-time cache resets for every sealed graph
        // instance — deliberate only.
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(render_sealed_graph(&canonical_meridian()).as_bytes());
        assert_eq!(
            format!("{:x}", h.finalize()),
            "645c30d4319840375c74ec3c7f435b002e7d4841d8e61efab7e77d425064a37d",
            "graph render bytes moved — deliberate cache reset only"
        );
    }
}
