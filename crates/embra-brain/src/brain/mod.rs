//! Brain module — system prompts and the legacy session-persistence
//! `Message` type.
//!
//! The Anthropic-specific `Brain` struct, SSE parser, wire types, and
//! tool snapshot logic moved to `crate::provider::anthropic` in Sprint 4
//! Stage 2. The `LlmProvider` trait at `crate::provider` is the new
//! outbound LLM surface.

pub mod prompts;
mod types;

pub use prompts::*;
pub use types::Message;
