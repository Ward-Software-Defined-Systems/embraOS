//! UI-agnostic core for embraOS console clients.
//!
//! Holds gRPC client + event types, application state, slash-command
//! handling, console-event reduction, live-reasoning buffering, neutral
//! style enums, and styled-text parsers.
//!
//! Two clients consume this crate today:
//! - `embra-console` (TUI) — maps neutral styles to ratatui types
//! - `embra-desktop` (in-OS GUI) — maps neutral styles to iced types
//!
//! Nothing here imports ratatui, crossterm, iced, or smithay. The boundary
//! is enforced by hand; any UI crate that breaks it should be flagged in
//! review.

pub mod commands;
pub mod events;
pub mod grpc;
pub mod reasoning;
pub mod render;
pub mod state;
pub mod style;
