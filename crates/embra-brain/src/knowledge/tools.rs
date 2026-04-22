//! Knowledge graph tool implementations.
//!
//! Tag syntax uses `|` as a delimiter between compound parameters.

use serde_json::json;

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::promotion::{promote_to_procedural, promote_to_semantic};
use super::retrieval::retrieve_relevant_knowledge;
use super::traversal::traverse;
use super::types::{content_preview, EdgeType, NodeType, SemanticCategory};

/// `[TOOL:knowledge_promote <entry_id> | <type> | <data>]`
pub async fn knowledge_promote(
    params: &str,
    db: &WardsonDbClient,
    config: &SystemConfig,
) -> String {
    let parts: Vec<&str> = params.splitn(3, '|').map(|s| s.trim()).collect();
    if parts.len() < 3 {
        return "Error: usage [TOOL:knowledge_promote <entry_id> | <type> | <data>]".into();
    }
    let entry_id = parts[0];
    let ptype = parts[1].to_lowercase();
    let data = parts[2];

    match ptype.as_str() {
        "semantic" => {
            let Some(category) = SemanticCategory::from_str(data) else {
                return format!("Error: Invalid category '{}'. Must be one of: fact, preference, decision, observation, pattern", data);
            };
            match promote_to_semantic(db, entry_id, category.clone(), config).await {
                Ok(new_id) => format!(
                    "Promoted entry {} to memory.semantic\nNew node ID: {}\nCategory: {}",
                    entry_id, new_id, category.as_str()
                ),
                Err(e) => format!("Error: {}", e),
            }
        }
        "procedural" => {
            match promote_to_procedural(db, entry_id, data, config).await {
                Ok(new_id) => format!(
                    "Promoted entry {} to memory.procedural\nNew node ID: {}",
                    entry_id, new_id
                ),
                Err(e) => format!("Error: {}", e),
            }
        }
        _ => "Error: Type must be 'semantic' or 'procedural'".into(),
    }
}

