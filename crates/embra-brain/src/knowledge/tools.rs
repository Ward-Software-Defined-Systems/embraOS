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

/// `knowledge_link <source_coll>:<source_id> | <edge_type> | <target_coll>:<target_id> | <weight>`
pub async fn knowledge_link(params: &str, db: &WardsonDbClient) -> String {
    let parts: Vec<&str> = params.splitn(4, '|').map(|s| s.trim()).collect();
    if parts.len() < 4 {
        return "Error: usage knowledge_link <source_coll>:<source_id> | <edge_type> | <target_coll>:<target_id> | <weight>".into();
    }
    let Some((src_coll, src_id)) = parts[0].split_once(':') else {
        return "Error: source must be <collection>:<id>".into();
    };
    let Some(edge_type) = EdgeType::from_str(parts[1]) else {
        return format!("Error: Invalid edge type '{}'. Brain-created types: enables, contradicts, refines, depends_on, related_to", parts[1]);
    };
    if !edge_type.is_brain_created() {
        return format!("Error: Invalid edge type '{}'. Brain-created types: enables, contradicts, refines, depends_on, related_to", parts[1]);
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

/// `knowledge_unlink_edge <edge_id>` — delete a single edge by ID
/// `knowledge_unlink_edge <src_coll>:<src_id> | <edge_type> | <tgt_coll>:<tgt_id>` —
/// delete matching edges. Symmetric edge types
/// (`same_session`/`temporal`/`tag_overlap`/`related_to`) are removed
/// bidirectionally; directional types (`enables`/`contradicts`/`refines`/
/// `depends_on`/`derived_from`) only remove the forward direction.
pub async fn knowledge_unlink_edge(params: &str, db: &WardsonDbClient) -> String {
    let trimmed = params.trim();
    if trimmed.is_empty() {
        return "Error: usage knowledge_unlink_edge <edge_id> or knowledge_unlink_edge <src_coll>:<src_id> | <edge_type> | <tgt_coll>:<tgt_id>".into();
    }

    if trimmed.contains('|') {
        // Form 2: triple parse. Bidirectional only for symmetric edge
        // types (Embra_Debug #63 — was unconditionally bidirectional).
        let parts: Vec<&str> = trimmed.splitn(3, '|').map(|s| s.trim()).collect();
        if parts.len() < 3 {
            return "Error: usage knowledge_unlink_edge <src_coll>:<src_id> | <edge_type> | <tgt_coll>:<tgt_id>".into();
        }
        let Some((src_coll, src_id)) = parts[0].split_once(':') else {
            return "Error: source must be <collection>:<id>".into();
        };
        let Some(edge_type) = EdgeType::from_str(parts[1]) else {
            return format!("Error: Invalid edge type '{}'. Valid types: same_session, temporal, tag_overlap, derived_from, enables, contradicts, refines, depends_on, related_to", parts[1]);
        };
        let Some((tgt_coll, tgt_id)) = parts[2].split_once(':') else {
            return "Error: target must be <collection>:<id>".into();
        };

        let etype = edge_type.as_str();
        let symmetric = edge_type.is_symmetric();
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
pub async fn knowledge_unlink_node(params: &str, db: &WardsonDbClient) -> String {
    let trimmed = params.trim();
    if trimmed.is_empty() {
        return "Error: usage knowledge_unlink_node <collection>:<id>".into();
    }
    let Some((coll, id)) = trimmed.split_once(':') else {
        return "Error: target must be <collection>:<id>".into();
    };
    let coll = coll.trim();
    let id = id.trim();
    if coll != "memory.semantic" && coll != "memory.procedural" {
        return format!(
            "Error: knowledge_unlink_node only operates on memory.semantic or memory.procedural (got '{}'). Use forget for memory.entries.",
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

/// `knowledge_update <collection>:<id> | <json_patch>` — update a semantic
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

    if coll != "memory.semantic" && coll != "memory.procedural" {
        return format!(
            "knowledge_update rejected (collection '{}' not supported — only memory.semantic or memory.procedural). Use forget + remember for memory.entries.",
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
    if result.truncated {
        out.push_str("\n[!] traversal truncated: node budget reached (kg_traversal_node_budget)");
    }
    let _ = max_seen;
    out
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

/// `knowledge_graph_stats` — windowless. Totals come from server-side
/// `count_only` and distributions from aggregate `$group`, so the report is
/// exact at ANY collection size. (The old version pulled every document
/// through fixed 10k/100k windows — ~91k full edge docs per call at
/// production scale — and went silently partial once a collection outgrew
/// its window.)
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
    out.push('\n');

    // Procedural count
    let proc_total = db.count("memory.procedural").await.unwrap_or(0);
    out.push_str(&format!("memory.procedural: {} nodes\n\n", proc_total));

    // Edge count + type distribution
    let edge_total = db.count("memory.edges").await.unwrap_or(0);
    out.push_str(&format!("memory.edges: {} edges\n", edge_total));
    let etype_counts = group_counts(
        &db.aggregate("memory.edges", &group_by_pipeline("edge_type")).await.unwrap_or_default(),
    );
    if !etype_counts.is_empty() {
        let mut pairs: Vec<(String, u64)> = etype_counts.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let summary: Vec<String> = pairs.iter().map(|(t, c)| format!("{}={}", t, c)).collect();
        out.push_str(&format!("  Types: {}\n", summary.join(", ")));
    }
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

    // Graph density (rough)
    let node_total = sem_total + proc_total + entries_total;
    if node_total > 0 {
        let density = edge_total as f64 / node_total as f64;
        out.push_str(&format!("Graph density: {:.1} edges/node\n", density));
    }

    // Orphan edges (endpoints that don't resolve) — surfaces the #40 issue
    // passively so users don't have to call knowledge_sweep_orphans to know.
    // Bounded work per stats call; coverage reported honestly against the
    // exact edge total now that we have one.
    let (scanned, orphans) = find_orphan_edges(db, PASSIVE_ORPHAN_SCAN_LIMIT).await;
    if scanned > 0 {
        let coverage = if (scanned as u64) < edge_total {
            format!(
                " (of {} total — raise knowledge_sweep_orphans limit for full coverage)",
                edge_total
            )
        } else {
            String::new()
        };
        out.push_str(&format!(
            "Orphan edges: {} of {} scanned{}{}",
            orphans.len(),
            scanned,
            coverage,
            if !orphans.is_empty() {
                " (run knowledge_sweep_orphans to clean up)"
            } else {
                ""
            }
        ));
    }

    out
}

/// Edges scanned by graph_stats' passive orphan check — bounds the work a
/// stats call does; the sweep tool's own `limit` goes higher for full runs.
const PASSIVE_ORPHAN_SCAN_LIMIT: usize = 100_000;

/// Aggregate pipeline: count documents grouped by `field`. Output row count
/// is bounded by the field's cardinality (edge_type ≤ 9, category = 5), so
/// no result window is needed — the scan itself runs server-side.
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

#[cfg(test)]
mod windowless_stats_tests {
    //! Shape guards for the windowless graph_stats / paginated orphan scan
    //! (no DB mock in this crate — contracts are enforced at the builder
    //! level, same pattern as the FIX-1..8 query-body tests).
    use super::{group_by_pipeline, group_counts, orphan_page_query_body, promoted_entries_filter};
    use serde_json::json;

    #[test]
    fn group_pipeline_counts_by_field() {
        let p = group_by_pipeline("edge_type");
        assert_eq!(
            p,
            json!([{ "$group": { "_id": "edge_type", "count": { "$count": {} } } }])
        );
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
    fn orphan_page_body_paginates_key_order_no_sort() {
        let body = orphan_page_query_body(20_000, 40_000);
        assert_eq!(body["limit"], json!(20_000));
        assert_eq!(body["offset"], json!(40_000));
        // Deliberately unsorted (doctrine exception): exhaustive pagination
        // rides stable key order; a sort would re-sort the whole collection
        // per page.
        assert!(body.get("sort").is_none());
    }
}

/// Page size for the exhaustive edge scan — well under WardSONDB's
/// `--max-query-limit` (100k) and bounds per-page memory.
const ORPHAN_SCAN_PAGE: usize = 20_000;

/// One page of the exhaustive edge scan. Deliberately UNSORTED — a doctrine
/// exception: offset pagination rides the stable UUIDv7 key order (the
/// storage scan order) as its cursor. This is a full-coverage maintenance
/// sweep, not a relevance window (the invariant's target), and adding a
/// sort would re-sort the whole matched set on every page.
fn orphan_page_query_body(page_limit: usize, offset: usize) -> serde_json::Value {
    json!({ "filter": {}, "limit": page_limit, "offset": offset })
}

/// Scan up to `limit` edges and return `(scanned, orphan_edge_ids)` where
/// orphans are edges whose source or target doc fails to resolve.
/// Paginated (pages of `ORPHAN_SCAN_PAGE`), so coverage is bounded only by
/// `limit` — not by any single query window; the old single-query version
/// went silently partial past the server's 100k max-query-limit.
async fn find_orphan_edges(db: &WardsonDbClient, limit: usize) -> (usize, Vec<String>) {
    let mut scanned = 0usize;
    let mut offset = 0usize;
    let mut orphan_ids: Vec<String> = Vec::new();

    while scanned < limit {
        let page_limit = ORPHAN_SCAN_PAGE.min(limit - scanned);
        let edges = db
            .query("memory.edges", &orphan_page_query_body(page_limit, offset))
            .await
            .unwrap_or_default();
        if edges.is_empty() {
            break;
        }
        scanned += edges.len();
        offset += edges.len();
        orphan_ids.extend(orphans_in_page(db, &edges).await);
        if edges.len() < page_limit {
            break; // final partial page — collection exhausted
        }
    }

    (scanned, orphan_ids)
}

/// Orphan detection over one page of edges. Batches endpoint reads per
/// collection via `{"_id": {"$in": [...]}}` so each page costs at most one
/// extra query per endpoint collection.
async fn orphans_in_page(db: &WardsonDbClient, edges: &[serde_json::Value]) -> Vec<String> {
    use std::collections::{HashMap, HashSet};

    if edges.is_empty() {
        return Vec::new();
    }

    // Collect unique (collection, id) endpoints per collection.
    let mut endpoints: HashMap<String, HashSet<String>> = HashMap::new();
    for edge in edges {
        for (coll_key, id_key) in [
            ("source_collection", "source_id"),
            ("target_collection", "target_id"),
        ] {
            let (Some(coll), Some(id)) = (
                edge.get(coll_key).and_then(|v| v.as_str()),
                edge.get(id_key).and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            endpoints
                .entry(coll.to_string())
                .or_default()
                .insert(id.to_string());
        }
    }

    // For each collection, batch-resolve and set-diff to find missing ids.
    let mut missing: HashMap<String, HashSet<String>> = HashMap::new();
    for (coll, ids) in &endpoints {
        if ids.is_empty() {
            continue;
        }
        let id_list: Vec<String> = ids.iter().cloned().collect();
        let filter = json!({
            "filter": {"_id": {"$in": id_list}},
            "limit": ids.len(),
        });
        let found = db.query(coll, &filter).await.unwrap_or_default();
        let found_ids: HashSet<String> = found
            .iter()
            .filter_map(|d| d.get("_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();
        let missing_ids: HashSet<String> = ids.difference(&found_ids).cloned().collect();
        if !missing_ids.is_empty() {
            missing.insert(coll.clone(), missing_ids);
        }
    }

    // Identify edges whose source or target is missing.
    let mut orphan_ids: Vec<String> = Vec::new();
    for edge in edges {
        let src_missing = match (
            edge.get("source_collection").and_then(|v| v.as_str()),
            edge.get("source_id").and_then(|v| v.as_str()),
        ) {
            (Some(c), Some(i)) => missing.get(c).map(|s| s.contains(i)).unwrap_or(false),
            _ => false,
        };
        let tgt_missing = match (
            edge.get("target_collection").and_then(|v| v.as_str()),
            edge.get("target_id").and_then(|v| v.as_str()),
        ) {
            (Some(c), Some(i)) => missing.get(c).map(|s| s.contains(i)).unwrap_or(false),
            _ => false,
        };
        if (src_missing || tgt_missing)
            && let Some(eid) = edge.get("_id").and_then(|v| v.as_str())
        {
            orphan_ids.push(eid.to_string());
        }
    }

    orphan_ids
}

/// Scan `memory.edges` for edges whose endpoints no longer resolve. Used to
/// clean up residue from historical `forget` calls (pre-cascade fix) or from
/// any direct-delete that bypassed `knowledge_unlink_node`.
pub async fn knowledge_sweep_orphans(
    db: &WardsonDbClient,
    dry_run: bool,
    limit: usize,
) -> String {
    // The scan is paginated, so the ceiling is a work bound (the 600s global
    // tool cap is the real backstop), not a query-window limit.
    let limit = limit.clamp(1, 1_000_000);
    let (scanned, orphans) = find_orphan_edges(db, limit).await;
    let orphan_count = orphans.len();

    if dry_run {
        return format!(
            "knowledge_sweep_orphans (dry_run):\n  scanned: {}\n  orphan_count: {}\n  deleted: 0",
            scanned, orphan_count
        );
    }

    let mut deleted: u64 = 0;
    for chunk in orphans.chunks(100) {
        let id_list: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
        let filter = json!({"_id": {"$in": id_list}});
        if let Ok(n) = db.delete_by_query("memory.edges", &filter).await {
            deleted += n;
        }
    }

    format!(
        "knowledge_sweep_orphans:\n  scanned: {}\n  orphan_count: {}\n  deleted: {}",
        scanned, orphan_count, deleted
    )
}

// ── knowledge_dump — JSONL export of the knowledge graph ──

/// Dump directory. Inside the tool-layer workspace jail (`/embra/workspace`,
/// bind-mounted from DATA at boot) but outside `repos/` — dumps are exports,
/// not repository content. Full-path const per repo precedent; do not widen
/// engineering's `WORKSPACE_ROOT` visibility for this.
const KG_DUMPS_DIR: &str = "/embra/workspace/KG_DUMPS";

/// Page size for the exhaustive dump scan — same bound rationale as
/// `ORPHAN_SCAN_PAGE` (well under the server's 100k `--max-query-limit`,
/// bounds per-page memory).
const DUMP_SCAN_PAGE: usize = 20_000;

/// The dumpable collections as `(short_name, wardsondb_collection)` in
/// canonical dump order: nodes first (entries, semantic, procedural), then
/// edges. Short names are the tool-facing vocabulary.
const DUMP_COLLECTIONS: &[(&str, &str)] = &[
    ("entries", "memory.entries"),
    ("semantic", "memory.semantic"),
    ("procedural", "memory.procedural"),
    ("edges", "memory.edges"),
];

/// One page of the exhaustive dump scan. Deliberately UNSORTED — the same
/// doctrine exception as `orphan_page_query_body`: offset pagination rides
/// the stable UUIDv7 key order as its cursor (exhaustive coverage, not a
/// relevance window). WardSONDB applies `offset`/`limit` after the filter in
/// every executor path, so a constant filter tiles the matched set without
/// skips or duplicates.
fn dump_page_query_body(
    filter: &serde_json::Value,
    page_limit: usize,
    offset: usize,
) -> serde_json::Value {
    json!({ "filter": filter, "limit": page_limit, "offset": offset })
}

/// Edge-type restriction for the edges pass.
fn dump_edge_type_filter(edge_types: &[String]) -> serde_json::Value {
    json!({ "edge_type": { "$in": edge_types } })
}

/// Map tool-facing short names to `memory.*` collections in canonical dump
/// order regardless of input order. `None` selects all four; unknown names
/// and an empty selection are errors.
fn resolve_dump_collections(requested: Option<&[String]>) -> Result<Vec<&'static str>, String> {
    let Some(requested) = requested else {
        return Ok(DUMP_COLLECTIONS.iter().map(|(_, full)| *full).collect());
    };
    if requested.is_empty() {
        return Err(
            "collections is empty — omit it for a full dump, or pick from: entries, semantic, procedural, edges"
                .into(),
        );
    }
    for name in requested {
        if !DUMP_COLLECTIONS
            .iter()
            .any(|(short, _)| *short == name.as_str())
        {
            return Err(format!(
                "Unknown collection '{}'. Valid: entries, semantic, procedural, edges",
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

/// Reject unknown edge types up front (all 9 stored types are dumpable —
/// auto-derived and brain-created alike); an empty list is an error.
fn validate_dump_edge_types(edge_types: &[String]) -> Result<(), String> {
    if edge_types.is_empty() {
        return Err("edge_types is empty — omit it to dump all edge types".into());
    }
    for t in edge_types {
        if EdgeType::from_str(t).is_none() {
            return Err(format!(
                "Unknown edge type '{}'. Valid: same_session, temporal, tag_overlap, derived_from, enables, contradicts, refines, depends_on, related_to",
                t
            ));
        }
    }
    Ok(())
}

/// First line of every dump — provenance for the file itself. Consumers
/// keyed on `type` (e.g. the kg_scan guardian example) skip it as an
/// unknown record type.
fn dump_meta_record(
    collections: &[&'static str],
    edge_types: Option<&[String]>,
    include_payload: bool,
) -> serde_json::Value {
    json!({
        "type": "meta",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "collections": collections,
        "edge_types": edge_types,
        "include_payload": include_payload,
    })
}

/// A node line: `type`/`_id`/`collection` lifted top-level for scanners; the
/// full stored doc rides under `data` unless the dump is slim.
fn dump_node_record(
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
fn dump_edge_record(doc: &serde_json::Value) -> serde_json::Value {
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
        dump_meta_record(selected, edge_types, include_payload)
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
        let mut offset = 0usize;
        loop {
            let docs = db
                .query(coll, &dump_page_query_body(&filter, DUMP_SCAN_PAGE, offset))
                .await
                .map_err(|e| format!("query {} failed at offset {}: {}", coll, offset, e))?;
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
            offset += page_len;
            if page_len < DUMP_SCAN_PAGE {
                break; // final partial page — collection exhausted
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
        dump_page_query_body, resolve_dump_collections, validate_dump_edge_types,
    };
    use serde_json::json;

    #[test]
    fn dump_page_body_paginates_key_order_no_sort() {
        // Same doctrine exception as the orphan scan: exhaustive pagination
        // rides stable key order; a sort key must never appear.
        let empty = json!({});
        let body = dump_page_query_body(&empty, 20_000, 40_000);
        assert_eq!(body["limit"], json!(20_000));
        assert_eq!(body["offset"], json!(40_000));
        assert_eq!(body["filter"], json!({}));
        assert!(body.get("sort").is_none());

        let filtered = dump_edge_type_filter(&["enables".to_string()]);
        let body = dump_page_query_body(&filtered, 20_000, 0);
        assert_eq!(body["filter"], json!({"edge_type": {"$in": ["enables"]}}));
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
                "memory.edges"
            ]
        );
        // Selections are re-ordered canonically (nodes before edges), not
        // echoed in input order.
        let sel = vec!["edges".to_string(), "semantic".to_string()];
        let some = resolve_dump_collections(Some(&sel)).unwrap();
        assert_eq!(some, vec!["memory.semantic", "memory.edges"]);
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
    fn dump_edge_types_reject_unknown() {
        let bad = vec!["enables".to_string(), "nope".to_string()];
        let err = validate_dump_edge_types(&bad).unwrap_err();
        assert!(err.contains("related_to"), "should list valid types: {err}");
        let good = vec!["same_session".to_string(), "depends_on".to_string()];
        assert!(validate_dump_edge_types(&good).is_ok());
        assert!(validate_dump_edge_types(&[]).is_err());
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
        let m = dump_meta_record(&["memory.edges"], None, true);
        assert_eq!(m["type"], "meta");
        assert!(m["generated_at"].as_str().is_some());
        assert_eq!(m["collections"], json!(["memory.edges"]));
        assert!(m["edge_types"].is_null());
        assert_eq!(m["include_payload"], json!(true));

        let types = vec!["enables".to_string()];
        let m2 = dump_meta_record(&["memory.edges"], Some(&types), false);
        assert_eq!(m2["edge_types"], json!(["enables"]));
        assert_eq!(m2["include_payload"], json!(false));
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
    description = "Promote an episodic memory entry to a semantic or procedural knowledge node. For kind=semantic, data is one of: fact, preference, decision, observation, pattern. For kind=procedural, data is a JSON object describing the procedure (preconditions, steps, outcomes). Promote only durable, reusable knowledge worth keeping across sessions — not every memory."
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
    description = "Create a directed, weighted, typed edge between two knowledge nodes. edge_type: enables | contradicts | refines | depends_on | related_to. weight is 0.0-1.0 indicating confidence. Use enables when the source is a prerequisite for the target; contradicts when they conflict; refines when the source is more specific than the target; depends_on when the source requires the target to hold; related_to for same-scope, non-hierarchical association."
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
    description = "Delete edges. Provide either edge_id (removes one edge by its document id) OR the full triple — source_collection + source_id + edge_type + target_collection + target_id. Symmetric edge types (same_session, temporal, tag_overlap, related_to) are removed bidirectionally; directional types (enables, contradicts, refines, depends_on, derived_from) remove only the forward direction. edge_id takes precedence when both are provided."
)]
pub struct KnowledgeUnlinkEdgeArgs {
    /// Specific edge document id. When set, the triple fields are ignored.
    #[serde(default)]
    pub edge_id: Option<String>,
    /// Triple form: source collection. Required together with the other
    /// four triple fields when edge_id is absent.
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
        // "<src_coll>:<src_id> | <edge_type> | <tgt_coll>:<tgt_id>".
        // Reconstruct whichever form the caller provided.
        let param = if let Some(eid) = self.edge_id.filter(|s| !s.is_empty()) {
            eid
        } else {
            match (
                self.source_collection.as_deref(),
                self.source_id.as_deref(),
                self.edge_type.as_deref(),
                self.target_collection.as_deref(),
                self.target_id.as_deref(),
            ) {
                (Some(sc), Some(si), Some(et), Some(tc), Some(ti))
                    if !sc.is_empty()
                        && !si.is_empty()
                        && !et.is_empty()
                        && !tc.is_empty()
                        && !ti.is_empty() =>
                {
                    format!("{}:{} | {} | {}:{}", sc, si, et, tc, ti)
                }
                _ => {
                    return Ok(
                        "knowledge_unlink_edge rejected (missing arguments). Provide edge_id OR the full triple: source_collection, source_id, edge_type, target_collection, target_id."
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
    description = "Delete a semantic or procedural node and cascade-remove all edges referencing it. Prefer this over manually deleting edges when the node itself should go. For episodic memory entries, use forget instead."
)]
pub struct KnowledgeUnlinkNodeArgs {
    pub collection: String,
    pub id: String,
}

impl KnowledgeUnlinkNodeArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{}:{}", self.collection, self.id);
        Ok(knowledge_unlink_node(&param, ctx.db).await)
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
        Ok(knowledge_update(&param, ctx.db).await)
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
    /// derived_from, enables, contradicts, refines, depends_on, related_to).
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
    description = "Find relevant knowledge via graph-aware retrieval (multi-signal ranking with depth-2 expansion). max_results defaults to 20, capped at 100. categories is a CSV of semantic categories to filter by (fact, preference, decision, observation, pattern). Call this before answering questions where prior context would help."
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
    description = "Return summary statistics of the knowledge graph: node counts by collection, edge counts by type, total density, and orphan-edge count."
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
    description = "Scan memory.edges and remove edges whose source or target doc no longer resolves. Orphans accumulate from pre-cascade forget calls and direct deletes that bypassed knowledge_unlink_node. Use dry_run=true to preview without deleting."
)]
pub struct KnowledgeSweepOrphansArgs {
    /// Preview orphan edges without deleting.
    #[serde(default)]
    pub dry_run: bool,
    /// Cap edges scanned per invocation; clamped to [1, 1000000]. The scan
    /// is paginated, so full-graph coverage just needs limit >= the edge
    /// total reported by knowledge_graph_stats.
    #[serde(default = "default_sweep_limit")]
    pub limit: usize,
}

fn default_sweep_limit() -> usize {
    10_000
}

impl KnowledgeSweepOrphansArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(knowledge_sweep_orphans(ctx.db, self.dry_run, self.limit).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_dump",
    is_side_effectful = true,
    description = "Export the knowledge graph as a JSONL dump file under /embra/workspace/KG_DUMPS (first line: meta header; then node lines from memory.entries/semantic/procedural and edge lines from memory.edges). collections optionally restricts to: entries, semantic, procedural, edges (default all four). edge_types optionally filters the edge pass by type. include_payload=false omits node payloads — slim dumps for structural scanning, sized for guardian_call's 2 MiB data_file bridge. Returns the file path plus per-collection written-vs-counted parity, byte total, and elapsed time."
)]
pub struct KnowledgeDumpArgs {
    /// Short collection names: entries | semantic | procedural | edges.
    /// Omit for all four.
    #[serde(default)]
    pub collections: Option<Vec<String>>,
    /// Edge types to include (same_session, temporal, tag_overlap,
    /// derived_from, enables, contradicts, refines, depends_on, related_to).
    /// Omit for all. Requires 'edges' among the dumped collections.
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
            .into_iter()
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
        // Cron fires registry tools with json!({}), so the defaults ARE the
        // cron behavior: full dump, all collections, all edge types.
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
