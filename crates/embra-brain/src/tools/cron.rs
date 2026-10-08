use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::db::WardsonDbClient;
use super::parse_duration;
use super::sessions::truncate_str;

/// Where every run is recorded: `{job_id, command, started_at, elapsed_ms,
/// is_error, result, truncated}`, one document per fire. The report the
/// console shows is built from the same text, so what the operator read
/// and what the intelligence reads back later (`cron_list runs=N`, the
/// events block of its next turn) agree.
pub(crate) const CRON_RUNS: &str = "cron_runs";
/// Most of a run's result kept, in bytes, cut on a character boundary.
pub(crate) const CRON_RESULT_MAX: usize = 4096;
/// A run is removed this many days after it started; asserted on every
/// boot (`migrations::ensure_ttl_policies`).
pub(crate) const CRON_RUN_RETENTION_DAYS: u64 = 30;
pub(crate) const CRON_RUN_TTL_FIELD: &str = "started_at";
/// Most runs `cron_list` shows per job.
pub(crate) const CRON_LIST_RUNS_MAX: usize = 20;
/// Bytes of a run's result shown on its line, cut on a character boundary.
const RUN_PREVIEW_MAX: usize = 160;

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
        if parts.len() == 2
            && let (Ok(hour), Ok(minute)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>())
        {
            let resolved = super::resolve_timezone(config_tz);
            let tz: Tz = resolved.parse().unwrap_or(chrono_tz::UTC);
            let next_utc = daily_next(Utc::now(), tz, hour, minute)?;
            return Some((86400, next_utc.to_rfc3339()));
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

/// What one job dispatches, read from its stored document in either shape.
#[derive(Debug, PartialEq)]
pub(crate) struct CronPlan {
    /// The command as it was written, for the report and the list.
    pub display: String,
    pub name: String,
    pub args: serde_json::Value,
    /// Why the job runs without its arguments, when it does.
    pub note: Option<String>,
}

/// A cron command: a tool name, optionally followed by a JSON object of
/// arguments. Checked when the job is scheduled — the tool exists, the
/// arguments are an object, the required ones are there — so a job that
/// could never run is refused with the reason, not failed at every fire.
pub(crate) fn parse_cron_command(command: &str) -> Result<(String, serde_json::Value), String> {
    let command = command.trim();
    let (name, rest) = match command.split_once(char::is_whitespace) {
        Some((n, r)) => (n, r.trim()),
        None => (command, ""),
    };
    if name.is_empty() {
        return Err("the command is empty; cron runs a tool by name".to_string());
    }
    let Some(tool) = super::registry::all_descriptors().find(|d| d.name == name) else {
        return Err(format!("'{name}' is not a tool; cron runs a registered tool by name"));
    };
    let args = if rest.is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str::<serde_json::Value>(rest) {
            Ok(v) if v.is_object() => v,
            _ => {
                return Err(format!(
                    "the arguments after the tool name must be a JSON object, \
                     e.g. system_logs {{\"service\":\"embra-brain\"}}; got: {rest}"
                ))
            }
        }
    };
    let schema = (tool.input_schema)();
    let missing: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|r| {
            r.iter()
                .filter_map(|k| k.as_str())
                .filter(|k| args.get(k).is_none())
                .collect()
        })
        .unwrap_or_default();
    if !missing.is_empty() {
        return Err(format!(
            "tool '{name}' needs {}; give it as a JSON object after the name",
            missing.join(", ")
        ));
    }
    Ok((name.to_string(), args))
}

