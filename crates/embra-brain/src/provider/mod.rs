//! Pluggable LLM provider abstraction.
//!
//! The Brain's outbound LLM surface is fronted by [`LlmProvider`]. Each
//! provider implementation owns its wire types, request body construction,
//! streaming parser, and tool schema translator. The loop driver in
//! `grpc_service.rs` consumes only the neutral IR (`provider::ir`) and
//! the trait below — it has no provider-specific code paths.
//!
//! Submodules land progressively:
//! - `ir` (this stage) — neutral IR types.
//! - `anthropic` (Stage 2) — refactored from `crate::brain`.
//! - `gemini` (Stages 3–6) — new.

pub mod anthropic;
pub mod gemini;
pub mod ir;

pub use ir::*;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value as JsonValue;

/// Identity of a provider. Persisted as a string in WardSONDB
/// (`config.system.api_provider`, `sessions.<name>.meta.provider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    Gemini,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Gemini => "gemini",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "anthropic" => Some(Self::Anthropic),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }
}

/// Per-provider tool manifest, built once at `Brain::new` from the
/// shared registry. The `wire_json` is whatever shape the provider's API
/// expects in the request body.
pub struct ToolManifest {
    /// Pre-serialized JSON ready to splice into a request body. For
    /// Anthropic this is `[{name, description, input_schema}, ...]`; for
    /// Gemini it is `[{functionDeclarations: [...]}]`.
    pub wire_json: JsonValue,
    /// SHA-256 over the canonical JSON, truncated to 16 hex chars.
    /// Used by Gemini's context-cache manager to detect staleness.
    pub fingerprint: String,
}

/// System prompt + identity hash. Identity hash is stable across turns
/// (no per-turn state injection) so context-cache reuse works.
pub struct SystemPromptBundle {
    pub text: String,
    pub fingerprint: String,
}

/// One event from a streaming turn. `Complete` carries the assembled
/// neutral-IR turn — the loop driver consumes that and ignores the
/// per-block deltas (which are forwarded to the TUI for live UX).
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// User-visible text delta (drives the TUI's typing feel).
    TextDelta(String),
    /// One block in the assistant turn finalized. Carries no payload
    /// here — the assembled blocks come back together in `Complete`.
    BlockComplete,
    /// Incremental tool-args streaming. Held for forward compat;
    /// providers may not emit this in v1.
    ToolArgsDelta {
        call_id: String,
        path: String,
        fragment: String,
    },
    /// Terminal event — turn assembled, ready for the loop driver.
    Complete(AssistantTurn),
    /// Provider-side error (4xx, 5xx, decode, network). Surfaced to
    /// the operator; non-fatal for the gRPC stream.
    Error(String),
}

/// Outcome of a key-validation probe. Drives wizard UX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationResult {
    Valid,
    InvalidKey,
    /// 403 — key is shaped correctly but not authorized (often missing
    /// billing on the Gemini side).
    Forbidden,
    /// 5xx, network, timeout. Wizard re-prompts.
    NetworkError,
    Unknown,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("network: {0}")]
    Network(String),
    #[error("decode: {0}")]
    Decode(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
}

/// Outbound LLM surface. One impl per provider.
///
/// `stream_turn` is the hot path — it consumes the full turn-history,
/// system prompt, and tool manifest, and returns a stream of
/// [`StreamEvent`]s ending in `Complete(AssistantTurn)`.
///
/// `build_tool_manifest` is called once at Brain construction. The
/// returned manifest is reused across every `stream_turn` call until the
/// registry or system prompt changes.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Human-readable model identifier for the status bar.
    /// Anthropic: `"opus-4.7"`. Gemini: `"gemini-3.1-pro"`.
    fn display_name(&self) -> &str;

    /// For persistence + slash-command UX.
    fn kind(&self) -> ProviderKind;

    /// Probe the provider's model-listing endpoint with the given key.
    /// Called by the config wizard; never on the hot path.
    async fn validate_key(&self, key: &str) -> ValidationResult;

    /// Stream one assistant turn. Returns a `BoxStream` whose terminal
    /// event is `StreamEvent::Complete(AssistantTurn)`.
    async fn stream_turn(
        &self,
        messages: &[ApiMessage],
        system: &SystemPromptBundle,
        tools: &ToolManifest,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError>;

    /// Translate the registry's typed-args descriptors into the
    /// provider's request-body tool shape. Single-shot, called from
    /// `Brain::new`.
    fn build_tool_manifest(
        &self,
        descriptors: &[&'static crate::tools::registry::ToolDescriptor],
    ) -> ToolManifest;
}
