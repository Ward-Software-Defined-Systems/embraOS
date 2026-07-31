//! Seed knowledge packs (`knowledge.v1`) — boot-loaded baseline knowledge.
//!
//! Curated packs of semantic/procedural nodes + edges reconciled into the
//! LIVE knowledge collections on every boot, the identity-projection
//! pattern applied to knowledge instead of identity: the committed default
//! pack teaches an instance how its own memory works (KNOWLEDGE-GRAPH.md
//! distilled), and operators can drop their own packs into STATE.
//!
//! ENSURE-PRESENT semantics (locked 2026-07-31, William's pick over a
//! version ledger): presence is checked by `_id` ONLY —
//! - `knowledge_update` edits STICK (docs are never patched by the
//!   reconcile);
//! - deleting/merging-away a pack-listed node RESURRECTS it next boot
//!   (revise the pack instead);
//! - pack content revisions ship as NEW node ids — dropping the old id
//!   from the pack un-lists it, so a subsequent operator deletion sticks.
//!
//! Seed nodes are ORDINARY `memory.semantic`/`memory.procedural` citizens:
//! retrieval, enrichment, traversal, audit, merge, and update all treat
//! them like promoted knowledge. They NEVER touch `identity.graph` (its
//! reconcile counts that collection unfiltered — a foreign writer would
//! break its sufficiency heuristic).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use serde_json::json;
use tracing::{info, warn};

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::edges::derive_edges;
use super::types::SemanticCategory;
use crate::identity_graph::format::id_violation;

/// Read-only packs baked into the rootfs by `post_build.sh` from the
/// committed `Seed_Knowledge/` directory.
pub const ROOTFS_SEED_DIR: &str = "/usr/share/embra/seed-knowledge";
/// Operator drop-in directory on STATE — wins filename collisions with the
/// rootfs (same doctrine as the import dirs).
pub const STATE_SEED_DIR: &str = "/embra/state/seed-knowledge";
/// Dev override: when set (non-empty), the ONLY directory scanned.
pub const SEED_DIR_ENV: &str = "EMBRA_SEED_DIR";

pub(crate) const FORMAT_KNOWLEDGE_V1: &str = "knowledge.v1";
/// Top-level node `origin` field AND edge `metadata.origin` — the
/// provenance label `provenance_summary` counts (keeping the exact
/// brain-authored arithmetic exact).
pub(crate) const ORIGIN_SEED: &str = "knowledge_seed";
const SEED_FILE_SUFFIX: &str = ".knowledge.json";
/// Same cap as the identity-import scanner.
const SEED_FILE_MAX_BYTES: u64 = 4 * 1024 * 1024;

// ── pack model ───────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct SeedOutcomes {
    pub success: String,
    pub failure: String,
}

#[derive(Debug)]
pub(crate) enum SeedNodeKind {
    Semantic {
        category: SemanticCategory,
        content: String,
    },
    Procedural {
        title: String,
        description: String,
        steps: Vec<String>,
        outcomes: Option<SeedOutcomes>,
    },
}

#[derive(Debug)]
pub(crate) struct SeedNode {
    pub id: String,
    pub kind: SeedNodeKind,
    pub tags: Vec<String>,
}

impl SeedNode {
    /// Seed nodes live ONLY in the two promoted-knowledge collections —
    /// never `identity.graph` (structural: there is no arm returning it).
    pub(crate) fn collection(&self) -> &'static str {
        match self.kind {
            SeedNodeKind::Semantic { .. } => "memory.semantic",
            SeedNodeKind::Procedural { .. } => "memory.procedural",
        }
    }
}

#[derive(Debug)]
pub(crate) struct SeedEdge {
    pub src: String,
    pub dst: String,
    pub relation: String,
    pub weight: f64,
}

#[derive(Debug)]
pub(crate) struct SeedPack {
    pub name: String,
    #[allow(dead_code)] // authoring metadata; surfaced in logs/docs only
    pub description: Option<String>,
    pub nodes: Vec<SeedNode>,
    pub edges: Vec<SeedEdge>,
}

// ── parse + validate (collect-all errors, format.rs style) ──────────────

