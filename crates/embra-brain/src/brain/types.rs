use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Token(String),
    Done(String), // full accumulated text
    Error(String),
}

#[derive(Debug, Deserialize)]
pub struct ApiResponse {
    #[serde(default)]
    pub id: Option<String>,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
pub struct ContentBlock {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(default)]
    pub text: String,
}
