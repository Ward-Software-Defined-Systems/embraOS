use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::db::WardsonDbClient;
use super::parse_duration;

/// Compute the next UTC fire time for `daily HH:MM` expressed in the operator's
/// timezone. Scans forward up to 7 local days to skip any DST spring-forward
/// gap (e.g. 02:30 local on US DST-transition days); returns None only if
/// nothing in the next week is representable, which would be pathological.
fn daily_next(
    now_utc: DateTime<Utc>,
    tz: Tz,
    hour: u32,
    minute: u32,
) -> Option<DateTime<Utc>> {
    if hour >= 24 || minute >= 60 {
        return None;
    }
    let target_time = chrono::NaiveTime::from_hms_opt(hour, minute, 0)?;
    let now_local = now_utc.with_timezone(&tz);
    let mut candidate = now_local.date_naive();
    for _ in 0..7 {
        if let Some(t) = tz
            .from_local_datetime(&candidate.and_time(target_time))
            .earliest()
        {
            let t_utc = t.with_timezone(&Utc);
            if t_utc > now_utc {
                return Some(t_utc);
            }
        }
        candidate += chrono::Duration::days(1);
    }
    None
}

/// Parse a schedule string into seconds between runs.
/// Formats: "every 5m", "every 1h", "every 30s", "hourly", "daily HH:MM"
/// Returns (interval_secs, next_run_rfc3339) or None if unparseable.
///
/// `config_tz` is consulted only for `daily HH:MM` — interval schedules are
/// timezone-agnostic. The value is resolved through `resolve_timezone` so both
/// IANA names ("America/Los_Angeles") and abbreviations ("PST") are accepted.
fn parse_schedule(schedule: &str, config_tz: &str) -> Option<(u64, String)> {
    let s = schedule.trim().to_lowercase();

    if s == "hourly" {
        let next = Utc::now() + chrono::Duration::seconds(3600);
        return Some((3600, next.to_rfc3339()));
    }

    if let Some(time_str) = s.strip_prefix("daily ") {
        let time_str = time_str.trim();
        let parts: Vec<&str> = time_str.split(':').collect();
        if parts.len() == 2 {
            if let (Ok(hour), Ok(minute)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                let resolved = super::resolve_timezone(config_tz);
                let tz: Tz = resolved.parse().unwrap_or(chrono_tz::UTC);
                let next_utc = daily_next(Utc::now(), tz, hour, minute)?;
                return Some((86400, next_utc.to_rfc3339()));
            }
        }
        return None;
    }

    if let Some(duration_str) = s.strip_prefix("every ") {
        let secs = parse_duration(duration_str.trim());
        if secs > 0 {
            let next = Utc::now() + chrono::Duration::seconds(secs as i64);
            return Some((secs, next.to_rfc3339()));
        }
    }

    None
}

/// Calculate the next run time from now given an interval in seconds.
fn next_run_from_now(interval_secs: u64) -> String {
    let next = Utc::now() + chrono::Duration::seconds(interval_secs as i64);
    next.to_rfc3339()
}

async fn ensure_collection(db: &WardsonDbClient) {
    if !db.collection_exists("crons").await.unwrap_or(true) {
        let _ = db.create_collection("crons").await;
    }
}

/// Add a cron job.
/// Param format: `<schedule> | <command>`
pub async fn cron_add(db: &WardsonDbClient, param: &str, config_tz: &str) -> String {
    if param.is_empty() {
        return "Usage: [TOOL:cron_add <schedule> | <command>]\n\
                Schedules: every 5m, every 1h, every 30s, hourly, daily 09:00\n\
                'daily HH:MM' is resolved in the configured timezone; avoid 02:00–03:00 on DST days.\n\
                Example: [TOOL:cron_add every 5m | system_status]"
            .into();
    }

    let parts: Vec<&str> = param.splitn(2, " | ").collect();
    if parts.len() < 2 {
        return "Usage: [TOOL:cron_add <schedule> | <command>]".into();
    }

    let schedule_str = parts[0].trim();
    let command = parts[1].trim();

    let (interval_secs, next_run) = match parse_schedule(schedule_str, config_tz) {
        Some(v) => v,
        None => return format!("Could not parse schedule: '{}'. Use formats like: every 5m, every 1h, hourly, daily 09:00", schedule_str),
    };

    ensure_collection(db).await;

    let doc = serde_json::json!({
        "schedule": schedule_str,
        "interval_secs": interval_secs,
        "command": command,
        "enabled": true,
        "last_run": null,
        "next_run": next_run,
        "created_at": Utc::now().to_rfc3339(),
    });

    match db.write("crons", &doc).await {
        Ok(id) => format!(
            "Cron job created (ID: {})\n  Schedule: {}\n  Command: {}\n  Next run: {}",
            id, schedule_str, command, next_run
        ),
        Err(e) => format!("Failed to create cron job: {}", e),
    }
}

