//! `knowledge_restore`: the records of a dump or removal file back into the
//! graph, by id.
//!
//! A removal record (`knowledge/removal.rs`) and a `knowledge_dump` file
//! share one format, so what a sweep, an unlink or a `forget` took out can
//! be put back, and a snapshot taken before a change can be replayed. The
//! restore is additive: a record whose id is present is skipped, nodes go
//! before edges, an edge is inserted only when both of its ends exist,
//! `identity.graph` records and the projection's edges are refused (the
//! projection is rebuilt from the sealed soul at boot), a slim node record
//! (no `data`) cannot be restored, and a restored node that carries a
//! stored embedding is placed in the vector index without inference.

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;
use tokio::io::AsyncBufReadExt;

use crate::config::SystemConfig;
use crate::db::client::{segment_ok, Segment};
use crate::db::WardsonDbClient;
use crate::embedding::write::{adopt_stored_vector, StoredVector};
use crate::tools::jail;

use super::tools::{node_ids, resolve_dump_collections, resolve_dump_file, DUMP_COLLECTIONS};

/// The fields the server sets on an insert, stripped from a record before
/// it is written: the intent is explicit, and `_created_at` is not
/// restorable (the server stamps now; the stored `created_at` is the
/// record's own).
const SERVER_FIELDS: [&str; 4] = ["_rev", "_created_at", "_updated_at", "_received_at"];

/// The provenance of an identity projection's edges, rebuilt at boot.
const PROJECTION_ORIGINS: [&str; 2] = ["identity_import", "user_profile"];

const IDENTITY_COLLECTION: &str = crate::identity_graph::IDENTITY_COLLECTION;

/// What became of one record.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Inserted,
    Present,
    Slim,
    EndMissing,
    IdentityRefused,
    NotSelected,
    Malformed(String),
    Failed(String),
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Tally {
    inserted: usize,
    present: usize,
    slim: usize,
    end_missing: usize,
}

/// The report of one run, rendered by `render`.
#[derive(Debug, Default)]
struct Report {
    source: Option<String>,
    /// Per collection, in dump order (`DUMP_COLLECTIONS`).
    tallies: Vec<(&'static str, Tally)>,
    identity_refused: usize,
    not_selected: usize,
    malformed: usize,
    malformed_first: Option<String>,
    failed: usize,
    failed_first: Option<String>,
    /// Placed, without a stored embedding, of another model or unreadable.
    vectors: (usize, usize, usize),
    records: usize,
}

impl Report {
    fn tally(&mut self, collection: &'static str) -> &mut Tally {
        if let Some(i) = self.tallies.iter().position(|(c, _)| *c == collection) {
            return &mut self.tallies[i].1;
        }
        self.tallies.push((collection, Tally::default()));
        &mut self.tallies.last_mut().expect("just pushed").1
    }

    fn count(&mut self, collection: Option<&'static str>, line: usize, outcome: Outcome) {
        self.records += 1;
        match outcome {
            Outcome::Inserted => self.tally(collection.unwrap_or("?")).inserted += 1,
            Outcome::Present => self.tally(collection.unwrap_or("?")).present += 1,
            Outcome::Slim => self.tally(collection.unwrap_or("?")).slim += 1,
            Outcome::EndMissing => self.tally("memory.edges").end_missing += 1,
            Outcome::IdentityRefused => self.identity_refused += 1,
            Outcome::NotSelected => self.not_selected += 1,
            Outcome::Malformed(why) => {
                self.malformed += 1;
                self.malformed_first.get_or_insert(format!("line {line}: {why}"));
            }
            Outcome::Failed(why) => {
                self.failed += 1;
                self.failed_first.get_or_insert(format!("line {line}: {why}"));
            }
        }
    }
}

/// The canonical name of a dump collection, from the record's.
fn canonical(collection: &str) -> Option<&'static str> {
    DUMP_COLLECTIONS.iter().map(|(_, full)| *full).find(|full| *full == collection)
}

