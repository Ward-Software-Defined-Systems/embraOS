//! Gemini provider: `gemini-3.1-pro-preview` via the public Generative
//! Language API.
//!
//! Submodules land progressively across Sprint 4:
//! - [`wire`] (Stage 3) — Gemini-shaped request / response types.
//! - [`tool_schema`] (Stage 3) — translator from registry descriptors
//!   to Gemini's OpenAPI-3.0 subset.
//! - `streaming` (Stage 4) — SSE parser.
//! - `cache` (Stage 6) — explicit Context Cache lifecycle manager.
//!
//! `GeminiProvider` itself lands in Stage 5; this module is the
//! Stage 3 skeleton.

pub mod tool_schema;
pub mod wire;
