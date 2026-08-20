//! Brain module — system prompts and the legacy session-persistence
//! `Message` type.
//!
//! The Anthropic-specific `Brain` struct, SSE parser, wire types, and
//! tool snapshot logic moved to `crate::provider::anthropic` in Sprint 4
//! Stage 2. The `LlmProvider` trait at `crate::provider` is the new
//! outbound LLM surface.

mod graph_render;
mod identity_render;
pub mod prompts;
mod soul_render;
mod types;
mod user_render;

pub use graph_render::{render_sealed_graph, render_user_graph};
pub use identity_render::render_identity;
pub use prompts::*;
pub use soul_render::render_constitution;
pub use types::{AttachmentRef, Message};
pub use user_render::render_user_profile;
