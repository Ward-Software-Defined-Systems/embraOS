//! KG-native identity: the sealed IDENTITY+SOUL graph and its machinery.
//!
//! In graph mode the sealed inner `soul` value in `soul.invariant` is a
//! canonical graph `{format:"graph.v1", name, nodes, edges}` instead of the
//! legacy flat `{purpose, ethical_lines, ...}` document. The sealed doc is
//! the source of truth; a live KG projection (one doc per node in the
//! `identity.graph` collection + one `memory.edges` doc per edge) derives
//! from it and is healed by an insert-missing-only boot reconcile.
//!
//! Mode detection everywhere is [`is_graph_soul`] on the loaded inner soul
//! value — legacy flat souls take the pre-existing code paths byte-for-byte
//! (`legacy_prompt_golden_tests` in brain/prompts.rs is the tripwire).

pub mod format;
pub mod project;
pub mod transform;

/// WardSONDB collection holding one doc per identity-graph node.
pub const IDENTITY_COLLECTION: &str = "identity.graph";

/// `format` marker of the canonical sealed graph value.
pub const FORMAT_GRAPH_V1: &str = "graph.v1";

/// Node `origin` values in the projection.
pub const ORIGIN_IMPORT: &str = "import";
pub const ORIGIN_LEARNED: &str = "learned";
pub const ORIGIN_USER: &str = "user";

/// `metadata.origin` on projected memory.edges docs.
pub const EDGE_ORIGIN_IDENTITY: &str = "identity_import";
pub const EDGE_ORIGIN_USER: &str = "user_profile";

/// Reserved node-id prefix for the locally-generated operator subgraph.
/// Import files may not use it (collision-proofing the projection).
pub const USER_ID_PREFIX: &str = "user_";

/// True when a loaded inner soul value is a graph-era canonical graph.
/// The guard requires both the format marker AND a nodes array so that no
/// conceivable legacy flat soul (whose keys are operator/LLM-authored) can
/// ever be misrouted onto the graph path.
pub fn is_graph_soul(inner: &serde_json::Value) -> bool {
    inner
        .get("format")
        .and_then(|f| f.as_str())
        .map(|f| f == FORMAT_GRAPH_V1)
        .unwrap_or(false)
        && inner.get("nodes").map(|n| n.is_array()).unwrap_or(false)
}

#[cfg(test)]
mod mode_detection_tests {
    use super::*;

    #[test]
    fn graph_soul_requires_both_marker_and_nodes() {
        assert!(is_graph_soul(&serde_json::json!({
            "format": "graph.v1", "name": "X", "nodes": [], "edges": []
        })));
        // Marker without nodes: not a graph soul.
        assert!(!is_graph_soul(
            &serde_json::json!({"format": "graph.v1"})
        ));
        // Nodes without marker: not a graph soul.
        assert!(!is_graph_soul(&serde_json::json!({"nodes": []})));
        // Legacy flat soul: never a graph soul.
        assert!(!is_graph_soul(&serde_json::json!({
            "purpose": "p", "ethical_lines": [], "values": []
        })));
        // Wrong format version: not (yet) a graph soul.
        assert!(!is_graph_soul(&serde_json::json!({
            "format": "graph.v2", "nodes": []
        })));
        assert!(!is_graph_soul(&serde_json::Value::Null));
    }
}
