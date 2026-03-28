//! Session manager stub.
//!
//! Provides the interface expected by grpc_service.rs.
//! Will be replaced with Phase 0's full SessionManager.

use std::sync::Arc;
use crate::db::client::WardsonClient;
use anyhow::Result;

/// Session info returned to gRPC callers.
pub struct SessionInfo {
    pub name: String,
    pub state: String,
    pub turn_count: u32,
    pub created_at: String,
    pub last_active: String,
    pub has_summary: bool,
}

/// Result of closing a session.
pub struct CloseResult {
    pub closed: String,
    pub switched_to: String,
}

pub struct SessionManager {
    _db: Arc<WardsonClient>,
}

impl SessionManager {
    pub async fn new(db: Arc<WardsonClient>) -> Result<Self> {
        Ok(Self { _db: db })
    }

    pub async fn list(&self) -> Result<Vec<SessionInfo>> {
        // TODO: Wire to Phase 0 session manager
        Ok(vec![])
    }

    pub async fn create(&mut self, name: &str) -> Result<SessionInfo> {
        // TODO: Wire to Phase 0 session manager
        Ok(SessionInfo {
            name: name.to_string(),
            state: "active".to_string(),
            turn_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            last_active: chrono::Utc::now().to_rfc3339(),
            has_summary: false,
        })
    }

    pub async fn switch(&mut self, name: &str) -> Result<SessionInfo> {
        // TODO: Wire to Phase 0 session manager
        Ok(SessionInfo {
            name: name.to_string(),
            state: "active".to_string(),
            turn_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            last_active: chrono::Utc::now().to_rfc3339(),
            has_summary: false,
        })
    }

    pub async fn close_current(&mut self) -> Result<CloseResult> {
        // TODO: Wire to Phase 0 session manager
        Ok(CloseResult {
            closed: String::new(),
            switched_to: String::new(),
        })
    }
}