/// A record's document as written: the server's fields and the dump's
/// `type` discriminator stripped, the record's `_id` kept.
fn stripped(data: &Value, id: &str) -> Value {
    let mut doc = data.clone();
    if let Some(obj) = doc.as_object_mut() {
        for field in SERVER_FIELDS {
            obj.remove(field);
        }
        obj.remove("type");
        obj.insert("_id".into(), Value::String(id.to_string()));
    }
    doc
}

/// Insert, or in a dry run look, by id: present ids are skipped.
async fn place(db: &WardsonDbClient, dry_run: bool, collection: &str, id: &str, doc: &Value) -> Outcome {
    if dry_run {
        return match db.read(collection, id).await {
            Ok(_) => Outcome::Present,
            Err(e) if crate::db::error::is_not_found(&e) => Outcome::Inserted,
            Err(e) => Outcome::Failed(format!("{collection}:{id} could not be read: {e}")),
        };
    }
    match db.write(collection, doc).await {
        Ok(_) => Outcome::Inserted,
        Err(e) if crate::db::error::is_conflict(&e) => Outcome::Present,
        Err(e) => Outcome::Failed(format!("{collection}:{id} could not be written: {e}")),
    }
}

/// The source line of the reply, from the file's meta record.
fn source_line(meta: &Value) -> String {
    let generated = meta.get("generated_at").and_then(|v| v.as_str()).unwrap_or("?");
    match meta.get("source") {
        Some(source) if source.get("tool").is_some() => {
            let tool = source.get("tool").and_then(|v| v.as_str()).unwrap_or("?");
            let target = source.get("target").and_then(|v| v.as_str()).unwrap_or("the graph");
            format!("removed by {tool} of {target} (generated {generated})")
        }
        Some(source) if source.get("file").is_some() => {
            format!("reshaped from {} (generated {generated})", source.get("file").and_then(|v| v.as_str()).unwrap_or("?"))
        }
        _ => format!("dump generated {generated}"),
    }
}

struct Pass<'a> {
    db: &'a WardsonDbClient,
    config: &'a SystemConfig,
    dry_run: bool,
    selected: HashSet<&'static str>,
}

impl Pass<'_> {
    async fn node(&self, record: &Value, known: &mut HashSet<String>, report: &mut Report, line: usize) {
        let collection = record.get("collection").and_then(|v| v.as_str());
        let id = record.get("_id").and_then(|v| v.as_str());
        let (Some(collection), Some(id)) = (collection, id) else {
            report.count(None, line, Outcome::Malformed("a node record without collection or _id".into()));
            return;
        };
        if collection == IDENTITY_COLLECTION {
            report.count(None, line, Outcome::IdentityRefused);
            return;
        }
        let Some(collection) = canonical(collection) else {
            report.count(None, line, Outcome::Malformed(format!("unknown collection '{collection}'")));
            return;
        };
        if !self.selected.contains(collection) {
            report.count(None, line, Outcome::NotSelected);
            return;
        }
        if let Err(e) = segment_ok(Segment::Id(id)) {
            report.count(None, line, Outcome::Malformed(format!("id refused: {e}")));
            return;
        }
        let Some(data) = record.get("data").filter(|d| d.is_object()) else {
            report.count(Some(collection), line, Outcome::Slim);
            return;
        };
        let doc = stripped(data, id);
        let outcome = place(self.db, self.dry_run, collection, id, &doc).await;
        if matches!(outcome, Outcome::Inserted | Outcome::Present) {
            known.insert(id.to_string());
        }
        if !self.dry_run
            && outcome == Outcome::Inserted
            && crate::embedding::cache::EMBEDDED_COLLECTIONS.contains(&collection)
        {
            match adopt_stored_vector(self.config, collection, id, &doc).await {
                StoredVector::Placed => report.vectors.0 += 1,
                StoredVector::NoEmbedding => report.vectors.1 += 1,
                StoredVector::OtherModel | StoredVector::Malformed => report.vectors.2 += 1,
                // No provider on this instance: the index is not in use, and
                // the backfill places the vector when one is.
                StoredVector::NoProvider => {}
            }
        }
        report.count(Some(collection), line, outcome);
    }

    async fn edge(&self, record: &Value, ends: &HashSet<String>, report: &mut Report, line: usize) {
        if !self.selected.contains("memory.edges") {
            report.count(None, line, Outcome::NotSelected);
            return;
        }
        let field = |key: &str| record.get(key).and_then(|v| v.as_str());
        let (Some(id), Some(source), Some(target)) = (field("_id"), field("source_id"), field("target_id")) else {
            report.count(None, line, Outcome::Malformed("an edge record without _id, source_id or target_id".into()));
            return;
        };
        if let Err(e) = segment_ok(Segment::Id(id)) {
            report.count(None, line, Outcome::Malformed(format!("id refused: {e}")));
            return;
        }
        let origin = record.get("metadata").and_then(|m| m.get("origin")).and_then(|v| v.as_str());
        if origin.is_some_and(|o| PROJECTION_ORIGINS.contains(&o)) {
            report.count(None, line, Outcome::IdentityRefused);
            return;
        }
        if !ends.contains(source) || !ends.contains(target) {
            report.count(None, line, Outcome::EndMissing);
            return;
        }
        let doc = stripped(record, id);
        let outcome = place(self.db, self.dry_run, "memory.edges", id, &doc).await;
        report.count(Some("memory.edges"), line, outcome);
    }
}

