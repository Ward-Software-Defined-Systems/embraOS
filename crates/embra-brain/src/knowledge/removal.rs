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

use tokio::io::AsyncWriteExt as _;

use serde_json::Value;

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
/// Built one edge at a time (`add`), so a sweep of a million orphan edges
/// holds their types and its curated lines, never the documents.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct RemovalSummary {
    pub total: usize,
    counts: [BTreeMap<String, usize>; 3],
    /// `(type, id, line)` per curated edge, sorted when read.
    curated_edges: Vec<(String, String, String)>,
    /// The nodes the call removes, `<collection>:<id>`.
    pub nodes: Vec<String>,
}

impl RemovalSummary {
    pub(crate) fn of(edges: &[Value]) -> Self {
        let mut summary = Self::default();
        for edge in edges {
            summary.add(edge);
        }
        summary
    }

    /// Count one edge; a curated one is named.
    pub(crate) fn add(&mut self, edge: &Value) {
        self.total += 1;
        let kind = edge.get("edge_type").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()).unwrap_or("(untyped)");
        let slot = match bucket_of(edge) {
            Bucket::Auto => 0,
            Bucket::Curated => 1,
            Bucket::Other => 2,
        };
        *self.counts[slot].entry(kind.to_string()).or_insert(0) += 1;
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
            self.curated_edges.push((kind.to_string(), field("_id").to_string(), line));
        }
    }

    pub(crate) fn with_nodes(mut self, nodes: Vec<String>) -> Self {
        self.nodes = nodes;
        self
    }

    fn ordered(&self, slot: usize) -> Vec<(String, usize)> {
        let mut v: Vec<(String, usize)> = self.counts[slot].iter().map(|(k, n)| (k.clone(), *n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// `(type, count)`, count-descending then name, as the stats print them.
    pub(crate) fn auto(&self) -> Vec<(String, usize)> {
        self.ordered(0)
    }

    pub(crate) fn curated(&self) -> Vec<(String, usize)> {
        self.ordered(1)
    }

    pub(crate) fn other(&self) -> Vec<(String, usize)> {
        self.ordered(2)
    }

    /// One line per curated edge: `<id> <source> → <target> <type> <weight>`,
    /// by type then id.
    pub(crate) fn curated_lines(&self) -> Vec<String> {
        let mut edges = self.curated_edges.clone();
        edges.sort();
        edges.into_iter().map(|(_, _, line)| line).collect()
    }

    pub(crate) fn auto_count(&self) -> usize {
        self.counts[0].values().sum()
    }

    pub(crate) fn curated_count(&self) -> usize {
        self.counts[1].values().sum()
    }

    pub(crate) fn other_count(&self) -> usize {
        self.counts[2].values().sum()
    }
}

fn counts_line(counts: &[(String, usize)]) -> String {
    counts.iter().map(|(kind, n)| format!("{kind}={n}")).collect::<Vec<_>>().join(", ")
}

/// The block every removing reply appends: the split, the curated edges by
/// id (capped, the record holding the rest), the nodes, and the record's
/// path. A dry run prints the same lines with `to remove` for `removed` and
/// no record.
pub(crate) fn removal_report(summary: &RemovalSummary, record: Option<&PartSet>, dry_run: bool) -> String {
    let verb = if dry_run { "to remove" } else { "removed" };
    let mut out = format!(
        "\n  edges {verb}: {} (auto {}, curated {}, other {})",
        summary.total,
        summary.auto_count(),
        summary.curated_count(),
        summary.other_count()
    );
    let (auto, curated, other) = (summary.auto(), summary.curated(), summary.other());
    if !auto.is_empty() {
        out.push_str(&format!("\n  auto: {}", counts_line(&auto)));
    }
    if !curated.is_empty() {
        out.push_str(&format!("\n  curated: {}", counts_line(&curated)));
        let lines = summary.curated_lines();
        for line in lines.iter().take(CURATED_LINES_SHOWN) {
            out.push_str("\n    ");
            out.push_str(line);
        }
        if lines.len() > CURATED_LINES_SHOWN {
            out.push_str(&format!("\n    ({} of {} shown; the record holds every edge)", CURATED_LINES_SHOWN, lines.len()));
        }
    }
    if !other.is_empty() {
        out.push_str(&format!("\n  other: {}", counts_line(&other)));
    }
    if !summary.nodes.is_empty() {
        out.push_str(&format!("\n  nodes {verb}: {}", summary.nodes.join(", ")));
    }
    match record {
        Some(set) => out.push_str(&format!("\n  record: {}", set.describe())),
        None if dry_run => out.push_str("\n  record: not written (dry_run)"),
        None => out.push_str("\n  record: not written (nothing to remove)"),
    }
    out
}

/// The most bytes in one part of a dump or a removal record: the console's
/// text upload (`media::text::TEXT_UPLOAD_MAX`) and `guardian_call`'s data
/// file (`tools::guardian::GUARDIAN_DATA_FILE_MAX`) take at most this, so
/// every part is usable as either without reshaping (`file_offer` allows
/// more). A dump of a production graph runs to hundreds of megabytes.
pub(crate) const DUMP_PART_MAX: u64 = 2 * 1024 * 1024;

/// One written part.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Part {
    pub path: PathBuf,
    /// Records in it, the meta line aside.
    pub records: usize,
    pub bytes: u64,
}

/// A written set: `<stem>-partNNNN.jsonl` files, or the one `<stem>.jsonl`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PartSet {
    pub stem: String,
    pub parts: Vec<Part>,
    /// Bytes written, meta lines included.
    pub bytes: u64,
    /// The cap each part was held under; 0 for one file.
    pub part_bytes: u64,
}

