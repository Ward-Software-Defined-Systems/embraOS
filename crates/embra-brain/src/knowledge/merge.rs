//! `knowledge_merge` — consolidate two knowledge nodes: the source node is
//! deleted and its meaningful edges are redirected to the surviving target.
//!
//! WardSONDB has no transactions, so "atomic" is impossible — the executor
//! is ORDERED IDEMPOTENT STEPS instead: additive/repairable writes first,
//! the irreversible source delete LAST. Any mid-run failure returns an
//! honest partial-state report; re-running with the same arguments
//! converges (redirected edges vanish from the source's arms, tag union
//! and pointer repairs skip themselves, the content append is
//! marker-guarded). The dry_run preview renders the SAME plan the executor
//! walks — preview == execution plan by construction.
//!
//! Spec: `knowledge_audit_merge_spec.md`; detection side:
//! `knowledge/audit.rs` (its `dedup_candidates` paste directly into these
//! args).

use std::collections::HashMap;

use chrono::Utc;
use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;
use crate::tools::registry::DispatchContext;

use super::audit::resolve_kg_collection;
use super::edges::derive_edges;
use super::traversal::{edge_query_body, parse_edge, source_arm_filter, target_arm_filter};
use super::types::{EdgeType, KnowledgeEdge};

/// Per-arm edge window for the plan's four indexed fetches. Average node
/// degree today is ~171 edge docs; a window hit means the node is too
/// edge-dense to merge on provably-complete data → hard abort, never a
/// silently-partial destructive plan.
const MERGE_EDGE_ARM_LIMIT: u32 = 10_000;
/// Delete chunk size — sweep_orphans precedent.
const MERGE_DELETE_CHUNK: usize = 100;

fn merged_from_marker(source_id: &str) -> String {
    format!("## Merged from {}", source_id)
}

// ── args ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    KeepTarget,
    MergeTags,
    MergeContent,
}

impl MergeStrategy {
    fn as_str(self) -> &'static str {
        match self {
            MergeStrategy::KeepTarget => "keep_target",
            MergeStrategy::MergeTags => "merge_tags",
            MergeStrategy::MergeContent => "merge_content",
        }
    }
}

fn default_merge_strategy() -> MergeStrategy {
    MergeStrategy::KeepTarget
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_merge",
    is_side_effectful = true,
    description = "Consolidate two knowledge nodes: the source node is DELETED and its meaningful edges (enables/contradicts/refines/depends_on/related_to, derived_from provenance, free-form relations) are redirected to the target. Colliding edges keep the higher weight; ties keep the target's. The source's auto-derived edges are dropped and the target's are re-derived over the unioned tags. Entries promoted to the source are re-pointed to the target. strategy: keep_target (default — target content kept, tags unioned; merge_tags is an alias) or merge_content (source content appended under a '## Merged from' section; memory.semantic only). Both nodes must be in the same collection (memory.semantic or memory.procedural). IRREVERSIBLE — there is no unmerge; ALWAYS preview with dry_run=true first. If a run aborts partway it reports what completed; re-running with the same arguments converges."
)]
pub struct KnowledgeMergeArgs {
    /// Collection of the node being merged away (deleted): memory.semantic
    /// or memory.procedural (short names accepted). Must equal
    /// target_collection.
    pub source_collection: String,
    /// Id of the node being merged away.
    pub source_id: String,
    /// Collection of the surviving node.
    pub target_collection: String,
    /// Id of the surviving node.
    pub target_id: String,
    /// keep_target (default): target content kept, tags unioned.
    /// merge_tags: alias of keep_target. merge_content: source content
    /// appended under a "## Merged from <id>" section (memory.semantic
    /// only).
    #[serde(default = "default_merge_strategy")]
    pub strategy: MergeStrategy,
    /// Preview the full plan without writing anything. Strongly recommended
    /// before every real merge.
    #[serde(default)]
    pub dry_run: bool,
}

impl KnowledgeMergeArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        run_knowledge_merge(ctx.db, ctx.config, self)
            .await
            .map_err(DispatchError::Handler)
    }
}

// ── validation (pure) ────────────────────────────────────────────────────

/// Everything checkable without the DB. Returns (collection, src_id,
/// tgt_id) — same-kind means same-collection with only two node kinds.
fn validate_merge_args(args: &KnowledgeMergeArgs) -> Result<(&'static str, String, String), String> {
    let src_coll = resolve_kg_collection(&args.source_collection)
        .map_err(|e| format!("knowledge_merge rejected: {}", e))?;
    let tgt_coll = resolve_kg_collection(&args.target_collection)
        .map_err(|e| format!("knowledge_merge rejected: {}", e))?;
    if src_coll != tgt_coll {
        return Err(format!(
            "knowledge_merge rejected: cross-kind merge ({} → {}) — semantic and procedural nodes cannot be merged into each other",
            src_coll, tgt_coll
        ));
    }
    let src_id = args.source_id.trim().to_string();
    let tgt_id = args.target_id.trim().to_string();
    if src_id.is_empty() || tgt_id.is_empty() {
        return Err("knowledge_merge rejected: source_id and target_id are required".into());
    }
    if src_id == tgt_id {
        return Err(
            "knowledge_merge rejected: self-merge (source and target are the same node)".into(),
        );
    }
    if args.strategy == MergeStrategy::MergeContent && src_coll == "memory.procedural" {
        return Err(
            "knowledge_merge rejected: merge_content is not supported for memory.procedural \
             (structured steps cannot be textually merged) — fold what you need into the target \
             with knowledge_update first, then merge with keep_target"
                .into(),
        );
    }
    Ok((src_coll, src_id, tgt_id))
}

