//! `knowledge_audit` — read-only hygiene detection over the knowledge graph.
//!
//! Three checks (dedup / orphans / rot) over `memory.semantic` +
//! `memory.procedural`, with edge context folded from ONE exhaustive
//! projected scan of `memory.edges` into compact per-node aggregates (never
//! a full adjacency list). A node another node supersedes (a declared
//! `supersedes` edge, `knowledge::supersession`) is history: it leaves every
//! check and is counted in the summary. Detection only — the destructive
//! counterpart is `knowledge_merge` (`knowledge/merge.rs`), which consumes
//! this tool's `dedup_candidates` verbatim (findings emit full collection
//! names for that reason).
//!
//! Every check holds on any operator's graph: structure (orphans), a
//! declaration (history), or token similarity under a tunable threshold
//! (dedup, and rot's newer near-duplicate). A contradiction check stood here
//! until 2026-10-11. Token overlap measures how related two notes are, not
//! whether they disagree: on that day's backup it flagged 894 pairs, 558 of
//! them two-tag facts sharing one tag, and found 1 of the 23 contradictions
//! the intelligence had linked itself. Rot read a `refines` edge as
//! "replaced by" until then; `knowledge_link` defines it as "more specific
//! than".
//!
//! Failure asymmetry, deliberate: a saturated NODE window only shrinks the
//! candidate set (warn-and-continue), but a failed EDGE-scan page would
//! under-count degrees and fabricate false orphans directly upstream of a
//! destructive merge pipeline — so any page error aborts the whole audit.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::client::MEMORY_FETCH_WINDOW;
use crate::db::WardsonDbClient;
use crate::tools::registry::DispatchContext;

use super::tools::{next_scan_page, scan_page_query_body, ScanPage};
use super::types::{content_preview, EdgeType, SESSION_DELETED_TAG, SUPERSESSION_FIELD, SUPERSESSION_KEY};

/// Page size for the exhaustive edge scan — same bound rationale as
/// `ORPHAN_SCAN_PAGE` (well under the server's 100k `--max-query-limit`,
/// bounds per-page memory).
const AUDIT_SCAN_PAGE: usize = 20_000;
/// Pairwise dedup/rot cap per `(collection, category)` group,
/// newest-first — precedent `MEMORY_SCAN_DUPE_SCAN_CAP` (tools/sessions.rs).
/// The largest production group today (observation, 369) fits uncapped.
const AUDIT_PAIRWISE_CAP: usize = 500;
const AUDIT_MAX_RESULTS_DEFAULT: usize = 50;
const AUDIT_MAX_RESULTS_CEILING: usize = 200;
const AUDIT_MIN_SIMILARITY_DEFAULT: f64 = 0.75;
/// Spec: skip work-in-progress nodes — a node created in the current
/// session may simply not have edges yet.
const ORPHAN_MIN_AGE_DAYS: i64 = 1;
const ORPHAN_HIGH_CONFIDENCE_DAYS: i64 = 7;
const ROT_UNACCESSED_DAYS: i64 = 90;
/// Rot ignores nodes younger than this (arg `min_age_days` overrides) —
/// fresh knowledge hasn't had time to be superseded.
const ROT_MIN_AGE_DAYS_DEFAULT: i64 = 30;
/// A retrieval/traverse hit within this window argues AGAINST rot —
/// demotes the finding's confidence one level.
const ROT_RECENT_ACCESS_DAYS: i64 = 30;
/// Semantic nodes with less trimmed content than this carry no substance
/// beyond their tags.
const EMPTY_PAYLOAD_MIN_CHARS: usize = 30;
/// What `checks: ["contradictions"]` answers (the module comment says why).
const CONTRADICTIONS_REMOVED: &str = "contradictions is no longer a check: word overlap measures how related two notes are, not whether they disagree. Link contradicts when you read a disagreement. Pick from: dedup, orphans, rot";
/// How many nodes of a supersession cycle the warning names.
const CYCLE_NAMES_SHOWN: usize = 10;
const PSEUDO_TITLE_MAX_CHARS: usize = 80;
const W_BODY: f64 = 0.5;
const W_TITLE: f64 = 0.3;
const W_TAGS: f64 = 0.2;
/// Body-token containment floor: "X" vs "X plus one more sentence" is the
/// dominant real dupe pattern, and raw Jaccard punishes it (the ratio can
/// sit at |A|/|B|). Set-space mirror of memory_scan's Subset heuristic.
const CONTAINMENT_SCORE_FLOOR: f64 = 0.8;

/// `(collection, id)` — the codebase-wide node key.
type NodeKey = (String, String);

// ── argument resolution ──────────────────────────────────────────────────

/// Accept both the spec's short vocabulary and full collection names;
/// reject everything else. Shared with `knowledge_merge`. Same restriction
/// as `knowledge_update`: identity.graph is projection-managed (untouchable
/// by curation tools), memory.entries has its own paths (forget /
/// memory_dedup).
pub(crate) fn resolve_kg_collection(name: &str) -> Result<&'static str, String> {
    match name.trim().to_lowercase().as_str() {
        "semantic" | "memory.semantic" => Ok("memory.semantic"),
        "procedural" | "memory.procedural" => Ok("memory.procedural"),
        other => Err(format!(
            "collection '{}' not supported — only memory.semantic or memory.procedural \
             (identity.graph is projection-managed; use forget for memory.entries)",
            other
        )),
    }
}

/// Canonical audited-collection order (semantic first, mirroring the dump's
/// nodes-first canonical ordering), regardless of input order.
const AUDIT_COLLECTIONS: [&str; 2] = ["memory.semantic", "memory.procedural"];

fn resolve_audit_collections(requested: Option<&[String]>) -> Result<Vec<&'static str>, String> {
    let Some(req) = requested else {
        return Ok(AUDIT_COLLECTIONS.to_vec());
    };
    if req.is_empty() {
        return Err(
            "collections is empty — omit it to scan both, or pick from: semantic, procedural"
                .into(),
        );
    }
    let mut picked: Vec<&'static str> = Vec::new();
    for name in req {
        let coll = resolve_kg_collection(name)?;
        if !picked.contains(&coll) {
            picked.push(coll);
        }
    }
    Ok(AUDIT_COLLECTIONS
        .iter()
        .copied()
        .filter(|c| picked.contains(c))
        .collect())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AuditCheck {
    Dedup,
    Orphans,
    Rot,
}

const ALL_CHECKS: [AuditCheck; 3] = [AuditCheck::Dedup, AuditCheck::Orphans, AuditCheck::Rot];

impl AuditCheck {
    fn as_str(self) -> &'static str {
        match self {
            AuditCheck::Dedup => "dedup",
            AuditCheck::Orphans => "orphans",
            AuditCheck::Rot => "rot",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "dedup" => Some(AuditCheck::Dedup),
            "orphans" => Some(AuditCheck::Orphans),
            "rot" => Some(AuditCheck::Rot),
            _ => None,
        }
    }
}

fn resolve_audit_checks(requested: Option<&[String]>) -> Result<Vec<AuditCheck>, String> {
    let Some(req) = requested else {
        return Ok(ALL_CHECKS.to_vec());
    };
    if req.is_empty() {
        return Err(
            "checks is empty — omit it to run all three, or pick from: dedup, orphans, rot".into(),
        );
    }
    let mut picked: Vec<AuditCheck> = Vec::new();
    for name in req {
        if name.trim().eq_ignore_ascii_case("contradictions") {
            return Err(CONTRADICTIONS_REMOVED.into());
        }
        let Some(check) = AuditCheck::parse(name) else {
            return Err(format!(
                "unknown check '{}' — pick from: dedup, orphans, rot",
                name
            ));
        };
        if !picked.contains(&check) {
            picked.push(check);
        }
    }
    Ok(ALL_CHECKS
        .iter()
        .copied()
        .filter(|c| picked.contains(c))
        .collect())
}

// ── node metadata (precomputed once; the pairwise pass is set math only) ──

struct NodeMeta {
    id: String,
    collection: &'static str,
    /// Semantic category, or the literal "procedural".
    category: String,
    /// Display title: semantic = first content line (preview-truncated);
    /// procedural = its title field.
    pseudo_title: String,
    title_tokens: HashSet<String>,
    body_tokens: HashSet<String>,
    tags_lower: HashSet<String>,
    empty_payload: bool,
    created_at: Option<DateTime<Utc>>,
    last_accessed: Option<DateTime<Utc>>,
}

