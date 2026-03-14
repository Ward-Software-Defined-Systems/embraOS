mod phases;
mod soul;

pub use phases::*;
pub use soul::*;

use serde::{Deserialize, Serialize};

use crate::brain::Message;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LearningPhase {
    UserConfiguration,
    IdentityFormation,
    SoulDefinition,
    InitialToolset,
    Confirmation,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningState {
    pub phase: LearningPhase,
    pub user_profile: Option<serde_json::Value>,
    pub identity: Option<serde_json::Value>,
    pub soul: Option<serde_json::Value>,
    pub tools_config: Option<serde_json::Value>,
    pub conversation_history: Vec<Message>,
}

impl LearningState {
    pub fn new() -> Self {
        Self {
            phase: LearningPhase::UserConfiguration,
            user_profile: None,
            identity: None,
            soul: None,
            tools_config: None,
            conversation_history: Vec::new(),
        }
    }
}
