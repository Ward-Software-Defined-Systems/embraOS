//! Console-local state types for the TUI.
//!
//! These mirror Phase 0's AppState/AppMode but without backend dependencies.
//! All data comes from gRPC ConsoleEvents.

use chrono::{DateTime, Utc};
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

/// How a tool call renders in the transcript. The web console's Tools
/// tab keeps every call, so its default is one marker line per call; the
/// serial console has no tab and keeps the full rows. Console-local, set
/// with `/show-tools`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowTools {
    /// The whole call: `[ok name] {input} result`, every line.
    Full,
    /// One dim line: `⚙ #seq  HH:MM:SS  name  0.8s`.
    Marker,
    /// Nothing: the responses alone.
    Off,
}

impl ShowTools {
    /// The default per transport: the web PTY has the Tools tab beside it.
    pub fn default_for(web_pty: bool) -> Self {
        if web_pty { ShowTools::Marker } else { ShowTools::Full }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Some(ShowTools::Full),
            "marker" => Some(ShowTools::Marker),
            "off" => Some(ShowTools::Off),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ShowTools::Full => "full",
            ShowTools::Marker => "marker",
            ShowTools::Off => "off",
        }
    }

    /// What the mode shows, for the command's answer.
    pub fn describe(self) -> &'static str {
        match self {
            ShowTools::Full => "the whole call under each row",
            ShowTools::Marker => "one line per call",
            ShowTools::Off => "the responses alone",
        }
    }
}

/// The marker-row data of a tool row: the call number, when it started
/// (the operator's clock, with seconds), how long it ran.
#[derive(Debug, Clone)]
pub struct ToolRow {
    pub seq: u64,
    pub name: String,
    pub is_error: bool,
    pub at: String,
    pub elapsed: Option<std::time::Duration>,
}

impl ToolRow {
    /// `⚙ #17  14:03:22  remember  0.8s`; `!` and a trailing `ERR` on an
    /// error; no `#` without a call number; no duration without one.
    pub fn marker_line(&self) -> String {
        let mut s = String::from(if self.is_error { "! " } else { "⚙ " });
        if self.seq > 0 {
            s.push_str(&format!("#{}  ", self.seq));
        }
        s.push_str(&self.at);
        s.push_str("  ");
        s.push_str(&self.name);
        if let Some(elapsed) = self.elapsed {
            s.push_str("  ");
            s.push_str(&format_elapsed(elapsed));
        }
        if self.is_error {
            s.push_str("  ERR");
        }
        s
    }
}

/// `0.8s` under ten seconds, `12s` under a minute, `2m 05s` beyond.
fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else if secs < 60.0 {
        format!("{}s", secs.round() as u64)
    } else {
        let whole = d.as_secs();
        format!("{}m {:02}s", whole / 60, whole % 60)
    }
}

/// The clock, with seconds, in the operator's zone.
fn clock_in_tz(at: DateTime<Utc>, tz_str: &str) -> String {
    match tz_str.parse::<Tz>() {
        Ok(tz) => at.with_timezone(&tz).format("%H:%M:%S").to_string(),
        Err(_) => at.format("%H:%M:%S").to_string(),
    }
}

/// A message displayed in the conversation area
#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: String,
    pub content: String,
    pub timestamp: String,
    /// The marker data of a tool row; `None` on every other row.
    pub tool: Option<ToolRow>,
}