impl NodeMeta {
    fn key(&self) -> NodeKey {
        (self.collection.to_string(), self.id.clone())
    }
}

/// Lowercase alphanumeric runs, minimum length 3 — delegates to the shared
/// rule (`knowledge/text.rs`) so audit similarity and retrieval content
/// matching stay structurally identical, never coincidentally so.
fn tokenize(s: &str) -> HashSet<String> {
    super::text::content_tokens(s)
}

fn parse_ts(doc: &serde_json::Value, field: &str) -> Option<DateTime<Utc>> {
    doc.get(field)
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

fn parse_node_meta(doc: &serde_json::Value, collection: &'static str) -> Option<NodeMeta> {
    let id = doc.get("_id")?.as_str()?.to_string();
    let (category, title_source, body, empty_payload) = if collection == "memory.semantic" {
        let content = doc.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let category = doc
            .get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("uncategorized")
            .to_string();
        let first_line = content.lines().next().unwrap_or("").to_string();
        let empty = content.trim().chars().count() < EMPTY_PAYLOAD_MIN_CHARS;
        (category, first_line, content.to_string(), empty)
    } else {
        let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let description = doc.get("description").and_then(|v| v.as_str()).unwrap_or("");
        let steps_empty = doc
            .get("steps")
            .and_then(|v| v.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true);
        let empty = description.trim().is_empty() && steps_empty;
        ("procedural".to_string(), title, description.to_string(), empty)
    };
    let tags_lower: HashSet<String> = doc
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str())
                .map(|t| t.to_lowercase())
                .collect()
        })
        .unwrap_or_default();
    Some(NodeMeta {
        id,
        collection,
        category,
        pseudo_title: content_preview(&title_source, PSEUDO_TITLE_MAX_CHARS),
        title_tokens: tokenize(&title_source),
        body_tokens: tokenize(&body),
        tags_lower,
        empty_payload,
        created_at: parse_ts(doc, "created_at"),
        last_accessed: parse_ts(doc, "last_accessed"),
    })
}

// ── similarity (zero deps — set math over precomputed tokens) ────────────

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = (a.len() + b.len()) as f64 - inter;
    inter / union
}

/// `|A ∩ B| / max(|A|, |B|)` — byte-matches the `tag_overlap` edge-weight
/// formula in `edges.rs` (deliberately NOT standard Jaccard).
fn overlap_max(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    inter / (a.len().max(b.len()) as f64)
}

/// Dedup score plus whether the containment floor fired.
fn dedup_score(a: &NodeMeta, b: &NodeMeta) -> (f64, bool) {
    let title_sim = jaccard(&a.title_tokens, &b.title_tokens);
    let body_sim = jaccard(&a.body_tokens, &b.body_tokens);
    let tag_sim = overlap_max(&a.tags_lower, &b.tags_lower);
    let raw = W_BODY * body_sim + W_TITLE * title_sim + W_TAGS * tag_sim;
    let containment = !a.body_tokens.is_empty() && !b.body_tokens.is_empty() && {
        let (small, large) = if a.body_tokens.len() <= b.body_tokens.len() {
            (&a.body_tokens, &b.body_tokens)
        } else {
            (&b.body_tokens, &a.body_tokens)
        };
        small.is_subset(large)
    };
    if containment {
        (raw.max(CONTAINMENT_SCORE_FLOOR), true)
    } else {
        (raw, false)
    }
}

/// Whether two nodes of one collection say the same thing by the audit's
/// own measure: `dedup_score` at the default threshold. The audit groups by
/// category before it pairs; this does not, because a duplicate filed under
/// another category is a duplicate still. `remember` marks such a link
/// candidate, so that a memory the graph already holds is merged rather
/// than linked.
pub(crate) fn near_duplicate(a: &serde_json::Value, b: &serde_json::Value, collection: &'static str) -> bool {
    match (parse_node_meta(a, collection), parse_node_meta(b, collection)) {
        (Some(a), Some(b)) => dedup_score(&a, &b).0 >= AUDIT_MIN_SIMILARITY_DEFAULT,
        _ => false,
    }
}

