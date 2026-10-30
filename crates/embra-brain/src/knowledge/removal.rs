//! What a node-deleting path removes, recorded before the delete.
//!
//! On 2026-10-30 `knowledge_sweep_orphans` removed 5,495 edges, 57 of them
//! links the brain had authored, and said `deleted: 5495`: no id, no type,
//! nothing to restore from (Embra #30, #32). Now `knowledge_sweep_orphans`,
//! `knowledge_unlink_node` and `forget` read every edge they are about to
//! remove with the paged arm scans (`node_edges`), write a removal record
//! under `KG_DUMPS` in the dump format (`write_removal_record`; a record
//! that cannot be written removes nothing), delete by id, and reply with
//! the removed edges split by type and every curated link named
//! (`removal_report`).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::db::WardsonDbClient;

use super::tools::{
    dump_edge_record, dump_meta_record, dump_node_record, next_scan_page, scan_page_query_body, ScanPage,
    ORPHAN_SCAN_PAGE,
};
use super::traversal::{source_arm_filter, target_arm_filter};
use super::types::EdgeType;

/// Curated edges a reply lists before "the record holds every edge".
pub(crate) const CURATED_LINES_SHOWN: usize = 100;

/// Every edge that names the node on either side: both arms, paged like the
/// orphan scan, deduplicated by `_id`, the documents whole. No window: a
/// record must hold every edge the delete takes, so a page that cannot be
/// read is an error and never a shorter list.
pub(crate) async fn node_edges(db: &WardsonDbClient, collection: &str, id: &str) -> Result<Vec<Value>, String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut edges: Vec<Value> = Vec::new();
    let arms = [
        Value::Object(source_arm_filter(collection, id, None, None)),
        Value::Object(target_arm_filter(collection, id, None, None)),
    ];
    for filter in &arms {
        let mut page = ScanPage::Offset(0);
        loop {
            let body = scan_page_query_body(filter, ORPHAN_SCAN_PAGE, &page);
            let (docs, meta) = db
                .query_with_meta("memory.edges", &body)
                .await
                .map_err(|e| format!("reading the edges of {collection}:{id} failed: {e}"))?;
            let page_len = docs.len();
            for doc in docs {
                let Some(edge_id) = doc.get("_id").and_then(|v| v.as_str()) else { continue };
                if seen.insert(edge_id.to_string()) {
                    edges.push(doc);
                }
            }
            match next_scan_page(&meta, &page, page_len, ORPHAN_SCAN_PAGE) {
                Some(next) => page = next,
                None => break,
            }
        }
    }
    Ok(edges)
}

/// The edges of several documents, each read with `node_edges`, as one
/// deduplicated list (an edge between two of them is read from both).
pub(crate) async fn edges_of_all(db: &WardsonDbClient, targets: &[(&str, &str)]) -> Result<Vec<Value>, String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut edges: Vec<Value> = Vec::new();
    for (collection, id) in targets {
        for doc in node_edges(db, collection, id).await? {
            let Some(edge_id) = doc.get("_id").and_then(|v| v.as_str()) else { continue };
            if seen.insert(edge_id.to_string()) {
                edges.push(doc);
            }
        }
    }
    Ok(edges)
}

/// What an edge is to the graph: derived by the brain at write time and
/// regenerated from node state (`auto`), authored through `knowledge_link`
/// (`curated`: the only edges that encode a judgement, lost for good when
/// their node goes), or neither (`derived_from`, the free-form relations of
/// an identity projection, an edge without a type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bucket {
    Auto,
    Curated,
    Other,
}

/// The bucket of a stored edge, by its type alone: no field marks a curated
/// edge (`knowledge_link` writes `metadata: {}`).
pub(crate) fn bucket_of(edge: &Value) -> Bucket {
    match edge.get("edge_type").and_then(|v| v.as_str()).and_then(EdgeType::parse_lossy) {
        Some(t) if t.is_auto_derived() => Bucket::Auto,
        Some(t) if t.is_brain_created() => Bucket::Curated,
        _ => Bucket::Other,
    }
}

/// The removed edges by bucket and type, with every curated edge named.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct RemovalSummary {
    pub total: usize,
    /// `(type, count)`, count-descending then name, as the stats print them.
    pub auto: Vec<(String, usize)>,
    pub curated: Vec<(String, usize)>,
    pub other: Vec<(String, usize)>,
    /// One line per curated edge: `<id> <source> → <target> <type> <weight>`,
    /// by type then id.
    pub curated_lines: Vec<String>,
    /// The nodes the call removes, `<collection>:<id>`.
    pub nodes: Vec<String>,
}