/// Every line of the file, parsed, with its number; a line that is not JSON
/// is `Err`.
async fn records(path: &Path) -> Result<Vec<(usize, Result<Value, String>)>, String> {
    let file = tokio::fs::File::open(path).await.map_err(|e| format!("{} could not be opened: {e}", path.display()))?;
    let mut lines = tokio::io::BufReader::new(file).lines();
    let mut out = Vec::new();
    let mut n = 0usize;
    while let Some(line) = lines.next_line().await.map_err(|e| format!("{} could not be read: {e}", path.display()))? {
        n += 1;
        if line.trim().is_empty() {
            continue;
        }
        out.push((n, serde_json::from_str::<Value>(&line).map_err(|e| e.to_string())));
    }
    Ok(out)
}

/// The tool: see the module comment. `root` is the workspace root (a
/// temporary one in tests).
pub(crate) async fn run_knowledge_restore(
    db: &WardsonDbClient,
    config: &SystemConfig,
    file: &str,
    collections: Option<Vec<String>>,
    dry_run: bool,
    root: &str,
) -> Result<String, String> {
    let started = std::time::Instant::now();
    if collections.as_deref().is_some_and(|c| c.iter().any(|name| name == "identity")) {
        return Err("identity is not restorable: the projection is rebuilt from the sealed soul at boot".into());
    }
    let selected: HashSet<&'static str> = resolve_dump_collections(collections.as_deref())?
        .into_iter()
        .filter(|c| *c != IDENTITY_COLLECTION)
        .collect();
    let path = resolve_dump_file(root, file).await?;
    let lines = records(&path).await?;
    // The graph's node ids first, when edges are to be placed: an end check
    // that cannot be made places no edge, and that is known before a node
    // is written.
    let mut ends: HashSet<String> = if selected.contains("memory.edges") {
        node_ids(db).await.map_err(|e| format!("the graph's node ids could not be listed ({e}); nothing was restored"))?
    } else {
        HashSet::new()
    };
    let pass = Pass { db, config, dry_run, selected };
    let mut report = Report::default();
    let mut known: HashSet<String> = HashSet::new();
    // Pass one: the meta line and every node, in file order.
    for (line, record) in &lines {
        let record = match record {
            Ok(r) => r,
            Err(e) => {
                report.count(None, *line, Outcome::Malformed(format!("not JSON: {e}")));
                continue;
            }
        };
        match record.get("type").and_then(|v| v.as_str()) {
            Some("meta") => report.source = Some(source_line(record)),
            Some("node") => pass.node(record, &mut known, &mut report, *line).await,
            Some("edge") => {}
            Some(other) => report.count(None, *line, Outcome::Malformed(format!("unknown record type '{other}'"))),
            None => report.count(None, *line, Outcome::Malformed("a record without a type".into())),
        }
    }
    ends.extend(known);
    // Pass two: the edges, each end a node of the graph or of pass one.
    for (line, record) in &lines {
        if let Ok(record) = record
            && record.get("type").and_then(|v| v.as_str()) == Some("edge")
        {
            pass.edge(record, &ends, &mut report, *line).await;
        }
    }
    let inserted: usize = report.tallies.iter().map(|(_, t)| t.inserted).sum();
    tracing::info!(
        target: "kg::restore",
        file = %path.display(),
        inserted,
        present = report.tallies.iter().map(|(_, t)| t.present).sum::<usize>(),
        end_missing = report.tallies.iter().map(|(_, t)| t.end_missing).sum::<usize>(),
        refused = report.identity_refused,
        malformed = report.malformed,
        failed = report.failed,
        dry_run,
        "knowledge records restored"
    );
    Ok(render(&report, &path, dry_run, started.elapsed().as_secs_f64()))
}

