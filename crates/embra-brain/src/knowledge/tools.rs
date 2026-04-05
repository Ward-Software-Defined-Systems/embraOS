//! Knowledge graph tool implementations (5 tools).
//!
//! Tag syntax uses `|` as a delimiter between compound parameters.

use serde_json::json;

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::promotion::{promote_to_procedural, promote_to_semantic};
use super::retrieval::retrieve_relevant_knowledge;
use super::traversal::traverse;
use super::types::{EdgeType, NodeType, SemanticCategory};

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
    let weight: f64 = match parts[3].parse() {
        Ok(w) => w,
        Err(_) => return "Error: Weight must be between 0.0 and 1.0".into(),
    };
    if !(0.0..=1.0).contains(&weight) {
        return "Error: Weight must be between 0.0 and 1.0".into();
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

/// `[TOOL:knowledge_query <query_text> [max_results] [categories]]`
pub async fn knowledge_query(
    params: &str,
    db: &WardsonDbClient,
    session_name: &str,
    config: &SystemConfig,
) -> String {
    // Parameters are naive whitespace-split; the query text can be the full string
    // if no flags are passed, otherwise we look for trailing integer + category CSV.
    // Simple approach: take full params as query_text; no trailing flags for now.
    // Users wanting max_results/categories can use structured syntax later.
    let query_text = params.trim();
    if query_text.is_empty() {
        return "Error: usage [TOOL:knowledge_query <query_text>]".into();
    }
    let max_results = 20usize;

    // Derive tags from query text as naive space-split words
    let query_tags: Vec<String> = query_text
        .split_whitespace()
        .map(|s| s.trim_start_matches('#').to_lowercase())
        .filter(|s| !s.is_empty() && s.len() > 2)
        .collect();

    let results = match retrieve_relevant_knowledge(
        db, session_name, &query_tags, query_text, max_results, config
    ).await {
        Ok(r) => r,
        Err(e) => return format!("Error: retrieval failed: {}", e),
    };

    if results.is_empty() {
        return format!("Knowledge query: \"{}\" (0 results)", query_text);
    }

    let mut out = format!("Knowledge query: \"{}\" ({} results)\n\n", query_text, results.len());
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
