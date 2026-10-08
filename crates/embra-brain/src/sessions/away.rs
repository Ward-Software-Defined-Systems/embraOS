//! What happened in embraOS while the operator was away from a session: the
//! digest a resume briefing carries. The brain builds it from what is stored
//! — memory entries created, sessions created and deleted, cron jobs that
//! ran, reminders that fired — so the briefing needs no tool call. It rides
//! the briefing turn's user message and is not persisted: the history keeps
//! `[Session resumed]`.

use chrono::{DateTime, Utc};

use crate::db::WardsonDbClient;
use crate::tools::sessions::truncate_str;

use super::{SessionMeta, SessionState};

/// Most new memory entries one digest reads. A full window makes the count a
/// lower bound.
const MEMORY_LIMIT: usize = 200;

/// Items listed per section; the rest are counted.
const SHOWN_PER_SECTION: usize = 5;

/// Byte cap on one listed memory entry or reminder.
const ITEM_PREVIEW_MAX: usize = 160;

/// The digest, before it is rendered.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct AwayDigest {
    /// New memory entries, newest first, as previews.
    pub memories: Vec<String>,
    /// Whether the memory query came back full, so there may be more.
    pub memories_at_limit: bool,
    pub sessions_created: Vec<String>,
    pub sessions_deleted: Vec<String>,
    /// Cron jobs whose last run falls in the absence, latest first, each
    /// with its latest recorded result when one was recorded.
    pub crons_ran: Vec<CronRan>,
    /// Reminders that fired: message and when they were due.
    pub reminders_fired: Vec<(String, DateTime<Utc>)>,
}

impl AwayDigest {
    fn is_empty(&self) -> bool {
        self.memories.is_empty()
            && self.sessions_created.is_empty()
            && self.sessions_deleted.is_empty()
            && self.crons_ran.is_empty()
            && self.reminders_fired.is_empty()
    }
}

/// A cron job that ran in the absence.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CronRan {
    pub command: String,
    pub schedule: String,
    pub last_run: DateTime<Utc>,
    /// A preview of the latest recorded run's result (`cron_runs`).
    pub result: Option<String>,
}

pub(super) fn time_field(doc: &serde_json::Value, field: &str) -> Option<DateTime<Utc>> {
    let text = doc.get(field)?.as_str()?;
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

pub(super) fn str_field<'a>(doc: &'a serde_json::Value, field: &str) -> &'a str {
    doc.get(field).and_then(|v| v.as_str()).unwrap_or("")
}

pub(super) fn preview(text: &str) -> String {
    let text = text.trim();
    if text.len() > ITEM_PREVIEW_MAX {
        format!("{}…", truncate_str(text, ITEM_PREVIEW_MAX))
    } else {
        text.to_string()
    }
}

/// The query for memory entries created after `since`: the server filters,
/// so the window fills only when that many entries are new. `since` is in the
/// form the entries store (`to_rfc3339`), and the server compares strings.
fn memories_since_body(since: &str) -> serde_json::Value {
    serde_json::json!({
        "filter": {"created_at": {"$gt": since}},
        "sort": [{"_created_at": "desc"}, {"_id": "desc"}],
        "limit": MEMORY_LIMIT,
        "fields": ["content", "created_at"],
    })
}

/// Memory entries created after `since`, newest first. The server already
/// filtered; this checks again on parsed times, so an entry stored in
/// another format is judged by its time and not its spelling.
fn memories_since(newest_first: &[serde_json::Value], since: DateTime<Utc>) -> Vec<String> {
    newest_first
        .iter()
        .filter(|doc| time_field(doc, "created_at").is_some_and(|t| t > since))
        .map(|doc| preview(str_field(doc, "content")))
        .collect()
}

/// Sessions created after `since` that still exist, and sessions deleted
/// after `since`, by name. The session being resumed is left out.
fn session_changes(
    metas: &[SessionMeta],
    since: DateTime<Utc>,
    current: &str,
) -> (Vec<String>, Vec<String>) {
    let mut created = Vec::new();
    let mut deleted = Vec::new();
    for meta in metas.iter().filter(|m| m.name != current) {
        if meta.state == SessionState::Deleted {
            let when = meta
                .deleted_at
                .as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok());
            if when.is_some_and(|t| t.with_timezone(&Utc) > since) {
                deleted.push(meta.name.clone());
            }
        } else if meta.created_at > since {
            created.push(meta.name.clone());
        }
    }
    created.sort();
    deleted.sort();
    (created, deleted)
}

