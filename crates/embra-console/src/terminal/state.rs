//! Console-local state types for the TUI.
//!
//! These mirror Phase 0's AppState/AppMode but without backend dependencies.
//! All data comes from gRPC ConsoleEvents.

use chrono::{Local, Utc};

/// Operating mode of the console
#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Setup(SetupState),
    Learning,
    Operational { session_name: String },
}

/// Config wizard step tracking
#[derive(Debug, Clone, PartialEq)]
pub struct SetupState {
    pub step: SetupStep,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetupStep {
    Name,
    ApiKey,
    Timezone,
    Confirm,
}

/// Arrow-key selector for options
#[derive(Debug, Clone)]
pub struct Selector {
    pub options: Vec<String>,
    pub selected: usize,
}

impl Selector {
    pub fn new(options: Vec<String>) -> Self {
        Self { options, selected: 0 }
    }

    pub fn up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn down(&mut self) {
        if self.selected + 1 < self.options.len() {
            self.selected += 1;
        }
    }

    pub fn current(&self) -> &str {
        &self.options[self.selected]
    }
}

/// A message displayed in the conversation area
#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: String,
    pub content: String,
    pub timestamp: String,
}

impl DisplayMessage {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            timestamp: Local::now().format("%b %d %H:%M").to_string(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new("system", content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new("user", content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new("assistant", content)
    }

    pub fn tool(name: &str, result: &str) -> Self {
        Self::new("tool", format!("[{}] {}", name, result))
    }
}

/// Full application state for the TUI
pub struct AppState {
    pub mode: AppMode,
    pub messages: Vec<DisplayMessage>,
    pub input_buffer: String,
    pub cursor_pos: usize,
    pub scroll_offset: u16,
    pub streaming_text: Option<String>,
    pub thinking: bool,
    pub thinking_name: String,
    pub status_message: String,
    pub should_quit: bool,
    pub selector: Option<Selector>,
    pub setup_default: Option<String>,
    pub config_name: String,
    pub config_version: String,
    pub pasted_lines: Option<Vec<String>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            // Start in Learning mode — brain sends ModeTransition with correct mode
            mode: AppMode::Learning,
            messages: Vec::new(),
            input_buffer: String::new(),
            cursor_pos: 0,
            scroll_offset: 0,
            streaming_text: None,
            thinking: false,
            thinking_name: "Embra".to_string(),
            status_message: String::new(),
            should_quit: false,
            selector: None,
            setup_default: None,
            config_name: "embraOS".to_string(),
            config_version: env!("CARGO_PKG_VERSION").to_string(),
            pasted_lines: None,
        }
    }

    /// Infer setup step from a SetupPrompt prompt string
    pub fn infer_setup_step(prompt: &str) -> SetupStep {
        let lower = prompt.to_lowercase();
        if lower.contains("name") && !lower.contains("api") {
            SetupStep::Name
        } else if lower.contains("api key") || lower.contains("anthropic") {
            SetupStep::ApiKey
        } else if lower.contains("timezone") {
            SetupStep::Timezone
        } else if lower.contains("confirm") || lower.contains("summary") {
            SetupStep::Confirm
        } else {
            SetupStep::Name
        }
    }

    pub fn input_placeholder(&self) -> &str {
        match &self.mode {
            AppMode::Setup(s) => match s.step {
                SetupStep::Name => "Enter a name (or press Enter for default)...",
                SetupStep::ApiKey => "Enter your Anthropic API key...",
                SetupStep::Timezone => "Enter timezone (or press Enter for default)...",
                SetupStep::Confirm => "Enter 1 to confirm or 2 to restart...",
            },
            AppMode::Learning => "Talk to your intelligence...",
            AppMode::Operational { .. } => "Type a message...",
        }
    }
}
