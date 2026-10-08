//! What happened in embraOS since the intelligence last saw a session: the
//! reminders that fired and the cron runs recorded after the session's
//! watermark (`SessionMeta.events_seen_at`, or `last_active` before the
//! first stamp). The next operator turn carries them as a block in its
//! model-facing text, ahead of the operator's words; nothing is
//! persisted, and the watermark moves to the turn's start once the turn
//! is saved, so every event is shown once. A resume briefing carries
//! `<while_away>` instead and skips this block.

use chrono::{DateTime, Utc};

use crate::db::WardsonDbClient;

use super::SessionMeta;
use super::away::{preview, str_field, time_field};

/// Most events of each kind one turn reads. A full window makes the
/// count a lower bound, and the rest wait for the next turn's watermark.
pub(crate) const EVENTS_WINDOW: usize = 50;

/// Items listed per section; the rest are counted.
const SHOWN_PER_SECTION: usize = 5;

/// The events, before they are rendered.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct EventsDigest {
    /// Reminders that fired after the watermark: message and when, newest
    /// first.
    pub reminders: Vec<(String, DateTime<Utc>)>,
    /// Cron runs recorded after the watermark: command, when it started,
    /// whether the dispatch failed, a preview of the result; newest first.
    pub cron_runs: Vec<(String, DateTime<Utc>, bool, String)>,
}

impl EventsDigest {
    fn is_empty(&self) -> bool {
        self.reminders.is_empty() && self.cron_runs.is_empty()
    }
}

/// Where a session's events start: its watermark, or its last activity
/// for a session that has never carried the block.
pub(crate) fn cutoff(meta: &SessionMeta) -> DateTime<Utc> {
    meta.events_seen_at.unwrap_or(meta.last_active)
}

/// The query for reminders that fired after `since`, newest first. A
/// reminder from before `fired_at` existed never matches: it fired
/// before any watermark did.
pub(crate) fn reminders_since_body(since: &str) -> serde_json::Value {
    serde_json::json!({
        "filter": {"fired_at": {"$gt": since}},
        "sort": [{"fired_at": "desc"}, {"_id": "desc"}],
        "limit": EVENTS_WINDOW,
    })
}

/// The query for cron runs recorded after `since`, newest first
/// (`idx_cron_runs_started_at` serves it).
pub(crate) fn cron_runs_since_body(since: &str) -> serde_json::Value {
    serde_json::json!({
        "filter": {"started_at": {"$gt": since}},
        "sort": [{"started_at": "desc"}, {"_id": "desc"}],
        "limit": EVENTS_WINDOW,
    })
}

/// The reminders of `docs` that fired after `since`, judged by their
/// parsed time, newest first.
fn reminders_fired(docs: &[serde_json::Value], since: DateTime<Utc>) -> Vec<(String, DateTime<Utc>)> {
    let mut fired: Vec<_> = docs
        .iter()
        .filter_map(|doc| {
            let at = time_field(doc, "fired_at")?;
            (at > since).then(|| (preview(str_field(doc, "message")), at))
        })
        .collect();
    fired.sort_by_key(|f| std::cmp::Reverse(f.1));
    fired
}

/// The runs of `docs` recorded after `since`, newest first.
pub(crate) fn runs_since(
    docs: &[serde_json::Value],
    since: DateTime<Utc>,
) -> Vec<(String, DateTime<Utc>, bool, String)> {
    let mut runs: Vec<_> = docs
        .iter()
        .filter_map(|doc| {
            let at = time_field(doc, "started_at")?;
            let is_error = doc.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
            (at > since).then(|| {
                (
                    str_field(doc, "command").to_string(),
                    at,
                    is_error,
                    preview(str_field(doc, "result")),
                )
            })
        })
        .collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.1));
    runs
}

/// Read what happened after `since`. A read that fails leaves its section
/// empty: the turn goes ahead without it.
pub(crate) async fn gather(db: &WardsonDbClient, since: DateTime<Utc>) -> EventsDigest {
    let stamp = since.to_rfc3339();
    let reminders = db
        .query("reminders", &reminders_since_body(&stamp))
        .await
        .unwrap_or_default();
    let runs = db
        .query(crate::tools::cron::CRON_RUNS, &cron_runs_since_body(&stamp))
        .await
        .unwrap_or_default();
    EventsDigest {
        reminders: reminders_fired(&reminders, since),
        cron_runs: runs_since(&runs, since),
    }
}

