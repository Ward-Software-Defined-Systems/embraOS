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
    #[error("tool '{tool}' exceeded {limit_secs}s global execution timeout")]
    Timeout { tool: String, limit_secs: u64 },
}

/// Structured result of one tool dispatch.
///
/// Every tool's inherent `run()` may return either `Result<String, _>` (the
/// 103 pre-media tools — the `#[embra_tool]` macro maps it through
/// `From<String>`) or `Result<ToolOutput, _>` when it has images to hand the
/// model (`image_view`, `image_generate`). `text` is what the dispatcher
/// size-caps (`apply_max_size`); `images` deliberately bypass that cap —
/// byte-truncating a base64 payload would corrupt it silently — and are
/// bounded by count (`MAX_TOOL_RESULT_IMAGES`) in the dispatcher instead.
#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub text: String,
    pub images: Vec<ToolImage>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        ToolOutput {
            text: text.into(),
            images: Vec::new(),
        }
    }

    pub fn with_image(mut self, image: ToolImage) -> Self {
        self.images.push(image);
        self
    }
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        ToolOutput::text(text)
    }
}

/// One image a tool returns alongside its text. `data` is the raw encoded
/// file (PNG/JPEG/GIF/WebP — already normalized by the media ingest path),
/// never base64: providers encode at wire time. `media_ref` is set when the
/// bytes also live in the MEDIA store, so the tool loop can emit the
/// operator-facing `MediaRef` frame (tools have no stream handle).
#[derive(Clone)]
pub struct ToolImage {
    pub media_type: String,
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub name: String,
    pub media_ref: Option<MediaRefMeta>,
}

impl std::fmt::Debug for ToolImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolImage")
            .field("media_type", &self.media_type)
            .field("bytes", &self.data.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .field("name", &self.name)
            .field("media_ref", &self.media_ref)
            .finish()
    }
}

/// Display metadata for an image in the MEDIA store — the wire-neutral
/// twin of the proto `MediaRef` (embra-brain converts). `origin` is
/// `attached` | `generated` | `viewed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRefMeta {
    pub id: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub byte_size: u64,
    pub name: String,
    pub origin: String,
    pub path: String,
    #[serde(default)]
    pub caption: String,
}

/// One tool invocation recorded in the current turn's trace.
///
/// `input_preview` and `result_preview` are bounded (≤2 KiB, byte-capped
/// char-boundary-safe, in the embra-brain populator) so the trace stays
/// small even for chatty tools.
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
