use chrono::Utc;

use crate::db::WardsonDbClient;
use super::parse_duration;

/// Parse a schedule string into seconds between runs.
/// Formats: "every 5m", "every 1h", "every 30s", "hourly", "daily HH:MM"
/// Returns (interval_secs, next_run_rfc3339) or None if unparseable.
fn parse_schedule(schedule: &str) -> Option<(u64, String)> {
    let s = schedule.trim().to_lowercase();

    if s == "hourly" {
        let next = Utc::now() + chrono::Duration::seconds(3600);
        return Some((3600, next.to_rfc3339()));
    }

    if let Some(time_str) = s.strip_prefix("daily ") {
        let time_str = time_str.trim();
        // Parse HH:MM
        let parts: Vec<&str> = time_str.split(':').collect();
        if parts.len() == 2 {
            if let (Ok(hour), Ok(minute)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                if hour < 24 && minute < 60 {
                    let now = Utc::now();
                    let today = now.date_naive();
                    let target_time = chrono::NaiveTime::from_hms_opt(hour, minute, 0)?;
                    let target_dt = today.and_time(target_time);
                    let target_utc = target_dt.and_utc();
                    let next = if target_utc <= now {
                        target_utc + chrono::Duration::days(1)
                    } else {
                        target_utc
                    };
                    return Some((86400, next.to_rfc3339()));
                }
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
pub async fn cron_add(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: [TOOL:cron_add <schedule> | <command>]\n\
                Schedules: every 5m, every 1h, every 30s, hourly, daily 09:00\n\
                Example: [TOOL:cron_add every 5m | system_status]"
            .into();
    }

    let parts: Vec<&str> = param.splitn(2, " | ").collect();
    if parts.len() < 2 {
        return "Usage: [TOOL:cron_add <schedule> | <command>]".into();
    }

    let schedule_str = parts[0].trim();
    let command = parts[1].trim();

    let (interval_secs, next_run) = match parse_schedule(schedule_str) {
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
