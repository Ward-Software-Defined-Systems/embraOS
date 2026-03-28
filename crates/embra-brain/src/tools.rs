//! Tool dispatch stub.
//!
//! Provides the interface expected by grpc_service.rs.
//! Will be replaced with Phase 0's full ~63 tool implementations.

use std::sync::Arc;
use tokio::sync::RwLock;
use crate::db::client::WardsonClient;
use crate::sessions::SessionManager;

/// A parsed tool invocation tag from Brain output.
pub struct ToolTag {
    pub name: String,
    pub input: String,
}

/// Result of executing a tool.
pub struct ToolResult {
    pub output: String,
    pub success: bool,
}

/// Extract [TOOL:name ...] tags from Brain response text.
pub fn extract_tool_tags(text: &str) -> Vec<ToolTag> {
    // TODO: Wire to Phase 0 tool tag parser
    let _ = text;
    vec![]
}

/// Dispatch a tool invocation to the appropriate handler.
pub async fn dispatch(
    tag: &ToolTag,
    _db: &Arc<WardsonClient>,
    _session_mgr: &Arc<RwLock<SessionManager>>,
) -> ToolResult {
    // TODO: Wire to Phase 0 tool dispatch
    ToolResult {
        output: format!("Tool '{}' not yet implemented in Phase 1", tag.name),
        success: false,
    }
}
