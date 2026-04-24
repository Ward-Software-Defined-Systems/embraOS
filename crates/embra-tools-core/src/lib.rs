//! Shared types for the embra tool registry.
//!
//! This crate deliberately avoids any dependency on `embra-brain` so that
//! the proc-macro's test fixtures and downstream adapters (future local /
//! QNM Brain implementations) can reference these types without pulling
//! in the full brain surface. `DispatchContext` and `ToolDescriptor` live
//! in `embra-brain` because they reference `WardsonDbClient`.

pub use serde_json::Value as JsonValue;

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("input deserialization failed for tool {tool}: {source}")]
    BadInput {
        tool: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("{0}")]
    Handler(String),
}

/// One tool invocation recorded in the current turn's trace.
///
/// `input_preview` and `result_preview` are bounded (≤200 chars in the
/// embra-brain populator) so the trace stays small even for chatty tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEntry {
    pub tool_name: String,
    pub tool_use_id: String,
    pub input_preview: String,
    /// RFC3339 UTC timestamp when dispatch started.
    pub started_at: String,
    pub elapsed_ms: u64,
    pub is_error: bool,
    pub result_preview: String,
}

/// In-memory trace of tool calls made within one user turn.
pub type TurnTrace = VecDeque<TraceEntry>;

/// Shared handle to a [`TurnTrace`]. Interior mutability via `Arc<Mutex>`
/// avoids propagating `&mut` through the `fn` handler signature on
/// `ToolDescriptor::handler`.
pub type TurnTraceHandle = Arc<Mutex<TurnTrace>>;

/// Construct an empty trace handle with a reasonable default capacity.
pub fn new_turn_trace_handle() -> TurnTraceHandle {
    Arc::new(Mutex::new(VecDeque::with_capacity(32)))
}
