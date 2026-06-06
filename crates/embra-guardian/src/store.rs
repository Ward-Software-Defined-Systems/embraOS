//! Persisted manifest. One [`ToolDoc`] per dynamic tool, stored by
//! `embra-brain` in the WardSONDB `guardian.tools` collection (one doc,
//! `_id == name`). The `.wasm` artifact lives on the DATA filesystem, not
//! in the DB; `source` lets reconcile rebuild a missing/stale artifact.
//!
//! `embra-guardian` stays decoupled from the brain's DB client via the
//! [`GuardianPersistence`] trait (the brain injects the impl), mirroring
//! how `host` takes an injected `HttpTransport`.

use embra_tools_core::BoxFut;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const TOOL_DOC_FORMAT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolStatus {
    /// Brain-drafted and soul-checked, awaiting operator approval. NOT
    /// built, NOT loaded into the overlay — a proposal, not a tool yet.
    /// Reached only via the brain's `guardian_propose`; the operator
    /// `/guardian-define` path skips straight to `Building`.
    Proposed,
    /// Validated; (re)build spawned, artifact not yet ready.
    Building,
    /// Artifact compiled + loaded into the overlay.
    Ready,
    /// Validation/build failed; kept for inspection, not loaded.
    Failed,
}

/// Outcome of the replicant check (the soul-spec evaluation a
/// brain-proposed tool must pass before it becomes a proposal). Pure
/// persisted metadata — `embra-guardian` stores it but never produces it;
/// the verdict is computed brain-side in `crate::guardian::replicant`.
/// Only `allow`/`escalate` verdicts are ever persisted (a `refuse` blocks
/// the proposal, so no `ToolDoc` is written). Absent on operator-defined
/// tools (`/guardian-define` bypasses the check).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicantRecord {
    /// "allow" or "escalate" (the operator decides on an escalate).
    pub verdict: String,
    /// Soul lines the judge flagged as touched (verbatim text).
    #[serde(default)]
    pub touched_lines: Vec<String>,
    /// The judge's one-paragraph rationale.
    #[serde(default)]
    pub rationale: String,
    /// Model id that produced the verdict (audit trail).
    #[serde(default)]
    pub model: String,
    /// RFC3339 timestamp of the judgment.
    pub judged_at: String,
}