fn str_field(obj: &serde_json::Value, key: &str) -> Option<String> {
    obj.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// Parse and validate one pack. Errors are COLLECTED, never first-fail —
/// an author fixes everything in one round trip. Objects missing the key
/// field (`id` for nodes, `src` for edges) are treated as `_comment`
/// markers and skipped, mirroring the graph.v1 convention.
pub(crate) fn parse_pack(raw: &str) -> Result<SeedPack, Vec<String>> {
    let value: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return Err(vec![format!("not valid JSON: {e}")]),
    };
    let mut errors: Vec<String> = Vec::new();

    match value.get("format").and_then(|v| v.as_str()) {
        Some(FORMAT_KNOWLEDGE_V1) => {}
        Some(other) => errors.push(format!(
            "format is '{other}' — expected '{FORMAT_KNOWLEDGE_V1}'"
        )),
        None => errors.push(format!("missing 'format' (expected '{FORMAT_KNOWLEDGE_V1}')")),
    }
    let name = match value.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => {
            errors.push("missing or empty 'name'".to_string());
            String::new()
        }
    };
    let description = value
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut nodes: Vec<SeedNode> = Vec::new();
    match value.get("nodes").and_then(|v| v.as_array()) {
        None => errors.push("missing 'nodes' array".to_string()),
        Some(arr) => {
            for (i, raw_node) in arr.iter().enumerate() {
                // `_comment` convention: objects without an `id` are markers.
                let Some(id) = str_field(raw_node, "id") else { continue };
                let tags = match raw_node.get("tags").and_then(|v| v.as_array()) {
                    Some(t) if t.iter().all(|x| x.is_string()) => t
                        .iter()
                        .filter_map(|x| x.as_str())
                        .map(|s| s.to_string())
                        .collect(),
                    _ => {
                        errors.push(format!(
                            "nodes[{i}] ('{id}'): 'tags' must be an array of strings"
                        ));
                        Vec::new()
                    }
                };
                let kind = match raw_node.get("kind").and_then(|v| v.as_str()) {
                    Some("semantic") => {
                        let category = raw_node
                            .get("category")
                            .and_then(|v| v.as_str())
                            .and_then(SemanticCategory::from_str);
                        let content = str_field(raw_node, "content").unwrap_or_default();
                        match (category, content.trim().is_empty()) {
                            (Some(category), false) => {
                                Some(SeedNodeKind::Semantic { category, content })
                            }
                            (None, _) => {
                                errors.push(format!(
                                    "nodes[{i}] ('{id}'): semantic nodes need 'category' — one of fact, preference, decision, observation, pattern"
                                ));
                                None
                            }
                            (_, true) => {
                                errors.push(format!(
                                    "nodes[{i}] ('{id}'): semantic nodes need non-empty 'content'"
                                ));
                                None
                            }
                        }
                    }
                    Some("procedural") => {
                        let title = str_field(raw_node, "title").unwrap_or_default();
                        let description = str_field(raw_node, "description").unwrap_or_default();
                        if title.trim().is_empty() || description.trim().is_empty() {
                            errors.push(format!(
                                "nodes[{i}] ('{id}'): procedural nodes need non-empty 'title' and 'description'"
                            ));
                            None
                        } else {
                            let steps = match raw_node.get("steps") {
                                None => Vec::new(),
                                Some(s) => match s.as_array() {
                                    Some(a) if a.iter().all(|x| x.is_string()) => a
                                        .iter()
                                        .filter_map(|x| x.as_str())
                                        .map(|x| x.to_string())
                                        .collect(),
                                    _ => {
                                        errors.push(format!(
                                            "nodes[{i}] ('{id}'): 'steps' must be an array of strings"
                                        ));
                                        Vec::new()
                                    }
                                },
                            };
                            let outcomes = match raw_node.get("outcomes") {
                                None => None,
                                Some(o) => {
                                    match (str_field(o, "success"), str_field(o, "failure")) {
                                        (Some(success), Some(failure)) => {
                                            Some(SeedOutcomes { success, failure })
                                        }
                                        _ => {
                                            errors.push(format!(
                                                "nodes[{i}] ('{id}'): 'outcomes' needs BOTH 'success' and 'failure' strings"
                                            ));
                                            None
                                        }
                                    }
                                }
                            };
                            Some(SeedNodeKind::Procedural {
                                title,
                                description,
                                steps,
                                outcomes,
                            })
                        }
                    }
                    other => {
                        errors.push(format!(
                            "nodes[{i}] ('{id}'): 'kind' must be semantic or procedural (got {:?})",
                            other.unwrap_or("<missing>")
                        ));
                        None
                    }
                };
                if let Some(kind) = kind {
                    nodes.push(SeedNode { id, kind, tags });
                }
            }
        }
    }

    let mut edges: Vec<SeedEdge> = Vec::new();
    match value.get("edges").and_then(|v| v.as_array()) {
        None => errors.push("missing 'edges' array (use [] for none)".to_string()),
        Some(arr) => {
            for (i, raw_edge) in arr.iter().enumerate() {
                let Some(src) = str_field(raw_edge, "src") else { continue };
                let dst = str_field(raw_edge, "dst").unwrap_or_default();
                let relation = str_field(raw_edge, "relation").unwrap_or_default();
                if dst.is_empty() || relation.trim().is_empty() {
                    errors.push(format!(
                        "edges[{i}]: 'src', 'dst', and a non-empty 'relation' are required"
                    ));
                    continue;
                }
                let weight = match raw_edge.get("weight") {
                    None => 1.0,
                    Some(w) => match w.as_f64() {
                        Some(w) if w > 0.0 && w <= 1.0 => w,
                        _ => {
                            errors.push(format!(
                                "edges[{i}] ({src} -> {dst}): 'weight' must be a number in (0.0, 1.0]"
                            ));
                            continue;
                        }
                    },
                };
                edges.push(SeedEdge { src, dst, relation, weight });
            }
        }
    }

    let pack = SeedPack { name, description, nodes, edges };
    errors.extend(validate_pack(&pack));
    if errors.is_empty() { Ok(pack) } else { Err(errors) }
}

