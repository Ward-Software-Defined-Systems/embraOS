mod commands;
mod input;
mod render;
mod ui;

pub use commands::*;
pub use input::*;
pub use render::*;
pub use ui::*;

use anyhow::Result;
use crossterm::{
    event::{
        self, Event, KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    },
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::brain::{Brain, Message, StreamEvent};
use crate::config::SystemConfig;
use crate::db::WardsonDbClient;
use crate::learning::{self, LearningPhase, LearningState};
use crate::proactive::Notification;
use crate::sessions::SessionManager;
use crate::tools;

// ── App Mode ──

pub enum AppMode {
    Setup(SetupState),
    Learning(LearningModeState),
    Operational { session_name: String },
}

pub struct SetupState {
    pub step: SetupStep,
    pub name: Option<String>,
    pub api_key: Option<String>,
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetupStep {
    Name,
    ApiKey,
    Timezone,
    Confirm,
}

pub struct LearningModeState {
    pub state: LearningState,
    pub awaiting_response: bool,
}

// ── Selector (arrow-key choice UI) ──

#[derive(Debug, Clone)]
pub struct Selector {
    pub options: Vec<String>,
    pub selected: usize,
}

impl Selector {
    pub fn new(options: Vec<String>) -> Self {
        Self {
            options,
            selected: 0,
        }
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

// ── App State ──

pub struct AppState {
    pub config: Option<SystemConfig>,
    pub brain: Option<Brain>,
    pub session_manager: Option<SessionManager>,
    pub db: WardsonDbClient,
    pub mode: AppMode,
    pub messages: Vec<DisplayMessage>,
    pub input_buffer: String,
    pub cursor_pos: usize,
    pub scroll_offset: u16,
    pub streaming_text: Option<String>,
    pub thinking: bool, // True while waiting for first token from Brain
    pub status_message: String,
    pub should_quit: bool,
    pub soul: Option<serde_json::Value>,
    pub identity: Option<serde_json::Value>,
    pub user_profile: Option<serde_json::Value>,
    pub selector: Option<Selector>,
    pub pasted_lines: Option<Vec<String>>, // Holds multi-line paste content
    pub pending_clipboard: Option<String>, // OSC 52 payload to write after next draw
}

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
            timestamp: chrono::Utc::now().format("%H:%M").to_string(),
        }
    }
}

// ── Entry Point ──

pub async fn run_terminal(
    db: &WardsonDbClient,
    is_first_run: bool,
    notification_rx: mpsc::Receiver<Notification>,
) -> Result<()> {
    let mut app_state = AppState {
        config: None,
        brain: None,
        session_manager: None,
        db: db.clone(),
        mode: AppMode::Setup(SetupState {
            step: SetupStep::Name,
            name: None,
            api_key: None,
            timezone: None,
        }),
        messages: Vec::new(),
        input_buffer: String::new(),
        cursor_pos: 0,
        scroll_offset: 0,
        streaming_text: None,
        thinking: false,
        status_message: "OK".into(),
        should_quit: false,
        soul: None,
        identity: None,
        user_profile: None,
        selector: None,
        pasted_lines: None,
        pending_clipboard: None,
    };

    if !is_first_run {
        // Returning run: load everything and enter operational mode
        let config = crate::config::load_config(db).await?;
        let soul = learning::load_soul(db).await?.unwrap_or_default();
        let identity_doc = load_document(db, "memory.identity").await?;
        let user_doc = load_document(db, "memory.user").await?;

        let session_context = "New session. No prior context.".to_string();
        let system_prompt = crate::brain::operational_mode(
            &config.name,
            &serde_json::to_string_pretty(&soul).unwrap_or_default(),
            &serde_json::to_string_pretty(&identity_doc).unwrap_or_default(),
            &serde_json::to_string_pretty(&user_doc).unwrap_or_default(),
            &session_context,
        );

        let mut session_manager = SessionManager::new(db.clone());
        let session_name =
            if let Some(existing) = session_manager.get_most_recent_active().await? {
                let history = session_manager.reattach(&existing.name).await?;
                for m in &history {
                    let role = if m.role == "user" { "You" } else { &m.role };
                    app_state
                        .messages
                        .push(DisplayMessage::new(role, &m.content));
                }
                let briefing = crate::brain::reconnection_briefing(
                    &config.name,
                    &existing.last_active.to_rfc3339(),
                );
                app_state
                    .messages
                    .push(DisplayMessage::new("system", briefing));
                existing.name
            } else {
                session_manager.create("main").await?;
                "main".to_string()
            };

        app_state.brain = Some(Brain::new(config.api_key.clone(), system_prompt));
        app_state.session_manager = Some(session_manager);
        app_state.soul = Some(soul);
        app_state.identity = Some(identity_doc);
        app_state.user_profile = Some(user_doc);
        app_state.config = Some(config);
        app_state.mode = AppMode::Operational { session_name };
    } else {
        // First run: show setup welcome
        let api_key_from_env = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());

        app_state.messages.push(DisplayMessage::new(
            "system",
            "╔══════════════════════════════════════════╗\n\
             ║     embraOS Phase 0 — First Run Setup    ║\n\
             ╚══════════════════════════════════════════╝",
        ));

        if api_key_from_env.is_some() {
            app_state.messages.push(DisplayMessage::new(
                "system",
                "Anthropic API key detected from environment.",
            ));
            if let AppMode::Setup(ref mut setup) = app_state.mode {
                setup.api_key = api_key_from_env;
            }
        }

        app_state.messages.push(DisplayMessage::new(
            "system",
            "What would you like to name your intelligence?",
        ));
        app_state.selector = Some(Selector::new(vec![
            "Embra (default)".into(),
            "Enter custom name".into(),
        ]));
    }

    // Enter TUI mode
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Enable keyboard enhancement for Shift+Enter detection (supported in modern terminals)
    let kb_enhanced = crossterm::execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok();
    crossterm::execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let notification_rx = Arc::new(Mutex::new(notification_rx));

    // Main event loop
    let result = run_event_loop(&mut term, &mut app_state, notification_rx).await;

    // Cleanup
    terminal::disable_raw_mode()?;
    if kb_enhanced {
        let _ = crossterm::execute!(term.backend_mut(), PopKeyboardEnhancementFlags);
    }
    crossterm::execute!(
        term.backend_mut(),
        crossterm::event::DisableBracketedPaste,
        LeaveAlternateScreen
    )?;

    // Detach session on exit
    if let AppMode::Operational {
        ref session_name, ..
    } = app_state.mode
    {
        if let Some(ref mut sm) = app_state.session_manager {
            sm.detach(session_name).await?;
        }
    }

    result
}