/// The latest recorded result per job, from runs sorted newest first.
fn latest_results(runs: &[serde_json::Value]) -> std::collections::HashMap<String, String> {
    let mut latest = std::collections::HashMap::new();
    for run in runs {
        let job = str_field(run, "job_id");
        if !job.is_empty() && !latest.contains_key(job) {
            latest.insert(job.to_string(), preview(str_field(run, "result")));
        }
    }
    latest
}

/// Cron jobs whose last run falls after `since`, latest first, with the
/// latest recorded result of each when `results` has one.
fn crons_ran_since(
    docs: &[serde_json::Value],
    since: DateTime<Utc>,
    results: &std::collections::HashMap<String, String>,
) -> Vec<CronRan> {
    let mut ran: Vec<_> = docs
        .iter()
        .filter_map(|doc| {
            let last = time_field(doc, "last_run")?;
            let command = match str_field(doc, "command") {
                "" => str_field(doc, "command_name"),
                command => command,
            };
            let id = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()).unwrap_or("");
            (last > since).then(|| CronRan {
                command: command.to_string(),
                schedule: str_field(doc, "schedule").to_string(),
                last_run: last,
                result: results.get(id).cloned(),
            })
        })
        .collect();
    ran.sort_by_key(|r| std::cmp::Reverse(r.last_run));
    ran
}