fn render(report: &Report, path: &Path, dry_run: bool, seconds: f64) -> String {
    let mut out = format!("knowledge_restore ← {}{}\n", path.display(), if dry_run { " (dry_run)" } else { "" });
    if let Some(source) = &report.source {
        out.push_str(&format!("  source: {source}\n"));
    }
    for (_, full) in DUMP_COLLECTIONS {
        let Some((_, tally)) = report.tallies.iter().find(|(c, _)| c == full) else { continue };
        if *full == "memory.edges" {
            out.push_str(&format!(
                "  {full}: {} inserted, {} present, {} end missing\n",
                tally.inserted, tally.present, tally.end_missing
            ));
        } else {
            out.push_str(&format!("  {full}: {} inserted, {} present, {} slim\n", tally.inserted, tally.present, tally.slim));
        }
    }
    if report.identity_refused > 0 {
        out.push_str(&format!(
            "  identity.graph: {} refused (the projection is rebuilt from the sealed soul)\n",
            report.identity_refused
        ));
    }
    if report.not_selected > 0 {
        out.push_str(&format!("  not selected: {}\n", report.not_selected));
    }
    if report.malformed > 0 {
        out.push_str(&format!(
            "  malformed: {} (first: {})\n",
            report.malformed,
            report.malformed_first.as_deref().unwrap_or("?")
        ));
    }
    let (placed, without, other) = report.vectors;
    if placed + without + other > 0 {
        out.push_str(&format!(
            "  vectors: {placed} placed, {without} without a stored embedding, {other} of another model or unreadable\n"
        ));
    }
    if report.failed > 0 {
        out.push_str(&format!("  failed: {} (first: {})\n", report.failed, report.failed_first.as_deref().unwrap_or("?")));
    }
    let inserted: usize = report.tallies.iter().map(|(_, t)| t.inserted).sum();
    out.push_str(&format!(
        "  total: {inserted} {} of {} records, {seconds:.2}s",
        if dry_run { "to insert" } else { "inserted" },
        report.records
    ));
    let nodes_inserted: usize = report
        .tallies
        .iter()
        .filter(|(c, _)| *c != "memory.edges")
        .map(|(_, t)| t.inserted)
        .sum();
    if nodes_inserted > 0 && !dry_run {
        out.push_str("\n  note: a restored node whose entry's pointer is missing is healed or adopted by knowledge_sweep_orphans");
    }
    out
}

