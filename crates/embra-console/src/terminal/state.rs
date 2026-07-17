//! Console-local state types for the TUI.
//!
//! These mirror Phase 0's AppState/AppMode but without backend dependencies.
//! All data comes from gRPC ConsoleEvents.

use chrono::Utc;
use chrono_tz::Tz;

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
    /// Sprint 4 → Sprint 5: provider selector step (Anthropic / Gemini /
    /// Ollama / LM Studio).
    Provider,
    ApiKey,
    /// Sprint 5: OpenAI-compat sub-flow — endpoint URL prompt.
    Endpoint,
    /// Sprint 5: OpenAI-compat sub-flow — bearer token prompt.
    BearerToken,
    /// Sprint 5: OpenAI-compat sub-flow — model selector populated
    /// from the probe.
    ModelSelect,
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
            timestamp: Utc::now().format("%b %d %H:%M").to_string(),
        }
    }

    pub fn new_with_tz(role: impl Into<String>, content: impl Into<String>, tz_str: &str) -> Self {
        let ts = if let Ok(tz) = tz_str.parse::<Tz>() {
            Utc::now().with_timezone(&tz).format("%b %d %H:%M").to_string()
        } else {
            Utc::now().format("%b %d %H:%M UTC").to_string()
        };
        Self {
            role: role.into(),
            content: content.into(),
            timestamp: ts,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new("system", content)
    }

    pub fn system_with_tz(content: impl Into<String>, tz_str: &str) -> Self {
        Self::new_with_tz("system", content, tz_str)
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

    pub fn tool_with_tz(name: &str, result: &str, tz_str: &str) -> Self {
        Self::new_with_tz("tool", format!("[{}] {}", name, result), tz_str)
    }

    /// Native-tool-use render (NATIVE-TOOLS-01 Stage 7). Includes the
    /// typed input JSON inline when non-empty and flags errors with an
    /// explicit "ERR" marker. Timeline row:
    /// `[ok git_status] {"path":"/tmp"} On branch main...`
    pub fn tool_native(name: &str, input_json: &str, result: &str, is_error: bool, tz_str: &str) -> Self {
        let marker = if is_error { "ERR" } else { "ok" };
        let input_summary = if input_json.is_empty() || input_json == "{}" {
            String::new()
        } else {
            format!(" {}", input_json)
        };
        Self::new_with_tz(
            "tool",
            format!("[{} {}]{} {}", marker, name, input_summary, result),
            tz_str,
        )
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
    /// Name of the tool currently mid-`tools::registry::dispatch`, surfaced
    /// by the brain on the existing Thinking signal so the operator-facing
    /// indicator can show "<name> is running <tool> (Ns)..." while a tool
    /// is in flight. `None` outside dispatch.
    pub current_tool: Option<String>,
    /// Wall-clock instant when `current_tool` last changed, used to render
    /// elapsed seconds next to the tool name. Reset only on a *changed*
    /// tool — duplicate signals for the same tool don't restart the clock.
    pub current_tool_started: Option<std::time::Instant>,
    pub status_message: String,
    pub should_quit: bool,
    pub selector: Option<Selector>,
    pub setup_default: Option<String>,
    pub config_name: String,
    pub config_version: String,
    pub config_tz: String,
    /// Active LLM model display name (e.g. `"opus-4.8"`,
    /// `"gemini-3.1-pro"`). Updated from the `Brain: …` token in
    /// ModeTransition messages; defaults to opus-4.8 for first-paint
    /// before any transition arrives.
    pub provider_model: String,
    pub pasted_lines: Option<Vec<String>>,
    /// Set by the /stop intercept and Esc-while-busy; consumed by the main
    /// loop OUTSIDE the select! (which fires the out-of-band StopTurn
    /// unary — the key handler has no client access by design).
    pub stop_requested: bool,
    pub multiline_mode: bool,
    /// embra-guardian-v1: when set (via `/guardian-define`), the next
    /// submitted multi-line/pasted buffer is delivered to the brain as
    /// `SlashCommand{"/guardian","define\n<module>"}` instead of a
    /// UserMessage. Reuses the multiline/paste accumulation; the serial
    /// path stays byte-identical when this is false.
    pub guardian_capture: bool,
    // EXPR-01 expression panel — cached state polled from brain every 3s
    pub expression_content: String,
    pub expression_version: u64,
    /// Live reasoning/CoT shards from the most recent turn. When
    /// non-empty the expression panel renders this windowed tail
    /// (italic dark-gray) instead of `expression_content`. Persists
    /// past `ResponseDone` so the operator can keep reading the last
    /// turn's reasoning between turns; cleared at the next user
    /// submit, on `SystemMessage::Error`, or on `ModeTransition`.
    /// Hard-capped at 64 KiB at receive time so a runaway reasoning
    /// stream can't grow console memory unbounded. Per the privacy
    /// contract this is NEVER persisted, NEVER replayed to the model,
    /// and the brain handler keeps it off `full_response`.
    pub live_reasoning: String,
    /// Expression-panel scroll offset in visual rows from the BOTTOM
    /// (0 = tail pinned, mirroring the conversation pane's
    /// `scroll_offset` semantics). Applies to whichever source the
    /// panel is showing (live reasoning or the `express` singleton).
    /// Mutated by Shift+Up/Down/PageUp/PageDown; reset to 0 whenever
    /// `live_reasoning` clears (user submit / error / mode transition)
    /// or a poll swaps in new `expression_content`, so the operator is
    /// never left scrolled into stale content.
    pub expression_scroll: u16,
    // Viewport dimensions (detected once at boot) — drive the panel size gate
    pub viewport_cols: u16,
    pub viewport_rows: u16,
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
            current_tool: None,
            current_tool_started: None,
            status_message: String::new(),
            should_quit: false,
            selector: None,
            setup_default: None,
            config_name: "embraOS".to_string(),
            config_version: env!("CARGO_PKG_VERSION").to_string(),
            config_tz: "UTC".to_string(),
            provider_model: "opus-4.8".to_string(),
            pasted_lines: None,
            stop_requested: false,
            multiline_mode: false,
            guardian_capture: false,
            expression_content: String::new(),
            expression_version: 0,
            live_reasoning: String::new(),
            expression_scroll: 0,
            viewport_cols: 80,
            viewport_rows: 24,
        }
    }

    /// Clear the live-reasoning buffer AND snap the expression-panel
    /// scroll back to the tail. The two must travel together: every
    /// clear also switches the panel's source (back to the `express`
    /// singleton), and a stale scroll offset would leave the operator
    /// pinned into content that no longer exists. Call this instead of
    /// touching `live_reasoning` directly at the clear sites.
    pub fn clear_live_reasoning(&mut self) {
        self.live_reasoning.clear();
        self.expression_scroll = 0;
    }

    /// EXPR-01 panel size gate — single source of truth shared by the
    /// renderer (whether to draw the band) and the key handler (whether
    /// Shift-scroll chords should mutate `expression_scroll`).
    pub fn expression_panel_visible(&self) -> bool {
        self.viewport_cols >= 80 && self.viewport_rows >= 20
    }

    /// Infer setup step from a SetupPrompt prompt string. Order
    /// matters — provider check runs before api-key because the
    /// "Anthropic"/"Gemini" tokens appear in both prompts; provider
    /// is identified by the explicit "provider" keyword. OpenAI-compat
    /// sub-flow steps (Endpoint / BearerToken / ModelSelect) check
    /// before the generic ApiKey/Anthropic/Gemini fallback.
    pub fn infer_setup_step(prompt: &str) -> SetupStep {
        let lower = prompt.to_lowercase();
        if lower.contains("provider")
            || (lower.contains("which") && (lower.contains("gemini") || lower.contains("claude")))
        {
            SetupStep::Provider
        } else if lower.contains("endpoint") || lower.contains("base url") {
            SetupStep::Endpoint
        } else if lower.contains("bearer") || lower.contains("auth") {
            SetupStep::BearerToken
        } else if lower.contains("select a model") || lower.contains("which model") {
            SetupStep::ModelSelect
        } else if lower.contains("name") && !lower.contains("api") {
            SetupStep::Name
        } else if lower.contains("api key") || lower.contains("anthropic") || lower.contains("gemini") {
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
                SetupStep::Provider => "↑/↓ to choose, Enter to confirm",
                SetupStep::ApiKey => "Enter your API key...",
                SetupStep::Endpoint => "Enter the endpoint URL (or press Enter for default)...",
                SetupStep::BearerToken => "Enter bearer token (or press Enter for no auth)...",
                SetupStep::ModelSelect => "↑/↓ to choose model, Enter to confirm",
                SetupStep::Timezone => "Enter timezone (or press Enter for default)...",
                SetupStep::Confirm => "Enter 1 to confirm or 2 to restart...",
            },
            AppMode::Learning => "Talk to your intelligence...",
            AppMode::Operational { .. } => "Type a message...",
        }
    }
}