impl DisplayMessage {
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
            tool: None,
        }
    }

    /// A replayed turn of a session's history: its speaker's row, marked
    /// `history` where a live row shows its time (history keeps none).
    pub fn history(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            timestamp: "history".to_string(),
            tool: None,
        }
    }

    /// The marker-row data: the call number, its start (the elapsed time
    /// back from now, or now itself without one) and its duration.
    pub fn stamped(
        mut self,
        seq: u64,
        started_at: DateTime<Utc>,
        elapsed: Option<std::time::Duration>,
        tz_str: &str,
    ) -> Self {
        if let Some(row) = self.tool.as_mut() {
            row.seq = seq;
            row.at = clock_in_tz(started_at, tz_str);
            row.elapsed = elapsed;
        }
        self
    }

    pub fn system_with_tz(content: impl Into<String>, tz_str: &str) -> Self {
        Self::new_with_tz("system", content, tz_str)
    }

    /// Media card: the text row every surface gets for a `MediaRef`
    /// (the pixel pane is additive on top of it). Timeline row:
    /// `[image generated] poster.png (1024×1024, 812 KB) — /embra/workspace/MEDIA/gen-….png — web: /api/media/gen-…`
    pub fn media_with_tz(m: &embra_common::proto::brain::MediaRef, tz_str: &str) -> Self {
        let replay = if m.replay { " (history)" } else { "" };
        Self::new_with_tz(
            "system",
            format!(
                "[image {}]{} {} ({}×{}, {} KB) — {} — web: /api/media/{}",
                m.origin,
                replay,
                m.name,
                m.width,
                m.height,
                m.byte_size / 1024,
                m.path,
                m.id
            ),
            tz_str,
        )
    }

    /// File card: the text rows every surface gets for a `FileRef`; no
    /// pane and nothing to fetch. The download path sits on a row of its
    /// own, so the web terminal's link provider finds it whole (ratatui
    /// never marks a wrapped row, and a path split across two rows would
    /// not match). Timeline rows:
    /// `[file attached] notes.md (12 KB) — /embra/workspace/uploads/notes.md`
    /// `web: /api/files/uploads/notes.md`
    pub fn file_with_tz(f: &embra_common::proto::brain::FileRef, tz_str: &str) -> Self {
        let replay = if f.replay { " (history)" } else { "" };
        let caption = if f.caption.is_empty() {
            String::new()
        } else {
            format!(" — {}", f.caption)
        };
        let web = embra_common::file_download_route(&f.path)
            .map(|route| format!("\nweb: {route}"))
            .unwrap_or_default();
        Self::new_with_tz(
            "system",
            format!(
                "[file {}]{} {} ({} KB){} — {}{}",
                f.origin,
                replay,
                f.name,
                f.byte_size / 1024,
                caption,
                f.path,
                web
            ),
            tz_str,
        )
    }

    /// The announcement of a live offer to the web terminal: a private OSC
    /// (`ESC ] 8771 ; /api/files/<path> BEL`) that never reaches the
    /// screen; embra-web's xterm handler starts the download from it. Only
    /// the web PTY gets it, and only once per offer (never on a repaint).
    pub fn download_osc(route: &str) -> String {
        format!("\x1b]{OSC_DOWNLOAD};{route}\x07")
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
        let mut row = Self::new_with_tz(
            "tool",
            format!("[{} {}]{} {}", marker, name, input_summary, result),
            tz_str,
        );
        // The marker data; `stamped` fills in the call number, the start
        // and the duration when the event handler has them.
        row.tool = Some(ToolRow {
            seq: 0,
            name: name.to_string(),
            is_error,
            at: clock_in_tz(Utc::now(), tz_str),
            elapsed: None,
        });
        row
    }
}

/// Full application state for the TUI
pub struct AppState {
    pub mode: AppMode,
    /// Media wave: the in-TUI image pane (ratatui-image). `media` is
    /// the last fetched image; `media_visible` is the operator toggle
    /// (`/media` / `/media off`); `last_media_id` remembers the newest
    /// id seen (incl. history replays) so `/media` can fetch on demand.
    pub media: Option<super::graphics::MediaPane>,
    pub media_visible: bool,
    pub last_media_id: Option<String>,
    /// In-flight fetch/decode for the pane (id) — prevents a pile-up of
    /// decodes when several MediaRef frames arrive in one turn.
    pub media_loading: Option<String>,
    /// Set by the event handler (non-replay MediaRef) or `/media <id>`;
    /// the main loop picks it up and spawns the fetch+decode task (the
    /// handler has no client access by design — same seam as
    /// `stop_requested`).
    pub pending_media_fetch: Option<embra_common::proto::brain::MediaRef>,
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
    /// The tool the brain just cleared and how long it ran: the brain
    /// clears `current_tool` before it sends the call's frame, and the
    /// frame's marker row takes the duration from here (`ToolExecution`
    /// arm). Consumed by that frame; a stash for another tool is dropped.
    pub last_tool_run: Option<(String, std::time::Duration)>,
    /// How tool rows render (`/show-tools`); set per transport at start.
    pub show_tools: ShowTools,
    pub status_message: String,
    pub should_quit: bool,
    pub selector: Option<Selector>,
    pub setup_default: Option<String>,
    /// The pending setup prompt asks for a secret (an API key, a bearer):
    /// the input box draws a mask and the echoed row names the length
    /// alone, until the answer is sent; a slash command typed meanwhile
    /// leaves it armed, as the brain keeps its key step through one (#20).
    pub secret_prompt: bool,
    pub config_name: String,
    pub config_version: String,
    pub config_tz: String,
    /// Active LLM model display name (e.g. `"opus-5"`,
    /// `"gemini-3.1-pro"`). Updated from the `Brain: …` token in
    /// ModeTransition messages; defaults to opus-5 for first-paint
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

/// The most rows the transcript keeps. Every frame restyles and rewraps
/// the whole history, so an unbounded history is a per-frame cost that
/// grows for the life of a web console (#40); the rows past the cap are
/// the oldest, which no terminal could scroll back to.
pub const MESSAGE_HISTORY_CAP: usize = 2000;

impl AppState {
    /// Drops the oldest rows past [`MESSAGE_HISTORY_CAP`]; the scroll
    /// offset counts from the bottom, so the view does not move.
    pub fn trim_history(&mut self) {
        if self.messages.len() > MESSAGE_HISTORY_CAP {
            let excess = self.messages.len() - MESSAGE_HISTORY_CAP;
            self.messages.drain(..excess);
        }
    }