fn validate_pack(pack: &SeedPack) -> Vec<String> {
    let mut errors = Vec::new();
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for node in &pack.nodes {
        if let Some(v) = id_violation(&node.id) {
            errors.push(format!("node '{}': {v}", node.id));
        }
        if node.id.starts_with("user_") {
            errors.push(format!(
                "node '{}': the 'user_' id prefix is reserved for the operator profile",
                node.id
            ));
        }
        if !seen_ids.insert(node.id.as_str()) {
            errors.push(format!("duplicate node id '{}'", node.id));
        }
    }
    let mut seen_triples: HashSet<(&str, &str, &str)> = HashSet::new();
    for edge in &pack.edges {
        for end in [&edge.src, &edge.dst] {
            if !seen_ids.contains(end.as_str()) {
                errors.push(format!(
                    "edge {} -> {}: '{end}' is not a node id in this pack (edges reference in-pack ids only)",
                    edge.src, edge.dst
                ));
            }
        }
        if !seen_triples.insert((&edge.src, &edge.dst, &edge.relation)) {
            errors.push(format!(
                "duplicate edge ({} -> {} [{}])",
                edge.src, edge.dst, edge.relation
            ));
        }
    }
    errors
}

// ── directory scan (import_flow pattern) ─────────────────────────────────

pub(crate) struct SeedPackFile {
    pub file_name: String,
    pub pack: SeedPack,
}

/// Env override is EXCLUSIVE; otherwise rootfs first, STATE second — the
/// BTreeMap insert order below makes STATE win filename collisions.
fn dirs_to_scan(env_val: Option<&str>) -> Vec<PathBuf> {
    match env_val {
        Some(dir) if !dir.trim().is_empty() => vec![PathBuf::from(dir.trim())],
        _ => vec![PathBuf::from(ROOTFS_SEED_DIR), PathBuf::from(STATE_SEED_DIR)],
    }
}

/// Duplicate pack NAMES across distinct files would conflate the per-pack
/// count fast-paths — first file (BTreeMap filename order) wins.
fn dedupe_pack_names(files: Vec<SeedPackFile>) -> (Vec<SeedPackFile>, Vec<String>) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    let mut issues = Vec::new();
    for f in files {
        if seen.insert(f.pack.name.clone()) {
            out.push(f);
        } else {
            issues.push(format!(
                "{}: skipped — duplicate pack name '{}' (an earlier file already provides it)",
                f.file_name, f.pack.name
            ));
        }
    }
    (out, issues)
}

pub(crate) fn scan_seed_dirs() -> (Vec<SeedPackFile>, Vec<String>) {
    let env_val = std::env::var(SEED_DIR_ENV).ok();
    let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in dirs_to_scan(env_val.as_deref()) {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if !name.ends_with(SEED_FILE_SUFFIX) || !path.is_file() {
                continue;
            }
            by_name.insert(name.to_string(), path);
        }
    }

    let mut files = Vec::new();
    let mut issues = Vec::new();
    for (file_name, path) in by_name {
        match std::fs::metadata(&path) {
            Ok(m) if m.len() > SEED_FILE_MAX_BYTES => {
                issues.push(format!(
                    "{file_name}: skipped — {} bytes exceeds the {SEED_FILE_MAX_BYTES}-byte cap",
                    m.len()
                ));
                continue;
            }
            Err(e) => {
                issues.push(format!("{file_name}: unreadable — {e}"));
                continue;
            }
            _ => {}
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                issues.push(format!("{file_name}: unreadable — {e}"));
                continue;
            }
        };
        match parse_pack(&raw) {
            Ok(pack) => files.push(SeedPackFile { file_name, pack }),
            Err(errors) => issues.push(format!("{file_name}: invalid — {}", errors.join("; "))),
        }
    }
    let (files, dup_issues) = dedupe_pack_names(files);
    issues.extend(dup_issues);
    (files, issues)
}

