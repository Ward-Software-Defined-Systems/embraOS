use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::db::WardsonDbClient;

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
) -> mpsc::Receiver<Notification> {
    let (tx, rx) = mpsc::channel(64);
    let db = db.clone();

    // Normal health checks every 5 minutes
    let tx_health = tx.clone();
    let db_health = db.clone();
    tokio::spawn(async move {
        // Initial delay to let the system stabilize
        tokio::time::sleep(Duration::from_secs(30)).await;

        loop {
            run_health_checks(&db_health, &tx_health).await;
            tokio::time::sleep(NORMAL_CHECK_INTERVAL).await;
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

async fn run_health_checks(db: &WardsonDbClient, tx: &mpsc::Sender<Notification>) {
    // Check WardSONDB health
    match db.health().await {
        Ok(true) => {}
        Ok(false) => {
            warn!("WardSONDB health check failed");
            let _ = tx
                .send(Notification::new(
                    Priority::Critical,
                    "WardSONDB is not responding. Data persistence may be affected.",
                ))
                .await;
        }
        Err(e) => {
            error!("WardSONDB health check error: {}", e);
            let _ = tx
                .send(Notification::new(
                    Priority::Critical,
                    format!("WardSONDB health check error: {}", e),
                ))
                .await;
        }
    }

    // Check system memory
    if let Some(usage) = get_memory_usage_mb() {
        if usage > 512 {
            let _ = tx
                .send(Notification::new(
                    Priority::Normal,
                    format!("High memory usage: {}MB", usage),
                ))
                .await;
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