impl PartSet {
    pub(crate) fn first(&self) -> &Path {
        &self.parts[0].path
    }

    /// The path for one part; `<first> … <last> (N parts)` for several.
    pub(crate) fn describe(&self) -> String {
        match self.parts.as_slice() {
            [] => format!("{} (nothing written)", self.stem),
            [_] => self.first().display().to_string(),
            [.., last] => format!(
                "{} … {} ({} parts)",
                self.first().display(),
                last.path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                self.parts.len()
            ),
        }
    }

    #[cfg(test)]
    pub(crate) fn records(&self) -> usize {
        self.parts.iter().map(|p| p.records).sum()
    }
}

/// A stem no file under `dir` uses yet: `<stem>`, else `<stem>-2`, `-3`, …
/// (both the one-file and the part forms are checked), so a second set in
/// the same second never truncates the first.
async fn free_stem(dir: &str, stem: &str) -> Result<String, String> {
    for attempt in 1..=1000u32 {
        let candidate = if attempt == 1 { stem.to_string() } else { format!("{stem}-{attempt}") };
        let taken = tokio::fs::try_exists(Path::new(dir).join(format!("{candidate}.jsonl"))).await.unwrap_or(true)
            || tokio::fs::try_exists(Path::new(dir).join(format!("{candidate}-part0001.jsonl"))).await.unwrap_or(true);
        if !taken {
            return Ok(candidate);
        }
    }
    Err(format!("Failed to name {stem} under {dir}: every name is taken"))
}

/// A dump written as parts under `dir`: `<stem>-partNNNN.jsonl`, each at
/// most `part_bytes` (0: one file, `<stem>.jsonl`), no record ever split
/// across two, every part headed by the meta line with `stem` and `part`
/// added. The dumps, the removal records and a reshaped dump all go
/// through it; `knowledge_restore` and `source_file` read a set back by
/// its stem.
pub(crate) struct PartWriter {
    dir: String,
    stem: String,
    part_bytes: u64,
    meta: Value,
    open: Option<(tokio::io::BufWriter<tokio::fs::File>, Part)>,
    parts: Vec<Part>,
    bytes: u64,
}

