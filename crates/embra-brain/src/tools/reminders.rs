//! The reminder tools: `countdown` and `reminder_list`, and `check_reminders`,
//! which the proactive loop runs.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;

use super::{ensure_collection, parse_duration};
use crate::db::WardsonDbClient;
use crate::tools::registry::DispatchContext;

async fn countdown(db: &WardsonDbClient, param: &str, act: bool) -> String {
    if param.is_empty() {
        return "Usage: countdown <duration> <message>\nExample: countdown 5m Check the build".into();
    }

    // Parse: "5m Check the build" or "20 minutes reminder text"
    let parts: Vec<&str> = param.splitn(2, ' ').collect();
    let (duration_str, message) = if parts.len() == 2 {
        (parts[0], parts[1])
    } else {
        (parts[0], "Reminder")
    };

    let seconds = parse_duration(duration_str);
    if seconds == 0 {
        return format!("Could not parse duration '{}'. Use formats like: 5m, 30s, 1h, '20 minutes'", duration_str);
    }

    let now = Utc::now();
    let trigger_at = now + chrono::Duration::seconds(seconds as i64);

    ensure_collection(db, "reminders").await;

    let doc = reminder_doc(message, trigger_at, now, act);

    match db.write("reminders", &doc).await {
        Ok(id) => format!(
            "Reminder set. Will fire at {} (in {}s).{}\nID: {}",
            trigger_at.format("%H:%M:%S UTC"),
            seconds,
            if act { " You will be given a turn to act on it when it fires." } else { "" },
            id
        ),
        Err(e) => format!("Failed to set reminder: {}", e),
    }
}

/// A reminder as it is stored. `act` asks for a model turn when it fires
/// (`proactive::ActTrigger`); a record without the field does not.
fn reminder_doc(
    message: &str,
    trigger_at: chrono::DateTime<Utc>,
    now: chrono::DateTime<Utc>,
    act: bool,
) -> serde_json::Value {
    serde_json::json!({
        "message": message,
        "trigger_at": trigger_at.to_rfc3339(),
        "created_at": now.to_rfc3339(),
        "fired": false,
        "act": act,
    })
}

/// Which reminders `reminder_list` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReminderFilter {
    #[default]
    Pending,
    Fired,
    All,
}

impl ReminderFilter {
    fn label(self) -> &'static str {
        match self {
            ReminderFilter::Pending => "pending",
            ReminderFilter::Fired => "fired",
            ReminderFilter::All => "all",
        }
    }
}

/// `reminder_list`'s default and most.
pub(crate) const REMINDER_LIST_DEFAULT: usize = 20;

pub(crate) const REMINDER_LIST_MAX: usize = 100;

/// The limit a call asked for, within bounds.
pub(crate) fn reminder_list_limit(limit: Option<u32>) -> usize {
    (limit.map(|n| n as usize).unwrap_or(REMINDER_LIST_DEFAULT)).clamp(1, REMINDER_LIST_MAX)
}

/// The query behind a filter: pending reminders soonest first (a record
/// without `fired` is pending, hence the `$or`, as in
/// `due_reminders_query_body`), fired ones newest first. `All` reads the
/// whole collection instead (`fetch_collection`); there is no query.
pub(crate) fn reminder_list_query_body(filter: ReminderFilter, limit: usize) -> Option<serde_json::Value> {
    match filter {
        ReminderFilter::Pending => Some(serde_json::json!({
            "filter": {"$or": [{"fired": false}, {"fired": {"$exists": false}}]},
            "sort": [{"trigger_at": "asc"}, {"_id": "asc"}],
            "limit": limit,
        })),
        ReminderFilter::Fired => Some(serde_json::json!({
            "filter": {"fired": true},
            "sort": [{"trigger_at": "desc"}, {"_id": "desc"}],
            "limit": limit,
        })),
        ReminderFilter::All => None,
    }
}

