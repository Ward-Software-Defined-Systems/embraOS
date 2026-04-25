use serde::{Deserialize, Serialize};

/// Legacy on-disk message shape — `SessionHistory` wraps `Vec<Message>`.
/// Used by sessions persistence and the gRPC conversation save path.
/// In-flight conversation now flows through `crate::provider::ir::ApiMessage`
/// (neutral) and `provider/anthropic/wire.rs::AnthropicWireMessage` (wire).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}