/// Reminders that fired and were due after `since` and by `now`, earliest
/// first. A reminder records that it fired, not when; it fires at its first
/// check after it is due.
fn reminders_fired_since(
    docs: &[serde_json::Value],
    since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Vec<(String, DateTime<Utc>)> {
    let mut fired: Vec<_> = docs
        .iter()
        .filter(|doc| doc.get("fired").and_then(|v| v.as_bool()).unwrap_or(false))
        .filter_map(|doc| {
            let due = time_field(doc, "trigger_at")?;
            (due > since && due <= now).then(|| (preview(str_field(doc, "message")), due))
        })
        .collect();
    fired.sort_by_key(|f| f.1);
    fired
}

/// Read what happened after `since`. A read that fails leaves its section
/// empty: the briefing goes ahead without it.
pub(crate) async fn gather(
    db: &WardsonDbClient,
    metas: &[SessionMeta],
    since: DateTime<Utc>,
    now: DateTime<Utc>,
    current: &str,
) -> AwayDigest {
    let entries = db
        .query("memory.entries", &memories_since_body(&since.to_rfc3339()))
        .await
        .unwrap_or_default();
    let crons = db.fetch_collection("crons").await.unwrap_or_default();
    let runs = db
        .query(
            crate::tools::cron::CRON_RUNS,
            &super::events::cron_runs_since_body(&since.to_rfc3339()),
        )
        .await
        .unwrap_or_default();
    let reminders = db.fetch_collection("reminders").await.unwrap_or_default();
    let (sessions_created, sessions_deleted) = session_changes(metas, since, current);
    let memories_at_limit = crate::db::client::window_saturated(entries.len(), MEMORY_LIMIT);
    if memories_at_limit {
        // Not silent: the digest says "at least", and the log says why.
        tracing::info!(
            target: "sessions",
            limit = MEMORY_LIMIT,
            "resume digest: new-memory window full — the count is a lower bound"
        );
    }
    AwayDigest {
        memories: memories_since(&entries, since),
        memories_at_limit,
        sessions_created,
        sessions_deleted,
        crons_ran: crons_ran_since(&crons, since, &latest_results(&runs)),
        reminders_fired: reminders_fired_since(&reminders, since, now),
    }
}

/// The digest as the briefing turn carries it, times in the operator's
/// timezone; `None` when nothing happened.
pub(crate) fn render(digest: &AwayDigest, since: DateTime<Utc>, tz: &str) -> Option<String> {
    if digest.is_empty() {
        return None;
    }
    let zone: chrono_tz::Tz = tz.parse().unwrap_or(chrono_tz::UTC);
    let at = |t: DateTime<Utc>| t.with_timezone(&zone).format("%Y-%m-%d %H:%M %Z").to_string();

    let mut out = format!("<while_away since=\"{}\">\n", at(since));
    let mut section = |title: String, items: Vec<String>, total: usize| {
        out.push_str(&title);
        out.push('\n');
        for item in items.iter().take(SHOWN_PER_SECTION) {
            out.push_str(&format!("- {item}\n"));
        }
        if total > SHOWN_PER_SECTION {
            out.push_str(&format!("- and {} more\n", total - SHOWN_PER_SECTION));
        }
    };
    if !digest.memories.is_empty() {
        let count = if digest.memories_at_limit {
            format!("at least {}", digest.memories.len())
        } else {
            digest.memories.len().to_string()
        };
        section(
            format!("New memory entries ({count}, newest first):"),
            digest.memories.clone(),
            digest.memories.len(),
        );
    }
    if !digest.sessions_created.is_empty() {
        section(
            "Sessions created:".to_string(),
            digest.sessions_created.clone(),
            digest.sessions_created.len(),
        );
    }
    if !digest.sessions_deleted.is_empty() {
        section(
            "Sessions deleted:".to_string(),
            digest.sessions_deleted.clone(),
            digest.sessions_deleted.len(),
        );
    }
    if !digest.crons_ran.is_empty() {
        let items = digest
            .crons_ran
            .iter()
            .map(|r| {
                let result = r.result.as_deref().map(|p| format!(": {p}")).unwrap_or_default();
                format!("{} ({}), last run {}{}", r.command, r.schedule, at(r.last_run), result)
            })
            .collect();
        section("Cron jobs that ran:".to_string(), items, digest.crons_ran.len());
    }
    if !digest.reminders_fired.is_empty() {
        let items = digest
            .reminders_fired
            .iter()
            .map(|(message, due)| format!("{message} (due {})", at(*due)))
            .collect();
        section("Reminders that fired:".to_string(), items, digest.reminders_fired.len());
    }
    out.push_str("</while_away>");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn meta(name: &str, created: &str, state: &str, deleted_at: Option<&str>) -> SessionMeta {
        serde_json::from_value(json!({
            "id": name, "name": name, "state": state,
            "created_at": created, "last_active": created,
            "deleted_at": deleted_at,
        }))
        .unwrap()
    }

    const SINCE: &str = "2026-09-29T12:00:00Z";
    const NOW: &str = "2026-09-29T15:00:00Z";

    #[test]
    fn the_memory_query_asks_for_entries_after_the_cutoff_under_a_limit() {
        let body = memories_since_body("2026-09-29T12:00:00+00:00");
        assert_eq!(body["filter"], json!({"created_at": {"$gt": "2026-09-29T12:00:00+00:00"}}));
        assert_eq!(body["limit"], json!(MEMORY_LIMIT));
        assert!(body["sort"].is_array());
    }

    #[test]
    fn memories_are_judged_by_their_time_not_their_spelling() {
        let docs = [
            json!({"content": "after, stored with +00:00", "created_at": "2026-09-29T13:00:00+00:00"}),
            json!({"content": "after, stored with Z", "created_at": "2026-09-29T14:00:00Z"}),
            json!({"content": "before", "created_at": "2026-09-29T11:59:59+00:00"}),
            json!({"content": "no time"}),
        ];
        assert_eq!(
            memories_since(&docs, t(SINCE)),
            ["after, stored with +00:00", "after, stored with Z"]
        );
    }

    #[test]
    fn a_long_entry_is_cut_on_a_character_boundary() {
        let long = "é".repeat(200);
        let docs = [json!({"content": long, "created_at": NOW})];
        let shown = &memories_since(&docs, t(SINCE))[0];
        assert!(shown.ends_with('…'));
        assert!(shown.len() <= ITEM_PREVIEW_MAX + '…'.len_utf8());
    }

    #[test]
    fn sessions_created_and_deleted_in_the_absence_are_named_and_the_current_one_is_not() {
        let metas = [
            meta("new-one", "2026-09-29T13:00:00Z", "Detached", None),
            meta("old-one", "2026-09-01T00:00:00Z", "Detached", None),
            meta("gone-now", "2026-09-01T00:00:00Z", "Deleted", Some("2026-09-29T14:00:00+00:00")),
            meta("gone-before", "2026-09-01T00:00:00Z", "Deleted", Some("2026-09-28T00:00:00+00:00")),
            meta("current", "2026-09-29T13:30:00Z", "Active", None),
        ];
        let (created, deleted) = session_changes(&metas, t(SINCE), "current");
        assert_eq!(created, ["new-one"]);
        assert_eq!(deleted, ["gone-now"]);
    }

    #[test]
    fn crons_are_listed_by_their_last_run_latest_first() {
        let docs = [
            json!({"command": "system_status", "schedule": "every 1h", "last_run": "2026-09-29T13:00:00+00:00"}),
            json!({"command": "time", "schedule": "every 5m", "last_run": "2026-09-29T14:55:00+00:00"}),
            json!({"command": "old", "schedule": "daily 09:00", "last_run": "2026-09-29T09:00:00+00:00"}),
            json!({"command": "never", "schedule": "every 1h", "last_run": null}),
        ];
        let ran = crons_ran_since(&docs, t(SINCE), &std::collections::HashMap::new());
        let commands: Vec<_> = ran.iter().map(|r| r.command.as_str()).collect();
        assert_eq!(commands, ["time", "system_status"]);
        assert!(ran.iter().all(|r| r.result.is_none()));
    }

    #[test]
    fn a_cron_result_rides_the_away_digest_when_one_was_recorded() {
        let docs = [
            json!({"_id": "job-a", "command": "time", "schedule": "every 5m", "last_run": "2026-09-29T14:55:00+00:00"}),
            json!({"_id": "job-b", "command": "system_status", "schedule": "every 1h", "last_run": "2026-09-29T13:00:00+00:00"}),
        ];
        let runs = [
            json!({"job_id": "job-a", "started_at": "2026-09-29T14:55:00+00:00", "result": "14:55 UTC"}),
            json!({"job_id": "job-a", "started_at": "2026-09-29T14:50:00+00:00", "result": "14:50 UTC"}),
        ];
        let ran = crons_ran_since(&docs, t(SINCE), &latest_results(&runs));
        assert_eq!(ran[0].result.as_deref(), Some("14:55 UTC"));
        assert_eq!(ran[1].result, None);
        let digest = AwayDigest { crons_ran: ran, ..Default::default() };
        let block = render(&digest, t(SINCE), "UTC").unwrap();
        assert!(block.contains("- time (every 5m), last run 2026-09-29 14:55 UTC: 14:55 UTC\n"), "{block}");
        assert!(block.contains("- system_status (every 1h), last run 2026-09-29 13:00 UTC\n"), "{block}");
    }

    #[test]
    fn only_reminders_that_fired_and_fell_due_in_the_absence_are_listed() {
        let docs = [
            json!({"message": "fired", "trigger_at": "2026-09-29T13:00:00+00:00", "fired": true}),
            json!({"message": "due, not fired yet", "trigger_at": "2026-09-29T14:00:00+00:00", "fired": false}),
            json!({"message": "fired before", "trigger_at": "2026-09-29T10:00:00+00:00", "fired": true}),
            json!({"message": "still ahead", "trigger_at": "2026-09-29T18:00:00+00:00", "fired": false}),
        ];
        let fired = reminders_fired_since(&docs, t(SINCE), t(NOW));
        assert_eq!(fired, [("fired".to_string(), t("2026-09-29T13:00:00Z"))]);
    }

    #[test]
    fn nothing_happened_means_no_block() {
        assert_eq!(render(&AwayDigest::default(), t(SINCE), "UTC"), None);
    }

    #[test]
    fn the_block_lists_each_section_in_the_operators_timezone_and_counts_the_rest() {
        let digest = AwayDigest {
            memories: (1..=7).map(|i| format!("entry {i}")).collect(),
            memories_at_limit: false,
            sessions_created: vec!["new-one".into()],
            sessions_deleted: vec!["gone-now".into()],
            crons_ran: vec![CronRan { command: "time".into(), schedule: "every 5m".into(), last_run: t("2026-09-29T14:55:00Z"), result: None }],
            reminders_fired: vec![("call back".into(), t("2026-09-29T13:00:00Z"))],
        };
        let block = render(&digest, t(SINCE), "America/Los_Angeles").unwrap();
        assert!(block.starts_with("<while_away since=\"2026-09-29 05:00 PDT\">"), "{block}");
        assert!(block.contains("New memory entries (7, newest first):\n- entry 1\n"), "{block}");
        assert!(block.contains("- entry 5\n- and 2 more\n"), "{block}");
        assert!(!block.contains("entry 6"), "{block}");
        assert!(block.contains("Sessions created:\n- new-one\n"), "{block}");
        assert!(block.contains("Sessions deleted:\n- gone-now\n"), "{block}");
        assert!(block.contains("- time (every 5m), last run 2026-09-29 07:55 PDT\n"), "{block}");
        assert!(block.contains("- call back (due 2026-09-29 06:00 PDT)\n"), "{block}");
        assert!(block.ends_with("</while_away>"), "{block}");
    }

    #[test]
    fn a_full_memory_window_is_reported_as_a_lower_bound() {
        let digest = AwayDigest {
            memories: vec!["x".into(); 3],
            memories_at_limit: true,
            ..Default::default()
        };
        let block = render(&digest, t(SINCE), "UTC").unwrap();
        assert!(block.contains("New memory entries (at least 3, newest first):"), "{block}");
    }
}