/// The block as the turn carries it, times in the operator's timezone;
/// `None` when nothing happened.
pub(crate) fn render(digest: &EventsDigest, since: DateTime<Utc>, tz: &str) -> Option<String> {
    if digest.is_empty() {
        return None;
    }
    let zone: chrono_tz::Tz = tz.parse().unwrap_or(chrono_tz::UTC);
    let at = |t: DateTime<Utc>| t.with_timezone(&zone).format("%Y-%m-%d %H:%M %Z").to_string();

    let mut out = format!(
        "<events_since_last_turn since=\"{}\">\nThese happened in embraOS since your last turn in this session. Mention or act on what bears on the operator's message, in a sentence; leave out the rest.\n",
        at(since)
    );
    let mut section = |title: &str, items: Vec<String>, total: usize| {
        out.push_str(title);
        out.push('\n');
        for item in items.iter().take(SHOWN_PER_SECTION) {
            out.push_str(&format!("- {item}\n"));
        }
        if total > SHOWN_PER_SECTION {
            out.push_str(&format!("- and {} more\n", total - SHOWN_PER_SECTION));
        }
    };
    if !digest.reminders.is_empty() {
        let items = digest
            .reminders
            .iter()
            .map(|(message, when)| format!("{message} (fired {})", at(*when)))
            .collect();
        section("Reminders that fired:", items, digest.reminders.len());
    }
    if !digest.cron_runs.is_empty() {
        let items = digest
            .cron_runs
            .iter()
            .map(|(command, when, is_error, result)| {
                let state = if *is_error { "ERR" } else { "ok" };
                if result.is_empty() {
                    format!("{command} at {}: {state}", at(*when))
                } else {
                    format!("{command} at {}: {state} — {result}", at(*when))
                }
            })
            .collect();
        section("Cron results (latest first):", items, digest.cron_runs.len());
    }
    out.push_str("</events_since_last_turn>");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    const SINCE: &str = "2026-10-08T12:00:00Z";

    #[test]
    fn the_reminder_query_asks_for_fires_after_the_watermark_under_a_limit() {
        let body = reminders_since_body("2026-10-08T12:00:00+00:00");
        assert_eq!(body["filter"], json!({"fired_at": {"$gt": "2026-10-08T12:00:00+00:00"}}));
        assert_eq!(body["sort"], json!([{"fired_at": "desc"}, {"_id": "desc"}]));
        assert_eq!(body["limit"], json!(EVENTS_WINDOW));
        let runs = cron_runs_since_body("2026-10-08T12:00:00+00:00");
        assert_eq!(runs["filter"], json!({"started_at": {"$gt": "2026-10-08T12:00:00+00:00"}}));
        assert_eq!(runs["sort"], json!([{"started_at": "desc"}, {"_id": "desc"}]));
        assert_eq!(runs["limit"], json!(EVENTS_WINDOW));
        assert_eq!(EVENTS_WINDOW, 50);
    }

    #[test]
    fn a_missing_watermark_falls_back_to_last_active() {
        let mut meta: SessionMeta = serde_json::from_value(json!({
            "id": "s", "name": "s", "state": "Active",
            "created_at": "2026-10-08T10:00:00Z", "last_active": "2026-10-08T11:00:00Z",
        }))
        .unwrap();
        assert_eq!(cutoff(&meta), t("2026-10-08T11:00:00Z"));
        meta.events_seen_at = Some(t("2026-10-08T11:30:00Z"));
        assert_eq!(cutoff(&meta), t("2026-10-08T11:30:00Z"));
        // Serde-additive: the field is absent from the document until stamped.
        let v = serde_json::to_value(&meta).unwrap();
        assert_eq!(v["events_seen_at"], json!("2026-10-08T11:30:00Z"));
        meta.events_seen_at = None;
        assert!(serde_json::to_value(&meta).unwrap().get("events_seen_at").is_none());
    }

    #[test]
    fn events_are_judged_by_their_time_newest_first() {
        let reminders = [
            json!({"message": "later", "fired_at": "2026-10-08T13:00:00+00:00"}),
            json!({"message": "earlier", "fired_at": "2026-10-08T12:30:00Z"}),
            json!({"message": "before the watermark", "fired_at": "2026-10-08T11:00:00+00:00"}),
            json!({"message": "never stamped", "fired": true}),
        ];
        let fired = reminders_fired(&reminders, t(SINCE));
        let names: Vec<_> = fired.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(names, ["later", "earlier"]);
        let runs = [
            json!({"command": "time", "started_at": "2026-10-08T12:05:00+00:00", "is_error": false, "result": "12:05"}),
            json!({"command": "old", "started_at": "2026-10-08T11:59:00+00:00", "is_error": false, "result": "x"}),
            json!({"command": "broken", "started_at": "2026-10-08T12:10:00+00:00", "is_error": true, "result": "cron dispatch failed: unknown tool"}),
        ];
        let since = runs_since(&runs, t(SINCE));
        assert_eq!(since.len(), 2);
        assert_eq!((since[0].0.as_str(), since[0].2), ("broken", true));
        assert_eq!((since[1].0.as_str(), since[1].2), ("time", false));
    }

    #[test]
    fn nothing_since_the_watermark_means_no_block() {
        assert_eq!(render(&EventsDigest::default(), t(SINCE), "UTC"), None);
    }

    #[test]
    fn the_block_lists_five_per_section_and_counts_the_rest() {
        let digest = EventsDigest {
            reminders: vec![("check the build".into(), t("2026-10-08T12:30:00Z"))],
            cron_runs: (1..=7)
                .map(|i| (format!("job{i}"), t("2026-10-08T12:10:00Z"), i == 3, format!("result {i}")))
                .collect(),
        };
        let block = render(&digest, t(SINCE), "America/Los_Angeles").unwrap();
        assert!(block.starts_with("<events_since_last_turn since=\"2026-10-08 05:00 PDT\">\n"), "{block}");
        assert!(block.contains("Reminders that fired:\n- check the build (fired 2026-10-08 05:30 PDT)\n"), "{block}");
        assert!(block.contains("Cron results (latest first):\n- job1 at 2026-10-08 05:10 PDT: ok — result 1\n"), "{block}");
        assert!(block.contains("- job3 at 2026-10-08 05:10 PDT: ERR — result 3\n"), "{block}");
        assert!(block.contains("- job5 at 2026-10-08 05:10 PDT: ok — result 5\n- and 2 more\n"), "{block}");
        assert!(!block.contains("job6"), "{block}");
        assert!(block.ends_with("</events_since_last_turn>"), "{block}");
    }
}