/// A stored time in the operator's zone, or the text as stored.
fn reminder_local_time(rfc3339: &str, tz: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(rfc3339) else {
        return rfc3339.to_string();
    };
    let zone: chrono_tz::Tz = tz.parse().unwrap_or(chrono_tz::UTC);
    t.with_timezone(&zone).format("%Y-%m-%d %H:%M %Z").to_string()
}

/// `reminder_list`'s text: the short id, the message, when it is or was
/// due, and for a fired one when it fired.
pub(crate) fn render_reminder_list(
    docs: &[serde_json::Value],
    filter: ReminderFilter,
    limit: usize,
    tz: &str,
) -> String {
    if docs.is_empty() {
        return match filter {
            ReminderFilter::Pending => "No pending reminders. Set one with countdown.".to_string(),
            ReminderFilter::Fired => "No fired reminders in the last seven days.".to_string(),
            ReminderFilter::All => "No reminders.".to_string(),
        };
    }
    let more = if docs.len() >= limit {
        format!(" (the first {limit}; raise limit for more)")
    } else {
        String::new()
    };
    let mut out = format!("=== Reminders ({}: {}{}) ===\n", filter.label(), docs.len(), more);
    for doc in docs {
        let id = doc
            .get("_id")
            .or(doc.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let short: String = id.chars().take(8).collect();
        let message = doc.get("message").and_then(|v| v.as_str()).unwrap_or("Reminder");
        let due = doc.get("trigger_at").and_then(|v| v.as_str()).unwrap_or("?");
        let fired = doc.get("fired").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut line = format!("  {}  {}  due {}", short, message, reminder_local_time(due, tz));
        if fired {
            match doc.get("fired_at").and_then(|v| v.as_str()) {
                Some(at) => line.push_str(&format!("  fired {}", reminder_local_time(at, tz))),
                None => line.push_str("  fired"),
            }
        }
        line.push('\n');
        out.push_str(&line);
    }
    out
}

async fn reminder_list(db: &WardsonDbClient, filter: ReminderFilter, limit: usize, tz: &str) -> String {
    ensure_collection(db, "reminders").await;
    let docs = match reminder_list_query_body(filter, limit) {
        Some(body) => match db.query("reminders", &body).await {
            Ok(d) => d,
            Err(e) => return format!("Failed to load reminders from the database: {}", e),
        },
        None => {
            let mut all = match db.fetch_collection("reminders").await {
                Ok(d) => d,
                Err(e) => return format!("Failed to load reminders from the database: {}", e),
            };
            all.sort_by(|a, b| {
                let ta = a.get("trigger_at").and_then(|v| v.as_str()).unwrap_or("");
                let tb = b.get("trigger_at").and_then(|v| v.as_str()).unwrap_or("");
                tb.cmp(ta)
            });
            all.truncate(limit);
            all
        }
    };
    render_reminder_list(&docs, filter, limit, tz)
}

/// What a reminder's lifetime is counted from, and for how long: it is
/// removed `REMINDER_RETENTION_DAYS` after it was DUE. Counted from
/// `created_at`, as migration v4 set it up, a reminder for more than seven
/// days ahead was removed before it could fire. The policy is asserted on
/// every boot (`migrations::ensure_ttl_policies`).
pub(crate) const REMINDER_TTL_FIELD: &str = "trigger_at";

pub(crate) const REMINDER_RETENTION_DAYS: u64 = 7;

/// Most due reminders one check looks at, earliest trigger first.
const REMINDER_WINDOW: usize = 500;

/// The reminders that are due at `now` and have not fired, earliest trigger
/// first, under an explicit window. A record without `fired` counts as not
/// fired (BUG-003), and WardSONDB matches a missing field with `$exists`
/// only, hence the `$or`. An `$or` is a collection scan; this collection is
/// small and reaped after seven days (migration v4), and nothing here is a
/// hot path. `trigger_at` compares as a string on the server, exactly as
/// `due_reminders` compares it here.
pub(crate) fn due_reminders_query_body(now: &str, limit: usize) -> serde_json::Value {
    serde_json::json!({
        "filter": {
            "trigger_at": {"$lte": now},
            "$or": [{"fired": false}, {"fired": {"$exists": false}}],
        },
        "sort": [{"trigger_at": "asc"}, {"_id": "asc"}],
        "limit": limit,
    })
}

/// Of `docs`, the reminders due at `now` that have not fired — earliest
/// trigger first, at most `max`. Timestamps compare as the RFC 3339 strings
/// they are stored as.
fn due_reminders<'a>(
    docs: &'a [serde_json::Value],
    now: &str,
    max: usize,
) -> Vec<&'a serde_json::Value> {
    let mut due: Vec<(&str, &serde_json::Value)> = docs
        .iter()
        .filter(|doc| {
            // Missing `fired` field means not yet fired (BUG-003 fix)
            !doc.get("fired").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .filter_map(|doc| {
            let trigger = doc.get("trigger_at").and_then(|v| v.as_str())?;
            (!trigger.is_empty() && trigger <= now).then_some((trigger, doc))
        })
        .collect();
    due.sort_by_key(|(trigger, _)| *trigger);
    due.into_iter().take(max).map(|(_, doc)| doc).collect()
}

/// A reminder that fired: the line the console shows, the message, and
/// whether the reminder asked for a turn (`act`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiredReminder {
    pub text: String,
    pub message: String,
    pub act: bool,
}