// ── the tool ────────────────────────────────────────────────────────────

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tools::registry::DispatchContext;

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "knowledge_restore",
    is_side_effectful = true,
    description = "Put the records of a knowledge_dump file or a removal record (the files under /embra/workspace/KG_DUMPS that forget, knowledge_unlink_node and knowledge_sweep_orphans write before they delete) back into the knowledge graph by id. Additive: a record whose id is present is skipped, nodes go before edges, an edge is inserted only when both of its ends exist, identity.graph records and the identity projection's edges are refused (the projection is rebuilt from the sealed soul at boot), and a slim node record (no payload) cannot be restored. A restored node that carries a stored embedding is placed in the vector index without re-embedding. dry_run=true counts what would be inserted and writes nothing."
)]
pub struct KnowledgeRestoreArgs {
    /// A file name under /embra/workspace/KG_DUMPS, or a path inside the
    /// workspace.
    pub file: String,
    /// Short names of the collections to restore: entries, semantic,
    /// procedural, edges. Omit for all of them.
    #[serde(default)]
    pub collections: Option<Vec<String>>,
    /// Count what would be inserted, skipped or refused; write nothing.
    #[serde(default)]
    pub dry_run: bool,
}

impl KnowledgeRestoreArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        run_knowledge_restore(ctx.db, ctx.config, &self.file, self.collections, self.dry_run, jail::WORKSPACE_ROOT)
            .await
            .map_err(DispatchError::Handler)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::removal::tests::{edge, TempDir};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn test_config() -> SystemConfig {
        serde_json::from_value(json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
            "created_at": "", "version": "test", "embedding_enabled": false
        }))
        .expect("minimal config deserializes")
    }

    fn data(v: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": v, "meta": {}}))
    }

    fn body(req: &Request) -> Value {
        serde_json::from_slice(&req.body).unwrap_or(Value::Null)
    }

    /// The graph's node collections: `memory.semantic` holds `ids`, the
    /// other three nothing.
    async fn graph_holds(server: &MockServer, ids: &[&str]) {
        for collection in super::super::tools::ENDPOINT_COLLECTIONS {
            let held: Vec<&str> = if collection == "memory.semantic" { ids.to_vec() } else { vec![] };
            Mock::given(method("POST"))
                .and(path(format!("/{collection}/query")))
                .respond_with(data(json!({"count": held.len()})))
                .mount(server)
                .await;
            let rows: Vec<Value> = held.iter().map(|id| json!({"_id": id, "count": 1})).collect();
            Mock::given(method("POST"))
                .and(path(format!("/{collection}/aggregate")))
                .respond_with(data(json!(rows)))
                .mount(server)
                .await;
        }
    }

    /// Every write, in order: "METHOD /path" with its body.
    async fn writes_seen(server: &MockServer) -> Vec<(String, Value)> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/docs"))
            .map(|r| (format!("{} {}", r.method, r.url.path()), body(r)))
            .collect()
    }

    /// A workspace root with a `KG_DUMPS` directory holding `name`.
    fn workspace_with(name: &str, lines: &[Value]) -> TempDir {
        let root = TempDir::new("restore");
        let dir = root.0.join("KG_DUMPS");
        std::fs::create_dir_all(&dir).unwrap();
        let text: String = lines.iter().map(|l| l.to_string() + "\n").collect();
        std::fs::write(dir.join(name), text).unwrap();
        root
    }

    fn node(collection: &str, id: &str, content: &str) -> Value {
        json!({"type": "node", "_id": id, "collection": collection, "data": {
            "_id": id, "_rev": 7, "_created_at": "2026-05-01T00:00:00Z", "_updated_at": "2026-05-02T00:00:00Z",
            "_received_at": "2026-05-01T00:00:00Z", "content": content, "created_at": "2026-05-01T00:00:00Z",
        }})
    }

    fn edge_record(id: &str, kind: &str, src: &str, tgt: &str) -> Value {
        let mut e = edge(id, kind, src, tgt, 0.8);
        e["type"] = json!("edge");
        e["_rev"] = json!(3);
        e
    }

    async fn server_accepting_writes(known: &[&str]) -> MockServer {
        let server = MockServer::start().await;
        graph_holds(&server, known).await;
        Mock::given(method("POST")).and(path("/memory.entries/docs")).respond_with(data(json!({"_id": "x"}))).mount(&server).await;
        Mock::given(method("POST")).and(path("/memory.semantic/docs")).respond_with(data(json!({"_id": "x"}))).mount(&server).await;
        Mock::given(method("POST")).and(path("/memory.procedural/docs")).respond_with(data(json!({"_id": "x"}))).mount(&server).await;
        Mock::given(method("POST")).and(path("/memory.edges/docs")).respond_with(data(json!({"_id": "x"}))).mount(&server).await;
        server
    }

    #[tokio::test]
    async fn nodes_are_inserted_before_edges_and_a_present_id_is_skipped() {
        // The edge comes first in the file; `n1` is already there (409);
        // `n2` is a node of the graph the file does not carry.
        let root = workspace_with(
            "r.jsonl",
            &[
                json!({"type": "meta", "generated_at": "2026-10-30T12:00:00Z", "collections": ["memory.entries", "memory.semantic", "memory.edges"], "edge_types": null, "include_payload": true, "source": {"tool": "forget", "target": "memory.entries:e1", "node": "memory.semantic:n1"}}),
                edge_record("c1", "related_to", "n1", "n2"),
                node("memory.entries", "e1", "one"),
                node("memory.semantic", "n1", "one"),
                node("memory.semantic", "n3", "three"),
            ],
        );
        let server = server_accepting_writes(&["n2"]).await;
        Mock::given(method("POST"))
            .and(path("/memory.semantic/docs"))
            .and(|req: &Request| body(req)["_id"] == "n1")
            .respond_with(ResponseTemplate::new(409).set_body_string("DOCUMENT_CONFLICT"))
            .with_priority(1)
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());

        let out = run_knowledge_restore(&db, &test_config(), "r.jsonl", None, false, root.path()).await.expect("restored");
        assert!(out.starts_with(&format!("knowledge_restore ← {}/KG_DUMPS/r.jsonl\n  source: removed by forget of memory.entries:e1 (generated 2026-10-30T12:00:00Z)\n  memory.entries: 1 inserted, 0 present, 0 slim\n  memory.semantic: 1 inserted, 1 present, 0 slim\n  memory.edges: 1 inserted, 0 present, 0 end missing\n  total: 3 inserted of 4 records, ", root.path())), "{out}");
        assert!(out.ends_with("\n  note: a restored node whose entry's pointer is missing is healed or adopted by knowledge_sweep_orphans"), "{out}");
        let writes = writes_seen(&server).await;
        let order: Vec<&str> = writes.iter().map(|w| w.0.as_str()).collect();
        assert_eq!(order, ["POST /memory.entries/docs", "POST /memory.semantic/docs", "POST /memory.semantic/docs", "POST /memory.edges/docs"]);
        assert_eq!(writes[3].1["_id"], "c1", "the edge goes last, its present end n1 and its graph end n2 both known");
    }

    #[tokio::test]
    async fn the_system_fields_are_stripped_and_the_id_kept() {
        let root = workspace_with("r.jsonl", &[node("memory.semantic", "n1", "one"), edge_record("c1", "refines", "n1", "n2")]);
        let server = server_accepting_writes(&["n2"]).await;
        let db = WardsonDbClient::from_url(&server.uri());
        run_knowledge_restore(&db, &test_config(), "r.jsonl", None, false, root.path()).await.expect("restored");
        let writes = writes_seen(&server).await;
        assert_eq!(writes[0].1, json!({"_id": "n1", "content": "one", "created_at": "2026-05-01T00:00:00Z"}));
        let edge = &writes[1].1;
        assert!(edge.get("type").is_none() && edge.get("_rev").is_none(), "{edge}");
        assert_eq!((edge["_id"].as_str(), edge["edge_type"].as_str(), edge["weight"].as_f64()), (Some("c1"), Some("refines"), Some(0.8)));
    }

    #[tokio::test]
    async fn an_edge_whose_end_is_missing_is_not_inserted() {
        let root = workspace_with("r.jsonl", &[edge_record("c1", "refines", "n1", "ghost")]);
        let server = server_accepting_writes(&["n1"]).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let out = run_knowledge_restore(&db, &test_config(), "r.jsonl", None, false, root.path()).await.expect("ran");
        assert!(out.contains("\n  memory.edges: 0 inserted, 0 present, 1 end missing\n  total: 0 inserted of 1 records, "), "{out}");
        assert!(writes_seen(&server).await.is_empty());
    }

    #[tokio::test]
    async fn an_identity_record_and_a_projection_edge_are_refused_and_counted() {
        let mut projection = edge_record("p1", "has_trait", "i1", "i2");
        projection["metadata"] = json!({"origin": "identity_import"});
        let root = workspace_with("r.jsonl", &[node("identity.graph", "i1", "self"), projection, edge_record("c1", "related_to", "n1", "i9")]);
        let server = server_accepting_writes(&["n1", "i9"]).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let out = run_knowledge_restore(&db, &test_config(), "r.jsonl", None, false, root.path()).await.expect("ran");
        assert!(out.contains("\n  memory.edges: 1 inserted, 0 present, 0 end missing\n  identity.graph: 2 refused (the projection is rebuilt from the sealed soul)\n"), "{out}");
        assert_eq!(writes_seen(&server).await.len(), 1, "the link into an identity node is restorable; the projection is not");
        let why = run_knowledge_restore(&db, &test_config(), "r.jsonl", Some(vec!["identity".into()]), false, root.path()).await.expect_err("refused");
        assert_eq!(why, "identity is not restorable: the projection is rebuilt from the sealed soul at boot");
    }

    #[tokio::test]
    async fn a_slim_record_a_malformed_line_and_an_unselected_collection_are_counted_and_the_rest_restored() {
        let root = TempDir::new("restore-mixed");
        let dir = root.0.join("KG_DUMPS");
        std::fs::create_dir_all(&dir).unwrap();
        let lines = [
            json!({"type": "node", "_id": "s1", "collection": "memory.semantic"}).to_string(),
            "not json".to_string(),
            json!({"type": "node", "collection": "memory.semantic", "data": {}}).to_string(),
            json!({"type": "node", "_id": "bad/id", "collection": "memory.semantic", "data": {"content": "x"}}).to_string(),
            json!({"type": "what", "_id": "w1"}).to_string(),
            node("memory.procedural", "p1", "a procedure").to_string(),
            node("memory.semantic", "n1", "one").to_string(),
        ];
        std::fs::write(dir.join("m.jsonl"), lines.join("\n") + "\n").unwrap();
        let server = server_accepting_writes(&[]).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let out = run_knowledge_restore(&db, &test_config(), "m.jsonl", Some(vec!["semantic".into()]), false, root.path()).await.expect("ran");
        assert!(out.contains("\n  memory.semantic: 1 inserted, 0 present, 1 slim\n  not selected: 1\n  malformed: 4 (first: line 2: not JSON: "), "{out}");
        assert!(out.contains("\n  total: 1 inserted of 7 records, "), "{out}");
        let writes = writes_seen(&server).await;
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].1["_id"], "n1");
    }

    #[tokio::test]
    async fn a_dry_run_writes_nothing_and_counts_by_reading() {
        let root = workspace_with("r.jsonl", &[node("memory.semantic", "n1", "one"), node("memory.semantic", "n2", "two"), edge_record("c1", "refines", "n1", "n2")]);
        let server = MockServer::start().await;
        graph_holds(&server, &[]).await;
        Mock::given(method("GET")).and(path("/memory.semantic/docs/n2")).respond_with(data(json!({"_id": "n2"}))).mount(&server).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let out = run_knowledge_restore(&db, &test_config(), "r.jsonl", None, true, root.path()).await.expect("ran");
        assert!(out.contains(" (dry_run)\n  memory.semantic: 1 inserted, 1 present, 0 slim\n  memory.edges: 1 inserted, 0 present, 0 end missing\n  total: 2 to insert of 3 records, "), "{out}");
        assert!(!out.contains("note:"), "{out}");
        assert!(writes_seen(&server).await.is_empty());
    }

    #[tokio::test]
    async fn a_file_outside_the_workspace_or_under_a_refused_place_is_refused() {
        let root = workspace_with("r.jsonl", &[]);
        let server = MockServer::start().await;
        let db = WardsonDbClient::from_url(&server.uri());
        for file in ["../r.jsonl", "/etc/hosts", "KG_DUMPS/../../r.jsonl"] {
            let why = run_knowledge_restore(&db, &test_config(), file, None, true, root.path()).await.expect_err(file);
            assert!(why.starts_with("Denied:") || why.contains("outside"), "{file}: {why}");
        }
        let why = run_knowledge_restore(&db, &test_config(), "missing.jsonl", None, true, root.path()).await.expect_err("missing");
        assert!(why.contains("could not be opened") || why.contains("Denied") || why.contains("not found"), "{why}");
        assert!(server.received_requests().await.unwrap_or_default().is_empty(), "a refused file reads nothing");
    }

    /// A removal record of the sweep restores what the sweep removed: the
    /// POST bodies are the edge documents, the server's fields and the
    /// dump's discriminator aside.
    #[tokio::test]
    async fn a_removal_record_restores_what_the_sweep_removed() {
        let root = TempDir::new("restore-round-trip");
        let dir = root.0.join("KG_DUMPS");
        std::fs::create_dir_all(&dir).unwrap();
        let edges = vec![edge("c1", "related_to", "n1", "n5", 0.8), edge("a1", "same_session", "n1", "n2", 0.5)];
        let source = json!({"tool": "knowledge_sweep_orphans", "target": null, "dangling": ["n5"]});
        let record = crate::knowledge::removal::write_removal_record(dir.to_str().unwrap(), "knowledge_sweep_orphans", source, &[], &edges)
            .await
            .expect("record");
        let name = record.file_name().unwrap().to_str().unwrap().to_string();
        let server = server_accepting_writes(&["n1", "n2", "n5"]).await;
        let db = WardsonDbClient::from_url(&server.uri());
        let out = run_knowledge_restore(&db, &test_config(), &name, None, false, root.path()).await.expect("restored");
        assert!(out.contains("\n  source: removed by knowledge_sweep_orphans of the graph (generated "), "{out}");
        assert!(out.contains("\n  memory.edges: 2 inserted, 0 present, 0 end missing\n  total: 2 inserted of 2 records, "), "{out}");
        let writes = writes_seen(&server).await;
        let restored: Vec<Value> = writes.into_iter().map(|w| w.1).collect();
        assert_eq!(restored, edges, "the edges come back as they were");
    }

    #[test]
    fn a_stored_vector_decodes_only_for_the_providers_model() {
        use crate::embedding::write::stored_vector;
        let vector = vec![0.5f32, -0.25, 0.125];
        let doc = json!({"embedding": crate::embedding::encode_vector(&vector), "embedding_model": "m"});
        assert_eq!(stored_vector(&doc, 3, "m").unwrap(), vector);
        assert_eq!(stored_vector(&doc, 3, "other").unwrap_err(), StoredVector::OtherModel);
        assert_eq!(stored_vector(&doc, 4, "m").unwrap_err(), StoredVector::Malformed, "the wrong width is no vector");
        assert_eq!(stored_vector(&json!({"content": "x"}), 3, "m").unwrap_err(), StoredVector::NoEmbedding);
        assert_eq!(stored_vector(&json!({"embedding": "", "embedding_model": "m"}), 3, "m").unwrap_err(), StoredVector::NoEmbedding);
    }

    #[test]
    fn knowledge_restore_args_defaults_and_schema() {
        let a: KnowledgeRestoreArgs = serde_json::from_str(r#"{"file": "r.jsonl"}"#).unwrap();
        assert!(a.collections.is_none() && !a.dry_run);
        let schema = serde_json::to_value(schemars::schema_for!(KnowledgeRestoreArgs)).unwrap();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["file"]));
    }
}
