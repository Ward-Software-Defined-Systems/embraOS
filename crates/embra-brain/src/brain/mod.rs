//! Brain module — Anthropic API integration.
//!
//! This is a stub that will be wired to Phase 0's brain module.
//! The StreamEvent enum is the bridge between Brain and gRPC service.

/// Events emitted by the Brain during streaming.
/// This is the same concept as Phase 0 — the consumer just changed
/// from the TUI event loop to the gRPC Converse handler.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Token(String),
    Done(String),
    Error(String),
}