// ── plan (pure core + thin async shell) ──────────────────────────────────

/// Both arm bodies for one node's exhaustive edge fetch — the traversal
/// builders, so the indexed top-level sibling-eq shape (and its never-`$or`
/// contract) stays defined in ONE place.
fn merge_arm_bodies(coll: &str, id: &str) -> (serde_json::Value, serde_json::Value) {
    (
        edge_query_body(source_arm_filter(coll, id, None, None), MERGE_EDGE_ARM_LIMIT),
        edge_query_body(target_arm_filter(coll, id, None, None), MERGE_EDGE_ARM_LIMIT),
    )
}

/// Endpoint patch for a redirect: rewrite ONLY the side that referenced the
/// merged-away node — a server-side merge PATCH must never be able to
/// clobber weight/metadata/created_at.
fn redirect_patch(src_side_is_source: bool, tgt_coll: &str, tgt_id: &str) -> serde_json::Value {
    if src_side_is_source {
        json!({ "source_id": tgt_id, "source_collection": tgt_coll })
    } else {
        json!({ "target_id": tgt_id, "target_collection": tgt_coll })
    }
}

/// Byte-shape of promotion.rs's `promoted_to` write.
fn promoted_repair_patch(tgt_coll: &str, tgt_id: &str) -> serde_json::Value {
    json!({ "promoted_to": { "collection": tgt_coll, "id": tgt_id } })
}

fn edge_ids_delete_filter(chunk: &[String]) -> serde_json::Value {
    json!({ "_id": { "$in": chunk } })
}

/// Union preserving target order, source's new tags appended in source
/// order (case-sensitive exact — matches tag storage semantics). Returns
/// (unioned, added).
fn union_tags(tgt: &[String], src: &[String]) -> (Vec<String>, Vec<String>) {
    let mut unioned = tgt.to_vec();
    let mut added = Vec::new();
    for t in src {
        if !unioned.contains(t) {
            unioned.push(t.clone());
            added.push(t.clone());
        }
    }
    (unioned, added)
}

/// merge_content plan: (new_content, note). The marker check is the re-run
/// guard — without it a retried merge would append the source section
/// twice.
fn plan_content(tgt_content: &str, src_content: &str, src_id: &str) -> (Option<String>, Option<String>) {
    let marker = merged_from_marker(src_id);
    if tgt_content.contains(&marker) {
        (
            None,
            Some(format!(
                "target already contains \"{}\" — content append skipped (re-run guard)",
                marker
            )),
        )
    } else {
        (
            Some(format!("{}\n\n{}\n\n{}", tgt_content, marker, src_content)),
            None,
        )
    }
}

/// Direction-aware conflict key relative to the TARGET node after redirect:
/// (edge-leaves-the-node, counterpart collection, counterpart id, type).
type ConflictKey = (bool, String, String, String);

#[derive(Default)]
struct EdgeDisposition {
    /// (edge_id, endpoint patch) — winners to rewrite.
    redirects: Vec<(String, serde_json::Value)>,
    /// Target's existing edges that lost a conflict to a heavier redirect.
    delete_conflict_target: Vec<String>,
    /// Source-side edge docs to delete (auto + self-pair + losing
    /// candidates), combined for chunked deletion.
    drop_ids: Vec<String>,
    drop_auto: usize,
    drop_self_pair: usize,
    drop_conflict_source: usize,
    /// Source's outgoing derived_from → memory.entries targets (promotion
    /// pointers to verify/repair).
    derived_from_entry_ids: Vec<String>,
    warnings: Vec<String>,
}