/// `[TOOL:knowledge_link <source_coll>:<source_id> | <edge_type> | <target_coll>:<target_id> | <weight>]`
pub async fn knowledge_link(params: &str, db: &WardsonDbClient) -> String {
    let parts: Vec<&str> = params.splitn(4, '|').map(|s| s.trim()).collect();
    if parts.len() < 4 {
        return "Error: usage [TOOL:knowledge_link <source_coll>:<source_id> | <edge_type> | <target_coll>:<target_id> | <weight>]".into();
    }
    let Some((src_coll, src_id)) = parts[0].split_once(':') else {
        return "Error: source must be <collection>:<id>".into();
    };
    let Some(edge_type) = EdgeType::from_str(parts[1]) else {
        return format!("Error: Invalid edge type '{}'. Brain-created types: enables, contradicts, refines, depends_on", parts[1]);
    };
    if !edge_type.is_brain_created() {
        return format!("Error: Invalid edge type '{}'. Brain-created types: enables, contradicts, refines, depends_on", parts[1]);
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

    // Validate source + target exist
    if db.read(src_coll, src_id).await.is_err() {
        return format!("Error: Source {}:{} not found", src_coll, src_id);
    }
    if db.read(tgt_coll, tgt_id).await.is_err() {
        return format!("Error: Target {}:{} not found", tgt_coll, tgt_id);
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
    if let Ok(existing) = db.query("memory.edges", &dup_filter).await {
        if !existing.is_empty() {
            return format!(
                "Error: Edge already exists from {}:{} to {}:{} with type {}",
                src_coll, src_id, tgt_coll, tgt_id, edge_type.as_str()
            );
        }
    }

    let edge_doc = json!({
        "source_id": src_id,
        "source_collection": src_coll,
        "target_id": tgt_id,
        "target_collection": tgt_coll,
        "edge_type": edge_type.as_str(),
        "weight": weight,
        "metadata": {},
        "created_at": chrono::Utc::now().to_rfc3339(),
    });
    match db.write("memory.edges", &edge_doc).await {
        Ok(edge_id) => format!(
            "Created edge: {}:{} --[{} w={}]--> {}:{}\nEdge ID: {}",
            src_coll, src_id, edge_type.as_str(), weight, tgt_coll, tgt_id, edge_id
        ),
        Err(e) => format!("Error: failed to create edge: {}", e),
    }
}

/// `[TOOL:knowledge_unlink_edge <edge_id>]` — delete a single edge by ID
/// `[TOOL:knowledge_unlink_edge <src_coll>:<src_id> | <edge_type> | <tgt_coll>:<tgt_id>]` — delete matching edges (bidirectional)
pub async fn knowledge_unlink_edge(params: &str, db: &WardsonDbClient) -> String {
    let trimmed = params.trim();
    if trimmed.is_empty() {
        return "Error: usage [TOOL:knowledge_unlink_edge <edge_id>] or [TOOL:knowledge_unlink_edge <src_coll>:<src_id> | <edge_type> | <tgt_coll>:<tgt_id>]".into();
    }

    if trimmed.contains('|') {
        // Form 2: triple parse, bidirectional delete
        let parts: Vec<&str> = trimmed.splitn(3, '|').map(|s| s.trim()).collect();
        if parts.len() < 3 {
            return "Error: usage [TOOL:knowledge_unlink_edge <src_coll>:<src_id> | <edge_type> | <tgt_coll>:<tgt_id>]".into();
        }
        let Some((src_coll, src_id)) = parts[0].split_once(':') else {
            return "Error: source must be <collection>:<id>".into();
        };
        let Some(edge_type) = EdgeType::from_str(parts[1]) else {
            return format!("Error: Invalid edge type '{}'. Valid types: same_session, temporal, tag_overlap, derived_from, enables, contradicts, refines, depends_on", parts[1]);
        };
        let Some((tgt_coll, tgt_id)) = parts[2].split_once(':') else {
            return "Error: target must be <collection>:<id>".into();
        };

        let etype = edge_type.as_str();
        let filter = json!({
            "$or": [
                {"source_id": src_id, "target_id": tgt_id, "edge_type": etype},
                {"source_id": tgt_id, "target_id": src_id, "edge_type": etype}
            ]
        });
        match db.delete_by_query("memory.edges", &filter).await {
            Ok(0) => format!(
                "Error: No edge found from {}:{} to {}:{} with type {}",
                src_coll, src_id, tgt_coll, tgt_id, etype
            ),
            Ok(1) => format!(
                "Removed 1 edge:\n  {}:{} --[{}]--> {}:{}",
                src_coll, src_id, etype, tgt_coll, tgt_id
            ),
            Ok(n) => format!(
                "Removed {} edges (bidirectional):\n  {}:{} --[{}]--> {}:{}\n  {}:{} --[{}]--> {}:{}",
                n, src_coll, src_id, etype, tgt_coll, tgt_id,
                tgt_coll, tgt_id, etype, src_coll, src_id
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

/// `[TOOL:knowledge_unlink_node <collection>:<id>]` — delete a semantic or
/// procedural node and cascade-remove every edge referencing it (source or target).
///
/// Scoped to `memory.semantic` and `memory.procedural`. Use `[TOOL:forget]` for
/// episodic `memory.entries` cleanup.
pub async fn knowledge_unlink_node(params: &str, db: &WardsonDbClient) -> String {
    let trimmed = params.trim();
    if trimmed.is_empty() {
        return "Error: usage [TOOL:knowledge_unlink_node <collection>:<id>]".into();
    }
    let Some((coll, id)) = trimmed.split_once(':') else {
        return "Error: target must be <collection>:<id>".into();
    };
    let coll = coll.trim();
    let id = id.trim();
    if coll != "memory.semantic" && coll != "memory.procedural" {
        return format!(
            "Error: knowledge_unlink_node only operates on memory.semantic or memory.procedural (got '{}'). Use [TOOL:forget] for memory.entries.",
            coll
        );
    }

    let node_doc = match db.read(coll, id).await {
        Ok(doc) => doc,
        Err(_) => return format!("Error: Node {}:{} not found", coll, id),
    };

    let preview_src = node_doc
        .get("content")
        .or_else(|| node_doc.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("(no preview)");
    let preview = content_preview(preview_src, 80);

    // Clear promoted_to on any episodic source entries that point at this node.
    // Done before the edge cascade so a partial failure leaves the system retryable
    // rather than with a stale pointer to a missing node.
    let derived_filter = json!({
        "filter": {
            "source_id": id,
            "source_collection": coll,
            "edge_type": "derived_from",
        },
        "limit": 50,
    });
    let mut cleared_entries = 0usize;
    if let Ok(derived_edges) = db.query("memory.edges", &derived_filter).await {
        for edge in derived_edges {
            let (Some(tgt_id), Some(tgt_coll)) = (
                edge.get("target_id").and_then(|v| v.as_str()),
                edge.get("target_collection").and_then(|v| v.as_str()),
            ) else { continue };
            if tgt_coll != "memory.entries" { continue; }
            if db.patch_document("memory.entries", tgt_id, &json!({"promoted_to": null})).await.is_ok() {
                cleared_entries += 1;
            }
        }
    }

    let edge_filter = json!({
        "$or": [
            {"source_id": id, "source_collection": coll},
            {"target_id": id, "target_collection": coll}
        ]
    });
    let edge_count = db.delete_by_query("memory.edges", &edge_filter).await.unwrap_or(0);

    if let Err(e) = db.delete(coll, id).await {
        return format!(
            "Error: cleared {} source entry(ies) and removed {} referencing edge(s) but failed to delete node {}:{}: {}",
            cleared_entries, edge_count, coll, id, e
        );
    }

    format!(
        "Removed node {}:{} (\"{}\"), {} referencing edge(s), cleared promoted_to on {} source entry(ies)",
        coll, id, preview, edge_count, cleared_entries
    )
}

/// `[TOOL:knowledge_update <collection>:<id> | <json_patch>]` — update a semantic
/// or procedural node in place while preserving every referencing edge.
///
/// JSON patch is an object containing only the fields to change. Immutable fields
/// (`_id`, `source_entry_id`, `source_session`, `created_at`, `access_count`,
/// `last_accessed`, `updated_at`) are rejected. `updated_at` is auto-refreshed.
///
/// Edges referencing the node by id are preserved automatically — `memory.edges`
/// stores references by id, not content, so a PATCH on the node doc leaves every
/// edge intact.
///
/// Auto-derived edges (`tag_overlap`, `temporal`) are NOT re-derived. If a tag
/// change makes specific edges stale, follow up with `knowledge_unlink_edge`.
pub async fn knowledge_update(params: &str, db: &WardsonDbClient) -> String {
    let trimmed = params.trim();
    if trimmed.is_empty() {
        return "knowledge_update rejected (missing arguments). Usage: [TOOL:knowledge_update <collection>:<id> | <json_patch>]".into();
    }
    let Some((target, patch_str)) = trimmed.split_once('|') else {
        return "knowledge_update rejected (missing `|` separator). Usage: [TOOL:knowledge_update <collection>:<id> | <json_patch>]".into();
    };
    let Some((coll, id)) = target.trim().split_once(':') else {
        return "knowledge_update rejected (target must be <collection>:<id>)".into();
    };
    let coll = coll.trim();
    let id = id.trim();
    if id.is_empty() {
        return "knowledge_update rejected (missing id after `:`)".into();
    }

    if coll != "memory.semantic" && coll != "memory.procedural" {
        return format!(
            "knowledge_update rejected (collection '{}' not supported — only memory.semantic or memory.procedural). Use [TOOL:forget] + [TOOL:remember] for memory.entries.",
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

    const IMMUTABLE: &[&str] = &[
        "_id",
        "source_entry_id",
        "source_session",
        "created_at",
        "access_count",
        "last_accessed",
        "updated_at",
    ];
    for field in IMMUTABLE {
        if obj.contains_key(*field) {
            return format!(
                "knowledge_update rejected (field '{}' is immutable)",
                field
            );
        }
    }

    let existing = match db.read(coll, id).await {
        Ok(doc) => doc,
        Err(_) => return format!("knowledge_update rejected (node {}:{} not found)", coll, id),
    };

    let changed_fields: Vec<String> = obj.keys().cloned().collect();
    obj.insert(
        "updated_at".to_string(),
        json!(chrono::Utc::now().to_rfc3339()),
    );

    if let Err(e) = db.patch_document(coll, id, &patch).await {
        return format!("knowledge_update failed: {}", e);
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

/// `[TOOL:knowledge_traverse <collection>:<id> [depth] [edge_types] [min_weight]]`
pub async fn knowledge_traverse(
    params: &str,
    db: &WardsonDbClient,
    config: &SystemConfig,
) -> String {
    // Tokenize by whitespace, first token = collection:id
    let mut toks = params.split_whitespace();
    let Some(start) = toks.next() else {
        return "Error: usage [TOOL:knowledge_traverse <collection>:<id> [depth] [edge_types] [min_weight]]".into();
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

    let edge_types: Option<Vec<EdgeType>> = toks.next().and_then(|s| {
        let parsed: Vec<EdgeType> = s.split(',')
            .filter_map(EdgeType::from_str)
            .collect();
        if parsed.is_empty() { None } else { Some(parsed) }
    });

    let min_weight: Option<f64> = toks.next().and_then(|s| s.parse().ok());

    let result = match traverse(db, start_id, start_coll, depth, edge_types, min_weight, config).await {
        Ok(r) => r,
        Err(e) => return format!("Error: traversal failed: {}", e),
    };

    // Format output
    let mut out = String::new();
    out.push_str(&format!("Traversal from {}:{}\n", start_coll, start_id));
    if let Some(start_node) = result.nodes.iter().find(|n| n.depth == 0) {
        let kind = match &start_node.node_type {
            NodeType::Episodic => "episodic".to_string(),
            NodeType::Semantic { category } => format!("semantic/{}", category.as_str()),
            NodeType::Procedural { title } => format!("procedural: {}", title),
        };
        out.push_str(&format!("Starting node: \"{}\" ({})\n\n", start_node.content_preview, kind));
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
                "  → {}:{} via {} (w={:.2})\n    \"{}\"\n",
                node.collection, node.id, etype, w, node.content_preview
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
    let _ = max_seen;
    out
}

/// `[TOOL:knowledge_query <query_text> [| <max_results> [| <categories_csv>]]]`
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
        return "Error: usage [TOOL:knowledge_query <query_text> [| <max_results> [| <categories_csv>]]]".into();
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

    // Derive tags from query text as naive space-split words
    let query_tags: Vec<String> = query_text
        .split_whitespace()
        .map(|s| s.trim_start_matches('#').to_lowercase())
        .filter(|s| !s.is_empty() && s.len() > 2)
        .collect();

    let mut results = match retrieve_relevant_knowledge(
        db, session_name, &query_tags, query_text, retrieve_n, config
    ).await {
        Ok(r) => r,
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

    // Count by source
    let mut direct = 0usize;
    let mut session = 0usize;
    let mut graph = 0usize;
    for r in &results {
        match r.source.as_str() {
            "direct_query" => direct += 1,
            "session_based" => session += 1,
            "graph_expansion" => graph += 1,
            _ => graph += 1,
        }
    }

    if results.is_empty() {
        return format!("Knowledge query: \"{}\" (0 results)", query_text);
    }

    let mut out = format!(
        "Knowledge query: \"{}\" ({} results — direct: {}, session: {}, graph: {})\n",
        query_text, results.len(), direct, session, graph
    );
    if direct == 0 {
        out.push_str("[No direct matches — showing graph-expanded results]\n");
    }
    out.push('\n');
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!(
            "{}. [{}] {} (score: {:.2})\n   Source: {}\n\n",
            i + 1, r.node.collection, r.node.content_preview, r.score, r.source
        ));
    }
    out
}

/// `[TOOL:knowledge_graph_stats]`
pub async fn knowledge_graph_stats(db: &WardsonDbClient) -> String {
    let mut out = String::from("Knowledge Graph Statistics:\n\n");

    // Semantic counts by category
    let sem_all = db.query("memory.semantic", &json!({ "filter": {}, "limit": 10000 })).await.unwrap_or_default();
    let mut cat_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for doc in &sem_all {
        if let Some(c) = doc.get("category").and_then(|v| v.as_str()) {
            *cat_counts.entry(c.to_string()).or_insert(0) += 1;
        }
    }
    out.push_str(&format!("memory.semantic: {} nodes\n", sem_all.len()));
    if !cat_counts.is_empty() {
        let cats: Vec<String> = ["fact", "preference", "decision", "observation", "pattern"]
            .iter()
            .map(|k| format!("{}={}", k, cat_counts.get(*k).copied().unwrap_or(0)))
            .collect();
        out.push_str(&format!("  Categories: {}\n", cats.join(", ")));
    }
    out.push('\n');

    // Procedural count
    let proc_all = db.query("memory.procedural", &json!({ "filter": {}, "limit": 10000 })).await.unwrap_or_default();
    out.push_str(&format!("memory.procedural: {} nodes\n\n", proc_all.len()));

    // Edge counts by type
    let edges_all = db.query("memory.edges", &json!({ "filter": {}, "limit": 100000 })).await.unwrap_or_default();
    let mut etype_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for doc in &edges_all {
        if let Some(t) = doc.get("edge_type").and_then(|v| v.as_str()) {
            *etype_counts.entry(t.to_string()).or_insert(0) += 1;
        }
    }
    out.push_str(&format!("memory.edges: {} edges\n", edges_all.len()));
    if !etype_counts.is_empty() {
        let mut pairs: Vec<(String, usize)> = etype_counts.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1));
        let summary: Vec<String> = pairs.iter().map(|(t, c)| format!("{}={}", t, c)).collect();
        out.push_str(&format!("  Types: {}\n", summary.join(", ")));
    }
    out.push('\n');

    // Entry promotion stats
    let entries_all = db.query("memory.entries", &json!({ "filter": {}, "limit": 10000 })).await.unwrap_or_default();
    let promoted: usize = entries_all.iter().filter(|d| {
        d.get("promoted_to").map(|v| !v.is_null()).unwrap_or(false)
    }).count();
    let total = entries_all.len();
    out.push_str(&format!(
        "memory.entries: {} total, {} promoted, {} unpromoted\n\n",
        total, promoted, total.saturating_sub(promoted)
    ));

    // Graph density (rough)
    let node_total = sem_all.len() + proc_all.len() + total;
    if node_total > 0 {
        let density = edges_all.len() as f64 / node_total as f64;
        out.push_str(&format!("Graph density: {:.1} edges/node", density));
    }

    out
}
