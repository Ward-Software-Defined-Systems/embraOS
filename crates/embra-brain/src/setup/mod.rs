//! OpenAI-compat wizard sub-flow helpers (Sprint 5).
//!
//! The first-run wizard runner remains in
//! [`crate::config::run_config_wizard_grpc`]; this module hosts the
//! preset-specific Endpoint → Bearer → Probe-and-Select sub-flow that
//! Ollama and LM Studio dispatch into. Anthropic and Gemini paths are
//! unchanged from Sprint 4.
//!
//! Public surface:
//! - [`wizard::run_openai_compat_subflow`] — drives the three-step
//!   sub-flow over the wizard's gRPC streams.
//! - [`wizard::normalize_endpoint`] — canonical URL form for the
//!   Endpoint step (trim trailing slash, default scheme, default port).
//! - [`wizard::OpenAiCompatSubflow`] — return value carrying endpoint
//!   (normalized), bearer (`None` if empty), and model id.

pub mod wizard;