impl PartWriter {
    pub(crate) async fn open(dir: &str, stem: &str, meta: Value, part_bytes: u64) -> Result<Self, String> {
        tokio::fs::create_dir_all(dir).await.map_err(|e| format!("Failed to create {dir}: {e}"))?;
        let stem = free_stem(dir, stem).await?;
        let mut writer = Self { dir: dir.to_string(), stem, part_bytes, meta, open: None, parts: Vec::new(), bytes: 0 };
        writer.next_part().await?;
        Ok(writer)
    }

    async fn next_part(&mut self) -> Result<(), String> {
        let index = self.parts.len() + 1;
        let name = if self.part_bytes == 0 { self.stem.clone() } else { format!("{}-part{index:04}", self.stem) };
        let path = Path::new(&self.dir).join(format!("{name}.jsonl"));
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map_err(|e| format!("Failed to create {}: {}", path.display(), e))?;
        let mut meta = self.meta.clone();
        meta["stem"] = Value::String(self.stem.clone());
        meta["part"] = Value::from(index);
        let line = format!("{meta}\n");
        let mut writer = tokio::io::BufWriter::new(file);
        writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;
        self.bytes += line.len() as u64;
        self.open = Some((writer, Part { path, records: 0, bytes: line.len() as u64 }));
        Ok(())
    }

    async fn close_part(&mut self) -> Result<(), String> {
        let Some((mut writer, part)) = self.open.take() else { return Ok(()) };
        let done = async {
            writer.flush().await?;
            writer.get_ref().sync_all().await
        };
        done.await.map_err(|e| format!("Failed to write {}: {}", part.path.display(), e))?;
        self.parts.push(part);
        Ok(())
    }

    /// One record. A new part is begun when this one would take the open
    /// part past the cap and the open part holds a record already, so a
    /// record larger than the cap has a part of its own.
    pub(crate) async fn record(&mut self, record: &Value) -> Result<(), String> {
        let line = format!("{record}\n");
        if self.part_bytes > 0
            && let Some((_, part)) = &self.open
            && part.records > 0
            && part.bytes + line.len() as u64 > self.part_bytes
        {
            self.close_part().await?;
            self.next_part().await?;
        }
        let Some((writer, part)) = self.open.as_mut() else {
            return Err("the part writer is closed".into());
        };
        writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("Failed to write {}: {}", part.path.display(), e))?;
        part.records += 1;
        part.bytes += line.len() as u64;
        self.bytes += line.len() as u64;
        Ok(())
    }

    /// Flush and sync the last part; the set.
    pub(crate) async fn finish(mut self) -> Result<PartSet, String> {
        self.close_part().await?;
        Ok(PartSet { stem: self.stem, parts: self.parts, bytes: self.bytes, part_bytes: self.part_bytes })
    }

    /// Every file of a call that failed goes.
    pub(crate) async fn discard(self) {
        for part in &self.parts {
            let _ = tokio::fs::remove_file(&part.path).await;
        }
        if let Some((_, part)) = &self.open {
            let _ = tokio::fs::remove_file(&part.path).await;
        }
    }
}

/// A removal record being written: `removed-<tool>-<ts>` under `dir`, a
/// dump — the meta line carrying `source` (the tool and its target), a node
/// line per `(collection, document)` with its payload, then every edge as
/// it is handed in (`edge`), in parts under `DUMP_PART_MAX`, so a sweep of
/// a million orphan edges holds none of their documents and every part of
/// its record can be carried out of the OS. `finish` flushes and syncs
/// before the first delete runs; `discard` removes the files of a call that
/// failed.
pub(crate) struct RecordSink {
    writer: PartWriter,
}

