//! The record a guided delete leaves (`/sessions delete`). The brain writes
//! it once the soft delete has succeeded; the model never does. It is one
//! observation tagged `session-deleted`, attributed to the deleted session,
//! and linked with `related_to` to the nodes the memorize turn preserved.

use chrono::{DateTime, Utc};
use serde_json::json;
use tracing::warn;

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;
use crate::knowledge::tools::write_link;
use crate::knowledge::types::{EdgeType, SemanticCategory, SESSION_DELETED_TAG};
use crate::tools::sessions::truncate_str;
use crate::tools::{remember_record, untag, Recorded};

/// Longest operator reason a record carries, in bytes. The session's meta
/// keeps the whole reason until the reap.
const DELETION_REASON_MAX: usize = 500;

/// Most preserved nodes one record links to.
const DELETION_RECORD_LINK_MAX: usize = 50;

/// Weight of a record's `related_to` link to a preserved node.
const DELETION_RECORD_LINK_WEIGHT: f64 = 0.5;

/// The record's text. `when` is the time of the deletion in the operator's
/// timezone. The reason is the operator's words: a word that reads as a tag
/// stays text (`untag`), line breaks fold to spaces, and a long reason is
/// cut on a character boundary.
pub(crate) fn deletion_record(name: &str, reason: &str, when: &str) -> String {
    let reason = untag(reason);
    let reason = if reason.is_empty() {
        "none given".to_string()
    } else if reason.len() > DELETION_REASON_MAX {
        format!("{}…", truncate_str(&reason, DELETION_REASON_MAX))
    } else {
        reason
    };
    format!("Session '{name}' was deleted on {when}. Operator's reason: {reason} #{SESSION_DELETED_TAG}")
}

/// The nodes the memorize turn wrote: their `source_session` is the deleted
/// session (the turn's `deleting_session`) and they are newer than the
/// turn's start. Newest first, under a limit.
pub(crate) fn preserved_nodes_query_body(name: &str, since: &str) -> serde_json::Value {
    json!({
        "filter": { "created_at": { "$gt": since }, "source_session": name },
        "sort": [{ "created_at": "desc" }],
        "limit": DELETION_RECORD_LINK_MAX,
    })
}

/// What `record_deletion` did.
#[derive(Debug, PartialEq)]
pub(crate) struct DeletionRecord {
    pub recorded: Recorded,
    /// The `related_to` links written from the record to preserved nodes.
    pub links: usize,
}