fn default_format() -> u32 {
    TOOL_DOC_FORMAT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDoc {
    #[serde(default = "default_format")]
    pub format_version: u32,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub source: String,
    #[serde(default)]
    pub caps: Vec<String>,
    pub status: ToolStatus,
    pub toolchain_version: String,
    pub source_sha256: String,
    #[serde(default)]
    pub build_log_tail: String,
    /// Replicant-check verdict, present on brain-proposed tools only.
    /// Additive + optional so pre-replicant docs (and operator-defined
    /// tools) load unchanged — does NOT bump `TOOL_DOC_FORMAT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicant: Option<ReplicantRecord>,
    pub created_at: String,
    pub updated_at: String,
}

impl ToolDoc {
    /// Build a `building` doc from a freshly validated module.
    pub fn building(
        name: &str,
        description: &str,
        input_schema: Value,
        source: &str,
        caps: Vec<String>,
        toolchain_version: &str,
        now_rfc3339: &str,
    ) -> Self {
        Self {
            format_version: TOOL_DOC_FORMAT,
            name: name.to_string(),
            description: description.to_string(),
            input_schema,
            source: source.to_string(),
            caps,
            status: ToolStatus::Building,
            toolchain_version: toolchain_version.to_string(),
            source_sha256: sha256_hex(source),
            build_log_tail: String::new(),
            replicant: None,
            created_at: now_rfc3339.to_string(),
            updated_at: now_rfc3339.to_string(),
        }
    }

    /// Serialize to the WardSONDB document shape (`_id == name`).
    pub fn to_value(&self) -> Value {
        let mut v = serde_json::to_value(self).expect("ToolDoc serializes");
        if let Value::Object(m) = &mut v {
            m.insert("_id".into(), Value::String(self.name.clone()));
        }
        v
    }

    pub fn from_value(v: &Value) -> Result<Self, String> {
        serde_json::from_value(v.clone()).map_err(|e| e.to_string())
    }

    /// True if `disk_source` still matches what was persisted — used by
    /// reconcile to detect drift.
    pub fn source_matches(&self, disk_source: &str) -> bool {
        self.source_sha256 == sha256_hex(disk_source)
    }
}

pub fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

/// Brain-injected durable store. Async via `BoxFut` (object-safe; the
/// brain boxes its WardSONDB calls) — no `async_trait` dep needed.
pub trait GuardianPersistence: Send + Sync {
    fn load_all(&self) -> BoxFut<'_, Result<Vec<ToolDoc>, String>>;
    fn upsert(&self, doc: ToolDoc) -> BoxFut<'_, Result<(), String>>;
    fn delete(&self, name: &str) -> BoxFut<'_, Result<(), String>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn doc_round_trips_through_value_with_id() {
        let d = ToolDoc::building(
            "web_search",
            "desc",
            json!({"type":"object","properties":{}}),
            "// guardian-tool: web_search\nfn run(i:&str)->String{String::new()}",
            vec!["http_get".into()],
            "1.94.1",
            "2026-05-17T00:00:00Z",
        );
        let v = d.to_value();
        assert_eq!(v["_id"], "web_search");
        assert_eq!(v["status"], "building");
        assert_eq!(v["format_version"], TOOL_DOC_FORMAT);
        let back = ToolDoc::from_value(&v).unwrap();
        assert_eq!(back.name, "web_search");
        assert_eq!(back.caps, vec!["http_get"]);
        assert_eq!(back.status, ToolStatus::Building);
        assert!(back.source_matches(&d.source));
        assert!(!back.source_matches("tampered"));
    }

    #[test]
    fn status_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&ToolStatus::Ready).unwrap(), "\"ready\"");
        assert_eq!(serde_json::to_string(&ToolStatus::Failed).unwrap(), "\"failed\"");
        assert_eq!(
            serde_json::to_string(&ToolStatus::Proposed).unwrap(),
            "\"proposed\""
        );
    }

    #[test]
    fn proposed_doc_with_verdict_round_trips() {
        let mut d = ToolDoc::building(
            "weather",
            "desc",
            json!({"type":"object","properties":{}}),
            "// guardian-tool: weather\nfn run(i:&str)->String{String::new()}",
            vec![],
            "1.94.1",
            "2026-06-06T00:00:00Z",
        );
        d.status = ToolStatus::Proposed;
        d.replicant = Some(ReplicantRecord {
            verdict: "allow".into(),
            touched_lines: vec!["values[0]".into()],
            rationale: "pure compute, no soul conflict".into(),
            model: "claude-opus-4-8".into(),
            judged_at: "2026-06-06T00:00:01Z".into(),
        });
        let v = d.to_value();
        assert_eq!(v["status"], "proposed");
        assert_eq!(v["replicant"]["verdict"], "allow");
        let back = ToolDoc::from_value(&v).unwrap();
        assert_eq!(back.status, ToolStatus::Proposed);
        let r = back.replicant.expect("verdict preserved");
        assert_eq!(r.verdict, "allow");
        assert_eq!(r.touched_lines, vec!["values[0]".to_string()]);
    }

    #[test]
    fn pre_format_doc_defaults() {
        // A doc missing format_version/caps/build_log_tail still loads.
        let v = json!({
            "name":"t","description":"d","input_schema":{"type":"object"},
            "source":"s","status":"ready","toolchain_version":"1.94.1",
            "source_sha256": sha256_hex("s"),
            "created_at":"x","updated_at":"y"
        });
        let d = ToolDoc::from_value(&v).unwrap();
        assert_eq!(d.format_version, TOOL_DOC_FORMAT);
        assert!(d.caps.is_empty());
        // Pre-replicant docs load with no verdict.
        assert!(d.replicant.is_none());
    }
}
