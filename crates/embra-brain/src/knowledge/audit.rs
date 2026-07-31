//! `knowledge_audit` — read-only hygiene detection over the knowledge graph.
//!
//! Four checks (dedup / orphans / rot / contradictions) over
//! `memory.semantic` + `memory.procedural`, with edge context folded from
//! ONE exhaustive projected scan of `memory.edges` into compact per-node
//! aggregates (never a full adjacency list). Detection only — the
//! destructive counterpart is `knowledge_merge` (`knowledge/merge.rs`),
//! which consumes this tool's `dedup_candidates` verbatim (findings emit
//! full collection names for that reason).
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
use super::types::{content_preview, EdgeType};

/// Page size for the exhaustive edge scan — same bound rationale as
/// `ORPHAN_SCAN_PAGE` (well under the server's 100k `--max-query-limit`,
/// bounds per-page memory).
const AUDIT_SCAN_PAGE: usize = 20_000;
/// Pairwise dedup/contradiction cap per `(collection, category)` group,
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
/// fresh knowledge hasn't had time to be superseded (2026-07-30 production
/// feedback: the title-marker heuristics alone flagged healthy young nodes).
const ROT_MIN_AGE_DAYS_DEFAULT: i64 = 30;
/// A retrieval/traverse hit within this window argues AGAINST rot —
/// demotes the finding's confidence one level.
const ROT_RECENT_ACCESS_DAYS: i64 = 30;
/// Semantic nodes with less trimmed content than this carry no substance
/// beyond their tags.
const EMPTY_PAYLOAD_MIN_CHARS: usize = 30;
const CONTRADICTION_TAG_OVERLAP_MIN: f64 = 0.5;
/// Contradiction candidates must share subject matter but diverge in
/// content: body-token similarity inside this default band ("saying the
/// same thing" sits above it, "different topics" below). Superseded by the
/// per-instance calibration when enough real `contradicts` pairs exist.
const CONTRADICTION_BODY_SIM_MIN_DEFAULT: f64 = 0.05;
const CONTRADICTION_BODY_SIM_MAX_DEFAULT: f64 = 0.5;
/// Final contradiction score = tag_overlap × category weight; below this
/// floor the pair is dropped — the category weights make observation/
/// pattern pairs (which usually coexist) clear it only at very high tag
/// overlap.
const CONTRADICTION_SCORE_FLOOR: f64 = 0.35;
/// Minimum measurable existing-contradicts pairs before the body-sim band
/// calibrates from this instance's own data instead of the defaults.
const CONTRADICTION_CALIBRATION_MIN_PAIRS: usize = 5;
const PSEUDO_TITLE_MAX_CHARS: usize = 80;
/// Matches `knowledge_query`'s tag derivation (`len() > 2`).
const MIN_TOKEN_LEN: usize = 3;
const W_BODY: f64 = 0.5;
const W_TITLE: f64 = 0.3;
const W_TAGS: f64 = 0.2;
/// Body-token containment floor: "X" vs "X plus one more sentence" is the
/// dominant real dupe pattern, and raw Jaccard punishes it (the ratio can
/// sit at |A|/|B|). Set-space mirror of memory_scan's Subset heuristic.
const CONTAINMENT_SCORE_FLOOR: f64 = 0.8;
const ROT_FINAL_TOKENS: [&str; 4] = ["final", "finalized", "last", "ultimate"];

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
    Contradictions,
}

const ALL_CHECKS: [AuditCheck; 4] = [
    AuditCheck::Dedup,
    AuditCheck::Orphans,
    AuditCheck::Rot,
    AuditCheck::Contradictions,
];

