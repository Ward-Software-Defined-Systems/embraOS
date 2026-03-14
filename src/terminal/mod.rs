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
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::brain::{Brain, Message, StreamEvent};
use crate::config::SystemConfig;
use crate::db::WardsonDbClient;
use crate::proactive::Notification;
use crate::sessions::SessionManager;
use crate::tools;

pub struct AppState {
    pub config: SystemConfig,
    pub brain: Brain,
    pub session_manager: SessionManager,
    pub db: WardsonDbClient,
    pub messages: Vec<DisplayMessage>,
    pub input_buffer: String,
    pub cursor_pos: usize,
    pub scroll_offset: u16,
    pub streaming_text: Option<String>,
    pub status_message: String,
    pub should_quit: bool,
    pub soul: Option<serde_json::Value>,
    pub identity: Option<serde_json::Value>,
    pub user_profile: Option<serde_json::Value>,
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

pub async fn run_terminal(
    db: &WardsonDbClient,
    config: &SystemConfig,
    notification_rx: mpsc::Receiver<Notification>,
) -> Result<()> {
    // Load soul, identity, user profile
    let soul = crate::learning::load_soul(db).await?.unwrap_or_default();
    let identity = load_document(db, "memory.identity").await?;
    let user_profile = load_document(db, "memory.user").await?;

    // Build operational system prompt
    let session_context = "New session. No prior context.".to_string();
    let system_prompt = crate::brain::operational_mode(
        &config.name,
        &serde_json::to_string_pretty(&soul).unwrap_or_default(),
        &serde_json::to_string_pretty(&identity).unwrap_or_default(),
        &serde_json::to_string_pretty(&user_profile).unwrap_or_default(),
        &session_context,
    );

    let brain = Brain::new(config.api_key.clone(), system_prompt);
    let mut session_manager = SessionManager::new(db.clone());

    // Restore or create session
    let session_name = if let Some(existing) = session_manager.get_most_recent_active().await? {
        let history = session_manager.reattach(&existing.name).await?;
        let mut messages: Vec<DisplayMessage> = history
            .iter()
            .map(|m| DisplayMessage::new(&m.role, &m.content))
            .collect();

        // Add reconnection message
        let briefing = crate::brain::reconnection_briefing(
            &config.name,
            &existing.last_active.to_rfc3339(),
        );
        messages.push(DisplayMessage::new("system", briefing));

        existing.name
    } else {
        session_manager.create("main").await?;
        "main".to_string()
    };

    let mut app_state = AppState {
        config: config.clone(),
        brain,
        session_manager,
        db: db.clone(),
        messages: Vec::new(),
        input_buffer: String::new(),
        cursor_pos: 0,
        scroll_offset: 0,
        streaming_text: None,
        status_message: "OK".into(),
        should_quit: false,
        soul: Some(soul),
        identity: Some(identity),
        user_profile: Some(user_profile),
    };

    // Enter TUI mode
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let notification_rx = Arc::new(Mutex::new(notification_rx));

    // Main event loop
    let result = run_event_loop(&mut terminal, &mut app_state, notification_rx, &session_name).await;

    // Cleanup
    terminal::disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    // Detach session on exit
    app_state.session_manager.detach(&session_name).await?;

    result
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut AppState,
    notification_rx: Arc<Mutex<mpsc::Receiver<Notification>>>,
    session_name: &str,
) -> Result<()> {
    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(128);

    // Spawn a dedicated task for reading terminal key events.
    // This avoids blocking the async event loop during poll().
    let (key_tx, mut key_rx) = mpsc::channel::<KeyEvent>(64);
    tokio::task::spawn_blocking(move || {
        loop {
            // Poll with a short timeout so the thread can exit when the channel closes
            if event::poll(std::time::Duration::from_millis(20)).unwrap_or(false) {
                if let Ok(Event::Key(key)) = event::read() {
                    if key_tx.blocking_send(key).is_err() {
                        break; // receiver dropped, exit
                    }
                }
            }
        }
    });

    loop {
        // Draw UI every iteration
        terminal.draw(|f| ui::draw(f, app, session_name))?;

        if app.should_quit {
            break;
        }

        tokio::select! {
            biased; // Prioritize stream events for smooth rendering

            // Stream events from Brain — highest priority for responsive streaming
            Some(stream_event) = stream_rx.recv() => {
                match stream_event {
                    StreamEvent::Token(token) => {
                        let text = app.streaming_text.get_or_insert_with(String::new);
                        text.push_str(&token);
                    }
                    StreamEvent::Done(full_text) => {
                        app.streaming_text = None;

                        // Check for tool invocations
                        let (display_text, tool_results) = handle_tool_calls(&full_text, &app.db).await;

                        app.messages.push(DisplayMessage::new(&app.config.name, &display_text));

                        // Save to session
                        let msg = Message::assistant(&full_text);
                        app.session_manager.append_message(session_name, &msg).await?;

                        // If there were tool results, send them back to Brain
                        if !tool_results.is_empty() {
                            let tool_msg = Message::user(&format!("[SYSTEM] Tool results:\n{}", tool_results));
                            app.session_manager.append_message(session_name, &tool_msg).await?;

                            let mut history = app.session_manager.load_history(session_name).await?;
                            history.push(tool_msg);
                            let rx = app.brain.send_message_streaming(&history).await?;

                            // Forward stream events
                            let tx2 = stream_tx.clone();
                            tokio::spawn(async move {
                                let mut rx = rx;
                                while let Some(evt) = rx.recv().await {
                                    let _ = tx2.send(evt).await;
                                }
                            });
                        }

                        app.status_message = "OK".into();
                    }
                    StreamEvent::Error(err) => {
                        app.streaming_text = None;
                        app.status_message = format!("Error: {}", err);
                        app.messages.push(DisplayMessage::new("system", format!("Error: {}", err)));
                    }
                }
            }

            // Terminal key events from dedicated reader thread
            Some(key) = key_rx.recv() => {
                match handle_key_event(key, app, session_name, &stream_tx).await? {
                    KeyAction::Continue => {},
                    KeyAction::Quit => {
                        app.should_quit = true;
                    }
                }
            }

            // Proactive notifications
            _ = async {
                let mut rx = notification_rx.lock().await;
                if let Some(notification) = rx.recv().await {
                    app.messages.push(DisplayMessage::new(
                        "system",
                        format!("[{}] {}", notification.priority_label(), notification.message),
                    ));
                }
            } => {}

            // Tick fallback — ensure redraws even if no events arrive
            _ = tokio::time::sleep(std::time::Duration::from_millis(33)) => {}
        }
    }

    Ok(())
}

pub enum KeyAction {
    Continue,
    Quit,
}

async fn handle_key_event(
    key: KeyEvent,
    app: &mut AppState,
    session_name: &str,
    stream_tx: &mpsc::Sender<StreamEvent>,
) -> Result<KeyAction> {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return Ok(KeyAction::Quit);
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return Ok(KeyAction::Quit);
        }
        KeyCode::Enter => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.input_buffer.push('\n');
                app.cursor_pos += 1;
            } else if !app.input_buffer.is_empty() {
                let input = app.input_buffer.clone();
                app.input_buffer.clear();
                app.cursor_pos = 0;

                // Check for slash commands
                if input.starts_with('/') {
                    let result = commands::handle_command(&input, app).await?;
                    if let Some(msg) = result {
                        app.messages.push(DisplayMessage::new("system", msg));
                    }
                    return Ok(KeyAction::Continue);
                }

                // Regular message
                app.messages
                    .push(DisplayMessage::new("You", &input));
                app.status_message = "Thinking...".into();

                // Save user message
                let msg = Message::user(&input);
                app.session_manager
                    .append_message(session_name, &msg)
                    .await?;

                // Load full history and send to Brain
                let history = app.session_manager.load_history(session_name).await?;
                let rx = app.brain.send_message_streaming(&history).await?;

                // Forward stream events to the main loop
                let tx = stream_tx.clone();
                tokio::spawn(async move {
                    let mut rx = rx;
                    while let Some(evt) = rx.recv().await {
                        let _ = tx.send(evt).await;
                    }
                });
            }
        }
        KeyCode::Char(c) => {
            app.input_buffer.insert(app.cursor_pos, c);
            app.cursor_pos += 1;
        }
        KeyCode::Backspace => {
            if app.cursor_pos > 0 {
                app.cursor_pos -= 1;
                app.input_buffer.remove(app.cursor_pos);
            }
        }
        KeyCode::Left => {
            if app.cursor_pos > 0 {
                app.cursor_pos -= 1;
            }
        }
        KeyCode::Right => {
            if app.cursor_pos < app.input_buffer.len() {
                app.cursor_pos += 1;
            }
        }
        KeyCode::Up => {
            app.scroll_offset = app.scroll_offset.saturating_add(1);
        }
        KeyCode::Down => {
            app.scroll_offset = app.scroll_offset.saturating_sub(1);
        }
        KeyCode::Home => {
            app.cursor_pos = 0;
        }
        KeyCode::End => {
            app.cursor_pos = app.input_buffer.len();
        }
        _ => {}
    }

    Ok(KeyAction::Continue)
}

