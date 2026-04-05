//! Cross-session knowledge graph (Phase 1 Sprint 2).
//!
//! Built on WardSONDB collections:
//! - `memory.semantic` — promoted factual/preference/decision/observation/pattern nodes
//! - `memory.procedural` — promoted how-to knowledge with structured steps
//! - `memory.edges` — typed, weighted edges connecting any two memory nodes
//!
//! The knowledge graph is purely application-layer; all storage and queries
//! go through the existing `WardsonDbClient`.

pub mod edges;
pub mod promotion;
pub mod retrieval;
pub mod tools;
pub mod traversal;
pub mod types;
