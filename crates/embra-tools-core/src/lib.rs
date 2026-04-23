//! Shared types for the embra tool registry.
//!
//! This crate deliberately avoids any dependency on `embra-brain` so that
//! the proc-macro's test fixtures and downstream adapters (future local /
//! QNM Brain implementations) can reference these types without pulling
//! in the full brain surface. `DispatchContext` and `ToolDescriptor` live
//! in `embra-brain` because they reference `WardsonDbClient`.

pub use serde_json::Value as JsonValue;

use std::future::Future;
use std::pin::Pin;

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