/// The heart of the merge: classify every edge doc touching the source and
/// resolve conflicts against the target's existing edges. Pure — heavily
/// tested. `src_edges`/`tgt_edges` come from the four arm fetches (deduped
/// by `_id`; the src↔tgt pair edges appear in BOTH lists and are handled
/// exclusively from the source side as `drop_self_pair`).
fn classify_and_resolve(
    src_edges: &[KnowledgeEdge],
    tgt_edges: &[KnowledgeEdge],
    src: (&str, &str),
    tgt: (&str, &str),
) -> EdgeDisposition {
    let mut disp = EdgeDisposition::default();

    let is = |coll: &str, id: &str, key: (&str, &str)| coll == key.0 && id == key.1;

    // Target's existing edges keyed for conflict lookups — excluding edges
    // that touch the source (those are the self-pair set, deleted wholesale).
    let mut existing: HashMap<ConflictKey, (String, f64)> = HashMap::new();
    for e in tgt_edges {
        let Some(eid) = e._id.clone() else { continue };
        let outgoing = is(&e.source_collection, &e.source_id, tgt);
        let (oc, oi) = if outgoing {
            (&e.target_collection, &e.target_id)
        } else {
            (&e.source_collection, &e.source_id)
        };
        if is(oc, oi, src) {
            continue; // self-pair edge — source-side classification owns it
        }
        let key: ConflictKey = (outgoing, oc.clone(), oi.clone(), e.edge_type.as_str().to_string());
        let entry = existing.entry(key).or_insert((eid.clone(), e.weight));
        if e.weight > entry.1 {
            *entry = (eid, e.weight);
        }
    }

    // Classify the source's edges.
    struct Candidate {
        key: ConflictKey,
        weight: f64,
        edge_id: String,
        patch: serde_json::Value,
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    for e in src_edges {
        let Some(eid) = e._id.clone() else {
            disp.warnings
                .push("edge doc without _id skipped (cannot be rewritten)".into());
            continue;
        };
        let src_is_source = is(&e.source_collection, &e.source_id, src);
        let (oc, oi) = if src_is_source {
            (&e.target_collection, &e.target_id)
        } else {
            (&e.source_collection, &e.source_id)
        };
        if is(oc, oi, tgt) {
            // Any edge between the pair — auto twins AND brain links alike:
            // a redirect would mint a self-loop.
            disp.drop_self_pair += 1;
            disp.drop_ids.push(eid);
            continue;
        }
        match &e.edge_type {
            EdgeType::SameSession | EdgeType::Temporal | EdgeType::TagOverlap => {
                // Auto-derived: describes the SOURCE's session/time/tag
                // neighborhood — semantically stale on the target. Dropped;
                // step 5's derive_edges refresh re-derives what's true for
                // the target (both twin docs land here, one per arm fetch).
                disp.drop_auto += 1;
                disp.drop_ids.push(eid);
            }
            _ => {
                if e.edge_type == EdgeType::DerivedFrom
                    && src_is_source
                    && e.target_collection == "memory.entries"
                {
                    disp.derived_from_entry_ids.push(e.target_id.clone());
                }
                candidates.push(Candidate {
                    key: (src_is_source, oc.clone(), oi.clone(), e.edge_type.as_str().to_string()),
                    weight: e.weight,
                    edge_id: eid,
                    patch: redirect_patch(src_is_source, tgt.0, tgt.1),
                });
            }
        }
    }

    // Self-dedupe candidates deterministically: per key, highest weight
    // wins, then smallest edge id.
    candidates.sort_by(|a, b| {
        a.key
            .cmp(&b.key)
            .then(b.weight.total_cmp(&a.weight))
            .then(a.edge_id.cmp(&b.edge_id))
    });
    let mut last_key: Option<ConflictKey> = None;
    for c in candidates {
        if last_key.as_ref() == Some(&c.key) {
            disp.drop_conflict_source += 1;
            disp.drop_ids.push(c.edge_id);
            continue;
        }
        last_key = Some(c.key.clone());
        // Against the target's existing edge with the same key: higher
        // weight wins; tie keeps the target's existing (spec rule 3).
        match existing.get(&c.key) {
            Some((existing_id, existing_w)) => {
                if c.weight > *existing_w {
                    disp.delete_conflict_target.push(existing_id.clone());
                    disp.redirects.push((c.edge_id, c.patch));
                } else {
                    disp.drop_conflict_source += 1;
                    disp.drop_ids.push(c.edge_id);
                }
            }
            None => disp.redirects.push((c.edge_id, c.patch)),
        }
    }

    disp
}

struct MergePlan {
    redirects: Vec<(String, serde_json::Value)>,
    delete_conflict_target: Vec<String>,
    drop_ids: Vec<String>,
    drop_auto: usize,
    drop_self_pair: usize,
    drop_conflict_source: usize,
    /// Entry ids whose promoted_to verifiably points at the source.
    repairs: Vec<String>,
    tags_to_add: Vec<String>,
    unioned_tags: Vec<String>,
    new_content: Option<String>,
    warnings: Vec<String>,
}

async fn fetch_node_edges(
    db: &WardsonDbClient,
    coll: &str,
    id: &str,
) -> Result<Vec<KnowledgeEdge>, String> {
    let (src_body, tgt_body) = merge_arm_bodies(coll, id);
    let mut out: Vec<KnowledgeEdge> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for body in [src_body, tgt_body] {
        let docs = db
            .query("memory.edges", &body)
            .await
            .map_err(|e| format!("knowledge_merge aborted: edge fetch for {}:{} failed: {}", coll, id, e))?;
        if docs.len() >= MERGE_EDGE_ARM_LIMIT as usize {
            return Err(format!(
                "knowledge_merge aborted: {}:{}'s edge window saturated ({} per direction) — \
                 refusing to plan a destructive merge on partial data; this node is too \
                 edge-dense to merge safely (knowledge_unlink_node is the escape hatch)",
                coll, id, MERGE_EDGE_ARM_LIMIT
            ));
        }
        for d in &docs {
            let Some(e) = parse_edge(d) else { continue };
            let Some(eid) = e._id.clone() else { continue };
            if seen.insert(eid) {
                out.push(e);
            }
        }
    }
    Ok(out)
}

/// Same-kind is validated upstream, so one `coll` covers both nodes.
async fn build_merge_plan(
    db: &WardsonDbClient,
    coll: &'static str,
    src_id: &str,
    tgt_id: &str,
    src_doc: &serde_json::Value,
    tgt_doc: &serde_json::Value,
    strategy: MergeStrategy,
) -> Result<MergePlan, String> {
    let src_edges = fetch_node_edges(db, coll, src_id).await?;
    let tgt_edges = fetch_node_edges(db, coll, tgt_id).await?;
    let mut disp = classify_and_resolve(&src_edges, &tgt_edges, (coll, src_id), (coll, tgt_id));

    // Promotion pointers: verify before planning writes. Pointer already at
    // the target → re-run convergence, skip silently; pointing elsewhere or
    // entry missing → warn, never touch (the derived_from edge itself still
    // redirects under the normal rules).
    let mut repairs = Vec::new();
    for entry_id in &disp.derived_from_entry_ids {
        match db.read("memory.entries", entry_id).await {
            Ok(entry) => {
                let ptr_is = |coll: &str, id: &str| {
                    entry
                        .get("promoted_to")
                        .and_then(|p| {
                            Some((p.get("collection")?.as_str()?, p.get("id")?.as_str()?))
                        })
                        .map(|(c, i)| c == coll && i == id)
                        .unwrap_or(false)
                };
                if ptr_is(coll, src_id) {
                    repairs.push(entry_id.clone());
                } else if !ptr_is(coll, tgt_id) {
                    disp.warnings.push(format!(
                        "entry {} promoted_to does not point at the source — left untouched",
                        entry_id
                    ));
                }
            }
            Err(_) => disp.warnings.push(format!(
                "entry {} (derived_from target) not found — its edge still redirects; \
                 knowledge_sweep_orphans territory if it stays missing",
                entry_id
            )),
        }
    }

    let tags_of = |doc: &serde_json::Value| -> Vec<String> {
        doc.get("tags")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|t| t.as_str()).map(String::from).collect())
            .unwrap_or_default()
    };
    let (unioned_tags, tags_to_add) = union_tags(&tags_of(tgt_doc), &tags_of(src_doc));

    let mut new_content = None;
    if strategy == MergeStrategy::MergeContent {
        let tgt_content = tgt_doc.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let src_content = src_doc.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let (content, note) = plan_content(tgt_content, src_content, src_id);
        new_content = content;
        if let Some(n) = note {
            disp.warnings.push(n);
        }
    }

    Ok(MergePlan {
        redirects: disp.redirects,
        delete_conflict_target: disp.delete_conflict_target,
        drop_ids: disp.drop_ids,
        drop_auto: disp.drop_auto,
        drop_self_pair: disp.drop_self_pair,
        drop_conflict_source: disp.drop_conflict_source,
        repairs,
        tags_to_add,
        unioned_tags,
        new_content,
        warnings: disp.warnings,
    })
}