async fn handle_tool_calls(text: &str, db: &WardsonDbClient) -> (String, String) {
    let mut display_text = text.to_string();
    let mut tool_results = String::new();

    let tool_patterns = ["[TOOL:system_status]", "[TOOL:check_update]", "[TOOL:search_memory]"];

    for pattern in &tool_patterns {
        if text.contains(pattern) {
            display_text = display_text.replace(pattern, "");
            match *pattern {
                "[TOOL:system_status]" => {
                    let status = tools::system_status(db).await;
                    tool_results.push_str(&format!(
                        "System Status:\n{}\n",
                        serde_json::to_string_pretty(&status).unwrap_or_default()
                    ));
                }
                "[TOOL:check_update]" => {
                    match tools::check_wardsondb_update().await {
                        Some(info) => {
                            tool_results.push_str(&format!(
                                "WardSONDB Update Available: v{}\n",
                                info.version
                            ));
                        }
                        None => {
                            tool_results.push_str("WardSONDB is up to date.\n");
                        }
                    }
                }
                "[TOOL:search_memory]" => {
                    tool_results.push_str("Memory search not yet implemented in Phase 0.\n");
                }
                _ => {}
            }
        }
    }

    (display_text.trim().to_string(), tool_results)
}

async fn load_document(
    db: &WardsonDbClient,
    collection: &str,
) -> Result<serde_json::Value> {
    if !db.collection_exists(collection).await? {
        return Ok(serde_json::json!({}));
    }
    let results = db.query(collection, &serde_json::json!({})).await?;
    Ok(results.into_iter().next().unwrap_or(serde_json::json!({})))
}