    pub fn new() -> Self {
        Self {
            // Start in Learning mode — brain sends ModeTransition with correct mode
            mode: AppMode::Learning,
            media: None,
            media_visible: false,
            last_media_id: None,
            media_loading: None,
            pending_media_fetch: None,
            messages: Vec::new(),
            input_buffer: String::new(),
            cursor_pos: 0,
            scroll_offset: 0,
            streaming_text: None,
            thinking: false,
            thinking_name: "Embra".to_string(),
            current_tool: None,
            current_tool_started: None,
            last_tool_run: None,
            show_tools: ShowTools::Full,
            status_message: String::new(),
            should_quit: false,
            selector: None,
            setup_default: None,
            secret_prompt: false,
            config_name: "embraOS".to_string(),
            config_version: env!("CARGO_PKG_VERSION").to_string(),
            config_tz: "UTC".to_string(),
            provider_model: "opus-5".to_string(),
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

    /// Media band shows when there is an image, the operator hasn't
    /// hidden it, and the terminal is tall enough to keep the
    /// conversation usable (mirror of `expression_panel_visible`).
    pub fn media_pane_visible(&self) -> bool {
        self.media_visible
            && self.media.is_some()
            && self.viewport_rows >= super::graphics::MEDIA_PANE_MIN_ROWS
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

    /// The text the input box draws. For a `secret` prompt every character
    /// of the buffer is a mask character (a line break stays one), unless
    /// the line is a slash command, the way out of a setup step; a
    /// credential typed as the argument of one of the secret command forms
    /// is masked by itself. The character count is the buffer's, so the
    /// rows and the cursor the layout derives stay right (#20).
    pub fn input_shown(&self) -> String {
        if self.secret_prompt && !self.input_buffer.starts_with('/') {
            self.input_buffer
                .chars()
                .map(|c| if c == '\n' { c } else { embra_tools_core::MASK_CHAR })
                .collect()
        } else {
            embra_tools_core::mask_secret_argument_keeping_length(&self.input_buffer)
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

/// The row the transcript shows for a secret the operator sent: its
/// length and nothing of its value (#20).
pub fn secret_row_text(input: &str) -> String {
    format!("[secret: {} characters]", input.chars().count())
}

#[cfg(test)]
mod show_tools_tests {
    use super::*;
    use chrono::TimeZone;
    use std::time::Duration;

    #[test]
    fn a_marker_row_names_the_call_its_time_and_its_duration() {
        let at = Utc.with_ymd_and_hms(2026, 10, 15, 14, 3, 22).unwrap();
        let m = DisplayMessage::tool_native("remember", "{}", "saved", false, "UTC")
            .stamped(17, at, Some(Duration::from_millis(812)), "UTC");
        assert_eq!(m.tool.as_ref().unwrap().marker_line(), "⚙ #17  14:03:22  remember  0.8s");
        // The content string is what it always was.
        assert!(m.content.starts_with("[ok remember]"));

        let e = DisplayMessage::tool_native("git_push", "{}", "fatal", true, "UTC")
            .stamped(17, at, Some(Duration::from_millis(812)), "UTC");
        assert_eq!(e.tool.as_ref().unwrap().marker_line(), "! #17  14:03:22  git_push  0.8s  ERR");

        let unnumbered = DisplayMessage::tool_native("recall", "{}", "", false, "UTC")
            .stamped(0, at, None, "UTC");
        assert_eq!(unnumbered.tool.as_ref().unwrap().marker_line(), "⚙ 14:03:22  recall");

        let zoned = DisplayMessage::tool_native("recall", "{}", "", false, "America/New_York")
            .stamped(1, at, Some(Duration::from_secs(125)), "America/New_York");
        assert_eq!(zoned.tool.as_ref().unwrap().at, "10:03:22");
        assert_eq!(zoned.tool.as_ref().unwrap().marker_line(), "⚙ #1  10:03:22  recall  2m 05s");
    }

    #[test]
    fn the_serial_default_is_full_and_the_web_default_is_marker() {
        assert_eq!(ShowTools::default_for(false), ShowTools::Full);
        assert_eq!(ShowTools::default_for(true), ShowTools::Marker);
        assert_eq!(AppState::new().show_tools, ShowTools::Full);
    }

    #[test]
    fn show_tools_parses_its_three_modes_and_rejects_the_rest() {
        assert_eq!(ShowTools::parse("full"), Some(ShowTools::Full));
        assert_eq!(ShowTools::parse(" Marker "), Some(ShowTools::Marker));
        assert_eq!(ShowTools::parse("OFF"), Some(ShowTools::Off));
        assert_eq!(ShowTools::parse("on"), None);
        assert_eq!(ShowTools::parse(""), None);
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

/// The OSC number the web terminal listens on for an offered file's route.
/// Private: no terminal or addon in the tree claims it.
pub const OSC_DOWNLOAD: u32 = 8771;

/// Write a live offer's announcement to the PTY, once. Gated on the web
/// PTY by the caller; a serial terminal would ignore the sequence anyway.
pub fn announce_download(path: &str) {
    use std::io::Write;
    if let Some(route) = embra_common::file_download_route(path) {
        let mut out = std::io::stdout();
        let _ = out.write_all(DisplayMessage::download_osc(&route).as_bytes());
        let _ = out.flush();
    }
}

#[cfg(test)]
mod file_card_tests {
    use super::DisplayMessage;
    use embra_common::proto::brain::FileRef;

    fn a_file(origin: &str, replay: bool, caption: &str) -> FileRef {
        FileRef {
            path: "/embra/workspace/uploads/notes.md".into(),
            name: "notes.md".into(),
            byte_size: 12 * 1024 + 300,
            media_type: "text/markdown".into(),
            origin: origin.into(),
            caption: caption.into(),
            tool_use_id: String::new(),
            replay,
        }
    }

    #[test]
    fn a_file_frame_renders_its_origin_size_and_path_and_its_web_path_on_its_own_row() {
        let row = DisplayMessage::file_with_tz(&a_file("attached", false, ""), "UTC");
        assert_eq!(row.role, "system");
        assert_eq!(
            row.content,
            "[file attached] notes.md (12 KB) — /embra/workspace/uploads/notes.md\nweb: /api/files/uploads/notes.md"
        );
        let lines: Vec<&str> = row.content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].starts_with("web: /api/files/"));
    }

    #[test]
    fn a_replayed_offer_says_history_and_carries_its_note() {
        let row = DisplayMessage::file_with_tz(&a_file("offered", true, "the report"), "UTC");
        assert_eq!(
            row.content,
            "[file offered] (history) notes.md (12 KB) — the report — /embra/workspace/uploads/notes.md\nweb: /api/files/uploads/notes.md"
        );
    }

    #[test]
    fn a_live_offer_is_announced_with_the_private_osc() {
        assert_eq!(super::OSC_DOWNLOAD, 8771);
        assert_eq!(
            DisplayMessage::download_osc("/api/files/uploads/notes.md"),
            "\x1b]8771;/api/files/uploads/notes.md\x07"
        );
    }

    #[test]
    fn a_file_outside_the_workspace_gets_no_web_row() {
        let mut f = a_file("offered", false, "");
        f.path = "/etc/motd".into();
        let row = DisplayMessage::file_with_tz(&f, "UTC");
        assert_eq!(row.content, "[file offered] notes.md (12 KB) — /etc/motd");
    }
}

#[cfg(test)]
mod history_cap_tests {
    use super::{AppState, DisplayMessage, MESSAGE_HISTORY_CAP};

    #[test]
    fn a_history_past_the_cap_drops_its_oldest_rows() {
        let mut app = AppState::new();
        for i in 0..(MESSAGE_HISTORY_CAP + 7) {
            app.messages.push(DisplayMessage::system_with_tz(format!("m{i}"), "UTC"));
        }
        app.trim_history();
        assert_eq!(app.messages.len(), MESSAGE_HISTORY_CAP);
        app.trim_history();
        assert_eq!(app.messages.len(), MESSAGE_HISTORY_CAP, "a second pass changes nothing");
    }
}