/// Write the record of a deleted session and link it to what the memorize
/// turn preserved. `since` is the start of that turn, `when` the time the
/// operator reads. A failure is logged and reported, never retried: the
/// deletion has happened either way.
pub(crate) async fn record_deletion(
    db: &WardsonDbClient,
    name: &str,
    reason: &str,
    since: DateTime<Utc>,
    when: &str,
    config: &SystemConfig,
) -> DeletionRecord {
    // What the turn preserved, read before the record exists: the record
    // carries the same session and would be among them. A collection that
    // does not exist yet holds nothing.
    let body = preserved_nodes_query_body(name, &since.to_rfc3339());
    let mut preserved: Vec<(&'static str, String)> = Vec::new();
    for collection in ["memory.semantic", "memory.procedural"] {
        match db.query(collection, &body).await {
            Ok(docs) => preserved.extend(
                docs.iter()
                    .filter_map(|doc| doc.get("_id").and_then(|v| v.as_str()))
                    .map(|id| (collection, id.to_string())),
            ),
            Err(e) if crate::db::error::is_not_found(&e) => {}
            Err(e) => warn!(target: "sessions", session = %name, collection, "deletion record: the preserved nodes were not read: {e}"),
        }
    }

    let recorded = remember_record(
        db,
        &deletion_record(name, reason, when),
        SemanticCategory::Observation,
        name,
        config,
    )
    .await;

    let mut links = 0;
    if let Recorded::Node { collection, id } = &recorded {
        for (target_collection, target_id) in preserved.iter().take(DELETION_RECORD_LINK_MAX) {
            match write_link(
                db,
                (*collection, id.as_str()),
                &EdgeType::RelatedTo,
                (*target_collection, target_id.as_str()),
                DELETION_RECORD_LINK_WEIGHT,
            )
            .await
            {
                Ok(_) => links += 1,
                Err(e) => warn!(target: "sessions", session = %name, "deletion record: no link to {target_collection}:{target_id}: {e}"),
            }
        }
    }
    DeletionRecord { recorded, links }
}

/// The sentence the operator's note gains, and whether it is a warning.
pub(crate) fn record_sentence(record: &DeletionRecord) -> (String, bool) {
    match &record.recorded {
        Recorded::Node { collection, id } => {
            let links = match record.links {
                0 => String::new(),
                1 => ", linked to 1 preserved memory".to_string(),
                n => format!(", linked to {n} preserved memories"),
            };
            (format!("The deletion is recorded as {collection}:{id}{links}."), false)
        }
        Recorded::EntryOnly { entry_id, error } => (
            format!("The deletion record was saved as entry {entry_id} without its node ({error})."),
            true,
        ),
        Recorded::Failed(error) => (format!("The deletion record could not be saved: {error}."), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn the_deletion_record_names_the_session_the_time_and_the_reason() {
        assert_eq!(
            deletion_record("qwen-eval", "superseded by the 3.8 run", "2026-10-10 14:03 PDT"),
            "Session 'qwen-eval' was deleted on 2026-10-10 14:03 PDT. Operator's reason: superseded by the 3.8 run #session-deleted"
        );
    }

    /// The operator's words are text: `remember` would lift `#cleanup` into
    /// the tags and drop it from the record. An issue number stays as it is.
    #[test]
    fn a_reason_with_hashtags_stays_text() {
        let record = deletion_record("scratch", "done with it #cleanup, see #5\nthanks", "t");
        assert!(record.ends_with("Operator's reason: done with it cleanup, see #5 thanks #session-deleted"), "{record}");
    }

    #[test]
    fn a_long_reason_is_cut_on_a_character_boundary() {
        let record = deletion_record("scratch", &"é".repeat(300), "t");
        let kept = format!("{}…", "é".repeat(DELETION_REASON_MAX / 2));
        assert!(record.ends_with(&format!("Operator's reason: {kept} #session-deleted")), "{record}");
    }

    #[test]
    fn an_empty_reason_reads_none_given() {
        let record = deletion_record("scratch", " \n ", "t");
        assert!(record.ends_with("Operator's reason: none given #session-deleted"), "{record}");
    }

    #[test]
    fn the_preserved_nodes_query_is_bounded_sorted_and_scoped_to_the_turn() {
        assert_eq!(
            preserved_nodes_query_body("old-proj", "2026-10-10T21:00:00+00:00"),
            json!({
                "filter": { "created_at": { "$gt": "2026-10-10T21:00:00+00:00" }, "source_session": "old-proj" },
                "sort": [{ "created_at": "desc" }],
                "limit": 50,
            })
        );
    }

    #[test]
    fn the_note_names_the_record_its_links_or_what_went_wrong() {
        let node = |links| DeletionRecord { recorded: Recorded::Node { collection: "memory.semantic", id: "n1".into() }, links };
        assert_eq!(record_sentence(&node(0)), ("The deletion is recorded as memory.semantic:n1.".to_string(), false));
        assert_eq!(
            record_sentence(&node(1)),
            ("The deletion is recorded as memory.semantic:n1, linked to 1 preserved memory.".to_string(), false)
        );
        assert_eq!(
            record_sentence(&node(4)),
            ("The deletion is recorded as memory.semantic:n1, linked to 4 preserved memories.".to_string(), false)
        );
        let entry_only = DeletionRecord {
            recorded: Recorded::EntryOnly { entry_id: "e1".into(), error: "500".into() },
            links: 0,
        };
        assert_eq!(
            record_sentence(&entry_only),
            ("The deletion record was saved as entry e1 without its node (500).".to_string(), true)
        );
        let failed = DeletionRecord { recorded: Recorded::Failed("down".into()), links: 0 };
        assert_eq!(record_sentence(&failed), ("The deletion record could not be saved: down.".to_string(), true));
    }

    fn data(v: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": v, "meta": {}}))
    }

    /// The preserved nodes are read before the record is written, the record
    /// is an observation of the deleted session, and it links to each of
    /// them. A node collection that does not exist yet holds nothing.
    #[tokio::test]
    async fn the_record_is_written_after_the_preserved_nodes_are_read_and_links_to_them() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/memory.semantic/query")).respond_with(data(json!([{"_id": "p1"}]))).mount(&server).await;
        Mock::given(method("POST")).and(path("/memory.entries/docs")).respond_with(data(json!({"_id": "e1"}))).mount(&server).await;
        let entry = json!({
            "_id": "e1", "content": "Session 'old-proj' was deleted", "tags": ["session-deleted"],
            "session": "old-proj", "promoted_to": null,
        });
        Mock::given(method("GET")).and(path("/memory.entries/docs/e1")).respond_with(data(entry)).mount(&server).await;
        Mock::given(method("POST")).and(path("/memory.semantic/docs")).respond_with(data(json!({"_id": "n1"}))).mount(&server).await;
        Mock::given(method("PATCH")).and(path("/memory.entries/docs/e1")).respond_with(data(json!({}))).mount(&server).await;
        Mock::given(method("GET")).and(path("/memory.semantic/docs/n1")).respond_with(data(json!({"_id": "n1"}))).mount(&server).await;
        Mock::given(method("GET")).and(path("/memory.semantic/docs/p1")).respond_with(data(json!({"_id": "p1"}))).mount(&server).await;
        Mock::given(method("POST")).and(path("/memory.edges/query")).respond_with(data(json!([]))).mount(&server).await;
        Mock::given(method("POST")).and(path("/memory.edges/docs")).respond_with(data(json!({"_id": "edge1"}))).mount(&server).await;

        let db = WardsonDbClient::from_url(&server.uri());
        let config: SystemConfig = serde_json::from_value(json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
            "created_at": "", "version": "test", "embedding_enabled": false
        }))
        .expect("minimal config deserializes");
        let since = DateTime::parse_from_rfc3339("2026-10-10T21:00:00+00:00").unwrap().with_timezone(&Utc);

        let record = record_deletion(&db, "old-proj", "superseded", since, "2026-10-10 14:03 PDT", &config).await;
        assert_eq!(record, DeletionRecord { recorded: Recorded::Node { collection: "memory.semantic", id: "n1".into() }, links: 1 });

        let requests = server.received_requests().await.unwrap_or_default();
        let seen: Vec<(String, serde_json::Value)> = requests
            .iter()
            .map(|r| (format!("{} {}", r.method, r.url.path()), serde_json::from_slice(&r.body).unwrap_or(serde_json::Value::Null)))
            .collect();
        let at = |what: &str| seen.iter().position(|(line, _)| line == what).unwrap_or_else(|| panic!("no {what}: {seen:?}"));
        assert!(at("POST /memory.semantic/query") < at("POST /memory.entries/docs"), "{seen:?}");

        let entry_write = &seen[at("POST /memory.entries/docs")].1;
        assert_eq!(entry_write["session"], "old-proj");
        assert_eq!(entry_write["tags"], json!(["session-deleted"]));
        assert_eq!(entry_write["content"], "Session 'old-proj' was deleted on 2026-10-10 14:03 PDT. Operator's reason: superseded");
        assert_eq!(seen[at("POST /memory.semantic/docs")].1["category"], "observation");

        let links: Vec<&serde_json::Value> = seen
            .iter()
            .filter(|(line, body)| line == "POST /memory.edges/docs" && body["edge_type"] == "related_to")
            .map(|(_, body)| body)
            .collect();
        assert_eq!(links.len(), 1, "{seen:?}");
        assert_eq!((&links[0]["source_id"], &links[0]["target_id"], &links[0]["weight"]), (&json!("n1"), &json!("p1"), &json!(0.5)));
    }
}