/// Order-independent pair key.
fn pair_key(a: &NodeKey, b: &NodeKey) -> (NodeKey, NodeKey) {
    if a <= b {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
}

// ── edge aggregates (one exhaustive projected scan) ──────────────────────

#[derive(Default)]
struct EdgeAggregates {
    /// Edge DOCS touching an audited node in either role. Symmetric auto
    /// edges are two docs, so this counts docs, not logical links.
    total_degree: HashMap<NodeKey, u32>,
    /// Docs whose type is brain-created or free-form (`Other`).
    meaningful_degree: HashMap<NodeKey, u32>,
    /// Node is the TARGET of a depends_on/enables edge — something relies
    /// on it (rot's old_unaccessed guard).
    incoming_dep_enables: HashMap<NodeKey, u32>,
    refines_pairs: HashSet<(NodeKey, NodeKey)>,
    /// Declared supersessions `(newer, older)` whose older end is audited:
    /// `supersedes` edges carrying the marker (`knowledge::supersession`).
    declarations: Vec<(NodeKey, NodeKey)>,
    edges_scanned: usize,
    meaningful_total: u64,
}

/// Meaningful = brain-created 5 + free-form (`Other`). Excluded: the three
/// auto-derived types AND `derived_from` — every promoted node has one, so
/// counting it would mask orphanhood exactly where it matters (spec listed
/// only same_session/tag_overlap; temporal and derived_from follow the same
/// rationale: "no structural connections").
fn edge_is_meaningful(t: &EdgeType) -> bool {
    t.is_brain_created() || matches!(t, EdgeType::Other(_))
}

/// Fold one projected edge doc into the aggregates. Maps are keyed only for
/// audited nodes (`node_keys`) so 285k edges never materialize in memory;
/// the pair sets record only pairs where BOTH endpoints are audited (the
/// only place they are consulted).
fn accumulate_edge(
    agg: &mut EdgeAggregates,
    doc: &serde_json::Value,
    node_keys: &HashSet<NodeKey>,
) {
    agg.edges_scanned += 1;
    let get = |f: &str| doc.get(f).and_then(|v| v.as_str()).unwrap_or("");
    let src: NodeKey = (get("source_collection").to_string(), get("source_id").to_string());
    let tgt: NodeKey = (get("target_collection").to_string(), get("target_id").to_string());
    let etype = EdgeType::parse_lossy(get("edge_type"));
    let meaningful = etype.as_ref().map(edge_is_meaningful).unwrap_or(false);
    if meaningful {
        agg.meaningful_total += 1;
    }
    let src_in = node_keys.contains(&src);
    let tgt_in = node_keys.contains(&tgt);
    if src_in {
        *agg.total_degree.entry(src.clone()).or_default() += 1;
        if meaningful {
            *agg.meaningful_degree.entry(src.clone()).or_default() += 1;
        }
    }
    if tgt_in {
        *agg.total_degree.entry(tgt.clone()).or_default() += 1;
        if meaningful {
            *agg.meaningful_degree.entry(tgt.clone()).or_default() += 1;
        }
        if matches!(etype, Some(EdgeType::DependsOn) | Some(EdgeType::Enables)) {
            *agg.incoming_dep_enables.entry(tgt.clone()).or_default() += 1;
        }
    }
    // A declaration is the type and the marker. The scan's projection
    // carries the marker under its dotted path; a whole document under
    // `metadata`.
    let marked = doc
        .get(SUPERSESSION_FIELD)
        .or_else(|| doc.get("metadata").and_then(|m| m.get(SUPERSESSION_KEY)))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if tgt_in && marked && matches!(etype, Some(EdgeType::Supersedes)) {
        agg.declarations.push((src.clone(), tgt.clone()));
    }
    if src_in && tgt_in && matches!(etype, Some(EdgeType::Refines)) {
        agg.refines_pairs.insert(pair_key(&src, &tgt));
    }
}

/// One page of the audit's exhaustive edge scan — the sanctioned no-sort
/// key-order pagination (`scan_page_query_body`) plus a server-side
/// projection: the aggregates need endpoints, type and the supersession
/// marker only, never payloads.
fn audit_edge_page_body(page_limit: usize, page: &ScanPage) -> serde_json::Value {
    let mut body = scan_page_query_body(&json!({}), page_limit, page);
    body["fields"] = json!([
        "source_id",
        "source_collection",
        "target_id",
        "target_collection",
        "edge_type",
        SUPERSESSION_FIELD
    ]);
    body
}

/// The nodes the declarations make history: an older end reached from a
/// current node through a chain of declarations (C supersedes B supersedes
/// A: A and B are history). A node in a cycle that reaches no current node
/// stays in the checks; the second value names those, sorted, for a
/// warning.
fn history_of(declarations: &[(NodeKey, NodeKey)]) -> (HashSet<NodeKey>, Vec<NodeKey>) {
    let superseded: HashSet<&NodeKey> = declarations.iter().map(|(_, older)| older).collect();
    let mut history: HashSet<NodeKey> = HashSet::new();
    loop {
        let mut grew = false;
        for (newer, older) in declarations {
            let newer_stands = !superseded.contains(newer) || history.contains(newer);
            if newer_stands && history.insert(older.clone()) {
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    let mut cycles: Vec<NodeKey> =
        superseded.into_iter().filter(|k| !history.contains(*k)).cloned().collect();
    cycles.sort();
    (history, cycles)
}

async fn scan_edges(
    db: &WardsonDbClient,
    node_keys: &HashSet<NodeKey>,
) -> Result<EdgeAggregates, String> {
    let mut agg = EdgeAggregates::default();
    let mut page = ScanPage::Offset(0);
    loop {
        let (docs, meta) = db
            .query_with_meta("memory.edges", &audit_edge_page_body(AUDIT_SCAN_PAGE, &page))
            .await
            .map_err(|e| {
                format!(
                    "audit aborted: edge scan failed at {} — a partial scan under-counts \
                     degrees and would fabricate false orphans ({})",
                    page.label(),
                    e
                )
            })?;
        if docs.is_empty() {
            break;
        }
        let page_len = docs.len();
        for doc in &docs {
            accumulate_edge(&mut agg, doc, node_keys);
        }
        match next_scan_page(&meta, &page, page_len, AUDIT_SCAN_PAGE) {
            Some(next) => page = next,
            None => break,
        }
    }
    Ok(agg)
}

// ── checks (pure functions over NodeMeta + EdgeAggregates) ───────────────

fn node_ref(n: &NodeMeta) -> serde_json::Value {
    json!({
        "collection": n.collection,
        "id": n.id,
        "title": n.pseudo_title,
        "category": n.category,
    })
}

fn age_days(ts: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<i64> {
    ts.map(|t| (now - t).num_days())
}

/// Rot's supersession evidence: the best (highest-similarity) NEWER
/// near-duplicate found for an older node during the pairwise pass.
struct Supersession {
    newer: NodeKey,
    newer_title: String,
    similarity: f64,
}

/// Record the OLDER node of a near-duplicate pair as superseded by the
/// newer one, keeping the highest-similarity witness per older node. Equal
/// or unknown timestamps can't be ordered — no supersession.
fn note_supersession(
    sups: &mut HashMap<NodeKey, Supersession>,
    a: &NodeMeta,
    b: &NodeMeta,
    score: f64,
) {
    let (Some(ca), Some(cb)) = (a.created_at, b.created_at) else {
        return;
    };
    if ca == cb {
        return;
    }
    let (older, newer) = if ca < cb { (a, b) } else { (b, a) };
    let entry = sups.entry(older.key()).or_insert_with(|| Supersession {
        newer: newer.key(),
        newer_title: newer.pseudo_title.clone(),
        similarity: score,
    });
    if score > entry.similarity {
        *entry = Supersession {
            newer: newer.key(),
            newer_title: newer.pseudo_title.clone(),
            similarity: score,
        };
    }
}

#[derive(Default)]
struct PairwiseFindings {
    dedup: Vec<serde_json::Value>,
    dedup_total: usize,
    /// older node → its best newer near-duplicate (rot's only witness).
    supersessions: HashMap<NodeKey, Supersession>,
    warnings: Vec<String>,
}

/// Dedup and rot's supersession probe share one pairwise pass per
/// `(collection, category)` group (pairs never cross groups). History is
/// left out before the cap. Groups iterate in sorted key order and nodes
/// keep their newest-first fetch order, so output is deterministic.
fn pairwise_findings(
    nodes: &[NodeMeta],
    agg: &EdgeAggregates,
    history: &HashSet<NodeKey>,
    min_similarity: f64,
    want_dedup: bool,
    want_rot: bool,
) -> PairwiseFindings {
    let mut out = PairwiseFindings::default();
    if !want_dedup && !want_rot {
        return out;
    }

    let mut groups: HashMap<(&'static str, &str), Vec<&NodeMeta>> = HashMap::new();
    for n in nodes {
        if history.contains(&n.key()) {
            continue;
        }
        groups.entry((n.collection, n.category.as_str())).or_default().push(n);
    }
    let mut keys: Vec<(&'static str, &str)> = groups.keys().copied().collect();
    keys.sort();

    let mut scored_dedup: Vec<(f64, bool, &NodeMeta, &NodeMeta)> = Vec::new();

    for key in keys {
        let group = &groups[&key];
        if group.len() > AUDIT_PAIRWISE_CAP {
            out.warnings.push(format!(
                "pairwise pass (dedup and rot's near-duplicate probe) limited to the {} newest of {} nodes in {}/{}",
                AUDIT_PAIRWISE_CAP,
                group.len(),
                key.0,
                key.1
            ));
        }
        let capped = &group[..group.len().min(AUDIT_PAIRWISE_CAP)];
        for i in 0..capped.len() {
            for j in (i + 1)..capped.len() {
                let (a, b) = (capped[i], capped[j]);
                // Two deletion records are two events: never a duplicate or
                // a supersession.
                if is_deletion_record(a) && is_deletion_record(b) {
                    continue;
                }
                // A refines link is intentional and means "more specific
                // than": the pair is neither a duplicate nor a replacement.
                // Without this the containment floor would flag the general
                // end of every refines pair as rot.
                if agg.refines_pairs.contains(&pair_key(&a.key(), &b.key())) {
                    continue;
                }
                let (score, subset) = dedup_score(a, b);
                if score >= min_similarity {
                    if want_dedup {
                        scored_dedup.push((score, subset, a, b));
                    }
                    // Rot's only witness: the OLDER of a near-duplicate pair
                    // is a candidate — the newer node covers it.
                    if want_rot {
                        note_supersession(&mut out.supersessions, a, b, score);
                    }
                }
            }
        }
    }

    scored_dedup.sort_by(|x, y| {
        y.0.partial_cmp(&x.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| (x.2.collection, &x.2.id).cmp(&(y.2.collection, &y.2.id)))
    });

    out.dedup_total = scored_dedup.len();

    out.dedup = scored_dedup
        .into_iter()
        .map(|(score, subset, a, b)| {
            let shared: Vec<&String> = {
                let mut v: Vec<&String> = a.tags_lower.intersection(&b.tags_lower).collect();
                v.sort();
                v
            };
            json!({
                "score": (score * 100.0).round() / 100.0,
                "node_a": node_ref(a),
                "node_b": node_ref(b),
                "overlap": shared,
                "rationale": format!(
                    "near-duplicate content in the same category '{}'{}",
                    a.category,
                    if subset { " (one body is a token-subset of the other)" } else { "" }
                ),
            })
        })
        .collect();

    out
}

/// The record a guided delete leaves (`sessions/delete_record.rs`): a node
/// that records an event, not a claim. Two records say nearly the same words
/// about two sessions and are neither duplicates nor a supersession, and a
/// record links only to what its delete preserved, which may be nothing.
fn is_deletion_record(n: &NodeMeta) -> bool {
    n.tags_lower.contains(SESSION_DELETED_TAG)
}

/// Orphans: zero meaningful edges, at least a day old. Unparseable
/// `created_at` → included with `age_days: null` at low confidence.
fn orphan_findings(
    nodes: &[NodeMeta],
    agg: &EdgeAggregates,
    now: DateTime<Utc>,
) -> Vec<serde_json::Value> {
    let mut found: Vec<(u8, i64, serde_json::Value)> = Vec::new();
    for n in nodes {
        if is_deletion_record(n) {
            continue;
        }
        let key = n.key();
        if agg.meaningful_degree.get(&key).copied().unwrap_or(0) > 0 {
            continue;
        }
        let age = age_days(n.created_at, now);
        if age.is_some_and(|d| d < ORPHAN_MIN_AGE_DAYS) {
            continue;
        }
        let total = agg.total_degree.get(&key).copied().unwrap_or(0);
        let (rank, confidence) = match age {
            Some(d) if d >= ORPHAN_HIGH_CONFIDENCE_DAYS => (0u8, "high"),
            Some(_) => (1u8, "medium"),
            None => (2u8, "low"),
        };
        let finding = json!({
            "collection": n.collection,
            "id": n.id,
            "title": n.pseudo_title,
            "edge_count": total,
            "meaningful_edges": 0,
            "auto_edges_only": total > 0,
            "age_days": age,
            "confidence": confidence,
            "rationale": if total > 0 {
                "only auto-derived edges (edge_count is docs — symmetric auto edges are two docs); no structural connections"
            } else {
                "no edges at all"
            },
        });
        found.push((rank, age.unwrap_or(-1), finding));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    found.into_iter().map(|(_, _, f)| f).collect()
}

/// Rot: the older node of a near-duplicate pair, at `min_similarity` — the
/// newer node covers it. That is the only witness; age without access
/// (when nothing depends on or is enabled by the node) and an empty payload
/// raise the confidence, and a recent retrieval lowers it. History never
/// flags: its supersession is declared. Nodes younger than `min_age_days`
/// are skipped.
fn rot_findings(
    nodes: &[NodeMeta],
    agg: &EdgeAggregates,
    history: &HashSet<NodeKey>,
    supersessions: &HashMap<NodeKey, Supersession>,
    min_age_days: i64,
    now: DateTime<Utc>,
) -> Vec<serde_json::Value> {
    let mut found: Vec<(i32, i64, serde_json::Value)> = Vec::new();
    for n in nodes {
        let key = n.key();
        if history.contains(&key) {
            continue;
        }
        // Min-age gate; an unknown created_at can never satisfy a
        // newer-than comparison, so it never flags either.
        let Some(age_d) = age_days(n.created_at, now) else { continue };
        if age_d < min_age_days {
            continue;
        }
        let Some(newer) = supersessions.get(&key) else { continue };

        let mut signals: Vec<&str> = vec!["newer_near_duplicate"];
        let mut phrases: Vec<String> = vec!["a newer node covers near-identical content".into()];
        // 2 = high, 1 = medium, 0 = low.
        let mut level: i32 = 1;

        // Confidence signals — never a witness on their own.
        let mut raised = false;
        let incoming = agg.incoming_dep_enables.get(&key).copied().unwrap_or(0);
        if incoming == 0
            && n.last_accessed
                .or(n.created_at)
                .is_some_and(|ts| (now - ts).num_days() > ROT_UNACCESSED_DAYS)
        {
            signals.push("old_unaccessed");
            phrases.push("not retrieved in >90 days and nothing depends_on/enables it".into());
            raised = true;
        }
        if n.empty_payload {
            signals.push("empty_payload");
            phrases.push("no substantive content beyond title/tags".into());
            raised = true;
        }
        if raised {
            level += 1;
        }
        // Recent retrieval argues against rot.
        if n.last_accessed
            .is_some_and(|ts| (now - ts).num_days() <= ROT_RECENT_ACCESS_DAYS)
        {
            signals.push("recently_accessed");
            phrases.push(format!(
                "accessed within the last {} days — less likely rot",
                ROT_RECENT_ACCESS_DAYS
            ));
            level -= 1;
        }
        let level = level.clamp(0, 2);
        let confidence = match level {
            2 => "high",
            1 => "medium",
            _ => "low",
        };

        let finding = json!({
            "collection": n.collection,
            "id": n.id,
            "title": n.pseudo_title,
            "signals": signals,
            "age_days": age_d,
            "confidence": confidence,
            "newer_duplicate": {
                "collection": newer.newer.0,
                "id": newer.newer.1,
                "title": newer.newer_title,
                "similarity": (newer.similarity * 100.0).round() / 100.0,
            },
            "rationale": phrases.join("; "),
        });
        found.push((level, age_d, finding));
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    found.into_iter().map(|(_, _, f)| f).collect()
}

// ── pipeline ─────────────────────────────────────────────────────────────

/// (min_similarity, max_results, min_age_days) with defaults applied and
/// clamped. `min_similarity` gates BOTH dedup pairs and rot's supersession
/// probe; `min_age_days` is rot-only.
fn clamp_audit_params(
    min_similarity: Option<f64>,
    max_results: Option<u32>,
    min_age_days: Option<u32>,
) -> (f64, usize, i64) {
    (
        min_similarity
            .unwrap_or(AUDIT_MIN_SIMILARITY_DEFAULT)
            .clamp(0.0, 1.0),
        (max_results.unwrap_or(AUDIT_MAX_RESULTS_DEFAULT as u32) as usize)
            .clamp(1, AUDIT_MAX_RESULTS_CEILING),
        min_age_days
            .map(|d| d as i64)
            .unwrap_or(ROT_MIN_AGE_DAYS_DEFAULT)
            .clamp(0, 3650),
    )
}

pub(crate) async fn run_knowledge_audit(
    db: &WardsonDbClient,
    args: KnowledgeAuditArgs,
) -> Result<String, String> {
    let collections = resolve_audit_collections(args.collections.as_deref())?;
    let checks = resolve_audit_checks(args.checks.as_deref())?;
    let (min_similarity, max_results, min_age_days) =
        clamp_audit_params(args.min_similarity, args.max_results, args.min_age_days);

    let now = Utc::now();
    let mut warnings: Vec<String> = Vec::new();

    // Node windows: saturation warns and continues — it only shrinks the
    // candidate set, never fabricates findings.
    let mut nodes: Vec<NodeMeta> = Vec::new();
    for coll in &collections {
        let docs = db
            .fetch_recent(coll, MEMORY_FETCH_WINDOW)
            .await
            .map_err(|e| format!("audit failed reading {}: {}", coll, e))?;
        if docs.len() >= MEMORY_FETCH_WINDOW {
            warnings.push(format!(
                "{} node window saturated at {} — the oldest nodes were not audited",
                coll, MEMORY_FETCH_WINDOW
            ));
        }
        nodes.extend(docs.iter().filter_map(|d| parse_node_meta(d, coll)));
    }
    let node_keys: HashSet<NodeKey> = nodes.iter().map(|n| n.key()).collect();

    // Every check consumes the edge aggregates (degrees, incoming
    // structural counts, refines pairs, declared supersessions).
    let agg = scan_edges(db, &node_keys).await?;
    let (history, cycles) = history_of(&agg.declarations);
    if !cycles.is_empty() {
        let named: Vec<String> =
            cycles.iter().take(CYCLE_NAMES_SHOWN).map(|(c, i)| format!("{c}:{i}")).collect();
        warnings.push(format!(
            "{} node(s) supersede one another in a cycle that reaches no current node, so they stay in the checks: {}{}",
            cycles.len(),
            named.join(", "),
            if cycles.len() > CYCLE_NAMES_SHOWN { ", …" } else { "" }
        ));
    }

    let want = |c: AuditCheck| checks.contains(&c);
    let pairwise = pairwise_findings(
        &nodes,
        &agg,
        &history,
        min_similarity,
        want(AuditCheck::Dedup),
        want(AuditCheck::Rot),
    );
    let orphans = if want(AuditCheck::Orphans) {
        orphan_findings(&nodes, &agg, now)
    } else {
        Vec::new()
    };
    let rot = if want(AuditCheck::Rot) {
        rot_findings(&nodes, &agg, &history, &pairwise.supersessions, min_age_days, now)
    } else {
        Vec::new()
    };
    warnings.extend(pairwise.warnings);

    let (orphan_total, rot_total) = (orphans.len(), rot.len());
    let issues_found = pairwise.dedup_total + orphan_total + rot_total;

    // Windowless totals so scan coverage is honestly comparable.
    let mut total_nodes: u64 = 0;
    for coll in &collections {
        total_nodes += db.count(coll).await.unwrap_or(0);
    }
    let total_edges = db.count("memory.edges").await.unwrap_or(0);
    if (agg.edges_scanned as u64) < total_edges {
        warnings.push(format!(
            "edge scan covered {} of {} edges (collection grew mid-scan)",
            agg.edges_scanned, total_edges
        ));
    }

    let truncate = |mut v: Vec<serde_json::Value>| {
        v.truncate(max_results);
        v
    };
    let report = json!({
        "summary": {
            "nodes_scanned": nodes.len(),
            "edges_scanned": agg.edges_scanned,
            "checks_run": checks.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
            "issues_found": issues_found,
            "history": history.len(),
        },
        "dedup_candidates": truncate(pairwise.dedup),
        "orphan_nodes": truncate(orphans),
        "rot_candidates": truncate(rot),
        "stats": {
            "total_nodes": total_nodes,
            "total_edges": total_edges,
            "meaningful_edges": agg.meaningful_total,
            "orphan_count": orphan_total,
            "dedup_pair_count": pairwise.dedup_total,
            "rot_count": rot_total,
        },
        "warnings": warnings,
    });
    serde_json::to_string_pretty(&report).map_err(|e| e.to_string())
}

// ── tool descriptor ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_audit",
    description = "Read-only hygiene audit of the knowledge graph (memory.semantic and memory.procedural; identity.graph and memory.entries are out of scope). checks selects any of: dedup (near-duplicate node pairs by content/tag similarity, threshold min_similarity, default 0.75; a pair linked by refines is not one), orphans (nodes with zero meaningful edges — auto same_session/temporal/tag_overlap and derived_from provenance do not count; nodes under 1 day old are skipped, 7+ days is high confidence), rot (the older node of a near-duplicate pair, its newer duplicate named; 90+ days without access and an empty payload raise confidence; a recent retrieval lowers it; nodes younger than min_age_days are skipped, default 30). A node another node supersedes (knowledge_link supersedes) is history: it leaves every check and is counted in the summary. Returns JSON: summary, per-check findings capped at max_results each (default 50, max 200), stats, warnings. Never modifies anything. Feed dedup_candidates to knowledge_merge, dry_run first."
)]
pub struct KnowledgeAuditArgs {
    /// Collections to scan: semantic | procedural (full memory.* names also
    /// accepted). Omit for both.
    #[serde(default)]
    pub collections: Option<Vec<String>>,
    /// Checks to run: dedup | orphans | rot. Omit for all three.
    #[serde(default)]
    pub checks: Option<Vec<String>>,
    /// Similarity threshold in [0.0, 1.0] for dedup pairs AND rot's newer
    /// near-duplicate. Default 0.75.
    #[serde(default)]
    pub min_similarity: Option<f64>,
    /// Findings cap per check. Default 50, clamped to [1, 200].
    #[serde(default)]
    pub max_results: Option<u32>,
    /// Rot check only: skip nodes younger than this many days. Default 30.
    #[serde(default)]
    pub min_age_days: Option<u32>,
}

impl KnowledgeAuditArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        run_knowledge_audit(ctx.db, self)
            .await
            .map_err(DispatchError::Handler)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn meta(
        collection: &'static str,
        id: &str,
        category: &str,
        title: &str,
        body: &str,
        tags: &[&str],
    ) -> NodeMeta {
        NodeMeta {
            id: id.to_string(),
            collection,
            category: category.to_string(),
            pseudo_title: title.to_string(),
            title_tokens: tokenize(title),
            body_tokens: tokenize(body),
            tags_lower: tags.iter().map(|t| t.to_lowercase()).collect(),
            empty_payload: body.trim().chars().count() < EMPTY_PAYLOAD_MIN_CHARS,
            created_at: Some(Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()),
            last_accessed: None,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap()
    }

    fn edge_doc(sc: &str, si: &str, tc: &str, ti: &str, etype: &str) -> serde_json::Value {
        json!({
            "source_collection": sc, "source_id": si,
            "target_collection": tc, "target_id": ti,
            "edge_type": etype,
        })
    }

    fn sup_for(older: &NodeMeta, newer_title: &str, newer_id: &str, sim: f64) -> HashMap<NodeKey, Supersession> {
        let mut m = HashMap::new();
        m.insert(
            older.key(),
            Supersession {
                newer: ("memory.semantic".to_string(), newer_id.to_string()),
                newer_title: newer_title.to_string(),
                similarity: sim,
            },
        );
        m
    }

    #[test]
    fn audit_edge_page_body_no_sort_projects_endpoint_fields() {
        let body = audit_edge_page_body(20_000, &ScanPage::Offset(40_000));
        assert_eq!(body["limit"], json!(20_000));
        assert_eq!(body["offset"], json!(40_000));
        assert!(body.get("sort").is_none());
        assert!(body.get("cursor").is_none());
        assert_eq!(
            body["fields"],
            json!(["source_id", "source_collection", "target_id", "target_collection", "edge_type", SUPERSESSION_FIELD])
        );
        let cursor = audit_edge_page_body(20_000, &ScanPage::Cursor("tok".into()));
        assert_eq!(cursor["cursor"], json!("tok"));
        assert!(cursor.get("offset").is_none());
        assert!(cursor.get("sort").is_none());
    }

    #[test]
    fn resolve_collections_accepts_short_and_full_rejects_identity_and_entries() {
        assert_eq!(
            resolve_audit_collections(None).unwrap(),
            vec!["memory.semantic", "memory.procedural"]
        );
        // Short + full forms, canonical order regardless of input order.
        let got = resolve_audit_collections(Some(&[
            "procedural".to_string(),
            "memory.semantic".to_string(),
        ]))
        .unwrap();
        assert_eq!(got, vec!["memory.semantic", "memory.procedural"]);
        assert!(resolve_audit_collections(Some(&[])).is_err());
        assert!(resolve_audit_collections(Some(&["identity.graph".to_string()])).is_err());
        assert!(resolve_audit_collections(Some(&["entries".to_string()])).is_err());
        assert!(resolve_audit_collections(Some(&["memory.entries".to_string()])).is_err());
    }

    #[test]
    fn resolve_checks_defaults_all_rejects_unknown_and_empty() {
        let all = resolve_audit_checks(None).unwrap();
        assert_eq!(all, vec![AuditCheck::Dedup, AuditCheck::Orphans, AuditCheck::Rot]);
        let got = resolve_audit_checks(Some(&["rot".to_string(), "dedup".to_string()])).unwrap();
        // Canonical order regardless of input order.
        assert_eq!(got, vec![AuditCheck::Dedup, AuditCheck::Rot]);
        assert!(resolve_audit_checks(Some(&[])).is_err());
        assert!(resolve_audit_checks(Some(&["bogus".to_string()])).is_err());
    }

    /// A caller that still asks for contradictions learns why there are none.
    #[test]
    fn the_contradictions_check_is_refused_with_its_reason() {
        let refused = resolve_audit_checks(Some(&["dedup".to_string(), " Contradictions ".to_string()]));
        assert_eq!(refused, Err(CONTRADICTIONS_REMOVED.to_string()));
        assert!(CONTRADICTIONS_REMOVED.contains("not whether they disagree"));
        assert!(CONTRADICTIONS_REMOVED.contains("Link contradicts"));
    }

    #[test]
    fn tokenize_lowercases_splits_nonalnum_drops_short() {
        let t = tokenize("The cert-refresh FAILED at 03:14, v2!");
        assert!(t.contains("the")); // 3 chars — exactly at the floor
        assert!(t.contains("cert"));
        assert!(t.contains("refresh"));
        assert!(t.contains("failed"));
        assert!(!t.contains("at")); // under MIN_TOKEN_LEN
        assert!(!t.contains("v2"));
        assert!(!t.contains("03"));
    }

    #[test]
    fn jaccard_and_overlap_max_bounds_and_known_values() {
        let a: HashSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let b: HashSet<String> = ["b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
        assert!((jaccard(&a, &b) - 2.0 / 5.0).abs() < 1e-9);
        // overlap_max byte-matches the edges.rs tag_overlap formula:
        // |A ∩ B| / max(|A|, |B|).
        assert!((overlap_max(&a, &b) - 2.0 / 4.0).abs() < 1e-9);
        let empty: HashSet<String> = HashSet::new();
        assert_eq!(jaccard(&a, &empty), 0.0);
        assert_eq!(overlap_max(&empty, &b), 0.0);
    }

    #[test]
    fn near_duplicate_is_the_dedup_score_at_the_audit_threshold() {
        let a = serde_json::json!({
            "_id": "a", "content": "the cert refresh works after manual generation",
            "category": "fact", "tags": ["cert"],
        });
        // The same statement with a sentence added, filed under another
        // category: the containment floor carries it over the threshold,
        // and the category is not asked.
        let b = serde_json::json!({
            "_id": "b", "content": "the cert refresh works after manual generation and one more detail sentence",
            "category": "observation", "tags": ["cert"],
        });
        assert!(near_duplicate(&a, &b, "memory.semantic"));
        let c = serde_json::json!({
            "_id": "c", "content": "wardsondb keeps a bitmap index per collection",
            "category": "fact", "tags": ["db"],
        });
        assert!(!near_duplicate(&a, &c, "memory.semantic"));
        // A document without an id is no node.
        assert!(!near_duplicate(&serde_json::json!({"content": "x"}), &a, "memory.semantic"));
    }

    #[test]
    fn dedup_score_formula_weights_and_containment_floor() {
        let a = meta(
            "memory.semantic",
            "a",
            "fact",
            "cert refresh works",
            "the cert refresh works after manual generation",
            &["cert"],
        );
        let b = meta(
            "memory.semantic",
            "b",
            "fact",
            "cert refresh works",
            "the cert refresh works after manual generation and one more detail sentence",
            &["cert"],
        );
        let (score, subset) = dedup_score(&a, &b);
        // a's body tokens are a subset of b's — the floor fires.
        assert!(subset);
        assert!(score >= CONTAINMENT_SCORE_FLOOR);

        // Disjoint bodies: raw weighted formula, no floor.
        let c = meta("memory.semantic", "c", "fact", "alpha", "alpha beta gamma", &["x"]);
        let d = meta("memory.semantic", "d", "fact", "delta", "delta epsilon zeta", &["y"]);
        let (score_cd, subset_cd) = dedup_score(&c, &d);
        assert!(!subset_cd);
        assert!(score_cd < 1e-9);
    }

    #[test]
    fn pair_key_normalization_order_independent() {
        let a = ("memory.semantic".to_string(), "zzz".to_string());
        let b = ("memory.semantic".to_string(), "aaa".to_string());
        assert_eq!(pair_key(&a, &b), pair_key(&b, &a));
    }

    #[test]
    fn edge_aggregates_meaningful_excludes_three_auto_and_derived_from_includes_other() {
        let node: NodeKey = ("memory.semantic".to_string(), "n1".to_string());
        let keys: HashSet<NodeKey> = [node.clone()].into_iter().collect();
        let mut agg = EdgeAggregates::default();
        for etype in ["same_session", "temporal", "tag_overlap", "derived_from"] {
            accumulate_edge(&mut agg, &edge_doc("memory.semantic", "n1", "memory.entries", "e1", etype), &keys);
        }
        assert_eq!(agg.meaningful_degree.get(&node).copied().unwrap_or(0), 0);
        assert_eq!(agg.total_degree[&node], 4);
        for etype in ["enables", "related_to", "navigatesFor"] {
            accumulate_edge(&mut agg, &edge_doc("memory.semantic", "n1", "memory.semantic", "x", etype), &keys);
        }
        assert_eq!(agg.meaningful_degree[&node], 3);
        assert_eq!(agg.meaningful_total, 3);
        assert_eq!(agg.edges_scanned, 7);
    }

    #[test]
    fn edge_aggregates_incoming_dep_enables_counts_target_role_only() {
        let node: NodeKey = ("memory.semantic".to_string(), "n1".to_string());
        let keys: HashSet<NodeKey> = [node.clone()].into_iter().collect();
        let mut agg = EdgeAggregates::default();
        // n1 as TARGET of depends_on / enables → counted.
        accumulate_edge(&mut agg, &edge_doc("memory.semantic", "x", "memory.semantic", "n1", "depends_on"), &keys);
        accumulate_edge(&mut agg, &edge_doc("memory.semantic", "y", "memory.semantic", "n1", "enables"), &keys);
        // n1 as SOURCE of depends_on → not incoming.
        accumulate_edge(&mut agg, &edge_doc("memory.semantic", "n1", "memory.semantic", "z", "depends_on"), &keys);
        // n1 as target of contradicts → not a dependency signal.
        accumulate_edge(&mut agg, &edge_doc("memory.semantic", "w", "memory.semantic", "n1", "contradicts"), &keys);
        assert_eq!(agg.incoming_dep_enables[&node], 2);
    }

    #[test]
    fn edge_aggregates_ignore_endpoints_outside_node_set() {
        let keys: HashSet<NodeKey> =
            [("memory.semantic".to_string(), "n1".to_string())].into_iter().collect();
        let mut agg = EdgeAggregates::default();
        accumulate_edge(&mut agg, &edge_doc("memory.entries", "e1", "memory.entries", "e2", "same_session"), &keys);
        assert!(agg.total_degree.is_empty());
        assert_eq!(agg.edges_scanned, 1);
        // Pair sets only record pairs where BOTH endpoints are audited.
        accumulate_edge(&mut agg, &edge_doc("memory.semantic", "n1", "memory.semantic", "other", "refines"), &keys);
        assert!(agg.refines_pairs.is_empty());
    }

    /// A declaration is a marked supersedes edge into an audited node. The
    /// scan projects the marker under its dotted path.
    #[test]
    fn edge_aggregates_record_only_marked_declarations() {
        let old: NodeKey = ("memory.semantic".to_string(), "old".to_string());
        let keys: HashSet<NodeKey> = [old.clone()].into_iter().collect();
        let mut agg = EdgeAggregates::default();
        let declared = |etype: &str, target: &str, marked: bool| {
            let mut doc = edge_doc("memory.semantic", "new", "memory.semantic", target, etype);
            if marked {
                doc[SUPERSESSION_FIELD] = json!(true);
            }
            doc
        };
        accumulate_edge(&mut agg, &declared("supersedes", "old", false), &keys);
        accumulate_edge(&mut agg, &declared("refines", "old", true), &keys);
        accumulate_edge(&mut agg, &declared("supersedes", "elsewhere", true), &keys);
        assert!(agg.declarations.is_empty());
        accumulate_edge(&mut agg, &declared("supersedes", "old", true), &keys);
        assert_eq!(agg.declarations, vec![(("memory.semantic".to_string(), "new".to_string()), old.clone())]);
        // A declaration is a meaningful edge for its older end.
        assert_eq!(agg.meaningful_degree[&old], 3);
    }

    fn key(id: &str) -> NodeKey {
        ("memory.semantic".to_string(), id.to_string())
    }

    #[test]
    fn a_chain_makes_every_link_history() {
        // c supersedes b, b supersedes a: c stands, a and b are history.
        let (history, cycles) = history_of(&[(key("b"), key("a")), (key("c"), key("b"))]);
        assert_eq!(history, [key("a"), key("b")].into_iter().collect());
        assert!(cycles.is_empty());
    }

    /// Nodes that supersede one another in a ring reach no current node:
    /// none of them is history, so they stay in the checks and are named.
    #[test]
    fn a_supersession_cycle_stays_in_the_checks_and_is_named() {
        let (history, cycles) =
            history_of(&[(key("a"), key("b")), (key("b"), key("c")), (key("c"), key("a")), (key("d"), key("e"))]);
        assert_eq!(history, [key("e")].into_iter().collect());
        assert_eq!(cycles, vec![key("a"), key("b"), key("c")]);
    }

    /// A superseded node is history: its near-duplicate successor is no
    /// dedup pair, it is no rot, and its declaration keeps it from being an
    /// orphan.
    #[test]
    fn a_superseded_node_is_history_in_every_check() {
        let mut old = meta("memory.semantic", "old", "fact", "same title here", "identical body content tokens", &["tag1"]);
        old.created_at = Some(now() - chrono::Duration::days(90));
        let mut new = meta("memory.semantic", "new", "fact", "same title here", "identical body content tokens", &["tag1"]);
        new.created_at = Some(now() - chrono::Duration::days(40));
        let keys: HashSet<NodeKey> = [old.key(), new.key()].into_iter().collect();
        let mut agg = EdgeAggregates::default();
        let mut declaration = edge_doc("memory.semantic", "new", "memory.semantic", "old", "supersedes");
        declaration[SUPERSESSION_FIELD] = json!(true);
        accumulate_edge(&mut agg, &declaration, &keys);
        let (history, _) = history_of(&agg.declarations);
        assert_eq!(history, [old.key()].into_iter().collect());
        let nodes = vec![old, new];

        let pw = pairwise_findings(&nodes, &agg, &history, 0.75, true, true);
        assert_eq!(pw.dedup_total, 0);
        assert!(pw.supersessions.is_empty());
        // Even a witness handed in does not flag history.
        let sups = sup_for(&nodes[0], "new", "new", 1.0);
        assert!(rot_findings(&nodes, &agg, &history, &sups, 0, now()).is_empty());
        assert!(orphan_findings(&nodes, &agg, now()).is_empty());

        // Without the declaration the same pair is a duplicate and rot.
        let pw = pairwise_findings(&nodes, &EdgeAggregates::default(), &HashSet::new(), 0.75, true, true);
        assert_eq!(pw.dedup_total, 1);
        assert_eq!(pw.supersessions.len(), 1);
    }

    #[test]
    fn orphan_excludes_under_one_day_high_confidence_at_seven() {
        let agg = EdgeAggregates::default();
        let mut fresh = meta("memory.semantic", "fresh", "fact", "t", "some body content here today", &[]);
        fresh.created_at = Some(now() - chrono::Duration::hours(5));
        let mut week = meta("memory.semantic", "week", "fact", "t", "some body content here today", &[]);
        week.created_at = Some(now() - chrono::Duration::days(10));
        let mut unparseable = meta("memory.semantic", "nodate", "fact", "t", "some body content here today", &[]);
        unparseable.created_at = None;
        let found = orphan_findings(&[fresh, week, unparseable], &agg, now());
        let ids: Vec<&str> = found.iter().map(|f| f["id"].as_str().unwrap()).collect();
        assert!(!ids.contains(&"fresh"));
        assert!(ids.contains(&"week"));
        assert!(ids.contains(&"nodate"));
        let week_f = found.iter().find(|f| f["id"] == "week").unwrap();
        assert_eq!(week_f["confidence"], "high");
        assert_eq!(week_f["edge_count"], 0);
        assert_eq!(week_f["auto_edges_only"], false);
        let nodate_f = found.iter().find(|f| f["id"] == "nodate").unwrap();
        assert_eq!(nodate_f["confidence"], "low");
        assert!(nodate_f["age_days"].is_null());
        // High confidence sorts first.
        assert_eq!(found[0]["id"], "week");
    }

    #[test]
    fn orphan_meaningful_edge_suppresses_auto_only_flags() {
        let node = meta("memory.semantic", "n1", "fact", "t", "some body content here today", &[]);
        let mut aged = node;
        aged.created_at = Some(now() - chrono::Duration::days(30));
        let keys: HashSet<NodeKey> = [aged.key()].into_iter().collect();
        let mut agg = EdgeAggregates::default();
        accumulate_edge(&mut agg, &edge_doc("memory.semantic", "n1", "memory.entries", "e", "same_session"), &keys);
        let found = orphan_findings(std::slice::from_ref(&aged), &agg, now());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["auto_edges_only"], true);
        assert_eq!(found[0]["edge_count"], 1);
        // A meaningful edge clears it.
        accumulate_edge(&mut agg, &edge_doc("memory.semantic", "x", "memory.semantic", "n1", "refines"), &keys);
        assert!(orphan_findings(std::slice::from_ref(&aged), &agg, now()).is_empty());
    }

    #[test]
    fn rot_flags_only_the_older_of_a_near_duplicate_pair() {
        // No newer near-duplicate: nothing, however old the node and
        // whatever its title says.
        let agg = EdgeAggregates::default();
        let titled = meta("memory.semantic", "a", "decision", "Final architecture decision v2", "body long enough to not be empty payload", &[]);
        assert!(rot_findings(std::slice::from_ref(&titled), &agg, &HashSet::new(), &HashMap::new(), ROT_MIN_AGE_DAYS_DEFAULT, now()).is_empty());

        // With one, the node flags at medium: the title no longer raises it.
        let sups = sup_for(&titled, "replacement", "n1", 0.9);
        let found = rot_findings(std::slice::from_ref(&titled), &agg, &HashSet::new(), &sups, ROT_MIN_AGE_DAYS_DEFAULT, now());
        assert_eq!(found.len(), 1);
        let sigs: Vec<&str> = found[0]["signals"].as_array().unwrap().iter().filter_map(|s| s.as_str()).collect();
        assert_eq!(sigs, vec!["newer_near_duplicate"]);
        assert_eq!(found[0]["confidence"], "medium");
        assert_eq!(found[0]["newer_duplicate"]["id"], "n1");
        assert_eq!(found[0]["newer_duplicate"]["similarity"], 0.9);
        assert!(found[0].get("superseded_by").is_none());
    }

    #[test]
    fn rot_min_age_skips_young_nodes() {
        let agg = EdgeAggregates::default();
        let mut young = meta("memory.semantic", "y", "fact", "t", "body long enough to not be empty payload", &[]);
        young.created_at = Some(now() - chrono::Duration::days(10));
        let sups = sup_for(&young, "newer", "n1", 0.9);
        // 10 days < default 30 → skipped even with supersession evidence.
        assert!(rot_findings(std::slice::from_ref(&young), &agg, &HashSet::new(), &sups, ROT_MIN_AGE_DAYS_DEFAULT, now()).is_empty());
        // min_age_days = 0 admits it.
        assert_eq!(rot_findings(std::slice::from_ref(&young), &agg, &HashSet::new(), &sups, 0, now()).len(), 1);
    }

    #[test]
    fn rot_recent_access_demotes_confidence() {
        let agg = EdgeAggregates::default();
        let mut node = meta("memory.semantic", "a", "fact", "plain title", "body long enough to not be empty payload", &[]);
        node.last_accessed = Some(now() - chrono::Duration::days(5));
        let sups = sup_for(&node, "newer", "n1", 0.9);
        let found = rot_findings(std::slice::from_ref(&node), &agg, &HashSet::new(), &sups, ROT_MIN_AGE_DAYS_DEFAULT, now());
        assert_eq!(found.len(), 1);
        let sigs: Vec<&str> = found[0]["signals"].as_array().unwrap().iter().filter_map(|s| s.as_str()).collect();
        assert!(sigs.contains(&"recently_accessed"));
        assert_eq!(found[0]["confidence"], "low"); // medium primary − recent access
    }

    /// `refines` means "more specific than". The general node and the one
    /// that adds detail to it share every word of the general one, so the
    /// containment floor scores them a near-duplicate; linked by refines,
    /// they are neither a duplicate nor a replacement.
    #[test]
    fn a_refines_pair_never_flags_rot() {
        let mut general = meta("memory.semantic", "old", "fact", "certificates live in the pki directory", "certificates live in the pki directory", &["pki"]);
        general.created_at = Some(now() - chrono::Duration::days(60));
        let mut detail = meta(
            "memory.semantic",
            "new",
            "fact",
            "certificates live in the pki directory",
            "certificates live in the pki directory and the ca cert is ca.pem",
            &["pki"],
        );
        detail.created_at = Some(now() - chrono::Duration::days(40));
        assert!(dedup_score(&general, &detail).0 >= AUDIT_MIN_SIMILARITY_DEFAULT, "a containment pair");
        let mut agg = EdgeAggregates::default();
        agg.refines_pairs.insert(pair_key(&general.key(), &detail.key()));
        let nodes = vec![general, detail];
        let pw = pairwise_findings(&nodes, &agg, &HashSet::new(), AUDIT_MIN_SIMILARITY_DEFAULT, true, true);
        assert_eq!(pw.dedup_total, 0);
        assert!(pw.supersessions.is_empty());
        assert!(rot_findings(&nodes, &agg, &HashSet::new(), &pw.supersessions, ROT_MIN_AGE_DAYS_DEFAULT, now()).is_empty());
    }

    #[test]
    fn pairwise_supersession_flags_older_of_near_duplicates() {
        let mut a = meta("memory.semantic", "a", "fact", "same title here", "identical body content tokens", &["tag1"]);
        a.created_at = Some(now() - chrono::Duration::days(90));
        let mut b = meta("memory.semantic", "b", "fact", "same title here", "identical body content tokens", &["tag1"]);
        b.created_at = Some(now() - chrono::Duration::days(40));
        let older_key = a.key();
        let newer_key = b.key();
        let pw = pairwise_findings(&[a, b], &EdgeAggregates::default(), &HashSet::new(), 0.75, false, true);
        assert_eq!(pw.supersessions.len(), 1);
        let s = pw.supersessions.get(&older_key).expect("older node is the candidate");
        assert_eq!(s.newer, newer_key);
        assert!(s.similarity >= 0.99);
    }

    #[test]
    fn rot_old_unaccessed_is_tiebreaker_guarded_by_incoming_dep_enables() {
        let mk = || {
            let mut n = meta(
                "memory.semantic",
                "old",
                "fact",
                "plain title",
                "body long enough to not be empty payload",
                &[],
            );
            n.created_at = Some(now() - chrono::Duration::days(200));
            n.last_accessed = None; // falls back to created_at
            n
        };
        let old = mk();
        let keys: HashSet<NodeKey> = [old.key()].into_iter().collect();
        let sups = sup_for(&old, "newer", "n1", 0.85);

        // Superseded + >90d unaccessed: the staleness tiebreaker lifts
        // medium → high.
        let found = rot_findings(std::slice::from_ref(&old), &EdgeAggregates::default(), &HashSet::new(), &sups, ROT_MIN_AGE_DAYS_DEFAULT, now());
        assert_eq!(found.len(), 1);
        assert!(found[0]["signals"].as_array().unwrap().iter().any(|s| s == "old_unaccessed"));
        assert_eq!(found[0]["confidence"], "high");

        // An incoming depends_on suppresses the staleness tiebreaker (the
        // node is load-bearing) — still flagged, but only medium.
        let depended = mk();
        let mut agg2 = EdgeAggregates::default();
        accumulate_edge(
            &mut agg2,
            &edge_doc("memory.semantic", "x", "memory.semantic", "old", "depends_on"),
            &keys,
        );
        let found2 = rot_findings(std::slice::from_ref(&depended), &agg2, &HashSet::new(), &sups, ROT_MIN_AGE_DAYS_DEFAULT, now());
        assert_eq!(found2.len(), 1);
        assert!(!found2[0]["signals"].as_array().unwrap().iter().any(|s| s == "old_unaccessed"));
        assert_eq!(found2[0]["confidence"], "medium");

        // Without any supersession evidence, staleness alone flags nothing.
        let lonely = mk();
        assert!(rot_findings(std::slice::from_ref(&lonely), &EdgeAggregates::default(), &HashSet::new(), &HashMap::new(), ROT_MIN_AGE_DAYS_DEFAULT, now()).is_empty());
    }

    #[test]
    fn rot_empty_payload_semantic_chars_procedural_desc_and_steps() {
        let sem = parse_node_meta(
            &json!({"_id": "s1", "content": "tiny", "category": "fact", "tags": [], "created_at": "2026-06-01T00:00:00Z"}),
            "memory.semantic",
        )
        .unwrap();
        assert!(sem.empty_payload);
        let proc_empty = parse_node_meta(
            &json!({"_id": "p1", "title": "T", "description": "", "steps": [], "tags": []}),
            "memory.procedural",
        )
        .unwrap();
        assert!(proc_empty.empty_payload);
        let proc_full = parse_node_meta(
            &json!({"_id": "p2", "title": "T", "description": "do the thing", "steps": [{"order": 1, "action": "x"}], "tags": []}),
            "memory.procedural",
        )
        .unwrap();
        assert!(!proc_full.empty_payload);
    }

    #[test]
    fn pseudo_title_semantic_first_line_procedural_title() {
        let sem = parse_node_meta(
            &json!({"_id": "s1", "content": "first line of the content\nsecond line", "category": "fact", "tags": []}),
            "memory.semantic",
        )
        .unwrap();
        assert_eq!(sem.pseudo_title, "first line of the content");
        assert_eq!(sem.category, "fact");
        let proc = parse_node_meta(
            &json!({"_id": "p1", "title": "Cert refresh procedure", "description": "d", "steps": [], "tags": []}),
            "memory.procedural",
        )
        .unwrap();
        assert_eq!(proc.pseudo_title, "Cert refresh procedure");
        assert_eq!(proc.category, "procedural");
    }

    #[test]
    fn dedup_threshold_inclusive_and_refines_pair_excluded() {
        let a = meta("memory.semantic", "a", "fact", "same title here", "identical body content tokens", &["tag1"]);
        let b = meta("memory.semantic", "b", "fact", "same title here", "identical body content tokens", &["tag1"]);
        let mut agg = EdgeAggregates::default();
        let found = pairwise_findings(&[a, b], &agg, &HashSet::new(), 1.0, true, false);
        // Identical nodes score exactly 1.0 — inclusive threshold keeps them.
        assert_eq!(found.dedup_total, 1);

        // A refines link marks the pair intentional — excluded.
        let a2 = meta("memory.semantic", "a", "fact", "same title here", "identical body content tokens", &["tag1"]);
        let b2 = meta("memory.semantic", "b", "fact", "same title here", "identical body content tokens", &["tag1"]);
        agg.refines_pairs.insert(pair_key(&a2.key(), &b2.key()));
        let found2 = pairwise_findings(&[a2, b2], &agg, &HashSet::new(), 1.0, true, false);
        assert_eq!(found2.dedup_total, 0);
    }

    /// Two records of guided deletes, built as the brain builds them, with
    /// the reason the operator most often gives none of. Without the tag
    /// the same pair is a duplicate and both are orphans.
    #[test]
    fn deletion_records_are_never_paired_or_orphaned() {
        let record = |id: &str, session: &str, tags: &[&str]| {
            let text = crate::sessions::delete_record::deletion_record(session, "", "2026-06-01 09:00 UTC");
            // As `remember` stores it: the tag is split off the text.
            let body = text.trim_end_matches(" #session-deleted").to_string();
            meta("memory.semantic", id, "observation", &body, &body, tags)
        };
        let no_edges = EdgeAggregates::default();

        let records = [record("a", "scratch", &["session-deleted"]), record("b", "test", &["session-deleted"])];
        let found = pairwise_findings(&records, &no_edges, &HashSet::new(), 0.75, true, true);
        assert_eq!(found.dedup_total, 0);
        assert!(found.supersessions.is_empty());
        assert!(orphan_findings(&records, &no_edges, now()).is_empty());

        let plain = [record("a", "scratch", &["other"]), record("b", "test", &["other"])];
        let found = pairwise_findings(&plain, &no_edges, &HashSet::new(), 0.75, true, true);
        assert_eq!(found.dedup_total, 1);
        assert_eq!(orphan_findings(&plain, &no_edges, now()).len(), 2);
    }

    #[test]
    fn pairwise_cap_takes_newest_and_warns() {
        // Nodes arrive newest-first from fetch_recent; the cap keeps the
        // head of that order and says so.
        let nodes: Vec<NodeMeta> = (0..(AUDIT_PAIRWISE_CAP + 3))
            .map(|i| meta("memory.semantic", &format!("n{}", i), "fact", "t", "body words here", &[]))
            .collect();
        let found = pairwise_findings(&nodes, &EdgeAggregates::default(), &HashSet::new(), 0.99, true, false);
        assert_eq!(found.warnings.len(), 1);
        assert!(found.warnings[0].contains("newest"));
        assert!(found.warnings[0].contains(&format!("{}", AUDIT_PAIRWISE_CAP)));
    }

    #[test]
    fn audit_params_clamped() {
        // Defaults.
        assert_eq!(
            clamp_audit_params(None, None, None),
            (AUDIT_MIN_SIMILARITY_DEFAULT, 50, ROT_MIN_AGE_DAYS_DEFAULT)
        );
        // Ceilings and floors.
        assert_eq!(clamp_audit_params(Some(2.0), Some(500), Some(999_999)), (1.0, 200, 3650));
        assert_eq!(clamp_audit_params(Some(-0.5), Some(0), Some(0)), (0.0, 1, 0));
    }

    /// Runs the audit against a live WardSONDB, for a check on a copy of an
    /// instance's data. Never against the instance itself:
    ///
    /// `EMBRA_AUDIT_DB=http://127.0.0.1:18091 cargo test -p embra-brain audit_on_a_live_copy -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a WardSONDB holding a copy of an instance's data"]
    async fn audit_on_a_live_copy() {
        let url = std::env::var("EMBRA_AUDIT_DB").expect("EMBRA_AUDIT_DB=<url of the copy>");
        let db = WardsonDbClient::from_url(&url);
        let args: KnowledgeAuditArgs = serde_json::from_value(json!({})).expect("defaults");
        let report = run_knowledge_audit(&db, args).await.expect("the audit runs");
        let v: serde_json::Value = serde_json::from_str(&report).expect("a JSON report");
        let shown = json!({"summary": v["summary"], "stats": v["stats"], "warnings": v["warnings"]});
        println!("{}", serde_json::to_string_pretty(&shown).expect("prints"));
    }

    #[test]
    fn knowledge_audit_args_defaults_and_schema_is_plain_object() {
        let a: KnowledgeAuditArgs = serde_json::from_value(json!({})).unwrap();
        assert!(a.collections.is_none());
        assert!(a.checks.is_none());
        assert!(a.min_similarity.is_none());
        assert!(a.max_results.is_none());
        assert!(a.min_age_days.is_none());

        // Anthropic rejects top-level oneOf/allOf/anyOf in input_schema.
        let schema = schemars::schema_for!(KnowledgeAuditArgs);
        let v = serde_json::to_value(&schema).unwrap();
        assert!(v.get("oneOf").is_none());
        assert!(v.get("allOf").is_none());
        assert!(v.get("anyOf").is_none());
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("object"));
    }
}