/// Fire the reminders that are due, at most `max` of them, and return them.
/// Called by the proactive engine, which passes the number of
/// notifications it can hand over right now: firing marks a reminder as
/// fired, so one that fires without a place to go is lost. What does not
/// fit stays in the store and fires on a later check.
pub async fn check_reminders(db: &WardsonDbClient, max: usize) -> Vec<FiredReminder> {
    if max == 0 {
        return Vec::new();
    }
    let now = Utc::now().to_rfc3339();
    // A tick whose read failed delivers nothing and says so; the loop goes on.
    let reminders = match db.query("reminders", &due_reminders_query_body(&now, REMINDER_WINDOW)).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "proactive", "due reminders not read this tick: {e}");
            return Vec::new();
        }
    };
    if crate::db::client::window_saturated(reminders.len(), REMINDER_WINDOW) {
        tracing::warn!(
            target: "wardsondb::window",
            collection = "reminders",
            limit = REMINDER_WINDOW,
            "due-reminder window saturated — the latest triggers wait for earlier ones to fire"
        );
    }

    let mut fired = Vec::new();

    for doc in due_reminders(&reminders, &now, max) {
        let message = doc
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Reminder");

        // Marked as fired, and when, BEFORE it is delivered: a mark that
        // cannot be written leaves the reminder in the store for the next
        // tick, where a delivered-then-unmarked one would fire on every
        // tick (#60). Nothing is dropped: it waits.
        let Some(id) = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()) else {
            tracing::warn!(target: "proactive", "a due reminder has no id and is not delivered");
            continue;
        };
        if let Err(e) = db.update("reminders", id, &mark_fired(doc, &now)).await {
            tracing::warn!(target: "proactive", reminder = id, "not marked as fired: {e}; it waits for the next tick");
            continue;
        }

        fired.push(FiredReminder {
            text: format!("Reminder: {}", message),
            message: message.to_string(),
            act: doc.get("act").and_then(|v| v.as_bool()).unwrap_or(false),
        });
    }

    fired
}

