use serde::{Deserialize, Serialize};

use crate::db::WardsonDbClient;

#[derive(Debug, Serialize)]
pub struct SystemStatus {
    pub version: String,
    pub uptime_seconds: u64,
    pub wardsondb_healthy: bool,
    pub wardsondb_collections: Vec<String>,
    pub memory_usage_mb: Option<u64>,
    pub soul_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    pub download_url: String,
}

static START_TIME: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

pub fn init_start_time() {
    START_TIME.get_or_init(std::time::Instant::now);
}

pub async fn system_status(db: &WardsonDbClient) -> SystemStatus {
    let uptime = START_TIME
        .get()
        .map(|t| t.elapsed().as_secs())
        .unwrap_or(0);

    let healthy = db.health().await.unwrap_or(false);

    let collections = db.list_collections().await.unwrap_or_default();

    let soul_status = if db
        .collection_exists("soul.invariant")
        .await
        .unwrap_or(false)
    {
        "sealed".to_string()
    } else {
        "unsealed".to_string()
    };

    let memory_usage_mb = get_memory_usage_mb();

    SystemStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: uptime,
        wardsondb_healthy: healthy,
        wardsondb_collections: collections,
        memory_usage_mb,
        soul_status,
    }
}

pub async fn check_wardsondb_update() -> Option<UpdateInfo> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.github.com/repos/ward-software-defined-systems/wardsondb/releases/latest")
        .header("User-Agent", "embraOS/0.1.0")
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let data: serde_json::Value = resp.json().await.ok()?;
    let latest_tag = data.get("tag_name")?.as_str()?;
    let latest_version = latest_tag.trim_start_matches('v');

    // Compare with current (we'd need to query the running WardSONDB for its version)
    let current_version = "0.1.0"; // Hardcoded for Phase 0

    if latest_version != current_version {
        let download_url = data
            .get("assets")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(|a| a.get("browser_download_url"))
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string();

        Some(UpdateInfo {
            version: latest_version.to_string(),
            current_version: current_version.to_string(),
            download_url,
        })
    } else {
        None
    }
}

fn get_memory_usage_mb() -> Option<u64> {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("VmRSS:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(kb) = parts.get(1).and_then(|v| v.parse::<u64>().ok()) {
                    return Some(kb / 1024);
                }
            }
        }
    }
    None
}