impl RecordSink {
    pub(crate) async fn open(
        dir: &str,
        tool: &str,
        source: Value,
        nodes: &[(&str, &Value)],
        with_edges: bool,
    ) -> Result<Self, String> {
        let stem = format!("removed-{tool}-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
        let mut collections: Vec<&str> = Vec::new();
        for (collection, _) in nodes {
            if !collections.contains(collection) {
                collections.push(collection);
            }
        }
        if with_edges {
            collections.push("memory.edges");
        }
        let meta = dump_meta_record(&collections, None, true, Some(&source));
        let mut sink = Self { writer: PartWriter::open(dir, &stem, meta, DUMP_PART_MAX).await? };
        for (collection, doc) in nodes {
            if let Err(e) = sink.writer.record(&dump_node_record(doc, collection, true)).await {
                sink.discard().await;
                return Err(e);
            }
        }
        Ok(sink)
    }

    /// One edge, as it is read.
    pub(crate) async fn edge(&mut self, edge: &Value) -> Result<(), String> {
        self.writer.record(&dump_edge_record(edge)).await
    }

    /// Flush and sync; the complete record.
    pub(crate) async fn finish(self) -> Result<PartSet, String> {
        self.writer.finish().await
    }

    /// The files of a call that failed go.
    pub(crate) async fn discard(self) {
        self.writer.discard().await
    }
}

/// The removal record of one call whose edges are in hand: a `RecordSink`
/// opened, every edge written, finished.
pub(crate) async fn write_removal_record(
    dir: &str,
    tool: &str,
    source: Value,
    nodes: &[(&str, &Value)],
    edges: &[Value],
) -> Result<PartSet, String> {
    let mut sink = RecordSink::open(dir, tool, source, nodes, !edges.is_empty()).await?;
    for edge in edges {
        if let Err(e) = sink.edge(edge).await {
            sink.discard().await;
            return Err(e);
        }
    }
    sink.finish().await
}

/// One line in the brain log per removing call, under the tool's target.
pub(crate) fn log_removal(tool: &str, target: &str, summary: &RemovalSummary, record: &PartSet) {
    let (edges, auto, curated, other, nodes) =
        (summary.total, summary.auto_count(), summary.curated_count(), summary.other_count(), summary.nodes.len());
    let record = record.describe();
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
        assert_eq!(summary.auto(), [("same_session".to_string(), 2), ("tag_overlap".to_string(), 1)]);
        assert_eq!(summary.curated(), [("related_to".to_string(), 2), ("refines".to_string(), 1)]);
        assert_eq!(
            summary.other(),
            [("(untyped)".to_string(), 1), ("derived_from".to_string(), 1), ("has_trait".to_string(), 1)]
        );
        // One at a time gives the same summary as all at once.
        let mut incremental = RemovalSummary::default();
        for edge in &edges {
            incremental.add(edge);
        }
        assert_eq!(incremental, summary);
        assert_eq!(
            summary.curated_lines(),
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
        let one = PartSet {
            stem: "removed-forget-x".into(),
            parts: vec![Part { path: PathBuf::from("/embra/workspace/KG_DUMPS/removed-forget-x.jsonl"), records: 103, bytes: 1 }],
            bytes: 1,
            part_bytes: 0,
        };
        let block = removal_report(&summary, Some(&one), false);
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
        assert_eq!(first.parts.len(), 1);
        let name = first.first().file_name().unwrap().to_str().unwrap().to_string();
        assert!(name.starts_with("removed-forget-") && name.ends_with("-part0001.jsonl"), "{name}");
        assert_ne!(first.first(), second.first(), "a second record never truncates the first");
        assert!(second.stem.ends_with("-2") || second.stem != first.stem, "{} vs {}", first.stem, second.stem);
        assert_eq!(first.describe(), first.first().display().to_string());

        let lines = record_lines(first.first());
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0]["type"], "meta");
        assert_eq!(lines[0]["source"], source);
        assert_eq!(lines[0]["collections"], json!(["memory.semantic", "memory.entries", "memory.edges"]));
        assert_eq!(lines[0]["include_payload"], true);
        assert_eq!((lines[0]["stem"].as_str(), lines[0]["part"].as_u64()), (Some(first.stem.as_str()), Some(1)));
        assert_eq!(lines[1], json!({"type": "node", "_id": "n1", "collection": "memory.semantic", "data": node}));
        assert_eq!(lines[2]["collection"], "memory.entries");
        assert_eq!(lines[3]["type"], "edge");
        assert_eq!(lines[3]["_id"], "c1");
        assert_eq!(lines[4]["edge_type"], "same_session");
        // A record of edges alone names no node collection.
        assert_eq!(record_lines(second.first())[0]["collections"], json!(["memory.edges"]));

