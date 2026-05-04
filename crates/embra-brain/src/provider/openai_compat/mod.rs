//! OpenAI Chat Completions-compatible provider.
//!
//! Single module covering both Ollama and LM Studio backends. The
//! presets share an HTTP client surface and wire format; the discriminator
//! is `OpenAiCompatPreset` (provided in Stage 3) which selects defaults
//! and labels.
//!
//! Module layout:
//! - [`wire`] — request/response/streaming types (snake_case JSON).
//! - [`conv`] — neutral IR ↔ wire translators with reasoning round-trip.
//! - [`tool_schema`] — tool-schema translator (light passthrough using
//!   the shared `provider::schema_util::inline_refs`).
//! - [`sanitize`] — always-on harmony token sanitization for tool-call
//!   names per Locked Decision #11.
//! - [`streaming`] — SSE parser with `delta.tool_calls[]` argument-shard
//!   assembly and defensive multi-key reasoning accumulator. **Stage 2.**
//!
//! Stage 1 lands the IR-translation surface; Stage 2 adds `streaming`;
//! Stage 3 wires the `LlmProvider` impl with `OpenAICompatProvider`,
//! preset constructors, `probe_models`, and `stream_turn`.

pub mod conv;
pub mod sanitize;
pub mod tool_schema;
pub mod wire;