impl RemovalSummary {
    pub(crate) fn of(edges: &[Value]) -> Self {
        let mut counts: [BTreeMap<String, usize>; 3] = Default::default();
        let mut curated: Vec<(String, String, String)> = Vec::new();
        for edge in edges {
            let kind = edge.get("edge_type").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()).unwrap_or("(untyped)");
            let slot = match bucket_of(edge) {
                Bucket::Auto => 0,
                Bucket::Curated => 1,
                Bucket::Other => 2,
            };
            *counts[slot].entry(kind.to_string()).or_insert(0) += 1;
            if slot == 1 {
                let field = |key: &str| edge.get(key).and_then(|v| v.as_str()).unwrap_or("?");
                let weight = edge.get("weight").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let line = format!(
                    "{} {}:{} → {}:{} {} {:.2}",
                    field("_id"),
                    field("source_collection"),
                    field("source_id"),
                    field("target_collection"),
                    field("target_id"),
                    kind,
                    weight
                );
                curated.push((kind.to_string(), field("_id").to_string(), line));
            }
        }
        curated.sort();
        let ordered = |map: BTreeMap<String, usize>| -> Vec<(String, usize)> {
            let mut v: Vec<(String, usize)> = map.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            v
        };
        let [auto, curated_counts, other] = counts;
        Self {
            total: edges.len(),
            auto: ordered(auto),
            curated: ordered(curated_counts),
            other: ordered(other),
            curated_lines: curated.into_iter().map(|(_, _, line)| line).collect(),
            nodes: Vec::new(),
        }
    }

    pub(crate) fn with_nodes(mut self, nodes: Vec<String>) -> Self {
        self.nodes = nodes;
        self
    }

    pub(crate) fn auto_count(&self) -> usize {
        self.auto.iter().map(|(_, n)| n).sum()
    }

    pub(crate) fn curated_count(&self) -> usize {
        self.curated.iter().map(|(_, n)| n).sum()
    }

    pub(crate) fn other_count(&self) -> usize {
        self.other.iter().map(|(_, n)| n).sum()
    }
}

fn counts_line(counts: &[(String, usize)]) -> String {
    counts.iter().map(|(kind, n)| format!("{kind}={n}")).collect::<Vec<_>>().join(", ")
}

/// The block every removing reply appends: the split, the curated edges by
/// id (capped, the record holding the rest), the nodes, and the record's
/// path. A dry run prints the same lines with `to remove` for `removed` and
/// no record.
pub(crate) fn removal_report(summary: &RemovalSummary, record: Option<&Path>, dry_run: bool) -> String {
    let verb = if dry_run { "to remove" } else { "removed" };
    let mut out = format!(
        "\n  edges {verb}: {} (auto {}, curated {}, other {})",
        summary.total,
        summary.auto_count(),
        summary.curated_count(),
        summary.other_count()
    );
    if !summary.auto.is_empty() {
        out.push_str(&format!("\n  auto: {}", counts_line(&summary.auto)));
    }
    if !summary.curated.is_empty() {
        out.push_str(&format!("\n  curated: {}", counts_line(&summary.curated)));
        for line in summary.curated_lines.iter().take(CURATED_LINES_SHOWN) {
            out.push_str("\n    ");
            out.push_str(line);
        }
        if summary.curated_lines.len() > CURATED_LINES_SHOWN {
            out.push_str(&format!(
                "\n    ({} of {} shown; the record holds every edge)",
                CURATED_LINES_SHOWN,
                summary.curated_lines.len()
            ));
        }
    }
    if !summary.other.is_empty() {
        out.push_str(&format!("\n  other: {}", counts_line(&summary.other)));
    }
    if !summary.nodes.is_empty() {
        out.push_str(&format!("\n  nodes {verb}: {}", summary.nodes.join(", ")));
    }
    match record {
        Some(path) => out.push_str(&format!("\n  record: {}", path.display())),
        None if dry_run => out.push_str("\n  record: not written (dry_run)"),
        None => out.push_str("\n  record: not written (nothing to remove)"),
    }
    out
}

/// `<dir>/<stem>.jsonl`, created new; a name already taken gets `-2`, `-3`,
/// … so a second call in the same second never truncates the first.
pub(crate) async fn open_new_jsonl(dir: &str, stem: &str) -> Result<(tokio::fs::File, PathBuf), String> {
    for attempt in 1..=1000u32 {
        let name = if attempt == 1 { format!("{stem}.jsonl") } else { format!("{stem}-{attempt}.jsonl") };
        let path = Path::new(dir).join(name);
        match tokio::fs::OpenOptions::new().write(true).create_new(true).open(&path).await {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Failed to create {}: {}", path.display(), e)),
        }
    }
    Err(format!("Failed to create {stem}.jsonl under {dir}: every name is taken"))
}