// ── Event Loop ──

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut AppState,
    notification_rx: Arc<Mutex<mpsc::Receiver<Notification>>>,
) -> Result<()> {
    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(128);

    // Terminal events: key presses and paste events
    #[derive(Debug)]
    enum TermEvent {
        Key(KeyEvent),
        Paste(String),
    }

    let (term_tx, mut term_rx) = mpsc::channel::<TermEvent>(64);
    tokio::task::spawn_blocking(move || loop {
        if event::poll(std::time::Duration::from_millis(20)).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(key)) => {
                    if term_tx.blocking_send(TermEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(Event::Paste(text)) => {
                    if term_tx.blocking_send(TermEvent::Paste(text)).is_err() {
                        break;
                    }
                }
                _ => {}
            }
        }
    });

    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        // Write pending OSC 52 clipboard data through the terminal backend
        if let Some(osc_payload) = app.pending_clipboard.take() {
            use std::io::Write;
            let backend = terminal.backend_mut();
            let _ = backend.write_all(osc_payload.as_bytes());
            let _ = backend.flush();
        }

        if app.should_quit {
            break;
        }

        tokio::select! {
            biased;

            Some(stream_event) = stream_rx.recv() => {
                handle_stream_event(stream_event, app, &stream_tx).await?;
            }

            Some(term_event) = term_rx.recv() => {
                match term_event {
                    TermEvent::Key(key) => {
                        handle_key_event(key, app, &stream_tx).await?;
                    }
                    TermEvent::Paste(text) => {
                        handle_paste(text, app);
                    }
                }
            }

            _ = async {
                let mut rx = notification_rx.lock().await;
                if let Some(notification) = rx.recv().await {
                    let notif_text = format!("[{}] {}", notification.priority_label(), notification.message);
                    app.messages.push(DisplayMessage::new("system", &notif_text));

                    // Send reminder notifications to the Brain so it can act on them
                    if matches!(app.mode, AppMode::Operational { .. }) {
                        if let AppMode::Operational { ref session_name } = app.mode {
                            let session_name = session_name.clone();
                            let system_msg = Message::user(&format!(
                                "[SYSTEM] Proactive notification: {}",
                                notif_text
                            ));
                            if let Some(ref mut sm) = app.session_manager {
                                let _ = sm.append_message(&session_name, &system_msg).await;
                                if let Ok(history) = sm.load_history(&session_name).await {
                                    if let Some(ref brain) = app.brain {
                                        if let Ok(rx) = brain.send_message_streaming(&history).await {
                                            let tx = stream_tx.clone();
                                            app.thinking = true;
                                            app.status_message = "Thinking...".into();
                                            tokio::spawn(async move {
                                                let mut rx = rx;
                                                while let Some(evt) = rx.recv().await {
                                                    let _ = tx.send(evt).await;
                                                }
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            } => {}

            _ = tokio::time::sleep(std::time::Duration::from_millis(33)) => {}
        }
    }

    Ok(())
}

// ── Stream Event Handler ──

async fn handle_stream_event(
    event: StreamEvent,
    app: &mut AppState,
    stream_tx: &mpsc::Sender<StreamEvent>,
) -> Result<()> {
    match event {
        StreamEvent::Token(token) => {
            app.thinking = false; // First token clears thinking indicator
            let text = app.streaming_text.get_or_insert_with(String::new);
            text.push_str(&token);
            // Reset scroll to bottom when streaming
            app.scroll_offset = 0;
        }
        StreamEvent::Done(full_text) => {
            app.streaming_text = None;
            app.thinking = false;
            app.scroll_offset = 0;

            // Check if we're in learning mode and need to handle phase completion
            let is_learning = matches!(app.mode, AppMode::Learning(_));

            let phase_complete = is_learning && full_text.contains("[PHASE_COMPLETE]");
            let display_text = if phase_complete {
                full_text.replace("[PHASE_COMPLETE]", "").trim().to_string()
            } else {
                full_text.clone()
            };

            // Check for tool invocations (operational mode)
            let config_tz = app
                .config
                .as_ref()
                .map(|c| c.timezone.as_str())
                .unwrap_or("UTC");
            let current_session = match &app.mode {
                AppMode::Operational { session_name } => session_name.as_str(),
                _ => "main",
            };
            let (display_text, tool_results) = if !is_learning {
                handle_tool_calls(&display_text, &app.db, config_tz, current_session).await
            } else {
                (display_text, String::new())
            };

            let name = app
                .config
                .as_ref()
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "Embra".into());

            if !display_text.is_empty() {
                app.messages.push(DisplayMessage::new(&name, &display_text));
            }

            if is_learning {
                // Add to learning conversation history
                if let AppMode::Learning(ref mut lm) = app.mode {
                    let clean_text = full_text.replace("[PHASE_COMPLETE]", "").trim().to_string();
                    if !clean_text.is_empty() {
                        lm.state
                            .conversation_history
                            .push(Message::assistant(&clean_text));
                    }
                    lm.awaiting_response = false;

                    if phase_complete {
                        // Handle phase completion
                        let config = app.config.clone().unwrap();
                        learning::handle_phase_complete(&mut lm.state, &app.db, &config).await?;

                        if lm.state.phase == LearningPhase::Complete {
                            // Transition to operational mode
                            transition_to_operational(app, stream_tx).await?;
                        } else {
                            // Start next phase
                            let phase_label = learning::phase_label(&lm.state.phase);
                            app.messages.push(DisplayMessage::new(
                                "system",
                                format!("── Phase: {} ──", phase_label),
                            ));

                            // Send phase kickoff and get initial AI message
                            send_learning_kickoff(app, stream_tx).await?;
                        }
                    }
                }
            } else {
                // Operational mode: save to session
                if let AppMode::Operational { ref session_name } = app.mode {
                    let session_name = session_name.clone();
                    if let Some(ref mut sm) = app.session_manager {
                        let msg = Message::assistant(&full_text);
                        sm.append_message(&session_name, &msg).await?;
                    }

                    // Handle tool results
                    if !tool_results.is_empty() {
                        let tool_msg = Message::user(&format!(
                            "[SYSTEM] Tool results:\n{}",
                            tool_results
                        ));
                        if let Some(ref mut sm) = app.session_manager {
                            sm.append_message(&session_name, &tool_msg).await?;
                            // load_history already includes the tool_msg we just appended (BUG-002 fix)
                            let history = sm.load_history(&session_name).await?;
                            if let Some(ref brain) = app.brain {
                                let rx = brain.send_message_streaming(&history).await?;
                                let tx2 = stream_tx.clone();
                                tokio::spawn(async move {
                                    let mut rx = rx;
                                    while let Some(evt) = rx.recv().await {
                                        let _ = tx2.send(evt).await;
                                    }
                                });
                            }
                        }
                    }
                }
            }

            app.status_message = "OK".into();
        }
        StreamEvent::Error(err) => {
            app.streaming_text = None;
            app.thinking = false;
            app.status_message = format!("Error: {}", err);
            app.messages
                .push(DisplayMessage::new("system", format!("Error: {}", err)));
            if let AppMode::Learning(ref mut lm) = app.mode {
                lm.awaiting_response = false;
            }
        }
    }
    Ok(())
}

// ── Key Event Handler ──

async fn handle_key_event(
    key: KeyEvent,
    app: &mut AppState,
    stream_tx: &mpsc::Sender<StreamEvent>,
) -> Result<()> {
    // If a selector is active, handle arrow keys and Enter for selection
    if app.selector.is_some() {
        match key.code {
            KeyCode::Up => {
                if let Some(ref mut sel) = app.selector {
                    sel.up();
                }
                return Ok(());
            }
            KeyCode::Down => {
                if let Some(ref mut sel) = app.selector {
                    sel.down();
                }
                return Ok(());
            }
            KeyCode::Enter => {
                let selection = app.selector.as_ref().unwrap().current().to_string();
                let selected_idx = app.selector.as_ref().unwrap().selected;
                app.selector = None;

                // Route selection to current mode handler
                match &app.mode {
                    AppMode::Setup(_) => {
                        handle_setup_selection(app, selected_idx, &selection, stream_tx)
                            .await?;
                    }
                    _ => {}
                }
                return Ok(());
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.should_quit = true;
                return Ok(());
            }
            _ => return Ok(()),
        }
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Enter => {
            if key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::ALT)
            {
                // Shift+Enter (enhanced terminals) or Alt+Enter (universal) for newline
                app.input_buffer.push('\n');
                app.cursor_pos += 1;
            } else {
                // Gather input: pasted lines already include any prior input_buffer
                // (folded in at paste time), so just join them directly.
                let input = if let Some(pasted) = app.pasted_lines.take() {
                    app.input_buffer.clear();
                    app.cursor_pos = 0;
                    pasted.join("\n")
                } else if !app.input_buffer.is_empty() {
                    let input = app.input_buffer.clone();
                    app.input_buffer.clear();
                    app.cursor_pos = 0;
                    input
                } else {
                    // Empty input — allow it for setup steps (defaults)
                    if matches!(app.mode, AppMode::Setup(_)) {
                        String::new()
                    } else {
                        return Ok(());
                    }
                };

                app.scroll_offset = 0;

                match &app.mode {
                    AppMode::Setup(_) => {
                        handle_setup_input(app, &input, stream_tx).await?;
                    }
                    AppMode::Learning(_) => {
                        if !input.is_empty() {
                            handle_learning_input(app, &input, stream_tx).await?;
                        }
                    }
                    AppMode::Operational { .. } => {
                        if !input.is_empty() {
                            handle_operational_input(app, &input, stream_tx).await?;
                        }
                    }
                }
            }
        }
        KeyCode::Char(c) => {
            let byte_pos = char_to_byte_pos(&app.input_buffer, app.cursor_pos);
            app.input_buffer.insert(byte_pos, c);
            app.cursor_pos += 1;
        }
        KeyCode::Backspace => {
            if app.cursor_pos > 0 {
                app.cursor_pos -= 1;
                let byte_pos = char_to_byte_pos(&app.input_buffer, app.cursor_pos);
                app.input_buffer.remove(byte_pos);
            } else if app.pasted_lines.is_some() {
                // Clear pasted content on backspace when cursor is at start
                app.pasted_lines = None;
            }
        }
        KeyCode::Left => {
            if app.cursor_pos > 0 {
                app.cursor_pos -= 1;
            }
        }
        KeyCode::Right => {
            if app.cursor_pos < app.input_buffer.chars().count() {
                app.cursor_pos += 1;
            }
        }
        KeyCode::Up => {
            if app.selector.is_none() {
                app.scroll_offset = app.scroll_offset.saturating_add(3);
            }
        }
        KeyCode::Down => {
            if app.selector.is_none() {
                app.scroll_offset = app.scroll_offset.saturating_sub(3);
            }
        }
        KeyCode::Home => {
            app.cursor_pos = 0;
        }
        KeyCode::End => {
            app.cursor_pos = app.input_buffer.chars().count();
        }
        _ => {}
    }
    Ok(())
}

// ── Paste Handler ──

fn handle_paste(text: String, app: &mut AppState) {
    let line_count = text.lines().count();
    if line_count > 1 || text.len() > 200 {
        // Multi-line or long paste: store for preview, send on Enter.
        // Fold any existing input_buffer content into the pasted lines so
        // it is visible in the preview and not silently merged on send.
        let mut lines: Vec<String> = Vec::new();
        if !app.input_buffer.is_empty() {
            lines.push(app.input_buffer.clone());
            app.input_buffer.clear();
            app.cursor_pos = 0;
        }
        // Append to any existing pasted content (multiple pastes stack)
        if let Some(existing) = app.pasted_lines.take() {
            lines.extend(existing);
        }
        lines.extend(text.lines().map(|l| l.to_string()));
        app.pasted_lines = Some(lines);
    } else {
        // Short single-line paste: insert inline at cursor
        let byte_pos = char_to_byte_pos(&app.input_buffer, app.cursor_pos);
        app.input_buffer.insert_str(byte_pos, &text);
        app.cursor_pos += text.chars().count();
    }
}

// ── Setup Mode ──

async fn handle_setup_selection(
    app: &mut AppState,
    selected_idx: usize,
    _selection: &str,
    stream_tx: &mpsc::Sender<StreamEvent>,
) -> Result<()> {
    let step = if let AppMode::Setup(ref setup) = app.mode {
        setup.step.clone()
    } else {
        return Ok(());
    };

    match step {
        SetupStep::Name => {
            if selected_idx == 0 {
                // Default: Embra
                app.messages
                    .push(DisplayMessage::new("You", "Embra"));
                advance_setup_name(app, "Embra");
            } else {
                // Custom name — switch to text input mode
                app.messages.push(DisplayMessage::new(
                    "system",
                    "Enter a custom name:",
                ));
            }
        }
        SetupStep::Timezone => {
            let tz = detect_timezone();
            if selected_idx == 0 {
                // Accept detected timezone
                app.messages
                    .push(DisplayMessage::new("You", tz.clone()));
                advance_setup_timezone(app, &tz);
            } else {
                // Custom timezone — switch to text input mode
                app.messages.push(DisplayMessage::new(
                    "system",
                    "Enter your timezone (e.g. America/New_York, PDT, UTC):",
                ));
            }
        }
        SetupStep::Confirm => {
            if selected_idx == 0 {
                // Confirmed — save and transition
                app.messages.push(DisplayMessage::new("You", "Yes"));
                finalize_setup(app, stream_tx).await?;
            } else {
                // Restart
                app.messages.push(DisplayMessage::new("You", "No, restart"));
                restart_setup(app);
            }
        }
        _ => {}
    }
    Ok(())
}

async fn handle_setup_input(
    app: &mut AppState,
    input: &str,
    stream_tx: &mpsc::Sender<StreamEvent>,
) -> Result<()> {
    let step = if let AppMode::Setup(ref setup) = app.mode {
        setup.step.clone()
    } else {
        return Ok(());
    };

    match step {
        SetupStep::Name => {
            let name = if input.trim().is_empty() {
                "Embra".to_string()
            } else {
                input.trim().to_string()
            };
            app.messages
                .push(DisplayMessage::new("You", name.clone()));
            advance_setup_name(app, &name);
        }
        SetupStep::ApiKey => {
            let key = input.trim().to_string();
            app.messages.push(DisplayMessage::new("You", "••••••••"));

            if key.is_empty() {
                app.messages.push(DisplayMessage::new(
                    "system",
                    "API key is required. Please enter your Anthropic API key:",
                ));
                return Ok(());
            }

            if !key.starts_with("sk-") {
                app.messages.push(DisplayMessage::new(
                    "system",
                    "Warning: API key doesn't start with 'sk-'. Proceeding anyway.",
                ));
            }

            if let AppMode::Setup(ref mut setup) = app.mode {
                setup.api_key = Some(key);
                advance_to_timezone(app);
            }
        }
        SetupStep::Timezone => {
            let tz = if input.trim().is_empty() {
                detect_timezone()
            } else {
                input.trim().to_string()
            };
            app.messages.push(DisplayMessage::new("You", tz.clone()));
            advance_setup_timezone(app, &tz);
        }
        SetupStep::Confirm => {
            // Selector handles this, but support text fallback
            app.messages
                .push(DisplayMessage::new("You", input.to_string()));
            if input.trim().eq_ignore_ascii_case("yes")
                || input.trim().eq_ignore_ascii_case("y")
                || input.trim().is_empty()
            {
                finalize_setup(app, stream_tx).await?;
            } else {
                restart_setup(app);
            }
        }
    }
    Ok(())
}

fn advance_setup_name(app: &mut AppState, name: &str) {
    if let AppMode::Setup(ref mut setup) = app.mode {
        setup.name = Some(name.to_string());

        if setup.api_key.is_some() {
            // API key from env, skip to timezone
            advance_to_timezone(app);
        } else {
            if let AppMode::Setup(ref mut setup) = app.mode {
                setup.step = SetupStep::ApiKey;
            }
            app.messages.push(DisplayMessage::new(
                "system",
                "Enter your Anthropic API key:",
            ));
        }
    }
}

fn advance_to_timezone(app: &mut AppState) {
    if let AppMode::Setup(ref mut setup) = app.mode {
        setup.step = SetupStep::Timezone;
    }
    let tz = detect_timezone();
    app.messages.push(DisplayMessage::new(
        "system",
        format!("What timezone are you in?"),
    ));
    app.selector = Some(Selector::new(vec![
        format!("{} (detected)", tz),
        "Enter custom timezone".into(),
    ]));
}

fn advance_setup_timezone(app: &mut AppState, tz: &str) {
    // Resolve abbreviations to IANA names (BUG-007)
    let resolved = tools::resolve_timezone(tz);
    if let AppMode::Setup(ref mut setup) = app.mode {
        setup.timezone = Some(resolved);
        setup.step = SetupStep::Confirm;
    }

    // Show summary with confirm selector
    if let AppMode::Setup(ref setup) = app.mode {
        let summary = format!(
            "Configuration summary:\n  Name: {}\n  API Key: {}\n  Timezone: {}\n  Mode: container",
            setup.name.as_deref().unwrap_or("Embra"),
            if setup.api_key.is_some() {
                "••••••••"
            } else {
                "(none)"
            },
            setup.timezone.as_deref().unwrap_or("UTC"),
        );
        app.messages
            .push(DisplayMessage::new("system", summary));
    }

    app.selector = Some(Selector::new(vec![
        "Yes, confirm".into(),
        "No, restart setup".into(),
    ]));
}

fn restart_setup(app: &mut AppState) {
    app.mode = AppMode::Setup(SetupState {
        step: SetupStep::Name,
        name: None,
        api_key: std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.is_empty()),
        timezone: None,
    });
    app.messages.push(DisplayMessage::new(
        "system",
        "Setup restarted. What would you like to name your intelligence?",
    ));
    app.selector = Some(Selector::new(vec![
        "Embra (default)".into(),
        "Enter custom name".into(),
    ]));
}

async fn finalize_setup(
    app: &mut AppState,
    stream_tx: &mpsc::Sender<StreamEvent>,
) -> Result<()> {
    let (name, api_key, timezone) = if let AppMode::Setup(ref setup) = app.mode {
        (
            setup.name.clone().unwrap_or_else(|| "Embra".into()),
            setup.api_key.clone().unwrap_or_default(),
            setup.timezone.clone().unwrap_or_else(|| "UTC".into()),
        )
    } else {
        return Ok(());
    };

    let config = SystemConfig {
        name: name.clone(),
        api_key,
        timezone,
        deployment_mode: "container".into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        version: env!("CARGO_PKG_VERSION").into(),
    };

    crate::config::save_config(&app.db, &config).await?;
    app.config = Some(config.clone());

    app.messages.push(DisplayMessage::new(
        "system",
        format!(
            "Configuration saved.\n\n\
             ═══ {} Learning Mode ═══\n\
             This is a guided conversation to establish identity and values.\n\
             Take your time — this shapes who {} will be.",
            name, name
        ),
    ));

    // Transition to learning mode
    let learning_state = LearningState::new();
    app.mode = AppMode::Learning(LearningModeState {
        state: learning_state,
        awaiting_response: false,
    });

    let brain = Brain::new(config.api_key.clone(), String::new());
    app.brain = Some(brain);

    app.messages.push(DisplayMessage::new(
        "system",
        "── Phase: User Configuration ──",
    ));

    send_learning_kickoff(app, stream_tx).await?;
    Ok(())
}

// ── Learning Mode ──

async fn send_learning_kickoff(
    app: &mut AppState,
    stream_tx: &mpsc::Sender<StreamEvent>,
) -> Result<()> {
    if let AppMode::Learning(ref mut lm) = app.mode {
        let config = app.config.as_ref().unwrap();

        // Set system prompt for current phase
        let system_prompt = learning::system_prompt_for_phase(&lm.state, config);
        if let Some(ref mut brain) = app.brain {
            brain.set_system_prompt(system_prompt);
        }

        // Send phase kickoff message
        let kickoff = learning::phase_kickoff(&lm.state.phase);
        lm.state.conversation_history.push(Message::user(&kickoff));
        lm.awaiting_response = true;

        app.status_message = "Thinking...".into();
        app.thinking = true;

        // Send to brain
        if let Some(ref brain) = app.brain {
            let rx = brain
                .send_message_streaming(&lm.state.conversation_history)
                .await?;
            let tx = stream_tx.clone();
            tokio::spawn(async move {
                let mut rx = rx;
                while let Some(evt) = rx.recv().await {
                    let _ = tx.send(evt).await;
                }
            });
        }
    }
    Ok(())
}

async fn handle_learning_input(
    app: &mut AppState,
    input: &str,
    stream_tx: &mpsc::Sender<StreamEvent>,
) -> Result<()> {
    // Don't accept input while waiting for a response
    if let AppMode::Learning(ref lm) = app.mode {
        if lm.awaiting_response {
            return Ok(());
        }
    }

    app.messages
        .push(DisplayMessage::new("You", input.to_string()));

    if let AppMode::Learning(ref mut lm) = app.mode {
        lm.state.conversation_history.push(Message::user(input));
        lm.awaiting_response = true;

        // Update system prompt (may have changed if documents were confirmed)
        let config = app.config.as_ref().unwrap();
        let system_prompt = learning::system_prompt_for_phase(&lm.state, config);
        if let Some(ref mut brain) = app.brain {
            brain.set_system_prompt(system_prompt);
        }

        app.status_message = "Thinking...".into();
        app.thinking = true;

        if let Some(ref brain) = app.brain {
            let rx = brain
                .send_message_streaming(&lm.state.conversation_history)
                .await?;
            let tx = stream_tx.clone();
            tokio::spawn(async move {
                let mut rx = rx;
                while let Some(evt) = rx.recv().await {
                    let _ = tx.send(evt).await;
                }
            });
        }
    }
    Ok(())
}

async fn transition_to_operational(
    app: &mut AppState,
    _stream_tx: &mpsc::Sender<StreamEvent>,
) -> Result<()> {
    // Save learning conversation
    if let AppMode::Learning(ref lm) = app.mode {
        let db = &app.db;
        if !db.collection_exists("sessions.learning.history").await? {
            db.create_collection("sessions.learning.history").await?;
        }
        let learning_history = serde_json::json!({
            "session_name": "learning",
            "turns": lm.state.conversation_history,
        });
        db.write("sessions.learning.history", &learning_history)
            .await?;
    }

    let config = app.config.as_ref().unwrap();
    let name = config.name.clone();

    app.messages.push(DisplayMessage::new(
        "system",
        format!(
            "═══ {} Learning Mode Complete ═══\n\
             {} is now configured and ready. Entering operational mode.",
            name, name
        ),
    ));

    // Load the confirmed documents
    let soul = learning::load_soul(&app.db).await?.unwrap_or_default();
    let identity = load_document(&app.db, "memory.identity").await?;
    let user_profile = load_document(&app.db, "memory.user").await?;

    // Build operational system prompt
    let session_context = "First session after Learning Mode. Brand new.".to_string();
    let system_prompt = crate::brain::operational_mode(
        &name,
        &serde_json::to_string_pretty(&soul).unwrap_or_default(),
        &serde_json::to_string_pretty(&identity).unwrap_or_default(),
        &serde_json::to_string_pretty(&user_profile).unwrap_or_default(),
        &session_context,
    );

    // Reinitialize brain with operational prompt
    if let Some(ref mut brain) = app.brain {
        brain.set_system_prompt(system_prompt);
    }

    app.soul = Some(soul);
    app.identity = Some(identity);
    app.user_profile = Some(user_profile);

    // Create first session
    let mut session_manager = SessionManager::new(app.db.clone());
    session_manager.create("main").await?;
    app.session_manager = Some(session_manager);

    app.mode = AppMode::Operational {
        session_name: "main".to_string(),
    };

    Ok(())
}

// ── Operational Mode ──

async fn handle_operational_input(
    app: &mut AppState,
    input: &str,
    stream_tx: &mpsc::Sender<StreamEvent>,
) -> Result<()> {
    // Slash commands
    if input.starts_with('/') {
        let result = commands::handle_command(input, app).await?;
        if let Some(msg) = result {
            app.messages.push(DisplayMessage::new("system", msg));
        }
        return Ok(());
    }

    let session_name = if let AppMode::Operational { ref session_name } = app.mode {
        session_name.clone()
    } else {
        return Ok(());
    };

    app.messages.push(DisplayMessage::new("You", input));
    app.status_message = "Thinking...".into();
    app.thinking = true;

    // Save user message
    let msg = Message::user(input);
    if let Some(ref mut sm) = app.session_manager {
        sm.append_message(&session_name, &msg).await?;
        let history = sm.load_history(&session_name).await?;

        if let Some(ref brain) = app.brain {
            let rx = brain.send_message_streaming(&history).await?;
            let tx = stream_tx.clone();
            tokio::spawn(async move {
                let mut rx = rx;
                while let Some(evt) = rx.recv().await {
                    let _ = tx.send(evt).await;
                }
            });
        }
    }

    Ok(())
}

// ── Tool Handling ──

async fn handle_tool_calls(
    text: &str,
    db: &WardsonDbClient,
    config_tz: &str,
    session_name: &str,
) -> (String, String) {
    let mut display_text = text.to_string();
    let mut tool_results = String::new();

    // Use safe tag extraction that ignores code blocks and inline code (BUG-001 fix)
    let tags = tools::extract_tool_tags(text);
    for tag in &tags {
        if let Some(result) = tools::dispatch(tag, db, config_tz, session_name).await {
            display_text = display_text.replace(tag, "");
            tool_results.push_str(&result);
            tool_results.push('\n');
        }
    }

    (display_text.trim().to_string(), tool_results)
}

// ── Helpers ──

async fn load_document(db: &WardsonDbClient, collection: &str) -> Result<serde_json::Value> {
    if !db.collection_exists(collection).await? {
        return Ok(serde_json::json!({}));
    }
    let results = db.query(collection, &serde_json::json!({})).await?;
    Ok(results.into_iter().next().unwrap_or(serde_json::json!({})))
}

/// Convert a char index to a byte index in a string
fn char_to_byte_pos(s: &str, char_pos: usize) -> usize {
    s.char_indices()
        .nth(char_pos)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(s.len())
}

fn detect_timezone() -> String {
    if let Ok(tz) = std::env::var("TZ") {
        if !tz.is_empty() {
            return tz;
        }
    }
    if let Ok(tz) = std::fs::read_to_string("/etc/timezone") {
        let tz = tz.trim().to_string();
        if !tz.is_empty() {
            return tz;
        }
    }
    "UTC".into()
}