// ── doc shapes + query bodies ────────────────────────────────────────────

/// Node doc: aligned with the promotion write shape minus provenance —
/// custom `_id`, `origin`/`pack` markers, NO `source_entry_id`/
/// `source_session` (every live read path is Value-based with defaults;
/// `confidence` omitted so semantic reads default to 0.9).
fn seed_node_doc(node: &SeedNode, pack: &str, now: &str) -> serde_json::Value {
    let mut doc = match &node.kind {
        SeedNodeKind::Semantic { category, content } => json!({
            "content": content,
            "category": category.as_str(),
        }),
        SeedNodeKind::Procedural { title, description, steps, outcomes } => {
            let steps: Vec<serde_json::Value> = steps
                .iter()
                .enumerate()
                .map(|(i, action)| json!({ "order": i + 1, "action": action }))
                .collect();
            let mut d = json!({
                "title": title,
                "description": description,
                "steps": steps,
            });
            if let Some(o) = outcomes {
                d["outcomes"] = json!({ "success": o.success, "failure": o.failure });
            }
            d
        }
    };
    doc["_id"] = json!(node.id);
    doc["tags"] = json!(node.tags);
    doc["origin"] = json!(ORIGIN_SEED);
    doc["pack"] = json!(pack);
    doc["access_count"] = json!(0);
    doc["last_accessed"] = serde_json::Value::Null;
    doc["created_at"] = json!(now);
    doc["updated_at"] = json!(now);
    doc
}

fn node_collections(pack: &SeedPack) -> HashMap<&str, &'static str> {
    pack.nodes
        .iter()
        .map(|n| (n.id.as_str(), n.collection()))
        .collect()
}

/// Edge doc: identity-projection pattern — no `_id` (server mints),
/// provenance under `metadata`.
fn seed_edge_doc(
    edge: &SeedEdge,
    colls: &HashMap<&str, &'static str>,
    pack: &str,
    now: &str,
) -> serde_json::Value {
    json!({
        "source_id": edge.src,
        "source_collection": colls.get(edge.src.as_str()).copied().unwrap_or("memory.semantic"),
        "target_id": edge.dst,
        "target_collection": colls.get(edge.dst.as_str()).copied().unwrap_or("memory.semantic"),
        "edge_type": edge.relation,
        "weight": edge.weight,
        "metadata": { "origin": ORIGIN_SEED, "pack": pack },
        "created_at": now,
    })
}

/// Node fast-path count (per pack per collection): two top-level eq keys —
/// a full-scan `count_only`, fine at promoted-collection scale.
fn seed_node_count_filter(pack: &str) -> serde_json::Value {
    json!({ "origin": ORIGIN_SEED, "pack": pack })
}

/// Edge fast-path count — dot-paths resolve server-side.
fn seed_edge_count_filter(pack: &str) -> serde_json::Value {
    json!({ "metadata.origin": ORIGIN_SEED, "metadata.pack": pack })
}

/// 3-eq existence probe, limit 1, NO sort — the sanctioned existence-scan
/// shape (identity reconcile precedent). Dedupes against ANY edge with the
/// same triple regardless of provenance.
fn seed_edge_probe_body(src: &str, dst: &str, relation: &str) -> serde_json::Value {
    json!({
        "filter": { "source_id": src, "target_id": dst, "edge_type": relation },
        "limit": 1,
    })
}

// ── reconcile (ensure-present, identity-projection template) ────────────

/// Pure fast-path predicate: counts cover the expectations AND the
/// first-node spot-probe resolved. `>=` on purpose — operator edits never
/// reduce presence, and packs only ever list what must exist.
fn nodes_fast_path_ok(
    sem_count: u64,
    proc_count: u64,
    sem_expected: usize,
    proc_expected: usize,
    probe_ok: bool,
) -> bool {
    sem_count >= sem_expected as u64 && proc_count >= proc_expected as u64 && probe_ok
}

/// Boot-tail entry (migrations tail, AFTER `ensure_identity_projection`).
/// Returns `()` — warn-don't-fail like every tail neighbor: a bad pack is
/// a journal warning, never a boot abort.
pub async fn ensure_seed_knowledge(db: &WardsonDbClient) {
    let (packs, issues) = scan_seed_dirs();
    for issue in &issues {
        warn!(target: "knowledge_seed", "{}", issue);
    }
    if packs.is_empty() {
        return;
    }
    // Migrations run BEFORE main.rs loads the config, so the loader loads
    // its own — best-effort: on a true first boot (no config doc yet) the
    // derive_edges enrichment of fresh nodes is silently skipped;
    // `edge_exists` dedupes make a later re-derive harmless anyway.
    let config = crate::config::load_config(db).await.ok();
    for pack_file in &packs {
        reconcile_pack(db, &pack_file.file_name, &pack_file.pack, config.as_ref()).await;
    }
}