/// The plan for a stored job. A job scheduled since the arguments became a
/// JSON object carries `command_name` and `command_args`. One scheduled
/// before carries `command` alone, or `command_args` with the raw text
/// under `_legacy_raw` (migration v7 kept it). Raw text that is a JSON
/// object is used; any other raw text is not passed, and the note says so.
pub(crate) fn cron_dispatch_plan(doc: &serde_json::Value) -> Option<CronPlan> {
    let command = doc.get("command").and_then(|v| v.as_str()).unwrap_or("").trim();
    let (name, raw): (String, Option<String>) =
        match doc.get("command_name").and_then(|v| v.as_str()) {
            Some(n) if !n.is_empty() => {
                let raw = doc
                    .get("command_args")
                    .and_then(|a| a.get("_legacy_raw"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                (n.to_string(), raw)
            }
            _ => match command.split_once(char::is_whitespace) {
                Some((n, r)) => (n.to_string(), Some(r.trim().to_string()).filter(|r| !r.is_empty())),
                None => (command.to_string(), None),
            },
        };
    if name.is_empty() {
        return None;
    }
    let structured = doc
        .get("command_args")
        .filter(|a| a.is_object() && a.get("_legacy_raw").is_none())
        .cloned();
    let (args, note) = match (structured, raw) {
        (Some(args), _) => (args, None),
        (None, Some(raw)) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) if v.is_object() => (v, None),
            _ => (
                serde_json::json!({}),
                Some(format!(
                    "its arguments \"{raw}\" are not a JSON object and were not passed; \
                     re-create the job with cron_add as `{name} {{\"key\": value}}`"
                )),
            ),
        },
        (None, None) => (serde_json::json!({}), None),
    };
    let display = if command.is_empty() { format!("{name} {args}") } else { command.to_string() };
    Some(CronPlan { display, name, args, note })
}

/// The next run time from now, given an interval in seconds.
fn next_run_from_now(interval_secs: u64) -> String {
    let next = Utc::now() + chrono::Duration::seconds(interval_secs as i64);
    next.to_rfc3339()
}

async fn ensure_collection(db: &WardsonDbClient) {
    if !db.collection_exists("crons").await.unwrap_or(true) {
        let _ = db.create_collection("crons").await;
    }
}

/// The runs collection, for a brain upgraded in place: the boot creates
/// it with its index, this covers the first fire before the next boot.
async fn ensure_runs_collection(db: &WardsonDbClient) {
    if !db.collection_exists(CRON_RUNS).await.unwrap_or(true) {
        let _ = db.create_collection(CRON_RUNS).await;
    }
}

/// One run as it is recorded. `is_error` is a dispatch error (an unknown
/// tool, bad arguments, a timeout); a tool that reports a problem in its
/// own text is a run that worked, by house style, and reads as one here.
pub(crate) fn cron_run_doc(
    job_id: &str,
    display: &str,
    started_at: DateTime<Utc>,
    elapsed_ms: u64,
    is_error: bool,
    result: &str,
) -> serde_json::Value {
    let kept = truncate_str(result, CRON_RESULT_MAX);
    serde_json::json!({
        "job_id": job_id,
        "command": display,
        "started_at": started_at.to_rfc3339(),
        "elapsed_ms": elapsed_ms,
        "is_error": is_error,
        "result": kept,
        "truncated": kept.len() < result.len(),
    })
}

