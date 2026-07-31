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
/// Semantic nodes with less trimmed content than this carry no substance
/// beyond their tags.
const EMPTY_PAYLOAD_MIN_CHARS: usize = 30;
const CONTRADICTION_TAG_OVERLAP_MIN: f64 = 0.5;
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

#[derive(Default)]
struct PairwiseFindings {
    dedup: Vec<serde_json::Value>,
    contradictions: Vec<serde_json::Value>,
    dedup_total: usize,
    contradiction_total: usize,
    warnings: Vec<String>,
}

/// Dedup + contradictions share one pairwise pass per `(collection,
/// category)` group (pairs never cross groups). Groups iterate in sorted
/// key order and nodes keep their newest-first fetch order, so output is
/// deterministic.
fn pairwise_findings(
    nodes: &[NodeMeta],
    agg: &EdgeAggregates,
    min_similarity: f64,
    want_dedup: bool,
    want_contra: bool,
) -> PairwiseFindings {
    let mut out = PairwiseFindings::default();
    if !want_dedup && !want_contra {
        return out;
    }

    let mut groups: HashMap<(&'static str, &str), Vec<&NodeMeta>> = HashMap::new();
    for n in nodes {
        groups.entry((n.collection, n.category.as_str())).or_default().push(n);
    }
    let mut keys: Vec<(&'static str, &str)> = groups.keys().copied().collect();
    keys.sort();

    let mut scored_dedup: Vec<(f64, bool, &NodeMeta, &NodeMeta)> = Vec::new();
    let mut scored_contra: Vec<(f64, &NodeMeta, &NodeMeta)> = Vec::new();

    for key in keys {
        let group = &groups[&key];
        if group.len() > AUDIT_PAIRWISE_CAP {
            out.warnings.push(format!(
                "dedup/contradiction pass limited to the {} newest of {} nodes in {}/{}",
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
                } else if want_contra
                    && overlap_max(&a.tags_lower, &b.tags_lower) >= CONTRADICTION_TAG_OVERLAP_MIN
                    && !agg.contradicts_pairs.contains(&pk)
                    && !agg.refines_pairs.contains(&pk)
                {
                    scored_contra.push((overlap_max(&a.tags_lower, &b.tags_lower), a, b));
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
            .then_with(|| (x.1.collection, &x.1.id).cmp(&(y.1.collection, &y.1.id)))
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
        .map(|(tag_sim, a, b)| {
            let shared: Vec<&String> = {
                let mut v: Vec<&String> = a.tags_lower.intersection(&b.tags_lower).collect();
                v.sort();
                v
            };
            json!({
                "score": (tag_sim * 100.0).round() / 100.0,
                "confidence": "low",
                "node_a": node_ref(a),
                "node_b": node_ref(b),
                "overlap": shared,
                "rationale": "shared topic tags with divergent content and no contradicts edge — read both nodes before acting",
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

fn rot_findings(
    nodes: &[NodeMeta],
    agg: &EdgeAggregates,
    now: DateTime<Utc>,
) -> Vec<serde_json::Value> {
    let mut found: Vec<(usize, i64, serde_json::Value)> = Vec::new();
    for n in nodes {
        let mut signals: Vec<&str> = Vec::new();
        let mut phrases: Vec<&str> = Vec::new();
        if n.title_tokens.iter().any(|t| ROT_FINAL_TOKENS.contains(&t.as_str())) {
            signals.push("final_in_title");
            phrases.push("title claims finality");
        }
        if title_has_version_marker(&n.pseudo_title) {
            signals.push("version_number");
            phrases.push("versioned title suggests superseded content");
        }
        let last_signal = n.last_accessed.or(n.created_at);
        let incoming = agg.incoming_dep_enables.get(&n.key()).copied().unwrap_or(0);
        if incoming == 0
            && last_signal.is_some_and(|ts| (now - ts).num_days() > ROT_UNACCESSED_DAYS)
        {
            signals.push("old_unaccessed");
            phrases.push("not retrieved in >90 days and nothing depends_on/enables it");
        }
        if n.empty_payload {
            signals.push("empty_payload");
            phrases.push("no substantive content beyond title/tags");
        }
        if signals.is_empty() {
            continue;
        }
        let age = age_days(n.created_at, now);
        let finding = json!({
            "collection": n.collection,
            "id": n.id,
            "title": n.pseudo_title,
            "signals": signals,
            "age_days": age,
            "rationale": phrases.join("; "),
        });
        found.push((signals.len(), age.unwrap_or(-1), finding));
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    found.into_iter().map(|(_, _, f)| f).collect()
}

// ── pipeline ─────────────────────────────────────────────────────────────

/// (min_similarity, max_results) with defaults applied and clamped.
fn clamp_audit_params(min_similarity: Option<f64>, max_results: Option<u32>) -> (f64, usize) {
    (
        min_similarity
            .unwrap_or(AUDIT_MIN_SIMILARITY_DEFAULT)
            .clamp(0.0, 1.0),
        (max_results.unwrap_or(AUDIT_MAX_RESULTS_DEFAULT as u32) as usize)
            .clamp(1, AUDIT_MAX_RESULTS_CEILING),
    )
}

pub(crate) async fn run_knowledge_audit(
    db: &WardsonDbClient,
    args: KnowledgeAuditArgs,
) -> Result<String, String> {
    let collections = resolve_audit_collections(args.collections.as_deref())?;
    let checks = resolve_audit_checks(args.checks.as_deref())?;
    let (min_similarity, max_results) = clamp_audit_params(args.min_similarity, args.max_results);

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
    let pairwise = pairwise_findings(
        &nodes,
        &agg,
        min_similarity,
        want(AuditCheck::Dedup),
        want(AuditCheck::Contradictions),
    );
    warnings.extend(pairwise.warnings);
    let orphans = if want(AuditCheck::Orphans) {
        orphan_findings(&nodes, &agg, now)
    } else {
        Vec::new()
    };
    let rot = if want(AuditCheck::Rot) {
        rot_findings(&nodes, &agg, now)
    } else {
        Vec::new()
    };

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
        },
        "warnings": warnings,
    });
    serde_json::to_string_pretty(&report).map_err(|e| e.to_string())
}

// ── tool descriptor ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_audit",
    description = "Read-only hygiene audit of the knowledge graph (memory.semantic and memory.procedural; identity.graph and memory.entries are out of scope). checks selects any of: dedup (near-duplicate node pairs by content/tag similarity, threshold min_similarity, default 0.75), orphans (nodes with zero meaningful edges — auto same_session/temporal/tag_overlap and derived_from provenance do not count; nodes under 1 day old are skipped, 7+ days is high confidence), rot (final/version markers in titles, 90+ days unaccessed with no incoming depends_on/enables, empty payload), contradictions (same category, high tag overlap, divergent content, no contradicts edge — low confidence, read both nodes before acting). Returns JSON: summary, per-check findings capped at max_results each (default 50, max 200), stats, warnings. Never modifies anything. Feed dedup_candidates to knowledge_merge, dry_run first."
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
    /// Dedup similarity threshold in [0.0, 1.0]. Default 0.75.
    #[serde(default)]
    pub min_similarity: Option<f64>,
    /// Findings cap per check. Default 50, clamped to [1, 200].
    #[serde(default)]
    pub max_results: Option<u32>,
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
    fn rot_final_in_title_is_token_match_not_substring() {
        let agg = EdgeAggregates::default();
        let hit = meta("memory.semantic", "a", "decision", "Final architecture decision", "body long enough to not be empty payload", &[]);
        let miss = meta("memory.semantic", "b", "decision", "The penultimate lastly-noted plan", "body long enough to not be empty payload", &[]);
        let found = rot_findings(&[hit, miss], &agg, now());
        let ids: Vec<&str> = found.iter().map(|f| f["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"a"));
        // "penultimate" contains "ultimate" and "lastly" contains "last" as
        // substrings — token matching must not fire on either.
        assert!(!ids.contains(&"b"));
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
    fn rot_old_unaccessed_needs_90_days_and_zero_incoming_dep_enables() {
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
        let agg = EdgeAggregates::default();
        let found = rot_findings(std::slice::from_ref(&old), &agg, now());
        assert_eq!(found.len(), 1);
        assert!(found[0]["signals"].as_array().unwrap().iter().any(|s| s == "old_unaccessed"));

        // A recent retrieval hit clears it.
        let mut recent = mk();
        recent.last_accessed = Some(now() - chrono::Duration::days(5));
        assert!(rot_findings(std::slice::from_ref(&recent), &agg, now()).is_empty());

        // An incoming depends_on clears it too.
        let depended = mk();
        let mut agg2 = EdgeAggregates::default();
        accumulate_edge(
            &mut agg2,
            &edge_doc("memory.semantic", "x", "memory.semantic", "old", "depends_on"),
            &keys,
        );
        assert!(rot_findings(std::slice::from_ref(&depended), &agg2, now()).is_empty());
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
        let found = pairwise_findings(&[a, b], &agg, 1.0, true, false);
        // Identical nodes score exactly 1.0 — inclusive threshold keeps them.
        assert_eq!(found.dedup_total, 1);

        // A refines link marks the pair intentional — excluded.
        let a2 = meta("memory.semantic", "a", "fact", "same title here", "identical body content tokens", &["tag1"]);
        let b2 = meta("memory.semantic", "b", "fact", "same title here", "identical body content tokens", &["tag1"]);
        agg.refines_pairs.insert(pair_key(&a2.key(), &b2.key()));
        let found2 = pairwise_findings(&[a2, b2], &agg, 1.0, true, false);
        assert_eq!(found2.dedup_total, 0);
    }

    #[test]
    fn contradiction_needs_tag_overlap_not_dedup_similar_no_contradicts_or_refines_edge() {
        let a = meta("memory.semantic", "a", "fact", "alpha topic claim", "alpha conclusion entirely", &["topic", "area"]);
        let b = meta("memory.semantic", "b", "fact", "beta topic claim", "beta conclusion divergent", &["topic", "area"]);
        let mut agg = EdgeAggregates::default();
        let found = pairwise_findings(&[a, b], &agg, 0.75, false, true);
        assert_eq!(found.contradiction_total, 1);
        assert_eq!(found.contradictions[0]["confidence"], "low");

        // An existing contradicts edge = already acknowledged.
        let a2 = meta("memory.semantic", "a", "fact", "alpha topic claim", "alpha conclusion entirely", &["topic", "area"]);
        let b2 = meta("memory.semantic", "b", "fact", "beta topic claim", "beta conclusion divergent", &["topic", "area"]);
        agg.contradicts_pairs.insert(pair_key(&a2.key(), &b2.key()));
        let found2 = pairwise_findings(&[a2, b2], &agg, 0.75, false, true);
        assert_eq!(found2.contradiction_total, 0);

        // Different categories never pair.
        let c = meta("memory.semantic", "c", "decision", "alpha topic claim", "alpha conclusion entirely", &["topic", "area"]);
        let d = meta("memory.semantic", "d", "observation", "beta topic claim", "beta conclusion divergent", &["topic", "area"]);
        let found3 = pairwise_findings(&[c, d], &EdgeAggregates::default(), 0.75, false, true);
        assert_eq!(found3.contradiction_total, 0);
    }

    #[test]
    fn pairwise_cap_takes_newest_and_warns() {
        // Nodes arrive newest-first from fetch_recent; the cap keeps the
        // head of that order and says so.
        let nodes: Vec<NodeMeta> = (0..(AUDIT_PAIRWISE_CAP + 3))
            .map(|i| meta("memory.semantic", &format!("n{}", i), "fact", "t", "body words here", &[]))
            .collect();
        let found = pairwise_findings(&nodes, &EdgeAggregates::default(), 0.99, true, false);
        assert_eq!(found.warnings.len(), 1);
        assert!(found.warnings[0].contains("newest"));
        assert!(found.warnings[0].contains(&format!("{}", AUDIT_PAIRWISE_CAP)));
    }

    #[test]
    fn audit_params_clamped() {
        // Defaults.
        assert_eq!(clamp_audit_params(None, None), (AUDIT_MIN_SIMILARITY_DEFAULT, 50));
        // Ceilings and floors.
        assert_eq!(clamp_audit_params(Some(2.0), Some(500)), (1.0, 200));
        assert_eq!(clamp_audit_params(Some(-0.5), Some(0)), (0.0, 1));
    }

    #[test]
    fn knowledge_audit_args_defaults_and_schema_is_plain_object() {
        let a: KnowledgeAuditArgs = serde_json::from_value(json!({})).unwrap();
        assert!(a.collections.is_none());
        assert!(a.checks.is_none());
        assert!(a.min_similarity.is_none());
        assert!(a.max_results.is_none());

        // Anthropic rejects top-level oneOf/allOf/anyOf in input_schema.
        let schema = schemars::schema_for!(KnowledgeAuditArgs);
        let v = serde_json::to_value(&schema).unwrap();
        assert!(v.get("oneOf").is_none());
        assert!(v.get("allOf").is_none());
        assert!(v.get("anyOf").is_none());
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("object"));
    }
}