async fn reconcile_pack(
    db: &WardsonDbClient,
    file_name: &str,
    pack: &SeedPack,
    config: Option<&SystemConfig>,
) {
    let now = chrono::Utc::now().to_rfc3339();
    let sem_expected = pack
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, SeedNodeKind::Semantic { .. }))
        .count();
    let proc_expected = pack.nodes.len() - sem_expected;

    // Stage 1 — cheap fast-path: per-collection filtered counts + one
    // spot-probe (healthy-boot cost: 2 counts + 1 read per pack).
    let count_filter = seed_node_count_filter(&pack.name);
    let sem_count = if sem_expected > 0 {
        db.count_filtered("memory.semantic", &count_filter).await.unwrap_or(0)
    } else {
        0
    };
    let proc_count = if proc_expected > 0 {
        db.count_filtered("memory.procedural", &count_filter).await.unwrap_or(0)
    } else {
        0
    };
    let probe_ok = match pack.nodes.first() {
        Some(first) => db.read(first.collection(), &first.id).await.is_ok(),
        None => true,
    };

    let mut healed_nodes = 0usize;
    let mut fresh: Vec<&SeedNode> = Vec::new();
    if !nodes_fast_path_ok(sem_count, proc_count, sem_expected, proc_expected, probe_ok) {
        // Stage 2 — exhaustive walk: insert-missing-only by _id (presence
        // check, never a content patch — edits stick).
        for node in &pack.nodes {
            if db.read(node.collection(), &node.id).await.is_ok() {
                continue;
            }
            let doc = seed_node_doc(node, &pack.name, &now);
            match db.write(node.collection(), &doc).await {
                Ok(_) => {
                    healed_nodes += 1;
                    fresh.push(node);
                }
                Err(e) => warn!(
                    target: "knowledge_seed",
                    "seed[{}]: writing node '{}' failed (next boot heals): {}",
                    pack.name, node.id, e
                ),
            }
        }
    }

    // Auto-edge enrichment for FRESHLY inserted nodes only: session=""
    // matches nothing (seeds carry no source_session), so no same_session
    // noise; tag_overlap is the point — it wires seeds into the operator's
    // organically-tagged knowledge for Step-4 expansion.
    if let Some(config) = config {
        for node in &fresh {
            let _ = derive_edges(
                db,
                &node.id,
                node.collection(),
                "",
                &node.tags,
                &now,
                config,
            )
            .await;
        }
    }

    // Edges: filtered-count fast-path, then 3-eq probe walk (probe failure
    // treated as exists — never double-insert; identity pattern).
    let mut healed_edges = 0usize;
    if !pack.edges.is_empty() {
        let expected = pack.edges.len() as u64;
        let actual = db
            .count_filtered("memory.edges", &seed_edge_count_filter(&pack.name))
            .await
            .unwrap_or(0);
        if actual < expected {
            let colls = node_collections(pack);
            for edge in &pack.edges {
                let exists = db
                    .query(
                        "memory.edges",
                        &seed_edge_probe_body(&edge.src, &edge.dst, &edge.relation),
                    )
                    .await
                    .map(|d| !d.is_empty())
                    .unwrap_or(true);
                if exists {
                    continue;
                }
                match db
                    .write("memory.edges", &seed_edge_doc(edge, &colls, &pack.name, &now))
                    .await
                {
                    Ok(_) => healed_edges += 1,
                    Err(e) => warn!(
                        target: "knowledge_seed",
                        "seed[{}]: writing edge {} -> {} failed (next boot heals): {}",
                        pack.name, edge.src, edge.dst, e
                    ),
                }
            }
        }
    }

    if healed_nodes > 0 || healed_edges > 0 {
        info!(
            target: "knowledge_seed",
            "seed[{}] ({}): healed {} nodes, {} edges",
            pack.name, file_name, healed_nodes, healed_edges
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_pack(nodes: serde_json::Value, edges: serde_json::Value) -> String {
        json!({
            "format": "knowledge.v1",
            "name": "test-pack",
            "nodes": nodes,
            "edges": edges,
        })
        .to_string()
    }

    fn sem_node(id: &str) -> serde_json::Value {
        json!({"id": id, "kind": "semantic", "category": "fact",
               "content": "some substantive claim", "tags": ["kg"]})
    }

    #[test]
    fn knowledge_v1_happy_path_parses_with_weight_default_1() {
        let raw = minimal_pack(
            json!([
                sem_node("seed_a"),
                {"id": "seed_b", "kind": "procedural", "title": "T", "description": "D",
                 "steps": ["one", "two"], "outcomes": {"success": "s", "failure": "f"},
                 "tags": []},
            ]),
            json!([{"src": "seed_a", "dst": "seed_b", "relation": "enables"}]),
        );
        let pack = parse_pack(&raw).expect("valid pack");
        assert_eq!(pack.name, "test-pack");
        assert_eq!(pack.nodes.len(), 2);
        assert_eq!(pack.nodes[0].collection(), "memory.semantic");
        assert_eq!(pack.nodes[1].collection(), "memory.procedural");
        assert_eq!(pack.edges.len(), 1);
        assert!((pack.edges[0].weight - 1.0).abs() < 1e-9, "weight defaults to 1.0");
    }

    #[test]
    fn comment_markers_skipped_in_nodes_and_edges() {
        let raw = minimal_pack(
            json!([{"_comment": "section: architecture"}, sem_node("seed_a")]),
            json!([{"_comment": "edges below"}]),
        );
        let pack = parse_pack(&raw).expect("comments are not errors");
        assert_eq!(pack.nodes.len(), 1);
        assert!(pack.edges.is_empty());
    }

    #[test]
    fn format_marker_and_nonempty_name_required() {
        let errs = parse_pack(r#"{"nodes": [], "edges": []}"#).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("'format'")));
        assert!(errs.iter().any(|e| e.contains("'name'")));
        let errs =
            parse_pack(r#"{"format": "graph.v1", "name": "x", "nodes": [], "edges": []}"#)
                .unwrap_err();
        assert!(errs.iter().any(|e| e.contains("expected 'knowledge.v1'")));
    }

    #[test]
    fn semantic_requires_known_category_and_content() {
        let raw = minimal_pack(
            json!([
                {"id": "a", "kind": "semantic", "category": "vibe", "content": "x", "tags": []},
                {"id": "b", "kind": "semantic", "category": "fact", "content": "  ", "tags": []},
            ]),
            json!([]),
        );
        let errs = parse_pack(&raw).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("'a'") && e.contains("category")));
        assert!(errs.iter().any(|e| e.contains("'b'") && e.contains("content")));
    }

    #[test]
    fn procedural_requires_title_description_outcomes_both_or_none() {
        let raw = minimal_pack(
            json!([
                {"id": "p1", "kind": "procedural", "title": "", "description": "d", "tags": []},
                {"id": "p2", "kind": "procedural", "title": "t", "description": "d",
                 "outcomes": {"success": "only"}, "tags": []},
                {"id": "p3", "kind": "procedural", "title": "t", "description": "d", "tags": []},
            ]),
            json!([]),
        );
        let errs = parse_pack(&raw).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("'p1'")));
        assert!(errs.iter().any(|e| e.contains("'p2'") && e.contains("BOTH")));
        assert!(!errs.iter().any(|e| e.contains("'p3'")), "outcomes are optional: {errs:?}");
    }

    #[test]
    fn tags_required_array_of_strings() {
        let raw = minimal_pack(
            json!([{"id": "a", "kind": "semantic", "category": "fact", "content": "x"}]),
            json!([]),
        );
        let errs = parse_pack(&raw).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("tags")));
        let raw = minimal_pack(
            json!([{"id": "a", "kind": "semantic", "category": "fact", "content": "x",
                    "tags": ["ok", 7]}]),
            json!([]),
        );
        assert!(parse_pack(&raw).is_err());
    }

    #[test]
    fn id_rules_mirror_server_and_forbid_user_prefix() {
        for (id, needle) in [
            ("_lead", "reserved"),
            ("user_thing", "user_"),
            ("", "empty"),
        ] {
            let raw = minimal_pack(
                json!([{"id": id, "kind": "semantic", "category": "fact",
                        "content": "x", "tags": []}]),
                json!([]),
            );
            match parse_pack(&raw) {
                // Empty id: the object is treated as valid-keyed (id present
                // but empty string) — id_violation catches it.
                Err(errs) => assert!(
                    errs.iter().any(|e| e.contains(needle)),
                    "id {id:?}: expected {needle} in {errs:?}"
                ),
                Ok(_) => panic!("id {id:?} must be rejected"),
            }
        }
    }

    #[test]
    fn duplicate_ids_and_duplicate_edge_triples_collect() {
        let raw = minimal_pack(
            json!([sem_node("seed_a"), sem_node("seed_a"), sem_node("seed_b")]),
            json!([
                {"src": "seed_a", "dst": "seed_b", "relation": "refines"},
                {"src": "seed_a", "dst": "seed_b", "relation": "refines"},
            ]),
        );
        let errs = parse_pack(&raw).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("duplicate node id")));
        assert!(errs.iter().any(|e| e.contains("duplicate edge")));
    }

    #[test]
    fn edges_reference_in_pack_ids_only() {
        let raw = minimal_pack(
            json!([sem_node("seed_a")]),
            json!([{"src": "seed_a", "dst": "elsewhere", "relation": "related_to"}]),
        );
        let errs = parse_pack(&raw).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("in-pack ids only")));
    }

    #[test]
    fn weight_bounds_zero_exclusive_one_inclusive() {
        for (w, ok) in [(json!(0.0), false), (json!(1.0), true), (json!(1.5), false)] {
            let raw = minimal_pack(
                json!([sem_node("seed_a"), sem_node("seed_b")]),
                json!([{"src": "seed_a", "dst": "seed_b", "relation": "refines", "weight": w}]),
            );
            assert_eq!(parse_pack(&raw).is_ok(), ok, "weight {w}");
        }
    }

    #[test]
    fn collect_all_errors_reports_everything_at_once() {
        let raw = json!({
            "format": "wrong",
            "name": "",
            "nodes": [
                {"id": "user_x", "kind": "semantic", "category": "nope", "content": "", "tags": 3}
            ],
            "edges": [{"src": "user_x", "dst": "ghost", "relation": ""}],
        })
        .to_string();
        let errs = parse_pack(&raw).unwrap_err();
        // One authoring round trip: format, name, tags, category/content,
        // user_ prefix, edge issues — all present together.
        assert!(errs.len() >= 5, "collected {} errors: {errs:?}", errs.len());
    }

    #[test]
    fn seed_semantic_doc_shape_aligns_with_promotion_minus_provenance() {
        let node = SeedNode {
            id: "seed_kg_density".into(),
            kind: SeedNodeKind::Semantic {
                category: SemanticCategory::Fact,
                content: "density is the design".into(),
            },
            tags: vec!["kg".into()],
        };
        let doc = seed_node_doc(&node, "embraos-kg", "2026-07-31T00:00:00Z");
        assert_eq!(doc["_id"], json!("seed_kg_density"));
        assert_eq!(doc["content"], json!("density is the design"));
        assert_eq!(doc["category"], json!("fact"));
        assert_eq!(doc["tags"], json!(["kg"]));
        assert_eq!(doc["origin"], json!("knowledge_seed"));
        assert_eq!(doc["pack"], json!("embraos-kg"));
        assert_eq!(doc["access_count"], json!(0));
        assert!(doc["last_accessed"].is_null());
        assert_eq!(doc["created_at"], doc["updated_at"]);
        // Deliberately absent: promotion provenance + confidence (reads
        // default 0.9).
        assert!(doc.get("source_entry_id").is_none());
        assert!(doc.get("source_session").is_none());
        assert!(doc.get("confidence").is_none());
    }

    #[test]
    fn seed_procedural_doc_shape_structured_steps_one_based_outcomes_optional() {
        let node = SeedNode {
            id: "seed_proc".into(),
            kind: SeedNodeKind::Procedural {
                title: "Audit then merge".into(),
                description: "hygiene round".into(),
                steps: vec!["audit".into(), "merge".into()],
                outcomes: None,
            },
            tags: vec![],
        };
        let doc = seed_node_doc(&node, "p", "2026-07-31T00:00:00Z");
        assert_eq!(doc["steps"], json!([
            {"order": 1, "action": "audit"},
            {"order": 2, "action": "merge"},
        ]));
        assert!(doc.get("outcomes").is_none(), "outcomes written only when present");
        assert_eq!(doc["title"], json!("Audit then merge"));
    }

    #[test]
    fn seed_edge_doc_resolves_collections_by_kind_metadata_origin_pack() {
        let pack = SeedPack {
            name: "p".into(),
            description: None,
            nodes: vec![
                SeedNode {
                    id: "s".into(),
                    kind: SeedNodeKind::Semantic {
                        category: SemanticCategory::Fact,
                        content: "x".into(),
                    },
                    tags: vec![],
                },
                SeedNode {
                    id: "pr".into(),
                    kind: SeedNodeKind::Procedural {
                        title: "t".into(),
                        description: "d".into(),
                        steps: vec![],
                        outcomes: None,
                    },
                    tags: vec![],
                },
            ],
            edges: vec![],
        };
        let colls = node_collections(&pack);
        let edge = SeedEdge {
            src: "s".into(),
            dst: "pr".into(),
            relation: "enables".into(),
            weight: 0.8,
        };
        let doc = seed_edge_doc(&edge, &colls, "p", "2026-07-31T00:00:00Z");
        assert_eq!(doc["source_collection"], json!("memory.semantic"));
        assert_eq!(doc["target_collection"], json!("memory.procedural"));
        assert_eq!(doc["metadata"], json!({"origin": "knowledge_seed", "pack": "p"}));
        assert!(doc.get("_id").is_none(), "server mints edge ids");
        assert_eq!(doc["weight"], json!(0.8));
    }

    #[test]
    fn seed_docs_never_target_identity_graph() {
        // Structural: the collection arm has exactly two returns.
        let sem = SeedNode {
            id: "a".into(),
            kind: SeedNodeKind::Semantic {
                category: SemanticCategory::Fact,
                content: "x".into(),
            },
            tags: vec![],
        };
        let proc_ = SeedNode {
            id: "b".into(),
            kind: SeedNodeKind::Procedural {
                title: "t".into(),
                description: "d".into(),
                steps: vec![],
                outcomes: None,
            },
            tags: vec![],
        };
        for n in [&sem, &proc_] {
            let coll = n.collection();
            assert!(coll == "memory.semantic" || coll == "memory.procedural");
            assert_ne!(coll, crate::identity_graph::IDENTITY_COLLECTION);
        }
    }

    #[test]
    fn seed_count_filters_are_two_key_eq_shapes() {
        assert_eq!(
            seed_node_count_filter("p"),
            json!({"origin": "knowledge_seed", "pack": "p"})
        );
        assert_eq!(
            seed_edge_count_filter("p"),
            json!({"metadata.origin": "knowledge_seed", "metadata.pack": "p"})
        );
    }

    #[test]
    fn seed_edge_probe_is_three_eq_limit_1_no_sort() {
        let body = seed_edge_probe_body("a", "b", "refines");
        assert_eq!(body["filter"], json!({"source_id": "a", "target_id": "b", "edge_type": "refines"}));
        assert_eq!(body["limit"], json!(1));
        assert!(body.get("sort").is_none(), "existence probe — sanctioned no-sort");
    }

    #[test]
    fn dirs_to_scan_env_override_is_exclusive_else_rootfs_then_state() {
        let dirs = dirs_to_scan(Some("/tmp/custom"));
        assert_eq!(dirs, vec![PathBuf::from("/tmp/custom")]);
        let dirs = dirs_to_scan(Some("   "));
        assert_eq!(dirs, vec![PathBuf::from(ROOTFS_SEED_DIR), PathBuf::from(STATE_SEED_DIR)]);
        let dirs = dirs_to_scan(None);
        assert_eq!(
            dirs,
            vec![PathBuf::from(ROOTFS_SEED_DIR), PathBuf::from(STATE_SEED_DIR)],
            "rootfs first so STATE overwrites in the BTreeMap"
        );
    }

    #[test]
    fn duplicate_pack_names_first_file_wins_with_issue() {
        let mk = |file: &str| SeedPackFile {
            file_name: file.into(),
            pack: SeedPack {
                name: "same".into(),
                description: None,
                nodes: vec![],
                edges: vec![],
            },
        };
        let (kept, issues) = dedupe_pack_names(vec![mk("a.knowledge.json"), mk("b.knowledge.json")]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].file_name, "a.knowledge.json");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("b.knowledge.json"));
    }

    #[test]
    fn committed_seed_packs_validate() {
        // Every pack committed under Seed_Knowledge/ must parse clean —
        // this is what stops an invalid pack from shipping in an image
        // (doc_examples_validate precedent). The default pack must exist
        // and keep its stable name.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Seed_Knowledge");
        let mut names: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("Seed_Knowledge/ exists") {
            let path = entry.expect("readable dir entry").path();
            let Some(fname) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if !fname.ends_with(SEED_FILE_SUFFIX) {
                continue;
            }
            let raw = std::fs::read_to_string(&path).expect("readable pack");
            match parse_pack(&raw) {
                Ok(pack) => {
                    assert!(!pack.nodes.is_empty(), "{fname}: a committed pack must carry nodes");
                    names.push(pack.name);
                }
                Err(errs) => panic!("{fname} is invalid:\n  {}", errs.join("\n  ")),
            }
        }
        assert!(
            names.iter().any(|n| n == "embraos-kg"),
            "the default embraos-kg pack must be present (found: {names:?})"
        );
    }

    #[test]
    fn nodes_fast_path_requires_both_counts_and_probe() {
        assert!(nodes_fast_path_ok(5, 2, 5, 2, true));
        assert!(nodes_fast_path_ok(6, 2, 5, 2, true), ">= — operator additions never trigger walks");
        assert!(!nodes_fast_path_ok(4, 2, 5, 2, true), "missing semantic node");
        assert!(!nodes_fast_path_ok(5, 1, 5, 2, true), "missing procedural node");
        assert!(!nodes_fast_path_ok(5, 2, 5, 2, false), "spot-probe miss forces the walk");
        assert!(nodes_fast_path_ok(0, 0, 0, 0, true), "empty pack is trivially present");
    }
}
