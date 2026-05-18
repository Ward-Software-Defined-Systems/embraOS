//! embra-guardian-v1 — intelligence-authored dynamic WASM tools.
//!
//! The intelligence pastes a Rust module; this crate **validates** it
//! statically (`validator`), **scaffolds + builds** it to `wasm32` with the
//! in-OS toolchain (`scaffold`/`build`), and **runs** the compiled artifact
//! in an embedded `wasmtime` sandbox (`host`). A persisted manifest
//! (`store`) + a process-global overlay (`overlay`) make tools survive
//! reboots and reachable through the `guardian_call` meta-tool in
//! `embra-brain`.
//!
//! This crate deliberately does NOT depend on `embra-brain` (avoids a
//! dependency cycle, mirrors the `embra-tools-core` dependency-light rule).
//! It depends only on `embra-tools-core` for the shared dispatch error type.
//!
//! Build order is gated on R1: `host` must compile + link for
//! `x86_64-unknown-linux-musl` (static) before the rest is fleshed out.

pub mod abi;
pub mod build;
pub mod caps;
pub mod error;
pub mod host;
pub mod overlay;
pub mod scaffold;
pub mod store;
pub mod validator;

pub use caps::{
    Capabilities, EgressPolicy, HttpTransport, SearchProvider, SearchRequest, SearchResponse,
    SearchResult,
};
pub use error::GuardianError;
pub use overlay::{runtime, CompiledTool, GuardianRuntime};
pub use scaffold::{assemble_lib_rs, scaffold, ScaffoldPaths};
pub use store::{GuardianPersistence, ToolDoc, ToolStatus};
pub use validator::{validate, ValidatedModule, ValidationError};
