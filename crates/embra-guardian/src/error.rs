//! Error type shared across guardian stages. Maps cleanly onto
//! `embra_tools_core::DispatchError::Handler` at the `embra-brain`
//! boundary so a guardian failure surfaces to the model as an
//! `is_error: true` tool result (never a brain panic).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GuardianError {
    #[error("wasm compile failed: {0}")]
    Compile(String),

    #[error("wasm instantiate failed: {0}")]
    Instantiate(String),

    #[error("guest is missing required ABI export `{0}`")]
    AbiMissing(&'static str),

    #[error("guest trapped: {0}")]
    Trap(String),

    #[error("guest exceeded the {0:?} execution deadline")]
    Timeout(std::time::Duration),

    #[error("guest exceeded the memory cap")]
    Oom,

    #[error("guest output {got} bytes exceeds cap {cap} bytes")]
    OutputTooLarge { got: usize, cap: usize },

    #[error("guest output is not valid UTF-8")]
    NonUtf8,

    #[error("guest memory access out of bounds: {0}")]
    MemoryAccess(String),

    #[error("path denied: {0}")]
    PathDenied(String),

    #[error("scaffold io error: {0}")]
    Io(String),

    #[error("build failed: {0}")]
    BuildFailed(String),

    #[error("validation failed: {0}")]
    Validation(String),

    #[error("persistence error: {0}")]
    Persistence(String),
}

impl GuardianError {
    /// Flatten to the string `embra-brain` wraps in
    /// `DispatchError::Handler` (kept short; the 2 MiB registry cap also
    /// applies downstream).
    pub fn into_handler_message(self, tool: &str) -> String {
        format!("guardian tool '{tool}' failed: {self}")
    }
}