/// List all cron jobs.
pub async fn cron_list(db: &WardsonDbClient) -> String {
    ensure_collection(db).await;

    let crons = db
        .query("crons", &serde_json::json!({}))
        .await
        .unwrap_or_default();

    if crons.is_empty() {
        return "No cron jobs configured. Add one with: [TOOL:cron_add <schedule> | <command>]"
            .into();
    }

    let mut output = format!("=== embraCRON Jobs ({}) ===\n", crons.len());
    for doc in &crons {
        let id = doc
            .get("_id")
            .or(doc.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let schedule = doc.get("schedule").and_then(|v| v.as_str()).unwrap_or("?");
        let command = doc.get("command").and_then(|v| v.as_str()).unwrap_or("?");
        let enabled = doc.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        let next_run = doc.get("next_run").and_then(|v| v.as_str()).unwrap_or("?");
        let last_run = doc
            .get("last_run")
            .and_then(|v| v.as_str())
            .unwrap_or("never");
        let status = if enabled { "ON" } else { "OFF" };

        output.push_str(&format!(
            "  [{}] [{}] {} → {}\n    Next: {} | Last: {}\n",
            id, status, schedule, command, next_run, last_run
        ));
    }
    output
}

/// Remove a cron job by ID.
pub async fn cron_remove(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: [TOOL:cron_remove <id>]".into();
    }

    let id = param.trim();
    match db.delete("crons", id).await {
        Ok(()) => format!("Cron job {} removed.", id),
        Err(e) => format!("Failed to remove cron job: {}", e),
    }
}

/// Check for due cron jobs and execute them. Called by the proactive engine.
/// Returns a list of result messages for fired crons.
pub async fn check_crons(db: &WardsonDbClient, config_tz: &str) -> Vec<String> {
    let crons = db
        .query("crons", &serde_json::json!({}))
        .await
        .unwrap_or_default();

    let now = Utc::now().to_rfc3339();
    let mut results = Vec::new();

    for doc in &crons {
        let enabled = doc.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        if !enabled {
            continue;
        }

        let next_run = doc.get("next_run").and_then(|v| v.as_str()).unwrap_or("");
        if next_run.is_empty() || next_run > now.as_str() {
            continue;
        }

        let command = doc
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let interval_secs = doc
            .get("interval_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(300);

        if command.is_empty() {
            continue;
        }

        // Execute the command via tool dispatch. Load config for dispatch; fall back
        // to a minimal in-memory SystemConfig if load fails (e.g., pre-wizard).
        let cfg = crate::config::load_config(db).await.unwrap_or_else(|_| crate::config::SystemConfig {
            name: "Embra".to_string(),
            api_key: String::new(),
            timezone: config_tz.to_string(),
            deployment_mode: "phase1".into(),
            created_at: String::new(),
            version: env!("CARGO_PKG_VERSION").into(),
            github_token: None,
            kg_temporal_window_secs: 1800,
            kg_max_traversal_depth: 3,
            kg_traversal_depth_ceiling: 5,
            kg_edge_candidate_limit: 50,
        });
        let tool_tag = format!("[TOOL:{}]", command);
        let result = super::dispatch(&tool_tag, db, &cfg, "cron").await;

        let result_text = result.unwrap_or_else(|| format!("Unknown command: {}", command));
        results.push(format!("embraCRON [{}]: {}", command, result_text));

        // Update last_run and next_run
        if let Some(id) = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()) {
            let mut updated = doc.clone();
            updated["last_run"] = serde_json::json!(Utc::now().to_rfc3339());
            updated["next_run"] = serde_json::json!(next_run_from_now(interval_secs));
            let _ = db.update("crons", id, &updated).await;
        }
    }

    results
}

#[cfg(test)]
mod daily_next_tests {
    use super::daily_next;
    use chrono::TimeZone;
    use chrono_tz::Tz;

    fn la() -> Tz {
        "America/Los_Angeles".parse().unwrap()
    }

    #[test]
    fn daily_0900_la_in_winter_is_1700_utc() {
        // 2026-01-15 12:00 UTC (04:00 PST) → next 09:00 PST is 2026-01-15 17:00 UTC.
        let now = chrono::Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        let next = daily_next(now, la(), 9, 0).unwrap();
        assert_eq!(next, chrono::Utc.with_ymd_and_hms(2026, 1, 15, 17, 0, 0).unwrap());
    }