/// The reminder as it is stored once it fired: `fired` and the time it
/// fired (`fired_at`), which the events block of the next turn and
/// `reminder_list` read. A record from before `fired_at` existed still
/// reads as fired, without a time.
fn mark_fired(doc: &serde_json::Value, now: &str) -> serde_json::Value {
    let mut updated = doc.clone();
    updated["fired"] = serde_json::json!(true);
    updated["fired_at"] = serde_json::json!(now);
    updated
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "countdown",
    description = "Set a reminder to fire after a duration. duration examples: \"5m\", \"30s\", \"1h\", \"20 minutes\". message defaults to \"Reminder\" if omitted. When it fires the operator is notified and your next turn carries it; with act=true you are also given a turn of your own when it fires, in the active session, to act on the message with your tools."
)]
pub struct CountdownArgs {
    /// Duration: "5m", "30s", "1h", "20 minutes".
    pub duration: String,
    /// Reminder message shown when the countdown fires.
    #[serde(default = "default_countdown_message")]
    pub message: String,
    /// Start a model turn in the active session when this fires, so you
    /// can act on it. Default false: the reminder is shown and listed only.
    #[serde(default)]
    pub act: Option<bool>,
}

fn default_countdown_message() -> String {
    "Reminder".into()
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "reminder_list",
    description = "List the reminders set with countdown: pending ones soonest first (the default), fired ones newest first with when they fired, or all. Each line gives the reminder's short id, its message and when it is or was due. A fired reminder is kept seven days past its due time."
)]
pub struct ReminderListArgs {
    /// Which reminders: pending (default), fired, or all.
    #[serde(default)]
    pub filter: ReminderFilter,
    /// How many to show (default 20, at most 100).
    #[serde(default)]
    pub limit: Option<u32>,
}

impl ReminderListArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(reminder_list(ctx.db, self.filter, reminder_list_limit(self.limit), ctx.config_tz).await)
    }
}

impl CountdownArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let joined = if self.message.is_empty() {
            self.duration
        } else {
            format!("{} {}", self.duration, self.message)
        };
        Ok(countdown(ctx.db, &joined, self.act.unwrap_or(false)).await)
    }
}

#[cfg(test)]
mod native_args_tests {
    use super::*;

    #[test]
    fn countdown_requires_duration_message_defaults() {
        let a: CountdownArgs =
            serde_json::from_value(serde_json::json!({"duration": "5m"})).unwrap();
        assert_eq!(a.duration, "5m");
        assert_eq!(a.message, "Reminder");

        let b: CountdownArgs = serde_json::from_value(serde_json::json!({
            "duration": "30s", "message": "check build"
        }))
        .unwrap();
        assert_eq!(b.message, "check build");

        // Missing required field
        let err = serde_json::from_value::<CountdownArgs>(serde_json::json!({})).unwrap_err();
        assert!(err.to_string().contains("duration"));
    }
}

#[cfg(test)]
mod reminder_tests {
    #[test]
    fn act_defaults_to_false_and_round_trips_on_a_reminder() {
        let now = chrono::Utc::now();
        let plain = super::reminder_doc("m", now, now, false);
        assert_eq!(plain["act"], false);
        assert_eq!(plain["fired"], false);
        let acting = super::reminder_doc("m", now, now, true);
        assert_eq!(acting["act"], true);
        // The tool's flag defaults to absent, read as false.
        let args: super::CountdownArgs = serde_json::from_value(serde_json::json!({"duration": "5m"})).unwrap();
        assert_eq!(args.act, None);
        let args: super::CountdownArgs =
            serde_json::from_value(serde_json::json!({"duration": "5m", "message": "x", "act": true})).unwrap();
        assert_eq!(args.act, Some(true));
    }

    #[test]
    fn the_pending_filter_matches_records_without_the_fired_field() {
        let body = super::reminder_list_query_body(super::ReminderFilter::Pending, 7).unwrap();
        assert_eq!(
            body["filter"],
            serde_json::json!({"$or": [{"fired": false}, {"fired": {"$exists": false}}]})
        );
        assert_eq!(body["sort"], serde_json::json!([{"trigger_at": "asc"}, {"_id": "asc"}]));
        assert_eq!(body["limit"], serde_json::json!(7));
        let fired = super::reminder_list_query_body(super::ReminderFilter::Fired, 7).unwrap();
        assert_eq!(fired["filter"], serde_json::json!({"fired": true}));
        assert_eq!(fired["sort"], serde_json::json!([{"trigger_at": "desc"}, {"_id": "desc"}]));
        assert!(super::reminder_list_query_body(super::ReminderFilter::All, 7).is_none());
    }