// ── rendering (pure) ─────────────────────────────────────────────────────

fn node_json(coll: &str, id: &str) -> serde_json::Value {
    json!({ "collection": coll, "id": id })
}

fn render_dry_run(
    plan: &MergePlan,
    src: (&str, &str),
    tgt: (&str, &str),
    strategy: MergeStrategy,
) -> String {
    let report = json!({
        "merged": false,
        "dry_run": true,
        "source": node_json(src.0, src.1),
        "target": node_json(tgt.0, tgt.1),
        "strategy": strategy.as_str(),
        "preview": {
            "edges_would_redirect": plan.redirects.len(),
            "edges_would_drop_duplicate": plan.drop_conflict_source,
            "edges_would_drop_auto": plan.drop_auto,
            "edges_would_drop_self_pair": plan.drop_self_pair,
            "target_edges_would_delete_conflict": plan.delete_conflict_target.len(),
            "entries_would_repoint": plan.repairs.len(),
            "tags_would_add": plan.tags_to_add,
            "content_would_append": plan.new_content.is_some(),
            "source_would_be_deleted": true,
        },
        "warnings": plan.warnings,
    });
    serde_json::to_string_pretty(&report).unwrap_or_else(|e| format!("render failed: {}", e))
}

#[derive(Default)]
struct ExecCounters {
    entries_repointed: u64,
    target_edges_deleted_conflict: u64,
    edges_redirected: u64,
    source_edges_deleted: u64,
    edges_regenerated: usize,
}

impl ExecCounters {
    fn to_json(&self) -> serde_json::Value {
        json!({
            "entries_repointed": self.entries_repointed,
            "target_edges_deleted_conflict": self.target_edges_deleted_conflict,
            "edges_redirected": self.edges_redirected,
            "source_edges_deleted": self.source_edges_deleted,
            "edges_regenerated": self.edges_regenerated,
        })
    }
}

/// Honest partial-state report. Once writes have started, a failure is a
/// STATE, not a mere error string — the counters tell the operator exactly
/// how far it got, and every completed step skips itself on re-run.
fn abort_json(step: &str, counters: &ExecCounters, error: String, action: &str) -> String {
    let report = json!({
        "merged": false,
        "aborted_at": step,
        "completed": counters.to_json(),
        "error": error,
        "action": action,
    });
    serde_json::to_string_pretty(&report).unwrap_or_else(|e| format!("render failed: {}", e))
}

fn render_success(
    plan: &MergePlan,
    counters: &ExecCounters,
    src: (&str, &str),
    tgt: (&str, &str),
    strategy: MergeStrategy,
) -> String {
    let report = json!({
        "merged": true,
        "source": node_json(src.0, src.1),
        "target": node_json(tgt.0, tgt.1),
        "strategy": strategy.as_str(),
        "edges_redirected": counters.edges_redirected,
        "edges_dropped_duplicate": plan.drop_conflict_source,
        "edges_dropped_auto": plan.drop_auto,
        "edges_dropped_self_pair": plan.drop_self_pair,
        "target_edges_deleted_conflict": counters.target_edges_deleted_conflict,
        "edges_regenerated": counters.edges_regenerated,
        "entries_repointed": counters.entries_repointed,
        "tags_added_to_target": plan.tags_to_add,
        "content_appended": plan.new_content.is_some(),
        "warnings": plan.warnings,
    });
    serde_json::to_string_pretty(&report).unwrap_or_else(|e| format!("render failed: {}", e))
}

// ── execution ────────────────────────────────────────────────────────────

const RERUN_ACTION: &str =
    "re-run knowledge_merge with the same arguments — completed steps are idempotent and skip themselves";