impl AuditCheck {
    fn as_str(self) -> &'static str {
        match self {
            AuditCheck::Dedup => "dedup",
            AuditCheck::Orphans => "orphans",
            AuditCheck::Rot => "rot",
            AuditCheck::Contradictions => "contradictions",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "dedup" => Some(AuditCheck::Dedup),
            "orphans" => Some(AuditCheck::Orphans),
            "rot" => Some(AuditCheck::Rot),
            "contradictions" => Some(AuditCheck::Contradictions),
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
            "checks is empty — omit it to run all four, or pick from: dedup, orphans, rot, contradictions"
                .into(),
        );
    }
    let mut picked: Vec<AuditCheck> = Vec::new();
    for name in req {
        let Some(check) = AuditCheck::parse(name) else {
            return Err(format!(
                "unknown check '{}' — pick from: dedup, orphans, rot, contradictions",
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

/// Lowercase alphanumeric runs, minimum length 3.
fn tokenize(s: &str) -> HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= MIN_TOKEN_LEN)
        .map(|t| t.to_string())
        .collect()
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
    contradicts_pairs: HashSet<(NodeKey, NodeKey)>,
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
    if src_in && tgt_in {
        match etype {
            Some(EdgeType::Refines) => {
                agg.refines_pairs.insert(pair_key(&src, &tgt));
            }
            Some(EdgeType::Contradicts) => {
                agg.contradicts_pairs.insert(pair_key(&src, &tgt));
            }
            _ => {}
        }
    }
}

/// One page of the audit's exhaustive edge scan — the sanctioned no-sort
/// key-order pagination (`scan_page_query_body`) plus a server-side
/// projection: the aggregates need endpoints + type only, never payloads.
fn audit_edge_page_body(page_limit: usize, page: &ScanPage) -> serde_json::Value {
    let mut body = scan_page_query_body(&json!({}), page_limit, page);
    body["fields"] = json!([
        "source_id",
        "source_collection",
        "target_id",
        "target_collection",
        "edge_type"
    ]);
    body
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

/// Contradiction body-similarity band — the divergence gate. Calibrated
/// per-instance from the body similarity of pairs already linked by real
/// `contradicts` edges ("what do actual contradictions look like?"): with
/// ≥ CONTRADICTION_CALIBRATION_MIN_PAIRS measurable pairs the band is
/// their p10–p90 (outlier-clamped); otherwise the static defaults.
struct ContradictionBand {
    lo: f64,
    hi: f64,
    measured_pairs: usize,
    source: &'static str,
}

fn contradiction_band(
    nodes: &[NodeMeta],
    contradicts_pairs: &HashSet<(NodeKey, NodeKey)>,
) -> ContradictionBand {
    let by_key: HashMap<NodeKey, &NodeMeta> = nodes.iter().map(|n| (n.key(), n)).collect();
    let mut sims: Vec<f64> = contradicts_pairs
        .iter()
        .filter_map(|(a, b)| {
            Some(jaccard(&by_key.get(a)?.body_tokens, &by_key.get(b)?.body_tokens))
        })
        .collect();
    if sims.len() >= CONTRADICTION_CALIBRATION_MIN_PAIRS {
        sims.sort_by(f64::total_cmp);
        let lo = sims[sims.len() / 10].max(0.01);
        let hi = sims[sims.len() * 9 / 10].min(0.85).max(lo);
        ContradictionBand { lo, hi, measured_pairs: sims.len(), source: "calibrated" }
    } else {
        ContradictionBand {
            lo: CONTRADICTION_BODY_SIM_MIN_DEFAULT,
            hi: CONTRADICTION_BODY_SIM_MAX_DEFAULT,
            measured_pairs: sims.len(),
            source: "defaults",
        }
    }
}

/// Category-aware contradiction weighting (2026-07-30 production feedback):
/// two observations or patterns usually coexist; divergent facts and
/// decisions are the pairs actually worth surfacing.
fn contradiction_category_weight(category: &str) -> f64 {
    match category {
        "fact" => 1.0,
        "decision" => 0.9,
        "preference" => 0.7,
        "procedural" => 0.6,
        _ => 0.4, // observation, pattern, uncategorized
    }
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
    contradictions: Vec<serde_json::Value>,
    dedup_total: usize,
    contradiction_total: usize,
    /// older node → its best newer near-duplicate (rot's primary gate).
    supersessions: HashMap<NodeKey, Supersession>,
    warnings: Vec<String>,
}

/// Dedup, rot's supersession probe, and contradictions share one pairwise
/// pass per `(collection, category)` group (pairs never cross groups).
/// Groups iterate in sorted key order and nodes keep their newest-first
/// fetch order, so output is deterministic.
fn pairwise_findings(
    nodes: &[NodeMeta],
    agg: &EdgeAggregates,
    min_similarity: f64,
    want_dedup: bool,
    want_contra: bool,
    want_rot: bool,
    band: &ContradictionBand,
) -> PairwiseFindings {
    let mut out = PairwiseFindings::default();
    if !want_dedup && !want_contra && !want_rot {
        return out;
    }

    let mut groups: HashMap<(&'static str, &str), Vec<&NodeMeta>> = HashMap::new();
    for n in nodes {
        groups.entry((n.collection, n.category.as_str())).or_default().push(n);
    }
    let mut keys: Vec<(&'static str, &str)> = groups.keys().copied().collect();
    keys.sort();

    let mut scored_dedup: Vec<(f64, bool, &NodeMeta, &NodeMeta)> = Vec::new();
    // (final score, tag_sim, body_sim, category weight, a, b)
    let mut scored_contra: Vec<(f64, f64, f64, f64, &NodeMeta, &NodeMeta)> = Vec::new();

    for key in keys {
        let group = &groups[&key];
        if group.len() > AUDIT_PAIRWISE_CAP {
            out.warnings.push(format!(
                "pairwise pass (dedup/rot-supersession/contradictions) limited to the {} newest of {} nodes in {}/{}",
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
                let pk = pair_key(&a.key(), &b.key());
                let (score, subset) = dedup_score(a, b);
                if score >= min_similarity {
                    // Refines-linked pairs are intentional relationships,
                    // not duplicates (spec).
                    if want_dedup && !agg.refines_pairs.contains(&pk) {
                        scored_dedup.push((score, subset, a, b));
                    }
                    // Rot's supersession gate: the OLDER of a near-duplicate
                    // pair is a rot candidate — the newer node covers it.
                    if want_rot {
                        note_supersession(&mut out.supersessions, a, b, score);
                    }
                } else if want_contra {
                    // Contradiction candidates must share the subject AND
                    // diverge: high tag overlap, body similarity inside the
                    // (calibrated) band — "sharing tags but saying the same
                    // thing" sits above the band and never surfaces.
                    let tag_sim = overlap_max(&a.tags_lower, &b.tags_lower);
                    let body_sim = jaccard(&a.body_tokens, &b.body_tokens);
                    let weight = contradiction_category_weight(&a.category);
                    let cscore = tag_sim * weight;
                    if tag_sim >= CONTRADICTION_TAG_OVERLAP_MIN
                        && body_sim >= band.lo
                        && body_sim <= band.hi
                        && cscore >= CONTRADICTION_SCORE_FLOOR
                        && !agg.contradicts_pairs.contains(&pk)
                        && !agg.refines_pairs.contains(&pk)
                    {
                        scored_contra.push((cscore, tag_sim, body_sim, weight, a, b));
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
    scored_contra.sort_by(|x, y| {
        y.0.partial_cmp(&x.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| (x.4.collection, &x.4.id).cmp(&(y.4.collection, &y.4.id)))
    });

    out.dedup_total = scored_dedup.len();
    out.contradiction_total = scored_contra.len();

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

    out.contradictions = scored_contra
        .into_iter()
        .map(|(cscore, tag_sim, body_sim, weight, a, b)| {
            let shared: Vec<&String> = {
                let mut v: Vec<&String> = a.tags_lower.intersection(&b.tags_lower).collect();
                v.sort();
                v
            };
            json!({
                "score": (cscore * 100.0).round() / 100.0,
                "tag_overlap": (tag_sim * 100.0).round() / 100.0,
                "body_similarity": (body_sim * 100.0).round() / 100.0,
                "category_weight": weight,
                "confidence": "low",
                "node_a": node_ref(a),
                "node_b": node_ref(b),
                "overlap": shared,
                "rationale": "shared topic tags, content that overlaps but diverges, no contradicts edge — read both nodes before acting",
            })
        })
        .collect();

    out
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

/// `v`/`V` immediately followed by a digit at a token boundary — v1, V2,
/// v3.0. Hand-rolled: embra-brain has no regex dependency, and a two-state
/// scan doesn't justify adding one. "very"/"velvet" never fire (the next
/// char must be a digit); "curve4" is excluded by the boundary check.
fn title_has_version_marker(title: &str) -> bool {
    let chars: Vec<char> = title.chars().collect();
    for i in 0..chars.len() {
        let c = chars[i];
        if (c == 'v' || c == 'V')
            && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit())
            && (i == 0 || !chars[i - 1].is_alphanumeric())
        {
            return true;
        }
    }
    false
}

/// Rot v2 (2026-07-30 production feedback): SUPERSESSION-GATED. A node is
/// flagged only when there is positive evidence something newer covers it —
/// a newer near-duplicate above `min_similarity` (the pairwise pass's
/// `supersessions`), or a `refines` edge connecting it to a NEWER node (the
/// strong form; deliberately direction-agnostic — stored refines direction
/// varies by authoring habit, the newness carries the signal). Title
/// markers, >90-day non-access, and empty payloads are TIEBREAKERS that
/// raise confidence but can never flag a node alone (they false-positived
/// on healthy nodes in production); a recent retrieval hit lowers
/// confidence one level. Nodes younger than `min_age_days` are skipped.
fn rot_findings(
    nodes: &[NodeMeta],
    agg: &EdgeAggregates,
    supersessions: &HashMap<NodeKey, Supersession>,
    min_age_days: i64,
    now: DateTime<Utc>,
) -> Vec<serde_json::Value> {
    let by_key: HashMap<NodeKey, &NodeMeta> = nodes.iter().map(|n| (n.key(), n)).collect();
    let mut refines_adj: HashMap<&NodeKey, Vec<&NodeKey>> = HashMap::new();
    for (a, b) in &agg.refines_pairs {
        refines_adj.entry(a).or_default().push(b);
        refines_adj.entry(b).or_default().push(a);
    }

    let mut found: Vec<(i32, i64, serde_json::Value)> = Vec::new();
    for n in nodes {
        let key = n.key();
        // Min-age gate; an unknown created_at can never satisfy a
        // newer-than comparison, so it never flags either.
        let Some(age_d) = age_days(n.created_at, now) else { continue };
        if age_d < min_age_days {
            continue;
        }

        // Primary evidence.
        let refiner: Option<&NodeMeta> = refines_adj
            .get(&key)
            .into_iter()
            .flatten()
            .filter_map(|k| by_key.get(*k).copied())
            .filter(|b| matches!((b.created_at, n.created_at), (Some(bc), Some(ac)) if bc > ac))
            .max_by_key(|b| b.created_at);
        let superseded = supersessions.get(&key);
        if refiner.is_none() && superseded.is_none() {
            continue;
        }

        let mut signals: Vec<&str> = Vec::new();
        let mut phrases: Vec<String> = Vec::new();
        // 2 = high, 1 = medium, 0 = low.
        let mut level: i32 = if refiner.is_some() {
            signals.push("refines_from_newer");
            phrases.push("a newer node refines this one".into());
            2
        } else {
            1
        };
        if superseded.is_some() {
            signals.push("superseded_by_newer");
            phrases.push("a newer node covers near-identical content".into());
        }

        // Tiebreakers — confidence only, never primary.
        let mut tiebreak = false;
        if n.title_tokens.iter().any(|t| ROT_FINAL_TOKENS.contains(&t.as_str())) {
            signals.push("final_in_title");
            phrases.push("title claims finality".into());
            tiebreak = true;
        }
        if title_has_version_marker(&n.pseudo_title) {
            signals.push("version_number");
            phrases.push("versioned title suggests superseded content".into());
            tiebreak = true;
        }
        let incoming = agg.incoming_dep_enables.get(&key).copied().unwrap_or(0);
        if incoming == 0
            && n.last_accessed
                .or(n.created_at)
                .is_some_and(|ts| (now - ts).num_days() > ROT_UNACCESSED_DAYS)
        {
            signals.push("old_unaccessed");
            phrases.push("not retrieved in >90 days and nothing depends_on/enables it".into());
            tiebreak = true;
        }
        if n.empty_payload {
            signals.push("empty_payload");
            phrases.push("no substantive content beyond title/tags".into());
            tiebreak = true;
        }
        if tiebreak {
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

        let superseded_by = if let Some(b) = refiner {
            json!({
                "collection": b.collection,
                "id": b.id,
                "title": b.pseudo_title,
                "similarity": superseded
                    .filter(|s| s.newer == b.key())
                    .map(|s| (s.similarity * 100.0).round() / 100.0),
                "via": "refines_edge",
            })
        } else {
            let s = superseded.expect("gate guarantees one primary");
            json!({
                "collection": s.newer.0,
                "id": s.newer.1,
                "title": s.newer_title,
                "similarity": (s.similarity * 100.0).round() / 100.0,
                "via": "content_similarity",
            })
        };

        let finding = json!({
            "collection": n.collection,
            "id": n.id,
            "title": n.pseudo_title,
            "signals": signals,
            "age_days": age_d,
            "confidence": confidence,
            "superseded_by": superseded_by,
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

    // All four checks consume the edge aggregates (degrees, incoming
    // structural counts, refines/contradicts pair exclusions).
    let agg = scan_edges(db, &node_keys).await?;

    let want = |c: AuditCheck| checks.contains(&c);
    let band = contradiction_band(&nodes, &agg.contradicts_pairs);
    let pairwise = pairwise_findings(
        &nodes,
        &agg,
        min_similarity,
        want(AuditCheck::Dedup),
        want(AuditCheck::Contradictions),
        want(AuditCheck::Rot),
        &band,
    );
    let orphans = if want(AuditCheck::Orphans) {
        orphan_findings(&nodes, &agg, now)
    } else {
        Vec::new()
    };
    let rot = if want(AuditCheck::Rot) {
        rot_findings(&nodes, &agg, &pairwise.supersessions, min_age_days, now)
    } else {
        Vec::new()
    };
    warnings.extend(pairwise.warnings);

    let (orphan_total, rot_total) = (orphans.len(), rot.len());
    let issues_found =
        pairwise.dedup_total + pairwise.contradiction_total + orphan_total + rot_total;

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
        },
        "dedup_candidates": truncate(pairwise.dedup),
        "orphan_nodes": truncate(orphans),
        "rot_candidates": truncate(rot),
        "contradiction_candidates": truncate(pairwise.contradictions),
        "stats": {
            "total_nodes": total_nodes,
            "total_edges": total_edges,
            "meaningful_edges": agg.meaningful_total,
            "orphan_count": orphan_total,
            "dedup_pair_count": pairwise.dedup_total,
            "rot_count": rot_total,
            "contradiction_count": pairwise.contradiction_total,
            "contradiction_calibration": {
                "body_similarity_band": [
                    (band.lo * 100.0).round() / 100.0,
                    (band.hi * 100.0).round() / 100.0
                ],
                "measured_pairs": band.measured_pairs,
                "source": band.source,
            },
        },
        "warnings": warnings,
    });
    serde_json::to_string_pretty(&report).map_err(|e| e.to_string())
}

// ── tool descriptor ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_audit",
    description = "Read-only hygiene audit of the knowledge graph (memory.semantic and memory.procedural; identity.graph and memory.entries are out of scope). checks selects any of: dedup (near-duplicate node pairs by content/tag similarity, threshold min_similarity, default 0.75), orphans (nodes with zero meaningful edges — auto same_session/temporal/tag_overlap and derived_from provenance do not count; nodes under 1 day old are skipped, 7+ days is high confidence), rot (supersession-gated: a node is flagged only when a newer node with similarity >= min_similarity exists, or a refines edge links it to a newer node — the strong form; finality/version title markers, 90+ days without access, and empty payloads raise confidence but never flag alone; a recent retrieval lowers it; nodes younger than min_age_days are skipped, default 30), contradictions (same category, shared tags, content that overlaps but DIVERGES — the body-similarity band is calibrated from this instance's existing contradicts edges when enough exist; category-weighted so fact/decision pairs rank far above observation/pattern pairs, which usually coexist; no existing contradicts edge; low confidence, read both nodes before acting). Returns JSON: summary, per-check findings capped at max_results each (default 50, max 200), stats incl. the contradiction calibration, warnings. Never modifies anything. Feed dedup_candidates to knowledge_merge, dry_run first."
)]
pub struct KnowledgeAuditArgs {
    /// Collections to scan: semantic | procedural (full memory.* names also
    /// accepted). Omit for both.
    #[serde(default)]
    pub collections: Option<Vec<String>>,
    /// Checks to run: dedup | orphans | rot | contradictions. Omit for all
    /// four.
    #[serde(default)]
    pub checks: Option<Vec<String>>,
    /// Similarity threshold in [0.0, 1.0] for dedup pairs AND rot's
    /// newer-node supersession probe. Default 0.75.
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

    fn default_band() -> ContradictionBand {
        ContradictionBand {
            lo: CONTRADICTION_BODY_SIM_MIN_DEFAULT,
            hi: CONTRADICTION_BODY_SIM_MAX_DEFAULT,
            measured_pairs: 0,
            source: "defaults",
        }
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
            json!(["source_id", "source_collection", "target_id", "target_collection", "edge_type"])
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
        assert_eq!(all.len(), 4);
        let got = resolve_audit_checks(Some(&["rot".to_string(), "dedup".to_string()])).unwrap();
        // Canonical order regardless of input order.
        assert_eq!(got, vec![AuditCheck::Dedup, AuditCheck::Rot]);
        assert!(resolve_audit_checks(Some(&[])).is_err());
        assert!(resolve_audit_checks(Some(&["bogus".to_string()])).is_err());
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
    fn rot_gate_requires_supersession_or_refines_markers_never_flag_alone() {
        // Production feedback: title markers false-positived on healthy
        // nodes. A marker-covered old node with NO supersession evidence
        // must not flag.
        let agg = EdgeAggregates::default();
        let marked = meta("memory.semantic", "a", "decision", "Final architecture decision v2", "body long enough to not be empty payload", &[]);
        assert!(rot_findings(std::slice::from_ref(&marked), &agg, &HashMap::new(), ROT_MIN_AGE_DAYS_DEFAULT, now()).is_empty());

        // With a newer near-duplicate the same node flags — and the markers
        // now raise confidence to high (medium primary + tiebreaker).
        let sups = sup_for(&marked, "replacement", "n1", 0.9);
        let found = rot_findings(std::slice::from_ref(&marked), &agg, &sups, ROT_MIN_AGE_DAYS_DEFAULT, now());
        assert_eq!(found.len(), 1);
        let sigs: Vec<&str> = found[0]["signals"].as_array().unwrap().iter().filter_map(|s| s.as_str()).collect();
        assert!(sigs.contains(&"superseded_by_newer"));
        assert!(sigs.contains(&"final_in_title"));
        assert!(sigs.contains(&"version_number"));
        assert_eq!(found[0]["confidence"], "high");
        assert_eq!(found[0]["superseded_by"]["id"], "n1");
        assert_eq!(found[0]["superseded_by"]["via"], "content_similarity");
    }

    #[test]
    fn rot_final_in_title_is_token_match_not_substring() {
        // "penultimate" contains "ultimate" and "lastly" contains "last" as
        // substrings — token matching must not fire on either.
        let agg = EdgeAggregates::default();
        let pen = meta("memory.semantic", "b", "decision", "The penultimate lastly-noted plan", "body long enough to not be empty payload", &[]);
        let sups = sup_for(&pen, "newer", "n1", 0.8);
        let found = rot_findings(std::slice::from_ref(&pen), &agg, &sups, ROT_MIN_AGE_DAYS_DEFAULT, now());
        assert_eq!(found.len(), 1);
        let sigs: Vec<&str> = found[0]["signals"].as_array().unwrap().iter().filter_map(|s| s.as_str()).collect();
        assert!(!sigs.contains(&"final_in_title"));
        assert_eq!(found[0]["confidence"], "medium"); // primary only, no tiebreaker
    }

    #[test]
    fn rot_min_age_skips_young_nodes() {
        let agg = EdgeAggregates::default();
        let mut young = meta("memory.semantic", "y", "fact", "t", "body long enough to not be empty payload", &[]);
        young.created_at = Some(now() - chrono::Duration::days(10));
        let sups = sup_for(&young, "newer", "n1", 0.9);
        // 10 days < default 30 → skipped even with supersession evidence.
        assert!(rot_findings(std::slice::from_ref(&young), &agg, &sups, ROT_MIN_AGE_DAYS_DEFAULT, now()).is_empty());
        // min_age_days = 0 admits it.
        assert_eq!(rot_findings(std::slice::from_ref(&young), &agg, &sups, 0, now()).len(), 1);
    }

    #[test]
    fn rot_recent_access_demotes_confidence() {
        let agg = EdgeAggregates::default();
        let mut node = meta("memory.semantic", "a", "fact", "plain title", "body long enough to not be empty payload", &[]);
        node.last_accessed = Some(now() - chrono::Duration::days(5));
        let sups = sup_for(&node, "newer", "n1", 0.9);
        let found = rot_findings(std::slice::from_ref(&node), &agg, &sups, ROT_MIN_AGE_DAYS_DEFAULT, now());
        assert_eq!(found.len(), 1);
        let sigs: Vec<&str> = found[0]["signals"].as_array().unwrap().iter().filter_map(|s| s.as_str()).collect();
        assert!(sigs.contains(&"recently_accessed"));
        assert_eq!(found[0]["confidence"], "low"); // medium primary − recent access
    }

    #[test]
    fn rot_refines_newer_partner_high_confidence_direction_agnostic() {
        let mut old = meta("memory.semantic", "old", "fact", "original take", "body long enough to not be empty payload", &[]);
        old.created_at = Some(now() - chrono::Duration::days(60));
        let mut newer = meta("memory.semantic", "new", "fact", "refined take", "different body content entirely here", &[]);
        newer.created_at = Some(now() - chrono::Duration::days(40));
        let mut agg = EdgeAggregates::default();
        // Unordered pair — the check keys on the partner being NEWER, not on
        // the stored edge direction (authoring direction varies).
        agg.refines_pairs.insert(pair_key(&old.key(), &newer.key()));
        let nodes = vec![old, newer];
        let found = rot_findings(&nodes, &agg, &HashMap::new(), ROT_MIN_AGE_DAYS_DEFAULT, now());
        // Only the OLDER end flags; the newer partner's counterpart is older.
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["id"], "old");
        assert_eq!(found[0]["confidence"], "high");
        assert_eq!(found[0]["superseded_by"]["id"], "new");
        assert_eq!(found[0]["superseded_by"]["via"], "refines_edge");
    }

    #[test]
    fn pairwise_supersession_flags_older_of_near_duplicates() {
        let mut a = meta("memory.semantic", "a", "fact", "same title here", "identical body content tokens", &["tag1"]);
        a.created_at = Some(now() - chrono::Duration::days(90));
        let mut b = meta("memory.semantic", "b", "fact", "same title here", "identical body content tokens", &["tag1"]);
        b.created_at = Some(now() - chrono::Duration::days(40));
        let older_key = a.key();
        let newer_key = b.key();
        let pw = pairwise_findings(&[a, b], &EdgeAggregates::default(), 0.75, false, false, true, &default_band());
        assert_eq!(pw.supersessions.len(), 1);
        let s = pw.supersessions.get(&older_key).expect("older node is the candidate");
        assert_eq!(s.newer, newer_key);
        assert!(s.similarity >= 0.99);
    }

    #[test]
    fn rot_version_marker_v_digit_boundary_only() {
        assert!(title_has_version_marker("Decision v2"));
        assert!(title_has_version_marker("v3.0 rollout plan"));
        assert!(title_has_version_marker("V1 spec"));
        assert!(!title_has_version_marker("very good plan"));
        assert!(!title_has_version_marker("velvet workflow"));
        assert!(!title_has_version_marker("curve4 tuning"));
        assert!(!title_has_version_marker("KV2 store"));
        assert!(!title_has_version_marker(""));
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
        let found = rot_findings(std::slice::from_ref(&old), &EdgeAggregates::default(), &sups, ROT_MIN_AGE_DAYS_DEFAULT, now());
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
        let found2 = rot_findings(std::slice::from_ref(&depended), &agg2, &sups, ROT_MIN_AGE_DAYS_DEFAULT, now());
        assert_eq!(found2.len(), 1);
        assert!(!found2[0]["signals"].as_array().unwrap().iter().any(|s| s == "old_unaccessed"));
        assert_eq!(found2[0]["confidence"], "medium");

        // Without any supersession evidence, staleness alone flags nothing.
        let lonely = mk();
        assert!(rot_findings(std::slice::from_ref(&lonely), &EdgeAggregates::default(), &HashMap::new(), ROT_MIN_AGE_DAYS_DEFAULT, now()).is_empty());
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
        let found = pairwise_findings(&[a, b], &agg, 1.0, true, false, false, &default_band());
        // Identical nodes score exactly 1.0 — inclusive threshold keeps them.
        assert_eq!(found.dedup_total, 1);

        // A refines link marks the pair intentional — excluded.
        let a2 = meta("memory.semantic", "a", "fact", "same title here", "identical body content tokens", &["tag1"]);
        let b2 = meta("memory.semantic", "b", "fact", "same title here", "identical body content tokens", &["tag1"]);
        agg.refines_pairs.insert(pair_key(&a2.key(), &b2.key()));
        let found2 = pairwise_findings(&[a2, b2], &agg, 1.0, true, false, false, &default_band());
        assert_eq!(found2.dedup_total, 0);
    }

    #[test]
    fn contradiction_needs_tag_overlap_not_dedup_similar_no_contradicts_or_refines_edge() {
        // Bodies share the subject token "conclusion" (jaccard 0.2 — inside
        // the default divergence band) while diverging in claims.
        let a = meta("memory.semantic", "a", "fact", "alpha topic claim", "alpha conclusion entirely", &["topic", "area"]);
        let b = meta("memory.semantic", "b", "fact", "beta topic claim", "beta conclusion divergent", &["topic", "area"]);
        let mut agg = EdgeAggregates::default();
        let found = pairwise_findings(&[a, b], &agg, 0.75, false, true, false, &default_band());
        assert_eq!(found.contradiction_total, 1);
        assert_eq!(found.contradictions[0]["confidence"], "low");
        assert_eq!(found.contradictions[0]["category_weight"], 1.0); // fact

        // An existing contradicts edge = already acknowledged.
        let a2 = meta("memory.semantic", "a", "fact", "alpha topic claim", "alpha conclusion entirely", &["topic", "area"]);
        let b2 = meta("memory.semantic", "b", "fact", "beta topic claim", "beta conclusion divergent", &["topic", "area"]);
        agg.contradicts_pairs.insert(pair_key(&a2.key(), &b2.key()));
        let found2 = pairwise_findings(&[a2, b2], &agg, 0.75, false, true, false, &default_band());
        assert_eq!(found2.contradiction_total, 0);

        // Different categories never pair.
        let c = meta("memory.semantic", "c", "decision", "alpha topic claim", "alpha conclusion entirely", &["topic", "area"]);
        let d = meta("memory.semantic", "d", "observation", "beta topic claim", "beta conclusion divergent", &["topic", "area"]);
        let found3 = pairwise_findings(&[c, d], &EdgeAggregates::default(), 0.75, false, true, false, &default_band());
        assert_eq!(found3.contradiction_total, 0);
    }

    #[test]
    fn contradiction_requires_body_divergence_band() {
        // Near-identical bodies ("sharing tags but saying the same thing"):
        // jaccard 4/6 ≈ 0.67 sits ABOVE the default band ceiling — excluded
        // even though the overall dedup score stays under the threshold.
        let a = meta("memory.semantic", "a", "fact", "first phrasing", "alpha beta gamma delta epsilon", &["topic", "area"]);
        let b = meta("memory.semantic", "b", "fact", "second phrasing", "alpha beta gamma delta zeta", &["topic", "area"]);
        let found = pairwise_findings(&[a, b], &EdgeAggregates::default(), 0.75, false, true, false, &default_band());
        assert_eq!(found.contradiction_total, 0);

        // Fully disjoint bodies (different topics wearing the same tags):
        // jaccard 0.0 sits BELOW the band floor — excluded.
        let c = meta("memory.semantic", "c", "fact", "one thing", "alpha beta gamma", &["topic", "area"]);
        let d = meta("memory.semantic", "d", "fact", "another thing", "delta epsilon zeta", &["topic", "area"]);
        let found2 = pairwise_findings(&[c, d], &EdgeAggregates::default(), 0.75, false, true, false, &default_band());
        assert_eq!(found2.contradiction_total, 0);
    }

    #[test]
    fn contradiction_category_weights_drop_observation_pairs() {
        // Observation pair at tag overlap 0.5: score 0.5 × 0.4 = 0.2 < the
        // 0.35 floor — observations coexist by nature.
        // Bodies share {conclusion, shared} but diverge on id-prefixed
        // tokens → jaccard 2/6 = 0.33, inside the divergence band.
        let mk = |id: &str, cat: &str, tags: &[&str]| {
            meta(
                "memory.semantic",
                id,
                cat,
                format!("{} claim", id).as_str(),
                &format!("{id}extra {id}other conclusion shared"),
                tags,
            )
        };
        let a = mk("a", "observation", &["topic", "extra"]);
        let b = mk("b", "observation", &["topic", "other"]);
        let found = pairwise_findings(&[a, b], &EdgeAggregates::default(), 0.75, false, true, false, &default_band());
        assert_eq!(found.contradiction_total, 0);

        // The same shape as facts clears the floor (0.5 × 1.0).
        let c = mk("c", "fact", &["topic", "extra"]);
        let d = mk("d", "fact", &["topic", "other"]);
        let found2 = pairwise_findings(&[c, d], &EdgeAggregates::default(), 0.75, false, true, false, &default_band());
        assert_eq!(found2.contradiction_total, 1);

        // Observations still surface at VERY high tag overlap (1.0 × 0.4 =
        // 0.4 ≥ 0.35) — coexistence is a prior, not a ban.
        let e = mk("e", "observation", &["topic", "area"]);
        let f = mk("f", "observation", &["topic", "area"]);
        let found3 = pairwise_findings(&[e, f], &EdgeAggregates::default(), 0.75, false, true, false, &default_band());
        assert_eq!(found3.contradiction_total, 1);
    }

    #[test]
    fn contradiction_band_calibrates_from_existing_pairs_or_defaults() {
        // Six known-contradiction pairs, each with body jaccard 2/6 = 1/3 →
        // the band calibrates to that observed similarity.
        let mut nodes: Vec<NodeMeta> = Vec::new();
        let mut pairs: HashSet<(NodeKey, NodeKey)> = HashSet::new();
        for i in 0..6 {
            let a = meta("memory.semantic", &format!("a{}", i), "fact", "t", "alpha beta gamma delta", &[]);
            let b = meta("memory.semantic", &format!("b{}", i), "fact", "t", "alpha beta epsilon zeta", &[]);
            pairs.insert(pair_key(&a.key(), &b.key()));
            nodes.push(a);
            nodes.push(b);
        }
        let band = contradiction_band(&nodes, &pairs);
        assert_eq!(band.source, "calibrated");
        assert_eq!(band.measured_pairs, 6);
        assert!((band.lo - 1.0 / 3.0).abs() < 1e-9);
        assert!((band.hi - 1.0 / 3.0).abs() < 1e-9);

        // Below the calibration minimum → static defaults, honestly labeled.
        let few: HashSet<(NodeKey, NodeKey)> = pairs.into_iter().take(2).collect();
        let band2 = contradiction_band(&nodes, &few);
        assert_eq!(band2.source, "defaults");
        assert_eq!(band2.measured_pairs, 2);
        assert!((band2.lo - CONTRADICTION_BODY_SIM_MIN_DEFAULT).abs() < 1e-9);
        assert!((band2.hi - CONTRADICTION_BODY_SIM_MAX_DEFAULT).abs() < 1e-9);
    }

    #[test]
    fn pairwise_cap_takes_newest_and_warns() {
        // Nodes arrive newest-first from fetch_recent; the cap keeps the
        // head of that order and says so.
        let nodes: Vec<NodeMeta> = (0..(AUDIT_PAIRWISE_CAP + 3))
            .map(|i| meta("memory.semantic", &format!("n{}", i), "fact", "t", "body words here", &[]))
            .collect();
        let found = pairwise_findings(&nodes, &EdgeAggregates::default(), 0.99, true, false, false, &default_band());
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
