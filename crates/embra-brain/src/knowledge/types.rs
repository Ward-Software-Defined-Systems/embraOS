//! Knowledge graph data model.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SemanticCategory {
    Fact,
    Preference,
    Decision,
    Observation,
    Pattern,
}

impl SemanticCategory {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "fact" => Some(Self::Fact),
            "preference" => Some(Self::Preference),
            "decision" => Some(Self::Decision),
            "observation" => Some(Self::Observation),
            "pattern" => Some(Self::Pattern),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Preference => "preference",
            Self::Decision => "decision",
            Self::Observation => "observation",
            Self::Pattern => "pattern",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticNode {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _id: Option<String>,
    pub content: String,
    pub category: SemanticCategory,
    pub tags: Vec<String>,
    pub source_entry_id: String,
    pub source_session: String,
    pub confidence: f64,
    pub access_count: u64,
    pub last_accessed: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProceduralStep {
    pub order: u32,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProceduralOutcomes {
    pub success: String,
    pub failure: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProceduralNode {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _id: Option<String>,
    pub title: String,
    pub description: String,
    pub preconditions: Vec<String>,
    pub steps: Vec<ProceduralStep>,
    pub outcomes: ProceduralOutcomes,
    pub tags: Vec<String>,
    pub source_entry_id: String,
    pub source_session: String,
    pub access_count: u64,
    pub last_accessed: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EdgeType {
    // Auto-derived at write time
    SameSession,
    Temporal,
    TagOverlap,
    // Brain-created
    DerivedFrom,
    Enables,
    Contradicts,
    Refines,
    DependsOn,
    /// Symmetric same-scope link. Used when two nodes concern the same topic /
    /// system area but neither enables, refines, nor contradicts the other.
    /// Still stored with a source → target direction; retrieval treats it as
    /// non-hierarchical.
    RelatedTo,
}

impl EdgeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SameSession => "same_session",
            Self::Temporal => "temporal",
            Self::TagOverlap => "tag_overlap",
            Self::DerivedFrom => "derived_from",
            Self::Enables => "enables",
            Self::Contradicts => "contradicts",
            Self::Refines => "refines",
            Self::DependsOn => "depends_on",
            Self::RelatedTo => "related_to",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "same_session" => Some(Self::SameSession),
            "temporal" => Some(Self::Temporal),
            "tag_overlap" => Some(Self::TagOverlap),
            "derived_from" => Some(Self::DerivedFrom),
            "enables" => Some(Self::Enables),
            "contradicts" => Some(Self::Contradicts),
            "refines" => Some(Self::Refines),
            "depends_on" => Some(Self::DependsOn),
            "related_to" => Some(Self::RelatedTo),
            _ => None,
        }
    }

    /// Brain-created edge types (allowed in `knowledge_link` tool).
    pub fn is_brain_created(&self) -> bool {
        matches!(
            self,
            Self::Enables
                | Self::Contradicts
                | Self::Refines
                | Self::DependsOn
                | Self::RelatedTo
        )
    }

    /// Symmetric edge types — auto-derived edges are inserted
    /// bidirectionally (see `knowledge::edges::push_bidirectional`), and
    /// `related_to` is documented as same-scope/non-hierarchical. The
    /// remaining brain-created types (`enables`, `contradicts`, `refines`,
    /// `depends_on`) plus `derived_from` (provenance from promotion) are
    /// directional and must not be deleted bidirectionally.
    pub fn is_symmetric(&self) -> bool {
        matches!(
            self,
            Self::SameSession | Self::Temporal | Self::TagOverlap | Self::RelatedTo
        )
    }
}

#[cfg(test)]
mod edge_type_tests {
    use super::EdgeType;

    #[test]
    fn related_to_roundtrip() {
        let t = EdgeType::from_str("related_to").expect("parse");
        assert_eq!(t, EdgeType::RelatedTo);
        assert_eq!(t.as_str(), "related_to");
        assert!(t.is_brain_created());
    }

    #[test]
    fn related_to_tolerates_case_and_whitespace() {
        assert_eq!(EdgeType::from_str("  Related_To "), Some(EdgeType::RelatedTo));
        assert_eq!(EdgeType::from_str("RELATED_TO"), Some(EdgeType::RelatedTo));
    }

    #[test]
    fn existing_brain_types_still_brain_created() {
        for t in [
            EdgeType::Enables,
            EdgeType::Contradicts,
            EdgeType::Refines,
            EdgeType::DependsOn,
        ] {
            assert!(t.is_brain_created());
        }
    }

    #[test]
    fn auto_derived_not_brain_created() {
        for t in [EdgeType::SameSession, EdgeType::Temporal, EdgeType::TagOverlap] {
            assert!(!t.is_brain_created());
        }
    }

    #[test]
    fn directional_types_not_symmetric() {
        // Embra_Debug #63: `enables`, `contradicts`, `refines`,
        // `depends_on`, and `derived_from` are documented directional.
        // `knowledge_unlink_edge`'s triple form must not bidirectional-
        // delete them.
        for t in [
            EdgeType::Enables,
            EdgeType::Contradicts,
            EdgeType::Refines,
            EdgeType::DependsOn,
            EdgeType::DerivedFrom,
        ] {
            assert!(!t.is_symmetric(), "{:?} should be directional", t);
        }
    }

    #[test]
    fn auto_derived_and_related_to_are_symmetric() {
        for t in [
            EdgeType::SameSession,
            EdgeType::Temporal,
            EdgeType::TagOverlap,
            EdgeType::RelatedTo,
        ] {
            assert!(t.is_symmetric(), "{:?} should be symmetric", t);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeEdge {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _id: Option<String>,
    pub source_id: String,
    pub source_collection: String,
    pub target_id: String,
    pub target_collection: String,
    pub edge_type: EdgeType,
    pub weight: f64,
    pub metadata: serde_json::Value,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeType {
    Episodic,
    Semantic { category: SemanticCategory },
    Procedural { title: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub collection: String,
    pub content_preview: String,
    pub node_type: NodeType,
    pub depth: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraversalResult {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<KnowledgeEdge>,
    pub depth_reached: u32,
    pub nodes_visited: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedNode {
    pub node: GraphNode,
    pub score: f64,
    pub source: String,
}

/// Truncate a string to at most `max_chars` Unicode scalar values, appending
/// an ellipsis if truncation occurred.
pub fn content_preview(s: &str, max_chars: usize) -> String {
    let trimmed: String = s.chars().take(max_chars).collect();
    if trimmed.chars().count() < s.chars().count() {
        format!("{}…", trimmed)
    } else {
        trimmed
    }
}