#[cfg(test)]
mod native_render_tests {
    use super::*;

    #[test]
    fn tool_native_ok_no_args() {
        let m = DisplayMessage::tool_native("system_status", "{}", "version 0.2.0", false, "UTC");
        assert!(m.content.contains("[ok system_status]"));
        assert!(!m.content.contains("[TOOL:"));
        assert!(m.content.contains("version 0.2.0"));
    }

    #[test]
    fn tool_native_ok_with_args_inlines_json() {
        let m = DisplayMessage::tool_native(
            "git_status",
            r#"{"path":"/embra/workspace"}"#,
            "clean",
            false,
            "UTC",
        );
        assert!(m.content.contains("[ok git_status]"));
        assert!(m.content.contains(r#"{"path":"/embra/workspace"}"#));
        assert!(!m.content.contains("[TOOL:"));
    }

    #[test]
    fn tool_native_error_marker() {
        let m = DisplayMessage::tool_native(
            "git_push",
            r#"{"path":"/nope"}"#,
            "fatal: not a git repository",
            true,
            "UTC",
        );
        assert!(m.content.contains("[ERR git_push]"));
    }

    #[test]
    fn infer_setup_step_recognizes_provider_prompt() {
        assert_eq!(
            AppState::infer_setup_step("Which AI provider would you like to use?"),
            SetupStep::Provider
        );
        assert_eq!(
            AppState::infer_setup_step("Select your provider"),
            SetupStep::Provider
        );
        // Variant: prompt with model names but no explicit "provider" word.
        assert_eq!(
            AppState::infer_setup_step("Which would you like — Claude or Gemini?"),
            SetupStep::Provider
        );
    }

    #[test]
    fn infer_setup_step_distinguishes_provider_from_api_key() {
        assert_eq!(
            AppState::infer_setup_step("Enter your Anthropic API key:"),
            SetupStep::ApiKey
        );
        assert_eq!(
            AppState::infer_setup_step("Enter your Gemini API key:"),
            SetupStep::ApiKey
        );
        assert_eq!(
            AppState::infer_setup_step("What would you like to name your intelligence?"),
            SetupStep::Name
        );
        assert_eq!(
            AppState::infer_setup_step("What timezone are you in?"),
            SetupStep::Timezone
        );
        assert_eq!(
            AppState::infer_setup_step("Configuration summary: …. Confirm?"),
            SetupStep::Confirm
        );
    }

    #[test]
    fn infer_setup_step_recognizes_openai_compat_subflow_prompts() {
        // Sprint 5: Endpoint / BearerToken / ModelSelect new variants.
        assert_eq!(
            AppState::infer_setup_step("Enter the Ollama endpoint URL (default: http://localhost:11434):"),
            SetupStep::Endpoint
        );
        assert_eq!(
            AppState::infer_setup_step("Enter base URL"),
            SetupStep::Endpoint
        );
        assert_eq!(
            AppState::infer_setup_step("Enter your LM Studio bearer token (optional, leave empty for no auth):"),
            SetupStep::BearerToken
        );
        assert_eq!(
            AppState::infer_setup_step("Configure auth"),
            SetupStep::BearerToken
        );
        assert_eq!(
            AppState::infer_setup_step("Select a model:"),
            SetupStep::ModelSelect
        );
        assert_eq!(
            AppState::infer_setup_step("Which model do you want?"),
            SetupStep::ModelSelect
        );
    }

    #[test]
    fn tool_native_never_emits_literal_tag_syntax() {
        for name in ["system_status", "recall", "git_status"] {
            for input in [r#"{}"#, r#"{"query":"alerts"}"#] {
                for result in ["some result", r#"contains [TOOL: text"#] {
                    let m = DisplayMessage::tool_native(name, input, result, false, "UTC");
                    // Render MUST NOT wrap the name in [TOOL:...] — that
                    // pattern belongs to the deleted legacy dispatcher.
                    assert!(
                        !m.content.starts_with("[TOOL:"),
                        "unexpected legacy prefix in render: {:?}",
                        m.content
                    );
                }
            }
        }
    }
}
