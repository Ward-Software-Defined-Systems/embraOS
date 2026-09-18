use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config;
use crate::db::WardsonDbClient;
use crate::provider::health::{self, ProviderProbe};

const NORMAL_CHECK_INTERVAL: Duration = Duration::from_secs(300); // 5 minutes
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(3600); // 1 hour

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Priority {
    Critical,
    Normal,
    Low,
}

impl Priority {
    pub fn label(&self) -> &'static str {
        match self {
            Priority::Critical => "CRITICAL",
            Priority::Normal => "NOTICE",
            Priority::Low => "INFO",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: String,
    pub priority: Priority,
    pub message: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub delivered: bool,
}

impl Notification {
    pub fn new(priority: Priority, message: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            priority,
            message: message.into(),
            created_at: chrono::Utc::now(),
            delivered: false,
        }
    }

    pub fn priority_label(&self) -> &str {
        self.priority.label()
    }
}

pub fn start_proactive_engine(
    db: &WardsonDbClient,
    config_tz: &str,
    boot_api_key: &str,
) -> mpsc::Receiver<Notification> {
    let (tx, rx) = mpsc::channel(64);
    let db = db.clone();
    let config_tz = config_tz.to_string();

    // Normal health checks every 5 minutes: WardSONDB + system memory,
    // then the active LLM provider's endpoint (the first run, ~30 s after
    // boot, is the model check the config load never did). A `/provider`
    // switch or a finished setup flow re-runs the tick immediately.
    let tx_health = tx.clone();
    let db_health = db.clone();
    let boot_key = boot_api_key.to_string();
    tokio::spawn(async move {
        // Initial delay to let the system stabilize
        tokio::time::sleep(Duration::from_secs(30)).await;

        let mut previous: Option<ProviderProbe> = None;
        loop {
            run_health_checks(&db_health, &tx_health).await;
            previous = run_provider_probe(&db_health, &boot_key, previous, &tx_health).await;
            tokio::select! {
                _ = tokio::time::sleep(NORMAL_CHECK_INTERVAL) => {}
                _ = health::probe_requested() => {}
            }
        }
    });

    // Reminder checks every 15 seconds
    let tx_reminders = tx.clone();
    let db_reminders = db.clone();
    tokio::spawn(async move {
        // Initial delay
        tokio::time::sleep(Duration::from_secs(10)).await;

        loop {
            let fired = crate::tools::check_reminders(&db_reminders).await;
            for msg in fired {
                let _ = tx_reminders
                    .send(Notification::new(Priority::Normal, msg))
                    .await;
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });

    // Cron checks every 15 seconds
    let tx_cron = tx.clone();
    let db_cron = db.clone();
    let config_tz_cron = config_tz;
    tokio::spawn(async move {
        // Initial delay
        tokio::time::sleep(Duration::from_secs(15)).await;

        loop {
            let fired = crate::tools::cron::check_crons(&db_cron, &config_tz_cron).await;
            for msg in fired {
                let _ = tx_cron
                    .send(Notification::new(Priority::Normal, msg))
                    .await;
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });

    // Update checks every hour
    let tx_update = tx;
    tokio::spawn(async move {
        // Initial delay
        tokio::time::sleep(Duration::from_secs(60)).await;

        loop {
            run_update_checks(&tx_update).await;
            tokio::time::sleep(UPDATE_CHECK_INTERVAL).await;
        }
    });

    rx
}

/// Non-blocking hand-off to the Converse stream. The channel is bounded
/// (64) and drained only while an operational stream holds the receiver;
/// a headless boot fills it, and an awaiting `send` would park this loop
/// — and every check after it — forever. Dropping a notification nobody
/// is listening to is the right trade.
fn push_notification(tx: &mpsc::Sender<Notification>, notification: Notification) {
    if let Err(e) = tx.try_send(notification) {
        warn!("proactive notification dropped (channel full or closed): {}", e);
    }
}

/// One provider probe: resolve the target from persisted config the way a
/// turn would, run it, publish it for `/status`, and tell the operator
/// about CHANGES (`health::transition_events`).
async fn run_provider_probe(
    db: &WardsonDbClient,
    boot_key: &str,
    previous: Option<ProviderProbe>,
    tx: &mpsc::Sender<Notification>,
) -> Option<ProviderProbe> {
    // A pre-wizard boot has no config yet → nothing to probe.
    let target = config::load_config(db)
        .await
        .ok()
        .and_then(|cfg| crate::grpc_service::provider_probe_target(&cfg, boot_key));
    let probe = health::probe(target.as_ref()).await;
    if probe.configured {
        if probe.healthy() {
            info!(
                target: "provider::health",
                kind = %probe.kind,
                model = %probe.model,
                endpoint = %probe.endpoint,
                latency_ms = ?probe.latency_ms,
                "provider endpoint ok"
            );
        } else {
            warn!(
                target: "provider::health",
                kind = %probe.kind,
                model = %probe.model,
                endpoint = %probe.endpoint,
                reachable = probe.reachable,
                model_present = ?probe.model_present,
                key_state = ?probe.key_state,
                error = ?probe.error,
                "provider endpoint check failed"
            );
        }
    }
    for notification in health::transition_events(previous.as_ref(), &probe) {
        push_notification(tx, notification);
    }
    health::record(probe.clone());
    Some(probe)
}

async fn run_health_checks(db: &WardsonDbClient, tx: &mpsc::Sender<Notification>) {
    // Check WardSONDB health with degradation awareness
    match db.health_detailed().await {
        Ok(detail) if detail.up => {
            if detail.status == "degraded" {
                warn!("WardSONDB storage engine degraded");
                let warning_msg = detail
                    .warning
                    .unwrap_or_else(|| "Storage engine degraded".into());
                push_notification(
                    tx,
                    Notification::new(
                        Priority::Critical,
                        format!(
                        "CRITICAL: WardSONDB storage engine is degraded — {}. Memory and session writes may be failing silently.",
                        warning_msg
                        ),
                    ),
                );
            }
            if detail.write_pressure.as_deref() == Some("high") {
                push_notification(
                    tx,
                    Notification::new(
                        Priority::Normal,
                        "WardSONDB write pressure is high — compaction in progress, non-essential queries may be slow.",
                    ),
                );
            }
        }
        Ok(_) => {
            warn!("WardSONDB health check failed");
            push_notification(
                tx,
                Notification::new(
                    Priority::Critical,
                    "WardSONDB is not responding. Data persistence may be affected.",
                ),
            );
        }
        Err(e) => {
            error!("WardSONDB health check error: {}", e);
            push_notification(
                tx,
                Notification::new(
                    Priority::Critical,
                    format!("WardSONDB health check error: {}", e),
                ),
            );
        }
    }

    // Check system memory
    if let Some(usage) = get_memory_usage_mb() {
        if usage > 512 {
            push_notification(
                tx,
                Notification::new(
                    Priority::Normal,
                    format!("High memory usage: {}MB", usage),
                ),
            );
        }
    }
}

async fn run_update_checks(tx: &mpsc::Sender<Notification>) {
    match crate::tools::check_wardsondb_update().await {
        Some(info) => {
            info!("WardSONDB update available: v{}", info.version);
            let _ = tx
                .send(Notification::new(
                    Priority::Low,
                    format!(
                        "WardSONDB update available: v{} (current: v{})",
                        info.version, info.current_version
                    ),
                ))
                .await;
        }
        None => {}
    }
}

fn get_memory_usage_mb() -> Option<u64> {
    // Read from /proc/self/status on Linux
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
