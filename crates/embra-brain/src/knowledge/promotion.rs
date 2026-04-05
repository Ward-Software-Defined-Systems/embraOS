//! Promotion: episodic entry → semantic/procedural node.
//!
//! Creates a provenance chain:
//! - New semantic/procedural node carries `source_entry_id` and `source_session`.
//! - Source `memory.entries` doc gets `promoted_to: {collection, id}` PATCHed in.
//! - A directed `derived_from` edge (new_node → source_entry) is inserted.
//! - Auto edges are derived for the new node (same_session, temporal, tag_overlap).

use anyhow::{anyhow, Result};
use chrono::Utc;
use serde_json::json;

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::edges::derive_edges;
use super::types::{EdgeType, SemanticCategory};

const PROCEDURAL_SCHEMA_HINT: &str = r#"{"title": "...", "description": "...", "preconditions": [...], "steps": [{"order": N, "action": "...", "notes": "..."}], "outcomes": {"success": "...", "failure": "..."}}"#;

/// Promote an episodic entry to `memory.semantic`. Returns the new semantic node _id.
pub async fn promote_to_semantic(
    db: &WardsonDbClient,
    entry_id: &str,
    category: SemanticCategory,
    config: &SystemConfig,
) -> Result<String> {
    let (source_doc, content, tags, session) = load_source_entry(db, entry_id).await?;

    let now = Utc::now().to_rfc3339();
    let semantic_doc = json!({
        "content": content,
        "category": category.as_str(),
        "tags": tags.clone(),
        "source_entry_id": entry_id,
        "source_session": session.clone(),
        "confidence": 0.9,
        "access_count": 0,
        "last_accessed": serde_json::Value::Null,
        "created_at": now,
        "updated_at": now,
    });

    let new_id = db.write("memory.semantic", &semantic_doc).await?;

    // PATCH the source entry to record the promotion.
    let _ = db.patch_document("memory.entries", entry_id, &json!({
        "promoted_to": { "collection": "memory.semantic", "id": new_id }
    })).await;

    // Directed derived_from edge.
    insert_derived_from_edge(
        db,
        &new_id,
        "memory.semantic",
        entry_id,
        "memory.entries",
        json!({ "promotion_type": "semantic", "category": category.as_str() }),
        &now,
    ).await;

    // Auto-derive edges for the new semantic node.
    let _ = derive_edges(
        db,
        &new_id,
        "memory.semantic",
        &session,
        &tags,
        &now,
        config,
    ).await;

    // Prevent unused warning; the source doc was loaded for validation.
    let _ = source_doc;

    Ok(new_id)
}

/// Promote an episodic entry to `memory.procedural`. Returns the new procedural node _id.
pub async fn promote_to_procedural(
    db: &WardsonDbClient,
    entry_id: &str,
    procedure_json: &str,
    config: &SystemConfig,
) -> Result<String> {
    let (source_doc, _content, tags, session) = load_source_entry(db, entry_id).await?;

    let parsed: serde_json::Value = serde_json::from_str(procedure_json)
        .map_err(|e| anyhow!("Invalid procedural data: {}\nExpected schema: {}", e, PROCEDURAL_SCHEMA_HINT))?;

    let title = parsed.get("title").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Invalid procedural data: missing field 'title'\nExpected schema: {}", PROCEDURAL_SCHEMA_HINT))?.to_string();
    let description = parsed.get("description").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Invalid procedural data: missing field 'description'\nExpected schema: {}", PROCEDURAL_SCHEMA_HINT))?.to_string();
    let preconditions: Vec<String> = parsed.get("preconditions")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let steps_val = parsed.get("steps")
        .ok_or_else(|| anyhow!("Invalid procedural data: missing field 'steps'\nExpected schema: {}", PROCEDURAL_SCHEMA_HINT))?;
    let outcomes_val = parsed.get("outcomes")
        .ok_or_else(|| anyhow!("Invalid procedural data: missing field 'outcomes'\nExpected schema: {}", PROCEDURAL_SCHEMA_HINT))?;
    let success = outcomes_val.get("success").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Invalid procedural data: missing field 'outcomes.success'\nExpected schema: {}", PROCEDURAL_SCHEMA_HINT))?.to_string();
    let failure = outcomes_val.get("failure").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Invalid procedural data: missing field 'outcomes.failure'\nExpected schema: {}", PROCEDURAL_SCHEMA_HINT))?.to_string();

    let now = Utc::now().to_rfc3339();
    let proc_doc = json!({
        "title": title,
        "description": description,
        "preconditions": preconditions,
        "steps": steps_val,
        "outcomes": { "success": success, "failure": failure },
        "tags": tags.clone(),
        "source_entry_id": entry_id,
        "source_session": session.clone(),
        "access_count": 0,
        "last_accessed": serde_json::Value::Null,
        "created_at": now,
        "updated_at": now,
    });

    let new_id = db.write("memory.procedural", &proc_doc).await?;

    let _ = db.patch_document("memory.entries", entry_id, &json!({
        "promoted_to": { "collection": "memory.procedural", "id": new_id }
    })).await;

    insert_derived_from_edge(
        db,
        &new_id,
        "memory.procedural",
        entry_id,
        "memory.entries",
        json!({ "promotion_type": "procedural" }),
        &now,
    ).await;

    let _ = derive_edges(
        db,
        &new_id,
        "memory.procedural",
        &session,
        &tags,
        &now,
        config,
    ).await;

    let _ = source_doc;

    Ok(new_id)
}

/// Load and validate a source entry. Errors if not found or already promoted.
/// Returns (raw_doc, content, tags, session).
async fn load_source_entry(
    db: &WardsonDbClient,
    entry_id: &str,
) -> Result<(serde_json::Value, String, Vec<String>, String)> {
    let doc = db.read("memory.entries", entry_id).await
        .map_err(|_| anyhow!("Entry {} not found in memory.entries", entry_id))?;

    if let Some(promoted) = doc.get("promoted_to") {
        if !promoted.is_null() {
            let coll = promoted.get("collection").and_then(|v| v.as_str()).unwrap_or("unknown");
            return Err(anyhow!("Entry {} already promoted to {}", entry_id, coll));
        }
    }

    let content = doc.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let tags: Vec<String> = doc.get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let session = doc.get("session").and_then(|v| v.as_str()).unwrap_or("").to_string();

    Ok((doc, content, tags, session))
}

async fn insert_derived_from_edge(
    db: &WardsonDbClient,
    source_id: &str,
    source_collection: &str,
    target_id: &str,
    target_collection: &str,
    metadata: serde_json::Value,
    created_at: &str,
) {
    let edge = json!({
        "source_id": source_id,
        "source_collection": source_collection,
        "target_id": target_id,
        "target_collection": target_collection,
        "edge_type": EdgeType::DerivedFrom.as_str(),
        "weight": 1.0,
        "metadata": metadata,
        "created_at": created_at,
    });
    let _ = db.write("memory.edges", &edge).await;
}