        // An unwritable directory is an error and leaves no file.
        let blocked = dir.0.join("file-not-dir");
        std::fs::write(&blocked, b"x").unwrap();
        let why = write_removal_record(blocked.to_str().unwrap(), "forget", source, &[], &edges).await.expect_err("refused");
        assert!(why.starts_with("Failed to create "), "{why}");
    }

    /// A set is written in parts under the cap, no record split, each part
    /// headed by the meta line with the stem and its number; `discard`
    /// removes every part; 0 writes one file; the cap fits the console's
    /// upload and the Guardian's data file.
    #[tokio::test]
    async fn a_set_is_written_in_parts_under_the_cap_and_no_record_is_split() {
        let dir = TempDir::new("parts");
        let meta = json!({"type": "meta", "collections": ["memory.edges"], "include_payload": true});
        let mut writer = PartWriter::open(dir.path(), "kg-dump-x", meta.clone(), 700).await.expect("opened");
        let records: Vec<Value> = (0..9).map(|i| edge(&format!("e{i}"), "related_to", "n1", "n2", 0.5)).collect();
        let big = json!({"_id": "big", "type": "edge", "edge_type": "related_to", "pad": "x".repeat(900)});
        for r in records.iter().chain(std::iter::once(&big)) {
            writer.record(r).await.expect("written");
        }
        let set = writer.finish().await.expect("finished");
        assert!(set.parts.len() >= 3, "{set:?}");
        assert_eq!(set.records(), 10);
        assert_eq!(set.part_bytes, 700);
        let mut ids = Vec::new();
        for (i, part) in set.parts.iter().enumerate() {
            let lines = record_lines(&part.path);
            assert_eq!((lines[0]["stem"].as_str(), lines[0]["part"].as_u64()), (Some("kg-dump-x"), Some(i as u64 + 1)), "{part:?}");
            assert_eq!(lines.len() - 1, part.records);
            assert_eq!(std::fs::metadata(&part.path).unwrap().len(), part.bytes);
            let name = part.path.file_name().unwrap().to_str().unwrap().to_string();
            assert_eq!(name, format!("kg-dump-x-part{:04}.jsonl", i + 1));
            // Under the cap, or one record that is larger than the cap on its own.
            assert!(part.bytes <= 700 || part.records == 1, "{part:?}");
            ids.extend(lines[1..].iter().map(|l| l["_id"].as_str().unwrap().to_string()));
        }
        let expected: Vec<String> = (0..9).map(|i| format!("e{i}")).chain(std::iter::once("big".to_string())).collect();
        assert_eq!(ids, expected, "every record once, in order, none split");
        assert!(set.describe().ends_with(&format!("({} parts)", set.parts.len())), "{}", set.describe());

        // The same stem again in the same second takes a suffix.
        let again = PartWriter::open(dir.path(), "kg-dump-x", meta.clone(), 700).await.expect("opened");
        assert_eq!(again.stem, "kg-dump-x-2");
        again.discard().await;
        assert!(!dir.0.join("kg-dump-x-2-part0001.jsonl").exists(), "discard removes the open part");

        // Zero: one file, the plain name.
        let mut one = PartWriter::open(dir.path(), "kg-dump-one", meta, 0).await.expect("opened");
        for r in &records {
            one.record(r).await.expect("written");
        }
        let set = one.finish().await.expect("finished");
        assert_eq!(set.parts.len(), 1);
        assert_eq!(set.first().file_name().unwrap().to_str().unwrap(), "kg-dump-one.jsonl");
        assert_eq!(set.describe(), set.first().display().to_string());

        const { assert!(DUMP_PART_MAX <= crate::media::text::TEXT_UPLOAD_MAX as u64, "a part is an upload") };
        const { assert!(DUMP_PART_MAX <= crate::tools::guardian::GUARDIAN_DATA_FILE_MAX, "a part is a data file") };
    }
}