    #[test]
    fn reminder_list_caps_its_limit() {
        assert_eq!(super::reminder_list_limit(None), 20);
        assert_eq!(super::reminder_list_limit(Some(0)), 1);
        assert_eq!(super::reminder_list_limit(Some(5)), 5);
        assert_eq!(super::reminder_list_limit(Some(1000)), 100);
    }

    #[test]
    fn reminder_list_renders_id_message_due_and_fired() {
        let docs = vec![
            serde_json::json!({"_id": "0199abcd-aaaa-7000-8000-000000000001", "message": "check the build",
                               "trigger_at": "2026-10-08T12:00:00+00:00", "fired": true,
                               "fired_at": "2026-10-08T12:00:07+00:00"}),
            serde_json::json!({"_id": "0199abce-bbbb-7000-8000-000000000002", "message": "stand up",
                               "trigger_at": "2026-10-08T13:00:00+00:00"}),
        ];
        let out = super::render_reminder_list(&docs, super::ReminderFilter::All, 20, "America/Los_Angeles");
        assert_eq!(
            out,
            "=== Reminders (all: 2) ===\n  0199abcd  check the build  due 2026-10-08 05:00 PDT  fired 2026-10-08 05:00 PDT\n  0199abce  stand up  due 2026-10-08 06:00 PDT\n"
        );
        let full = super::render_reminder_list(&docs, super::ReminderFilter::Pending, 2, "UTC");
        assert!(full.starts_with("=== Reminders (pending: 2 (the first 2; raise limit for more)) ===\n"), "{full}");
        assert_eq!(
            super::render_reminder_list(&[], super::ReminderFilter::Pending, 20, "UTC"),
            "No pending reminders. Set one with countdown."
        );
    }

    #[test]
    fn a_fired_reminder_is_stamped_with_when_it_fired() {
        let doc = serde_json::json!({
            "_id": "r1", "message": "check the build",
            "trigger_at": "2026-10-08T12:00:00+00:00", "created_at": "2026-10-08T11:55:00+00:00",
            "fired": false,
        });
        let fired = mark_fired(&doc, "2026-10-08T12:00:07+00:00");
        assert_eq!(fired["fired"], true);
        assert_eq!(fired["fired_at"], "2026-10-08T12:00:07+00:00");
        assert_eq!(fired["message"], "check the build");
        assert_eq!(fired["trigger_at"], "2026-10-08T12:00:00+00:00");
    }
    use super::*;
    use serde_json::json;

    const NOW: &str = "2026-09-27T12:00:00+00:00";

    fn reminder(id: &str, trigger_at: &str, fired: Option<bool>) -> serde_json::Value {
        let mut doc = json!({"_id": id, "message": id, "trigger_at": trigger_at});
        if let Some(f) = fired {
            doc["fired"] = json!(f);
        }
        doc
    }

