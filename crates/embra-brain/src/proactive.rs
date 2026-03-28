//! Proactive engine stub.
//!
//! Provides the interface expected by grpc_service.rs.
//! Will be replaced with Phase 0's full proactive notification engine.

use std::sync::Arc;
use tokio::sync::broadcast;
use crate::db::client::WardsonClient;

pub struct ProactiveEngine {
    _db: Arc<WardsonClient>,
    sender: broadcast::Sender<String>,
}

impl ProactiveEngine {
    pub fn new(db: Arc<WardsonClient>) -> Self {
        let (sender, _) = broadcast::channel(64);
        Self { _db: db, sender }
    }

    /// Start background proactive tasks (health checks, reminders, cron).
    pub async fn start(&self) {
        // TODO: Wire to Phase 0 proactive engine
        // - Health checks every 5 min
        // - Reminder checks every 15s
        // - Cron job checks every 15s
        tracing::info!("Proactive engine started (stub)");
    }

    /// Subscribe to proactive notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.sender.subscribe()
    }
}