/// Add a cron job.
/// Param format: `<schedule> | <command>`
pub async fn cron_add(db: &WardsonDbClient, param: &str, config_tz: &str) -> String {
    if param.is_empty() {
        return "Usage: cron_add <schedule> | <command>\n\
                Schedules: every 5m, every 1h, every 30s, hourly, daily 09:00\n\
                'daily HH:MM' is resolved in the configured timezone; avoid 02:00–03:00 on DST days.\n\
                Example: cron_add every 5m | system_status"
            .into();
    }

    let parts: Vec<&str> = param.splitn(2, " | ").collect();
    if parts.len() < 2 {
        return "Usage: cron_add <schedule> | <command>".into();
    }

    let schedule_str = parts[0].trim();
    let command = parts[1].trim();

    let (interval_secs, next_run) = match parse_schedule(schedule_str, config_tz) {
        Some(v) => v,
        None => return format!("Could not parse schedule: '{}'. Use formats like: every 5m, every 1h, hourly, daily 09:00", schedule_str),
    };

    let (command_name, command_args) = match parse_cron_command(command) {
        Ok(parsed) => parsed,
        Err(why) => return format!("Cron job not created: {why}"),
    };

    ensure_collection(db).await;

    let doc = serde_json::json!({
        "schedule": schedule_str,
        "interval_secs": interval_secs,
        "command": command,
        "command_name": command_name,
        "command_args": command_args,
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

/// The query for a job's last `limit` recorded runs, newest first
/// (`idx_cron_runs_started_at` serves the sort).
pub(crate) fn runs_query_body(job_id: &str, limit: usize) -> serde_json::Value {
    serde_json::json!({
        "filter": {"job_id": job_id},
        "sort": [{"started_at": "desc"}, {"_id": "desc"}],
        "limit": limit,
    })
}

/// A stored time in the operator's zone, or the text as stored when it
/// does not parse.
fn local_time(rfc3339: &str, tz: &str) -> String {
    let Ok(t) = DateTime::parse_from_rfc3339(rfc3339) else {
        return rfc3339.to_string();
    };
    let zone: Tz = tz.parse().unwrap_or(chrono_tz::UTC);
    t.with_timezone(&zone).format("%Y-%m-%d %H:%M:%S %Z").to_string()
}

/// A run's result on one line.
fn run_preview(result: &str) -> String {
    let one_line: String = result.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.len() > RUN_PREVIEW_MAX {
        format!("{}…", truncate_str(&one_line, RUN_PREVIEW_MAX))
    } else {
        one_line
    }
}

/// `cron_list`'s text: the jobs as before, and under each job its recorded
/// runs when `runs_by_job` has them (newest first, as queried).
pub(crate) fn render_cron_list(
    crons: &[serde_json::Value],
    runs_by_job: &std::collections::HashMap<String, Vec<serde_json::Value>>,
    tz: &str,
) -> String {
    let mut output = format!("=== embraCRON Jobs ({}) ===\n", crons.len());
    for doc in crons {
        let id = doc_id(doc).unwrap_or("?");
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
        if let Some(runs) = runs_by_job.get(id) {
            if runs.is_empty() {
                output.push_str("    runs: none recorded\n");
            }
            for run in runs {
                let started = run.get("started_at").and_then(|v| v.as_str()).unwrap_or("?");
                let elapsed = run.get("elapsed_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                let is_error = run.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                let result = run.get("result").and_then(|v| v.as_str()).unwrap_or("");
                let marker = if is_error { "ERR" } else { "ok" };
                output.push_str(&format!(
                    "    run {} {} {}ms: {}\n",
                    local_time(started, tz),
                    marker,
                    elapsed,
                    run_preview(result)
                ));
            }
        }
    }
    output
}

/// List all cron jobs; with `runs` above zero, each job's last `runs`
/// recorded runs under it.
pub async fn cron_list(db: &WardsonDbClient, runs: usize, config_tz: &str) -> String {
    ensure_collection(db).await;

    let crons = db
        .fetch_collection("crons")
        .await
        .unwrap_or_default();

    if crons.is_empty() {
        return "No cron jobs configured. Add one with: cron_add <schedule> | <command>"
            .into();
    }

    let mut runs_by_job = std::collections::HashMap::new();
    if runs > 0 {
        for doc in &crons {
            if let Some(id) = doc_id(doc) {
                let found = db
                    .query(CRON_RUNS, &runs_query_body(id, runs))
                    .await
                    .unwrap_or_default();
                runs_by_job.insert(id.to_string(), found);
            }
        }
    }
    render_cron_list(&crons, &runs_by_job, config_tz)
}

/// The id of a stored job, as WardSONDB returns it.
fn doc_id(doc: &serde_json::Value) -> Option<&str> {
    doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str())
}

/// Shortest prefix of an id that `cron_remove` resolves, in characters.
///
/// A job's id is the UUIDv7 WardSONDB minted for its document,
/// `tttttttt-tttt-7rrr-vrrr-rrrrrrrrrrrr`, whose first twelve hex digits
/// are a 48-bit millisecond timestamp. Eight characters are the top 32
/// bits of that timestamp, one value per 65.5 s: jobs created more than a
/// minute apart always differ within them, and jobs created in the same
/// minute collide and are named as ambiguous. Shorter prefixes collapse by
/// construction (seven characters cover 17.5 minutes, six almost five
/// hours). Eight is also the first block of the id as `cron_list` prints
/// it. Guards in `id_prefix_tests`.
pub(crate) const MIN_ID_PREFIX: usize = 8;

/// What an id, or a prefix of one, resolves to among the stored jobs.
#[derive(Debug, PartialEq)]
pub(crate) enum IdMatch {
    /// Exactly one job: its full id.
    One(String),
    /// A prefix of several jobs: their full ids, sorted.
    Several(Vec<String>),
    /// No job has this id or prefix.
    None,
    /// Fewer than `MIN_ID_PREFIX` characters, and not an exact id.
    TooShort,
}

/// Resolve `wanted` against the ids of the stored jobs: an exact id at any
/// length (a UUID compares without case), otherwise a prefix of at least
/// `MIN_ID_PREFIX` characters.
pub(crate) fn resolve_id_prefix(ids: &[String], wanted: &str) -> IdMatch {
    let wanted = wanted.trim();
    if let Some(exact) = ids.iter().find(|id| id.eq_ignore_ascii_case(wanted)) {
        return IdMatch::One(exact.clone());
    }
    if wanted.chars().count() < MIN_ID_PREFIX {
        return IdMatch::TooShort;
    }
    let prefix = wanted.to_ascii_lowercase();
    let mut hits: Vec<String> = ids
        .iter()
        .filter(|id| id.to_ascii_lowercase().starts_with(&prefix))
        .cloned()
        .collect();
    match hits.len() {
        0 => IdMatch::None,
        1 => IdMatch::One(hits.remove(0)),
        _ => {
            hits.sort();
            IdMatch::Several(hits)
        }
    }
}

/// Remove a cron job by its id, or by a unique prefix of it
/// (`resolve_id_prefix`). The full-id case takes the same read of the
/// collection, so one path answers every miss with the same text.
pub async fn cron_remove(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: cron_remove <id>".into();
    }

    let wanted = param.trim();
    ensure_collection(db).await;
    let crons = match db.fetch_collection("crons").await {
        Ok(docs) => docs,
        Err(e) => return format!("Failed to read cron jobs: {}", e),
    };
    let ids: Vec<String> = crons.iter().filter_map(doc_id).map(str::to_string).collect();
    match resolve_id_prefix(&ids, wanted) {
        IdMatch::One(id) => match db.delete("crons", &id).await {
            Ok(()) => format!("Cron job {} removed.", id),
            Err(e) => format!("Failed to remove cron job: {}", e),
        },
        IdMatch::Several(ids) => format!(
            "{} is a prefix of {} cron jobs: {}; nothing removed. Give more of the id; cron_list shows ids.",
            wanted,
            ids.len(),
            ids.join(", ")
        ),
        IdMatch::None => format!("no cron job has id or prefix {}; cron_list shows ids", wanted),
        IdMatch::TooShort => format!(
            "{} is too short; give at least {} characters of the id, or the full id; cron_list shows ids",
            wanted, MIN_ID_PREFIX
        ),
    }
}

/// Check for due cron jobs and execute them. Called by the proactive engine.
/// Returns a list of result messages for fired crons.
pub async fn check_crons(db: &WardsonDbClient, config_tz: &str) -> Vec<String> {
    let crons = db
        .fetch_collection("crons")
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

        let Some(plan) = cron_dispatch_plan(doc) else { continue };
        let interval_secs = doc
            .get("interval_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(300);

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
            kg_traversal_edge_limit: 500,
            kg_traversal_node_budget: 1000,
            api_provider: "anthropic".to_string(),
            gemini_model: None,
            anthropic_model: None,
            anthropic_effort: None,
            gemini_effort: None,
            embedding_enabled: None,
            embedding_model: None,
            image_provider: None,
            image_model: None,
            git_tokens: None,
            anthropic_api_key: None,
            gemini_api_key: None,
            max_tool_iterations: None,
            show_reasoning: None,
            openai_compat: crate::config::OpenAiCompatConfig::default(),
        });
        // Direct registry dispatch — no model round-trip. The arguments are
        // the job's own (`cron_dispatch_plan`); a job whose arguments could
        // not be read runs without them and says so, here and in its report.
        if let Some(note) = &plan.note {
            tracing::warn!(target: "cron", command = %plan.display, "{}", note);
        }

        // Crons fire outside a user turn, so there's no in-turn trace to
        // record into. A fresh empty handle + turn_index 0 keeps the
        // DispatchContext signature satisfied without polluting user
        // turn traces.
        let cron_trace = embra_tools_core::new_turn_trace_handle();
        let ctx = super::registry::DispatchContext {
            db,
            config: &cfg,
            session_name: "cron",
            config_tz: &cfg.timezone,
            trace: &cron_trace,
            turn_index: 0,
        };
        let started = std::time::Instant::now();
        let started_at = Utc::now();
        let (result_text, is_error) = match super::registry::dispatch(&plan.name, plan.args.clone(), ctx)
        .await
        {
            // Cron consumes the text only — a cron-fired media tool's images
            // have no operator stream to land on.
            Ok(out) => (out.text, false),
            Err(e) => (format!("cron dispatch failed: {e}"), true),
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        // The run is recorded whatever happens to its report: the channel
        // may drop the report, the record stays.
        if let Some(id) = doc_id(doc) {
            ensure_runs_collection(db).await;
            let run = cron_run_doc(id, &plan.display, started_at, elapsed_ms, is_error, &result_text);
            if let Err(e) = db.write(CRON_RUNS, &run).await {
                tracing::warn!(target: "cron", command = %plan.display, "cron run not recorded: {e}");
            }
        }
        let note = plan.note.as_ref().map(|n| format!(" ({n})")).unwrap_or_default();
        results.push(format!("embraCRON [{}]: {}{}", plan.display, result_text, note));

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
mod run_record_tests {
    use super::*;

    #[test]
    fn a_run_document_carries_the_job_its_timing_and_a_capped_result() {
        let at: DateTime<Utc> = "2026-10-08T12:00:00Z".parse().unwrap();
        let doc = cron_run_doc("0199abcd-0000-7000-8000-000000000000", "system_status", at, 42, false, "ok");
        assert_eq!(doc["job_id"], "0199abcd-0000-7000-8000-000000000000");
        assert_eq!(doc["command"], "system_status");
        assert_eq!(doc["started_at"], "2026-10-08T12:00:00+00:00");
        assert_eq!(doc["elapsed_ms"], 42);
        assert_eq!(doc["is_error"], false);
        assert_eq!(doc["result"], "ok");
        assert_eq!(doc["truncated"], false);
        let long = "x".repeat(CRON_RESULT_MAX + 1);
        let doc = cron_run_doc("j", "time", at, 1, true, &long);
        assert_eq!(doc["result"].as_str().unwrap().len(), CRON_RESULT_MAX);
        assert_eq!(doc["truncated"], true);
        assert_eq!(doc["is_error"], true);
        assert_eq!(CRON_RESULT_MAX, 4096);
        assert_eq!(CRON_RUN_RETENTION_DAYS, 30);
        assert_eq!(CRON_RUN_TTL_FIELD, "started_at");
    }

    #[test]
    fn a_result_is_cut_on_a_character_boundary() {
        let at: DateTime<Utc> = "2026-10-08T12:00:00Z".parse().unwrap();
        // Three-byte characters: the cap falls inside one.
        let text = "日".repeat(CRON_RESULT_MAX / 3 + 10);
        let doc = cron_run_doc("j", "time", at, 1, false, &text);
        let kept = doc["result"].as_str().unwrap();
        assert!(kept.len() <= CRON_RESULT_MAX);
        assert!(kept.len() > CRON_RESULT_MAX - 3);
        assert!(text.starts_with(kept));
        assert_eq!(doc["truncated"], true);
    }
}

#[cfg(test)]
mod list_tests {
    use super::*;
    use serde_json::json;

    fn job() -> serde_json::Value {
        json!({
            "_id": "0199abcd-0000-7000-8000-000000000000", "schedule": "every 5m",
            "command": "system_status", "enabled": true,
            "next_run": "2026-10-08T12:05:00+00:00", "last_run": "2026-10-08T12:00:00+00:00",
        })
    }

    #[test]
    fn the_runs_query_is_per_job_sorted_newest_first_under_a_limit() {
        let body = runs_query_body("0199abcd-0000-7000-8000-000000000000", 3);
        assert_eq!(body["filter"], json!({"job_id": "0199abcd-0000-7000-8000-000000000000"}));
        assert_eq!(body["sort"], json!([{"started_at": "desc"}, {"_id": "desc"}]));
        assert_eq!(body["limit"], json!(3));
        assert_eq!(CRON_LIST_RUNS_MAX, 20);
    }

    #[test]
    fn cron_list_without_runs_prints_as_before() {
        let out = render_cron_list(&[job()], &std::collections::HashMap::new(), "UTC");
        assert_eq!(
            out,
            "=== embraCRON Jobs (1) ===\n  [0199abcd-0000-7000-8000-000000000000] [ON] every 5m → system_status\n    Next: 2026-10-08T12:05:00+00:00 | Last: 2026-10-08T12:00:00+00:00\n"
        );
    }

    #[test]
    fn cron_list_with_runs_shows_each_jobs_runs_under_it() {
        let mut runs_by_job = std::collections::HashMap::new();
        runs_by_job.insert(
            "0199abcd-0000-7000-8000-000000000000".to_string(),
            vec![
                json!({"started_at": "2026-10-08T12:00:00+00:00", "elapsed_ms": 42, "is_error": false,
                       "result": "uptime 3h\nmemory 177 MB"}),
                json!({"started_at": "2026-10-08T11:55:00+00:00", "elapsed_ms": 7, "is_error": true,
                       "result": format!("cron dispatch failed: {}", "x".repeat(300))}),
            ],
        );
        let out = render_cron_list(&[job()], &runs_by_job, "America/Los_Angeles");
        assert!(out.contains("    run 2026-10-08 05:00:00 PDT ok 42ms: uptime 3h memory 177 MB\n"), "{out}");
        assert!(out.contains("    run 2026-10-08 04:55:00 PDT ERR 7ms: cron dispatch failed: xxx"), "{out}");
        let err_line = out.lines().find(|l| l.contains("ERR 7ms")).unwrap();
        assert!(err_line.ends_with('…'), "{err_line}");
        assert!(err_line.len() < RUN_PREVIEW_MAX + 60);
        // A job with no recorded runs says so when runs were asked for.
        let mut empty = std::collections::HashMap::new();
        empty.insert("0199abcd-0000-7000-8000-000000000000".to_string(), Vec::new());
        assert!(render_cron_list(&[job()], &empty, "UTC").contains("    runs: none recorded\n"));
    }
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
// Tool definitions only. `check_crons` above runs a job through the same
// registry these register into (`registry::dispatch`).

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tools::registry::DispatchContext;

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "cron_add",
    is_side_effectful = true,
    description = "Schedule recurring tool execution. schedule accepts \"every 5m\", \"every 1h\", \"every 30s\", \"hourly\", \"daily HH:MM\" (resolved in the configured timezone; avoid 02:00-03:00 on DST days). command is a tool name, optionally followed by a JSON object of arguments, e.g. system_logs {\"service\":\"embra-brain\"}; the tool must exist and its required arguments must be given, or the job is refused. Cron dispatches it at each fire."
)]
pub struct CronAddArgs {
    pub schedule: String,
    /// A tool name, optionally followed by a JSON object of arguments.
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
    description = "List all scheduled cron jobs with their id, schedule, command, enabled flag, and next-run timestamp. With runs=N, each job's last N recorded runs are listed under it: when each started, whether the dispatch failed, how long it took, and a preview of its result. Runs are kept for thirty days."
)]
pub struct CronListArgs {
    /// Recorded runs to show per job, newest first (default 0, at most 20).
    #[serde(default)]
    pub runs: Option<u32>,
}

impl CronListArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let runs = (self.runs.unwrap_or(0) as usize).min(CRON_LIST_RUNS_MAX);
        Ok(cron_list(ctx.db, runs, ctx.config_tz).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "cron_remove",
    is_side_effectful = true,
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
mod command_tests {
    //! A cron command is a tool name and a JSON object of arguments,
    //! checked when the job is scheduled; a stored job of either shape
    //! yields the plan the executor runs.
    use super::*;
    use serde_json::json;

    #[test]
    fn a_tool_name_alone_schedules_with_no_arguments() {
        assert_eq!(parse_cron_command("system_status").unwrap(), ("system_status".to_string(), json!({})));
        assert_eq!(parse_cron_command("  system_status  ").unwrap().0, "system_status");
    }

    #[test]
    fn a_json_object_after_the_name_is_the_arguments() {
        let (name, args) = parse_cron_command(r#"system_logs {"service": "embra-brain", "lines": 50}"#).unwrap();
        assert_eq!(name, "system_logs");
        assert_eq!(args, json!({"service": "embra-brain", "lines": 50}));
    }

    #[test]
    fn a_name_that_is_not_a_tool_is_refused() {
        let why = parse_cron_command("make_coffee").unwrap_err();
        assert!(why.contains("not a tool"), "{why}");
        assert!(parse_cron_command("").is_err());
    }

    #[test]
    fn arguments_that_are_not_a_json_object_are_refused() {
        // The shape that was silently cut off before: words after the name.
        let why = parse_cron_command("system_logs embra-brain").unwrap_err();
        assert!(why.contains("JSON object"), "{why}");
        assert!(parse_cron_command("system_logs [1, 2]").is_err());
    }

    #[test]
    fn a_missing_required_argument_is_refused_when_scheduled_not_at_the_fire() {
        let why = parse_cron_command("remember").unwrap_err();
        assert!(why.contains("needs") && why.contains("content"), "{why}");
        assert!(parse_cron_command(r#"remember {"content": "check the build"}"#).is_ok());
    }

    #[test]
    fn a_job_runs_with_the_arguments_it_was_scheduled_with() {
        let doc = json!({"command": r#"system_logs {"service":"embrad"}"#,
                         "command_name": "system_logs", "command_args": {"service": "embrad"}});
        let plan = cron_dispatch_plan(&doc).unwrap();
        assert_eq!(plan.name, "system_logs");
        assert_eq!(plan.args, json!({"service": "embrad"}));
        assert_eq!(plan.note, None);
        assert_eq!(plan.display, r#"system_logs {"service":"embrad"}"#);
    }

    #[test]
    fn a_job_from_before_runs_with_what_can_be_read_and_says_what_cannot() {
        // Migration v7 kept the raw text; a JSON object in it is read.
        let doc = json!({"command": r#"system_logs {"service":"embrad"}"#, "command_name": "system_logs",
                         "command_args": {"_legacy_raw": r#"{"service":"embrad"}"#}});
        let plan = cron_dispatch_plan(&doc).unwrap();
        assert_eq!(plan.args, json!({"service": "embrad"}));
        assert!(plan.note.is_none());
        // Raw text that is not a JSON object is not passed, and the job says so.
        let doc = json!({"command": "system_logs embrad", "command_name": "system_logs",
                         "command_args": {"_legacy_raw": "embrad"}});
        let plan = cron_dispatch_plan(&doc).unwrap();
        assert_eq!(plan.args, json!({}));
        assert!(plan.note.as_deref().unwrap().contains("not passed"), "{:?}", plan.note);
        // A document with the command string alone, from before v7.
        let plan = cron_dispatch_plan(&json!({"command": "time"})).unwrap();
        assert_eq!((plan.name.as_str(), plan.args.clone(), plan.note.clone()), ("time", json!({}), None));
        assert!(cron_dispatch_plan(&json!({"command": ""})).is_none());
    }
}

#[cfg(test)]
mod id_prefix_tests {
    use super::{IdMatch, MIN_ID_PREFIX, resolve_id_prefix};

    /// A UUIDv7-shaped id whose timestamp is `ts_ms`.
    fn id_at(ts_ms: u64, tail: &str) -> String {
        let hex = format!("{:012x}", ts_ms);
        format!("{}-{}-7000-8000-{tail:0>12}", &hex[..8], &hex[8..])
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    const A: &str = "01a0edf5-4316-7a2b-8c3d-000000000001";
    const B: &str = "01a0edf5-9999-7a2b-8c3d-000000000002";
    const C: &str = "01b1ffff-0000-7a2b-8c3d-000000000003";

    #[test]
    fn a_full_id_resolves_to_itself() {
        assert_eq!(resolve_id_prefix(&ids(&[A, B, C]), A), IdMatch::One(A.to_string()));
    }

    #[test]
    fn a_unique_prefix_resolves_to_the_one_job_with_its_full_id() {
        assert_eq!(resolve_id_prefix(&ids(&[A, B, C]), "01b1ffff"), IdMatch::One(C.to_string()));
        assert_eq!(resolve_id_prefix(&ids(&[A, B, C]), "01a0edf5-43"), IdMatch::One(A.to_string()));
    }

    /// Two jobs created in the same minute share their first eight
    /// characters: the prefix names both and removes neither.
    #[test]
    fn an_ambiguous_prefix_names_every_matching_job_and_resolves_none() {
        assert_eq!(
            resolve_id_prefix(&ids(&[B, A, C]), "01a0edf5"),
            IdMatch::Several(ids(&[A, B]))
        );
    }

    /// Short prefixes are refused before matching, unique or not: seven
    /// characters would cover every job of a 17-minute window.
    #[test]
    fn a_prefix_shorter_than_eight_characters_is_refused_before_it_is_matched() {
        assert_eq!(resolve_id_prefix(&ids(&[C]), "01b1fff"), IdMatch::TooShort);
        assert_eq!(resolve_id_prefix(&ids(&[C]), ""), IdMatch::TooShort);
    }

    #[test]
    fn an_unknown_id_or_prefix_resolves_to_nothing() {
        assert_eq!(resolve_id_prefix(&ids(&[A, B]), "ffffffff"), IdMatch::None);
        assert_eq!(resolve_id_prefix(&ids(&[A, B]), C), IdMatch::None);
        assert_eq!(resolve_id_prefix(&[], "01a0edf5"), IdMatch::None);
    }

    #[test]
    fn matching_ignores_ascii_case() {
        assert_eq!(resolve_id_prefix(&ids(&[A, C]), "01B1FFFF"), IdMatch::One(C.to_string()));
        assert_eq!(resolve_id_prefix(&ids(&[A, C]), &C.to_uppercase()), IdMatch::One(C.to_string()));
    }

    /// Why eight: the first eight hex digits are the top 32 bits of the
    /// 48-bit millisecond timestamp, one value per 65,536 ms.
    #[test]
    fn eight_characters_separate_jobs_created_more_than_a_minute_apart() {
        let t0 = 1_700_000_000_000u64;
        let a = id_at(t0, "a");
        let later = id_at(t0 + 65_536, "b");
        let same_minute = id_at(t0 + 1, "c");
        assert_eq!(MIN_ID_PREFIX, 8);
        assert_ne!(&a[..MIN_ID_PREFIX], &later[..MIN_ID_PREFIX]);
        assert_eq!(&a[..MIN_ID_PREFIX], &same_minute[..MIN_ID_PREFIX]);
        assert_eq!(
            resolve_id_prefix(&[a.clone(), later.clone(), same_minute.clone()], &later[..8]),
            IdMatch::One(later)
        );
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
            .map(|d| d.name)
            .filter(|n| matches!(*n, "cron_add" | "cron_list" | "cron_remove"))
            .collect();
        assert_eq!(names.len(), 3, "all 3 cron tools register: {:?}", names);
    }
}