    #[test]
    fn daily_0900_la_in_summer_is_1600_utc() {
        // 2026-07-15 12:00 UTC (05:00 PDT) → next 09:00 PDT is 2026-07-15 16:00 UTC.
        let now = chrono::Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let next = daily_next(now, la(), 9, 0).unwrap();
        assert_eq!(next, chrono::Utc.with_ymd_and_hms(2026, 7, 15, 16, 0, 0).unwrap());
    }

    #[test]
    fn daily_in_past_rolls_to_tomorrow() {
        // 2026-01-15 18:00 UTC (10:00 PST) → next 09:00 PST is 2026-01-16 17:00 UTC (today has passed).
        let now = chrono::Utc.with_ymd_and_hms(2026, 1, 15, 18, 0, 0).unwrap();
        let next = daily_next(now, la(), 9, 0).unwrap();
        assert_eq!(next, chrono::Utc.with_ymd_and_hms(2026, 1, 16, 17, 0, 0).unwrap());
    }

    #[test]
    fn dst_gap_falls_back_to_next_day() {
        // 2026-03-08 is spring-forward in the US; 02:30 local does not exist.
        // `daily_next` should fall through to 2026-03-09 02:30 (valid).
        let now = chrono::Utc.with_ymd_and_hms(2026, 3, 8, 0, 0, 0).unwrap();
        let next = daily_next(now, la(), 2, 30);
        assert!(next.is_some(), "should fall forward to next-day target");
    }

    #[test]
    fn invalid_hour_minute_returns_none() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        assert!(daily_next(now, la(), 25, 0).is_none());
        assert!(daily_next(now, la(), 9, 60).is_none());
    }

    #[test]
    fn utc_tz_is_no_op() {
        // 2026-01-15 08:00 UTC → next 09:00 UTC is 2026-01-15 09:00 UTC.
        let now = chrono::Utc.with_ymd_and_hms(2026, 1, 15, 8, 0, 0).unwrap();
        let next = daily_next(now, chrono_tz::UTC, 9, 0).unwrap();
        assert_eq!(next, chrono::Utc.with_ymd_and_hms(2026, 1, 15, 9, 0, 0).unwrap());
    }
}

// ── Native tool-use registrations (NATIVE-TOOLS-01) ──
//
// Tool DEFINITIONS only — Stage 6 rewrites the executor at the top of this
// file to invoke the registry directly instead of synthesizing [TOOL:...]
// strings. During Stages 2-5 the legacy executor still synthesizes and
// calls into the old string dispatcher.

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tools::registry::DispatchContext;

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "cron_add",
    description = "Schedule recurring tool execution. schedule accepts \"every 5m\", \"every 1h\", \"every 30s\", \"hourly\", \"daily HH:MM\" (resolved in the configured timezone; avoid 02:00-03:00 on DST days). command is the tool invocation as a single string that cron will dispatch at each fire."
)]
pub struct CronAddArgs {
    pub schedule: String,
    /// The tool-dispatch command to execute. During Stage 2 this is still a
    /// free-form string that the legacy executor wraps in `[TOOL:...]`;
    /// Stage 6 moves to a structured `{command_name, command_args}` doc.
    pub command: String,
}

impl CronAddArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} | {}", self.schedule, self.command);
        Ok(cron_add(ctx.db, &param, ctx.config_tz).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "cron_list",
    description = "List all scheduled cron jobs with their id, schedule, command, enabled flag, and next-run timestamp."
)]
pub struct CronListArgs {}

impl CronListArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(cron_list(ctx.db).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "cron_remove",
    description = "Remove a cron job by id."
)]
pub struct CronRemoveArgs {
    pub id: String,
}

impl CronRemoveArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(cron_remove(ctx.db, &self.id).await)
    }
}

#[cfg(test)]
mod native_args_tests {
    use super::*;

    #[test]
    fn cron_add_requires_schedule_and_command() {
        let a: CronAddArgs = serde_json::from_value(serde_json::json!({
            "schedule": "every 5m", "command": "system_status"
        }))
        .unwrap();
        assert_eq!(a.schedule, "every 5m");
        assert_eq!(a.command, "system_status");

        let err =
            serde_json::from_value::<CronAddArgs>(serde_json::json!({"schedule": "x"})).unwrap_err();
        assert!(err.to_string().contains("command"));
    }

    #[test]
    fn cron_tools_register() {
        let names: Vec<&'static str> = inventory::iter::<crate::tools::registry::ToolDescriptor>()
            .into_iter()
            .map(|d| d.name)
            .filter(|n| matches!(*n, "cron_add" | "cron_list" | "cron_remove"))
            .collect();
        assert_eq!(names.len(), 3, "all 3 cron tools register: {:?}", names);
    }
}