async fn execute_merge_plan(
    db: &WardsonDbClient,
    config: &SystemConfig,
    plan: &MergePlan,
    src: (&'static str, &str),
    tgt: (&'static str, &str),
    tgt_doc: &serde_json::Value,
    strategy: MergeStrategy,
) -> String {
    let mut counters = ExecCounters::default();

    // Step 1 — target update (additive: tags union, optional content
    // append, updated_at). Skipped entirely when nothing changes.
    let mut patch = serde_json::Map::new();
    if !plan.tags_to_add.is_empty() {
        patch.insert("tags".into(), json!(plan.unioned_tags));
    }
    if let Some(content) = &plan.new_content {
        patch.insert("content".into(), json!(content));
    }
    if !patch.is_empty() {
        patch.insert("updated_at".into(), json!(Utc::now().to_rfc3339()));
        if let Err(e) = db
            .patch_document(tgt.0, tgt.1, &serde_json::Value::Object(patch))
            .await
        {
            return abort_json(
                "step_1_target_update",
                &counters,
                format!("target patch failed: {}", e),
                RERUN_ACTION,
            );
        }
    }

    // Step 2 — promotion-pointer repairs.
    for entry_id in &plan.repairs {
        match db
            .patch_document("memory.entries", entry_id, &promoted_repair_patch(tgt.0, tgt.1))
            .await
        {
            Ok(_) => counters.entries_repointed += 1,
            Err(e) => {
                return abort_json(
                    "step_2_promoted_to_repairs",
                    &counters,
                    format!("repairing entry {} failed: {}", entry_id, e),
                    RERUN_ACTION,
                );
            }
        }
    }

    // Step 3 — delete conflict-losing target edges FIRST, then redirect
    // winners. Loser-first converges: if the run dies between the two, the
    // un-redirected winner is still on the source's arms and re-plans
    // cleanly. Redirect-first + crash would leave a duplicate pair on the
    // target that no re-plan can see (the candidate no longer touches the
    // source).
    for chunk in plan.delete_conflict_target.chunks(MERGE_DELETE_CHUNK) {
        match db
            .delete_by_query("memory.edges", &edge_ids_delete_filter(chunk))
            .await
        {
            Ok(n) => counters.target_edges_deleted_conflict += n,
            Err(e) => {
                return abort_json(
                    "step_3_conflict_deletes",
                    &counters,
                    format!("deleting conflict-losing target edges failed: {}", e),
                    RERUN_ACTION,
                );
            }
        }
    }
    for (edge_id, patch) in &plan.redirects {
        match db.patch_document("memory.edges", edge_id, patch).await {
            Ok(_) => counters.edges_redirected += 1,
            Err(e) => {
                return abort_json(
                    "step_3_redirects",
                    &counters,
                    format!("redirecting edge {} failed: {}", edge_id, e),
                    RERUN_ACTION,
                );
            }
        }
    }

    // Step 4 — drop the source's auto/self-pair/losing edge docs.
    for chunk in plan.drop_ids.chunks(MERGE_DELETE_CHUNK) {
        match db
            .delete_by_query("memory.edges", &edge_ids_delete_filter(chunk))
            .await
        {
            Ok(n) => counters.source_edges_deleted += n,
            Err(e) => {
                return abort_json(
                    "step_4_source_edge_drops",
                    &counters,
                    format!("dropping source edges failed: {}", e),
                    RERUN_ACTION,
                );
            }
        }
    }

    // Step 5 — auto-edge refresh over the unioned tags, anchored to the
    // target's own session/timestamp (fills tag_overlap for newly-unioned
    // tags; edge_exists dedupes). derive_edges never hard-fails.
    let session = tgt_doc
        .get("source_session")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let created_at = tgt_doc
        .get("created_at")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    counters.edges_regenerated =
        derive_edges(db, tgt.1, tgt.0, session, &plan.unioned_tags, created_at, config)
            .await
            .unwrap_or(0);

    // Step 6 — delete the source node LAST (the one irreversible write).
    if let Err(e) = db.delete(src.0, src.1).await {
        return abort_json(
            "step_6_source_delete",
            &counters,
            format!("source node delete failed: {}", e),
            "all edges are redirected — re-run knowledge_merge (converges), or knowledge_unlink_node the source",
        );
    }

    render_success(plan, &counters, src, tgt, strategy)
}

pub(crate) async fn run_knowledge_merge(
    db: &WardsonDbClient,
    config: &SystemConfig,
    args: KnowledgeMergeArgs,
) -> Result<String, String> {
    let (coll, src_id, tgt_id) = validate_merge_args(&args)?;
    let strategy = args.strategy;

    let src_doc = db.read(coll, &src_id).await.map_err(|_| {
        format!(
            "knowledge_merge rejected: source node {}:{} not found — if a previous merge run \
             reported failure only at the final delete step, the merge already completed",
            coll, src_id
        )
    })?;
    let tgt_doc = db
        .read(coll, &tgt_id)
        .await
        .map_err(|_| format!("knowledge_merge rejected: target node {}:{} not found", coll, tgt_id))?;

    let plan = build_merge_plan(db, coll, &src_id, &tgt_id, &src_doc, &tgt_doc, strategy).await?;

    if args.dry_run {
        return Ok(render_dry_run(&plan, (coll, &src_id), (coll, &tgt_id), strategy));
    }
    Ok(execute_merge_plan(db, config, &plan, (coll, &src_id), (coll, &tgt_id), &tgt_doc, strategy).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(
        id: &str,
        sc: &str,
        si: &str,
        tc: &str,
        ti: &str,
        etype: EdgeType,
        weight: f64,
    ) -> KnowledgeEdge {
        KnowledgeEdge {
            _id: Some(id.to_string()),
            source_id: si.to_string(),
            source_collection: sc.to_string(),
            target_id: ti.to_string(),
            target_collection: tc.to_string(),
            edge_type: etype,
            weight,
            metadata: serde_json::Value::Null,
            created_at: "2026-06-01T00:00:00Z".to_string(),
        }
    }

    const SEM: &str = "memory.semantic";
    const SRC: (&str, &str) = (SEM, "src");
    const TGT: (&str, &str) = (SEM, "tgt");

    fn args(v: serde_json::Value) -> KnowledgeMergeArgs {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn merge_arm_bodies_indexed_sorted_limited_never_contain_or() {
        let (src_body, tgt_body) = merge_arm_bodies(SEM, "abc");
        for (body, id_key, coll_key) in [
            (&src_body, "source_id", "source_collection"),
            (&tgt_body, "target_id", "target_collection"),
        ] {
            // Top-level sibling equality keys — the indexed arm shape.
            assert_eq!(body["filter"][id_key], serde_json::json!("abc"));
            assert_eq!(body["filter"][coll_key], serde_json::json!(SEM));
            assert_eq!(body["limit"], serde_json::json!(MERGE_EDGE_ARM_LIMIT));
            assert_eq!(
                body["sort"],
                serde_json::json!([{"weight": "desc"}, {"created_at": "desc"}])
            );
            // Any combinator reverts the arm to a full collection scan.
            assert!(!body.to_string().contains("$or"));
        }
    }

    #[test]
    fn validate_rejects_cross_kind_identity_entries_and_normalizes_short_names() {
        let ok = validate_merge_args(&args(json!({
            "source_collection": "semantic", "source_id": "a",
            "target_collection": "memory.semantic", "target_id": "b"
        })))
        .unwrap();
        assert_eq!(ok, (SEM, "a".to_string(), "b".to_string()));

        let cross = validate_merge_args(&args(json!({
            "source_collection": "semantic", "source_id": "a",
            "target_collection": "procedural", "target_id": "b"
        })));
        assert!(cross.unwrap_err().contains("cross-kind"));

        for coll in ["identity.graph", "memory.entries", "entries"] {
            let e = validate_merge_args(&args(json!({
                "source_collection": coll, "source_id": "a",
                "target_collection": coll, "target_id": "b"
            })));
            assert!(e.is_err(), "{} must be rejected", coll);
        }
    }

    #[test]
    fn validate_rejects_self_merge_and_empty_ids() {
        let same = validate_merge_args(&args(json!({
            "source_collection": "semantic", "source_id": "a",
            "target_collection": "semantic", "target_id": "a"
        })));
        assert!(same.unwrap_err().contains("self-merge"));
        let empty = validate_merge_args(&args(json!({
            "source_collection": "semantic", "source_id": "  ",
            "target_collection": "semantic", "target_id": "b"
        })));
        assert!(empty.is_err());
    }

    #[test]
    fn strategy_defaults_keep_target_merge_tags_alias_unknown_rejected() {
        let a = args(json!({
            "source_collection": "semantic", "source_id": "a",
            "target_collection": "semantic", "target_id": "b"
        }));
        assert_eq!(a.strategy, MergeStrategy::KeepTarget);
        assert!(!a.dry_run);
        let b = args(json!({
            "source_collection": "semantic", "source_id": "a",
            "target_collection": "semantic", "target_id": "b",
            "strategy": "merge_tags", "dry_run": true
        }));
        assert_eq!(b.strategy, MergeStrategy::MergeTags);
        assert!(b.dry_run);
        let bad: Result<KnowledgeMergeArgs, _> = serde_json::from_value(json!({
            "source_collection": "semantic", "source_id": "a",
            "target_collection": "semantic", "target_id": "b",
            "strategy": "overwrite"
        }));
        assert!(bad.is_err());
    }

    #[test]
    fn merge_content_rejected_for_procedural() {
        let e = validate_merge_args(&args(json!({
            "source_collection": "procedural", "source_id": "a",
            "target_collection": "procedural", "target_id": "b",
            "strategy": "merge_content"
        })));
        assert!(e.unwrap_err().contains("merge_content is not supported for memory.procedural"));
        // keep_target on procedural is fine.
        assert!(validate_merge_args(&args(json!({
            "source_collection": "procedural", "source_id": "a",
            "target_collection": "procedural", "target_id": "b"
        })))
        .is_ok());
    }

    #[test]
    fn classify_drops_auto_twins_and_self_pair_redirects_meaningful() {
        let src_edges = vec![
            // Auto twins to a third node — both docs dropped.
            edge("e1", SEM, "src", "memory.entries", "x", EdgeType::SameSession, 1.0),
            edge("e2", "memory.entries", "x", SEM, "src", EdgeType::SameSession, 1.0),
            edge("e3", SEM, "src", SEM, "y", EdgeType::TagOverlap, 0.5),
            edge("e4", SEM, "src", SEM, "y", EdgeType::Temporal, 0.7),
            // Edges between the pair — self-pair, any type.
            edge("e5", SEM, "src", SEM, "tgt", EdgeType::SameSession, 1.0),
            edge("e6", SEM, "tgt", SEM, "src", EdgeType::SameSession, 1.0),
            edge("e7", SEM, "src", SEM, "tgt", EdgeType::Refines, 0.9),
            // Meaningful outgoing + incoming + free-form + provenance.
            edge("e8", SEM, "src", SEM, "z", EdgeType::Enables, 0.8),
            edge("e9", SEM, "w", SEM, "src", EdgeType::DependsOn, 0.6),
            edge("e10", "identity.graph", "precise", SEM, "src", EdgeType::Other("navigatesFor".into()), 1.0),
            edge("e11", SEM, "src", "memory.entries", "entry1", EdgeType::DerivedFrom, 1.0),
        ];
        let disp = classify_and_resolve(&src_edges, &[], SRC, TGT);
        assert_eq!(disp.drop_auto, 4);
        assert_eq!(disp.drop_self_pair, 3);
        assert_eq!(disp.drop_conflict_source, 0);
        assert_eq!(disp.redirects.len(), 4);
        assert_eq!(disp.derived_from_entry_ids, vec!["entry1".to_string()]);
        // Patches rewrite exactly the side that referenced the source.
        let by_id: HashMap<&str, &serde_json::Value> =
            disp.redirects.iter().map(|(id, p)| (id.as_str(), p)).collect();
        assert_eq!(by_id["e8"], &json!({"source_id": "tgt", "source_collection": SEM}));
        assert_eq!(by_id["e9"], &json!({"target_id": "tgt", "target_collection": SEM}));
        assert_eq!(by_id["e10"], &json!({"target_id": "tgt", "target_collection": SEM}));
        assert_eq!(by_id["e11"], &json!({"source_id": "tgt", "source_collection": SEM}));
        // drop_ids covers auto + self-pair.
        assert_eq!(disp.drop_ids.len(), 7);
    }

    #[test]
    fn conflict_higher_weight_wins_tie_keeps_target_existing() {
        // Candidate heavier than target's existing → redirect + delete loser.
        let src_edges = vec![edge("c1", SEM, "src", SEM, "z", EdgeType::Enables, 0.9)];
        let tgt_edges = vec![edge("t1", SEM, "tgt", SEM, "z", EdgeType::Enables, 0.5)];
        let disp = classify_and_resolve(&src_edges, &tgt_edges, SRC, TGT);
        assert_eq!(disp.redirects.len(), 1);
        assert_eq!(disp.delete_conflict_target, vec!["t1".to_string()]);

        // Tie → target's existing wins, candidate dropped.
        let src_edges = vec![edge("c1", SEM, "src", SEM, "z", EdgeType::Enables, 0.5)];
        let tgt_edges = vec![edge("t1", SEM, "tgt", SEM, "z", EdgeType::Enables, 0.5)];
        let disp = classify_and_resolve(&src_edges, &tgt_edges, SRC, TGT);
        assert!(disp.redirects.is_empty());
        assert!(disp.delete_conflict_target.is_empty());
        assert_eq!(disp.drop_conflict_source, 1);
        assert_eq!(disp.drop_ids, vec!["c1".to_string()]);
    }

    #[test]
    fn conflict_key_covers_direction_counterpart_and_type() {
        // Same counterpart + type but OPPOSITE direction — no conflict.
        let src_edges = vec![edge("c1", SEM, "src", SEM, "z", EdgeType::DependsOn, 0.3)];
        let tgt_edges = vec![edge("t1", SEM, "z", SEM, "tgt", EdgeType::DependsOn, 0.9)];
        let disp = classify_and_resolve(&src_edges, &tgt_edges, SRC, TGT);
        assert_eq!(disp.redirects.len(), 1);
        assert!(disp.delete_conflict_target.is_empty());

        // Same direction + counterpart, different type — no conflict either.
        let src_edges = vec![edge("c1", SEM, "src", SEM, "z", EdgeType::Refines, 0.3)];
        let tgt_edges = vec![edge("t1", SEM, "tgt", SEM, "z", EdgeType::Enables, 0.9)];
        let disp = classify_and_resolve(&src_edges, &tgt_edges, SRC, TGT);
        assert_eq!(disp.redirects.len(), 1);
        assert!(disp.delete_conflict_target.is_empty());
    }

    #[test]
    fn redirect_candidates_self_dedupe_deterministically() {
        // Two source edges that would land on the SAME target key: the
        // heavier one wins; equal weights fall back to smaller edge id.
        let src_edges = vec![
            edge("c2", SEM, "src", SEM, "z", EdgeType::Enables, 0.4),
            edge("c1", SEM, "src", SEM, "z", EdgeType::Enables, 0.8),
        ];
        let disp = classify_and_resolve(&src_edges, &[], SRC, TGT);
        assert_eq!(disp.redirects.len(), 1);
        assert_eq!(disp.redirects[0].0, "c1");
        assert_eq!(disp.drop_conflict_source, 1);
        assert_eq!(disp.drop_ids, vec!["c2".to_string()]);
    }

    #[test]
    fn target_self_pair_edges_do_not_enter_conflict_map() {
        // The pair's own refines edge (from the target's perspective) must
        // not block the redirect of an unrelated candidate, and is deleted
        // via the source-side self-pair classification instead.
        let src_edges = vec![
            edge("pair", SEM, "src", SEM, "tgt", EdgeType::Refines, 0.9),
            edge("c1", SEM, "src", SEM, "z", EdgeType::Refines, 0.5),
        ];
        let tgt_edges = vec![edge("pair", SEM, "src", SEM, "tgt", EdgeType::Refines, 0.9)];
        let disp = classify_and_resolve(&src_edges, &tgt_edges, SRC, TGT);
        assert_eq!(disp.drop_self_pair, 1);
        assert_eq!(disp.redirects.len(), 1);
        assert_eq!(disp.redirects[0].0, "c1");
    }

    #[test]
    fn patch_shapes_promoted_repair_and_delete_filter() {
        assert_eq!(
            promoted_repair_patch(SEM, "node9"),
            json!({ "promoted_to": { "collection": SEM, "id": "node9" } })
        );
        assert_eq!(
            redirect_patch(true, SEM, "t"),
            json!({ "source_id": "t", "source_collection": SEM })
        );
        assert_eq!(
            redirect_patch(false, SEM, "t"),
            json!({ "target_id": "t", "target_collection": SEM })
        );
        let ids = vec!["a".to_string(), "b".to_string()];
        assert_eq!(edge_ids_delete_filter(&ids), json!({ "_id": { "$in": ["a", "b"] } }));
    }

    #[test]
    fn tag_union_dedupes_preserves_target_order_appends_new() {
        let tgt = vec!["cert".to_string(), "embra-web".to_string()];
        let src = vec!["Trustd".to_string(), "cert".to_string(), "pki".to_string()];
        let (unioned, added) = union_tags(&tgt, &src);
        assert_eq!(unioned, vec!["cert", "embra-web", "Trustd", "pki"]);
        assert_eq!(added, vec!["Trustd", "pki"]);
        // Case-sensitive exact — "Trustd" and "trustd" are distinct tags,
        // matching storage semantics.
        let (u2, a2) = union_tags(&["trustd".to_string()], &["Trustd".to_string()]);
        assert_eq!(u2.len(), 2);
        assert_eq!(a2, vec!["Trustd"]);
    }

    #[test]
    fn merged_content_appends_marker_and_rerun_guard_skips_when_present() {
        let (content, note) = plan_content("target body", "source body", "abc123");
        let c = content.unwrap();
        assert!(c.starts_with("target body"));
        assert!(c.contains("## Merged from abc123"));
        assert!(c.ends_with("source body"));
        assert!(note.is_none());

        let (content2, note2) = plan_content(&c, "source body", "abc123");
        assert!(content2.is_none());
        assert!(note2.unwrap().contains("re-run guard"));
    }

    fn sample_plan() -> MergePlan {
        MergePlan {
            redirects: vec![
                ("e1".into(), redirect_patch(true, SEM, "tgt")),
                ("e2".into(), redirect_patch(false, SEM, "tgt")),
            ],
            delete_conflict_target: vec!["t1".into()],
            drop_ids: vec!["a1".into(), "a2".into(), "p1".into(), "l1".into()],
            drop_auto: 2,
            drop_self_pair: 1,
            drop_conflict_source: 1,
            repairs: vec!["entry1".into()],
            tags_to_add: vec!["pki".into()],
            unioned_tags: vec!["cert".into(), "pki".into()],
            new_content: None,
            warnings: vec![],
        }
    }

    #[test]
    fn dry_run_preview_counts_equal_plan_counts() {
        let plan = sample_plan();
        let rendered = render_dry_run(&plan, SRC, TGT, MergeStrategy::KeepTarget);
        let v: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(v["merged"], false);
        assert_eq!(v["dry_run"], true);
        let p = &v["preview"];
        assert_eq!(p["edges_would_redirect"], json!(plan.redirects.len()));
        assert_eq!(p["edges_would_drop_duplicate"], json!(plan.drop_conflict_source));
        assert_eq!(p["edges_would_drop_auto"], json!(plan.drop_auto));
        assert_eq!(p["edges_would_drop_self_pair"], json!(plan.drop_self_pair));
        assert_eq!(
            p["target_edges_would_delete_conflict"],
            json!(plan.delete_conflict_target.len())
        );
        assert_eq!(p["entries_would_repoint"], json!(plan.repairs.len()));
        assert_eq!(p["tags_would_add"], json!(["pki"]));
        assert_eq!(p["content_would_append"], false);
        assert_eq!(p["source_would_be_deleted"], true);
        // The preview deliberately omits edges_regenerated — unknowable
        // before derive_edges runs.
        assert!(p.get("edges_regenerated").is_none());
    }

    #[test]
    fn merge_output_shapes_executed_and_aborted() {
        let plan = sample_plan();
        let counters = ExecCounters {
            entries_repointed: 1,
            target_edges_deleted_conflict: 1,
            edges_redirected: 2,
            source_edges_deleted: 4,
            edges_regenerated: 3,
        };
        let ok: serde_json::Value =
            serde_json::from_str(&render_success(&plan, &counters, SRC, TGT, MergeStrategy::KeepTarget))
                .unwrap();
        assert_eq!(ok["merged"], true);
        assert_eq!(ok["strategy"], "keep_target");
        assert_eq!(ok["edges_redirected"], 2);
        assert_eq!(ok["edges_dropped_auto"], 2);
        assert_eq!(ok["edges_dropped_self_pair"], 1);
        assert_eq!(ok["edges_dropped_duplicate"], 1);
        assert_eq!(ok["target_edges_deleted_conflict"], 1);
        assert_eq!(ok["edges_regenerated"], 3);
        assert_eq!(ok["entries_repointed"], 1);
        assert_eq!(ok["source"]["id"], "src");
        assert_eq!(ok["target"]["id"], "tgt");

        let ab: serde_json::Value = serde_json::from_str(&abort_json(
            "step_3_redirects",
            &counters,
            "boom".into(),
            RERUN_ACTION,
        ))
        .unwrap();
        assert_eq!(ab["merged"], false);
        assert_eq!(ab["aborted_at"], "step_3_redirects");
        assert_eq!(ab["completed"]["edges_redirected"], 2);
        assert_eq!(ab["error"], "boom");
        assert!(ab["action"].as_str().unwrap().contains("re-run"));
    }

    #[test]
    fn knowledge_merge_schema_is_plain_object() {
        // Anthropic rejects top-level oneOf/allOf/anyOf in input_schema —
        // the strategy enum must render inline/under definitions, never as
        // a top-level combinator.
        let schema = schemars::schema_for!(KnowledgeMergeArgs);
        let v = serde_json::to_value(&schema).unwrap();
        assert!(v.get("oneOf").is_none());
        assert!(v.get("allOf").is_none());
        assert!(v.get("anyOf").is_none());
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("object"));
    }
}