    fn ids(due: &[&serde_json::Value]) -> Vec<String> {
        due.iter().map(|d| d["_id"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn a_reminder_outlives_its_own_wait() {
        // What the database compares is the stored string, as a string.
        let now = chrono::DateTime::parse_from_rfc3339(NOW).unwrap().with_timezone(&Utc);
        let hours = |h: i64| now + chrono::Duration::hours(h);
        let doc = reminder_doc("the quarterly review", hours(200), now, false);
        let kept_from = doc[REMINDER_TTL_FIELD].as_str().expect("the lifetime field is written");
        assert_eq!(kept_from, hours(200).to_rfc3339());
        assert_eq!(doc["fired"], json!(false));

        // The worker removes what is older than now - retention. On the day
        // the reminder is due it has been stored for 200 hours: more than
        // the retention, and it must still be there.
        let retention = chrono::Duration::days(REMINDER_RETENTION_DAYS as i64);
        assert!(hours(200) - now > retention, "the case: a wait longer than the retention");
        let cutoff_when_due = (hours(200) - retention).to_rfc3339();
        assert!(kept_from >= cutoff_when_due.as_str(), "kept until it is due");
        assert!(
            doc["created_at"].as_str().unwrap() < cutoff_when_due.as_str(),
            "counted from created_at it would be gone by then"
        );
        // And it goes once the retention has passed AFTER it was due.
        let cutoff_later = (hours(200) + chrono::Duration::hours(1)).to_rfc3339();
        assert!(kept_from < cutoff_later.as_str());
    }

    #[test]
    fn due_query_is_windowed_sorted_and_matches_records_without_the_field() {
        let body = due_reminders_query_body(NOW, 500);
        assert_eq!(body["limit"], json!(500));
        // One key per array element: a multi-key object sorts alphabetically.
        assert_eq!(body["sort"], json!([{"trigger_at": "asc"}, {"_id": "asc"}]));
        assert_eq!(
            body["filter"],
            json!({
                "trigger_at": {"$lte": NOW},
                "$or": [{"fired": false}, {"fired": {"$exists": false}}],
            })
        );
    }

    #[test]
    fn only_due_unfired_reminders_fire() {
        let docs = vec![
            reminder("due", "2026-09-27T11:59:00+00:00", Some(false)),
            reminder("exactly-now", NOW, Some(false)),
            reminder("later", "2026-09-27T12:00:01+00:00", Some(false)),
            reminder("already-fired", "2026-09-27T11:00:00+00:00", Some(true)),
            // Written before the field existed: counts as not fired.
            reminder("legacy", "2026-09-27T10:00:00+00:00", None),
            reminder("no-trigger", "", Some(false)),
            json!({"_id": "no-trigger-field", "message": "x", "fired": false}),
        ];
        assert_eq!(ids(&due_reminders(&docs, NOW, 16)), ["legacy", "due", "exactly-now"]);
    }

    #[test]
    fn the_earliest_fire_first_and_the_rest_wait() {
        // Stored newest first; three are due and there is room for two.
        let docs = vec![
            reminder("c", "2026-09-27T11:30:00+00:00", Some(false)),
            reminder("a", "2026-09-27T09:00:00+00:00", Some(false)),
            reminder("b", "2026-09-27T10:00:00+00:00", Some(false)),
        ];
        assert_eq!(ids(&due_reminders(&docs, NOW, 2)), ["a", "b"]);
        assert_eq!(ids(&due_reminders(&docs, NOW, 3)), ["a", "b", "c"]);
        assert!(due_reminders(&docs, NOW, 0).is_empty());
    }
}

#[cfg(test)]
mod mark_first_tests {
    use super::check_reminders;
    use crate::db::WardsonDbClient;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn fired_with(mark: ResponseTemplate) -> Vec<String> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reminders/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "meta": {},
                "data": [{"_id": "r1", "message": "stretch", "trigger_at": "2020-01-01T00:00:00+00:00", "fired": false}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT")).and(path("/reminders/docs/r1")).respond_with(mark).mount(&server).await;
        check_reminders(&WardsonDbClient::from_url(&server.uri()), 5)
            .await
            .into_iter()
            .map(|f| f.message)
            .collect()
    }

    /// A reminder is delivered only once its mark is written; a mark that
    /// fails leaves it in the store, undelivered, for the next tick (#60).
    #[tokio::test]
    async fn a_reminder_is_delivered_only_once_it_is_marked_as_fired() {
        assert_eq!(fired_with(ResponseTemplate::new(500)).await, Vec::<String>::new());
        assert_eq!(
            fired_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true}))).await,
            vec!["stretch".to_string()]
        );
    }
}