/// The removal record of one call: `removed-<tool>-<ts>.jsonl` under `dir`,
/// a dump file — the meta line carrying `source` (the tool and its target),
/// a node line per `(collection, document)` with its payload, then every
/// edge. Written whole and synced before the first delete runs; nothing is
/// removed when it cannot be.
pub(crate) async fn write_removal_record(
    dir: &str,
    tool: &str,
    source: Value,
    nodes: &[(&str, &Value)],
    edges: &[Value],
) -> Result<PathBuf, String> {
    tokio::fs::create_dir_all(dir).await.map_err(|e| format!("Failed to create {dir}: {e}"))?;
    let stem = format!("removed-{tool}-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
    let (file, path) = open_new_jsonl(dir, &stem).await?;
    let mut collections: Vec<&str> = Vec::new();
    for (collection, _) in nodes {
        if !collections.contains(collection) {
            collections.push(collection);
        }
    }
    if !edges.is_empty() {
        collections.push("memory.edges");
    }
    let mut lines = Vec::with_capacity(1 + nodes.len() + edges.len());
    lines.push(dump_meta_record(&collections, None, true, Some(&source)));
    for (collection, doc) in nodes {
        lines.push(dump_node_record(doc, collection, true));
    }
    for edge in edges {
        lines.push(dump_edge_record(edge));
    }
    let written = async {
        let mut writer = tokio::io::BufWriter::new(file);
        for line in &lines {
            writer.write_all(line.to_string().as_bytes()).await?;
            writer.write_all(b"\n").await?;
        }
        writer.flush().await?;
        writer.into_inner().sync_all().await
    }
    .await;
    if let Err(e) = written {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(format!("Failed to write {}: {}", path.display(), e));
    }
    Ok(path)
}

/// One line in the brain log per removing call, under the tool's target.
pub(crate) fn log_removal(tool: &str, target: &str, summary: &RemovalSummary, record: &Path) {
    let (edges, auto, curated, other, nodes) =
        (summary.total, summary.auto_count(), summary.curated_count(), summary.other_count(), summary.nodes.len());
    let record = record.display().to_string();
    match tool {
        "knowledge_sweep_orphans" => tracing::info!(
            target: "kg::sweep", tool, target, edges, auto, curated, other, nodes, record, "knowledge removed; the record holds every edge"
        ),
        "knowledge_unlink_node" => tracing::info!(
            target: "kg::unlink", tool, target, edges, auto, curated, other, nodes, record, "knowledge removed; the record holds every edge"
        ),
        _ => tracing::info!(
            target: "kg::forget", tool, target, edges, auto, curated, other, nodes, record, "knowledge removed; the record holds every edge"
        ),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A directory of this process's own under the system temp directory,
    /// removed when dropped.
    pub(crate) struct TempDir(pub PathBuf);
    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let d = std::env::temp_dir().join(format!(
                "embra-removal-test-{}-{}-{}",
                tag,
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&d).unwrap();
            TempDir(std::fs::canonicalize(&d).unwrap())
        }
        pub(crate) fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn edge(id: &str, kind: &str, src: &str, tgt: &str, weight: f64) -> Value {
        json!({
            "_id": id, "edge_type": kind, "weight": weight,
            "source_collection": "memory.semantic", "source_id": src,
            "target_collection": "memory.semantic", "target_id": tgt,
            "metadata": {}, "created_at": "2026-09-14T02:55:20Z",
        })
    }

    /// The lines of a record file, parsed.
    pub(crate) fn record_lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn the_split_buckets_every_edge_type_and_tolerates_a_missing_one() {
        let edges = vec![
            edge("a1", "same_session", "n1", "n2", 0.5),
            edge("a2", "tag_overlap", "n1", "n3", 0.3),
            edge("a3", "same_session", "n1", "n4", 0.5),
            edge("c1", "related_to", "n1", "n5", 0.8),
            edge("c2", "refines", "n6", "n1", 0.92),
            edge("c3", "related_to", "n7", "n1", 0.7),
            edge("o1", "derived_from", "n1", "e1", 1.0),
            edge("o2", "has_trait", "n1", "i1", 1.0),
            json!({"_id": "o3", "source_id": "n1", "target_id": "n8"}),
        ];
        let summary = RemovalSummary::of(&edges);
        assert_eq!((summary.total, summary.auto_count(), summary.curated_count(), summary.other_count()), (9, 3, 3, 3));
        assert_eq!(summary.auto, [("same_session".to_string(), 2), ("tag_overlap".to_string(), 1)]);
        assert_eq!(summary.curated, [("related_to".to_string(), 2), ("refines".to_string(), 1)]);
        assert_eq!(
            summary.other,
            [("(untyped)".to_string(), 1), ("derived_from".to_string(), 1), ("has_trait".to_string(), 1)]
        );
        assert_eq!(
            summary.curated_lines,
            [
                "c2 memory.semantic:n6 → memory.semantic:n1 refines 0.92",
                "c1 memory.semantic:n1 → memory.semantic:n5 related_to 0.80",
                "c3 memory.semantic:n7 → memory.semantic:n1 related_to 0.70",
            ]
        );
        assert_eq!(bucket_of(&json!({"edge_type": ""})), Bucket::Other);
        assert_eq!(bucket_of(&json!({"edge_type": "supersedes"})), Bucket::Curated);
        assert_eq!(bucket_of(&json!({"edge_type": "temporal"})), Bucket::Auto);
    }

    #[test]
    fn the_curated_list_is_capped_and_says_so() {
        let edges: Vec<Value> = (0..103).map(|i| edge(&format!("c{i:03}"), "related_to", "n1", &format!("n{i}"), 0.5)).collect();
        let summary = RemovalSummary::of(&edges).with_nodes(vec!["memory.semantic:n1".into()]);
        let block = removal_report(&summary, Some(Path::new("/embra/workspace/KG_DUMPS/removed-forget-x.jsonl")), false);
        assert!(block.starts_with("\n  edges removed: 103 (auto 0, curated 103, other 0)\n  curated: related_to=103\n    c000 "), "{block}");
        assert!(block.contains("\n    c099 memory.semantic:n1 → memory.semantic:n99 related_to 0.50\n    (100 of 103 shown; the record holds every edge)\n  nodes removed: memory.semantic:n1\n  record: /embra/workspace/KG_DUMPS/removed-forget-x.jsonl"), "{block}");
        assert!(!block.contains("c100 "), "{block}");
        let dry = removal_report(&RemovalSummary::of(&edges[..2]), None, true);
        assert_eq!(
            dry,
            "\n  edges to remove: 2 (auto 0, curated 2, other 0)\n  curated: related_to=2\n    c000 memory.semantic:n1 → memory.semantic:n0 related_to 0.50\n    c001 memory.semantic:n1 → memory.semantic:n1 related_to 0.50\n  record: not written (dry_run)"
        );
        assert_eq!(
            removal_report(&RemovalSummary::default(), None, false),
            "\n  edges removed: 0 (auto 0, curated 0, other 0)\n  record: not written (nothing to remove)"
        );
    }

    #[tokio::test]
    async fn a_record_is_a_dump_with_its_source_and_a_same_second_record_takes_a_suffix() {
        let dir = TempDir::new("record");
        let node = json!({"_id": "n1", "content": "one", "embedding": "AAAA"});
        let entry = json!({"_id": "e1", "content": "one", "promoted_to": {"collection": "memory.semantic", "id": "n1"}});
        let edges = vec![edge("c1", "related_to", "n1", "n5", 0.8), edge("a1", "same_session", "n1", "n2", 0.5)];
        let source = json!({"tool": "forget", "target": "memory.entries:e1", "node": "memory.semantic:n1"});
        let first = write_removal_record(dir.path(), "forget", source.clone(), &[("memory.semantic", &node), ("memory.entries", &entry)], &edges)
            .await
            .expect("written");
        let second = write_removal_record(dir.path(), "forget", source.clone(), &[], &edges).await.expect("written");
        assert!(first.file_name().unwrap().to_str().unwrap().starts_with("removed-forget-"), "{first:?}");
        assert_ne!(first, second, "a second record never truncates the first");
        assert!(second.to_str().unwrap().ends_with("-2.jsonl") || first.to_str().unwrap() != second.to_str().unwrap());

        let lines = record_lines(&first);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0]["type"], "meta");
        assert_eq!(lines[0]["source"], source);
        assert_eq!(lines[0]["collections"], json!(["memory.semantic", "memory.entries", "memory.edges"]));
        assert_eq!(lines[0]["include_payload"], true);
        assert_eq!(lines[1], json!({"type": "node", "_id": "n1", "collection": "memory.semantic", "data": node}));
        assert_eq!(lines[2]["collection"], "memory.entries");
        assert_eq!(lines[3]["type"], "edge");
        assert_eq!(lines[3]["_id"], "c1");
        assert_eq!(lines[4]["edge_type"], "same_session");
        // A record of edges alone names no node collection.
        assert_eq!(record_lines(&second)[0]["collections"], json!(["memory.edges"]));

        // An unwritable directory is an error and leaves no file.
        let blocked = dir.0.join("file-not-dir");
        std::fs::write(&blocked, b"x").unwrap();
        let why = write_removal_record(blocked.to_str().unwrap(), "forget", source, &[], &edges).await.expect_err("refused");
        assert!(why.starts_with("Failed to create "), "{why}");
    }
}
