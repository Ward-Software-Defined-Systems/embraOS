//! Full TUI terminal for embra-console.
//!
//! ratatui-based terminal driven by gRPC ConsoleEvents from embra-apid.
//! Renders the full Phase 0 visual experience: styled text, JSON highlighting,
//! thinking indicator, multi-line input, selectors, and mode transitions.

mod commands;
mod input;
mod render;
pub mod state;
mod ui;

use state::*;
use crate::grpc_client::{BrainClient, ConsoleEvent};
use embra_common::proto::apid::*;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal::{self, disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, stdout};
use std::time::Duration;
use tokio::sync::mpsc;

pub async fn run(mut client: BrainClient, _device: Option<String>) -> Result<()> {
    println!("[TUI] opening conversation...");
    let (in_tx, mut out_rx) = client.open_conversation("").await?;
    println!("[TUI] conversation opened");

    // Determine terminal size: CLI override > TIOCGWINSZ > default 80x24
    let override_cols: Option<u16> = std::env::var("EMBRA_COLUMNS").ok().and_then(|v| v.parse().ok());
    let override_rows: Option<u16> = std::env::var("EMBRA_ROWS").ok().and_then(|v| v.parse().ok());
    let (detected_cols, detected_rows) = terminal::size().unwrap_or((0, 0));
    let cols = override_cols.unwrap_or(if detected_cols > 0 { detected_cols } else { 80 });
    let rows = override_rows.unwrap_or(if detected_rows > 0 { detected_rows } else { 24 });
    println!("[TUI] size: {}x{} (detected: {}x{})", cols, rows, detected_cols, detected_rows);

    // Initialize ratatui terminal
    // Skip EnterAlternateScreen — doesn't work over QEMU serial (-nographic)
    enable_raw_mode()?;

    // For serial console, always use fixed viewport since TIOCGWINSZ is unreliable
    let use_cols = if cols > 0 { cols } else { 80 };
    let use_rows = if rows > 0 { rows } else { 24 };

    let backend = CrosstermBackend::new(stdout());
    let mut terminal_tui = Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, use_cols, use_rows)),
        },
    )?;
    // Delay to let embrad finish its dup2 redirect, then clear any log bleed-through
    tokio::time::sleep(Duration::from_millis(500)).await;
    terminal_tui.clear()?;
    // Double-clear to catch any late log messages
    tokio::time::sleep(Duration::from_millis(200)).await;
    terminal_tui.clear()?;

    let mut app = AppState::new();
    app.status_message = "OK".to_string();

    // Spawn terminal event reader
    let (term_tx, mut term_rx) = mpsc::channel::<Event>(100);
    std::thread::spawn(move || {
        loop {
            if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(ev) = event::read() {
                    if term_tx.blocking_send(ev).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Main event loop
    loop {
        terminal_tui.draw(|f| ui::draw(f, &app))?;

        if app.should_quit {
            break;
        }

        tokio::select! {
            biased;

            // gRPC events (highest priority)
            event = out_rx.recv() => {
                match event {
                    Some(ev) => handle_console_event(ev, &mut app),
                    None => {
                        app.messages.push(DisplayMessage::system("Disconnected from server."));
                        app.should_quit = true;
                    }
                }
            }

            // Terminal events
            Some(ev) = term_rx.recv() => {
                if let Event::Key(key) = ev {
                    handle_key_event(key, &mut app, &in_tx).await?;
                }
            }

            // Tick for animations (thinking dots)
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                // Just redraw
            }
        }
    }

    // Cleanup
    disable_raw_mode()?;
    Ok(())
}

fn handle_console_event(event: ConsoleEvent, app: &mut AppState) {
    match event {
        ConsoleEvent::Token(text) => {
            app.thinking = false;
            match &mut app.streaming_text {
                Some(s) => s.push_str(&text),
                None => app.streaming_text = Some(text),
            }
        }
        ConsoleEvent::ResponseDone(full) => {
            app.streaming_text = None;
            app.thinking = false;
            app.messages.push(DisplayMessage::assistant(&full));
            app.scroll_offset = 0;
        }
        ConsoleEvent::SystemMessage { content, .. } => {
            app.messages.push(DisplayMessage::system(&content));
            app.scroll_offset = 0;
        }
        ConsoleEvent::ToolExecution { name, result, .. } => {
            app.messages.push(DisplayMessage::tool(&name, &result));
            app.scroll_offset = 0;
        }
        ConsoleEvent::ThinkingState { is_thinking, name } => {
            app.thinking = is_thinking;
            if !name.is_empty() {
                app.thinking_name = name;
            }
        }
        ConsoleEvent::ModeTransition { from_mode: _, to_mode, message } => {
            // to_mode: 1=Setup, 2=Learning, 3=Operational
            match to_mode {
                1 => {
                    app.mode = AppMode::Setup(SetupState { step: SetupStep::Name });
                }
                2 => {
                    app.mode = AppMode::Learning;
                }
                3 => {
                    // Extract session name from message (format: "Operational — Session: <name>")
                    let session = message.split("Session: ")
                        .nth(1)
                        .unwrap_or("main")
                        .to_string();
                    app.mode = AppMode::Operational { session_name: session };
                }
                _ => {}
            }
            if !message.is_empty() {
                app.messages.push(DisplayMessage::system(&message));
            }
            app.scroll_offset = 0;
        }
        ConsoleEvent::SetupPrompt { field_type, prompt, options, default_value } => {
            app.messages.push(DisplayMessage::system(&prompt));

            if field_type == "confirm" || field_type == "selector" {
                if !options.is_empty() {
                    app.selector = Some(Selector::new(options));
                }
            }

            if !default_value.is_empty() {
                app.setup_default = Some(default_value);
            }

            // Update setup step
            if let AppMode::Setup(ref mut setup) = app.mode {
                setup.step = AppState::infer_setup_step(&prompt);
            }

            app.scroll_offset = 0;
        }
    }
}

async fn handle_key_event(
    key: KeyEvent,
    app: &mut AppState,
    in_tx: &mpsc::Sender<ConversationRequest>,
) -> Result<()> {
    match (key.code, key.modifiers) {
        // Quit
        (KeyCode::Char('c'), KeyModifiers::CONTROL) |
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }

        // Selector navigation
        (KeyCode::Up, _) if app.selector.is_some() => {
            if let Some(ref mut sel) = app.selector {
                sel.up();
            }
        }
        (KeyCode::Down, _) if app.selector.is_some() => {
            if let Some(ref mut sel) = app.selector {
                sel.down();
            }
        }

        // Enter — send input or selector choice
        (KeyCode::Enter, _) => {
            if let Some(selector) = app.selector.take() {
                // Send selector choice
                let choice = selector.current().to_string();
                let _ = in_tx.send(ConversationRequest {
                    request_type: Some(conversation_request::RequestType::UserMessage(
                        UserMessage { content: choice }
                    )),
                }).await;
            } else if let Some(pasted) = app.pasted_lines.take() {
                // Send pasted content
                let content = pasted.join("\n");
                app.messages.push(DisplayMessage::user(&content));
                let _ = in_tx.send(ConversationRequest {
                    request_type: Some(conversation_request::RequestType::UserMessage(
                        UserMessage { content }
                    )),
                }).await;
            } else {
                let mut input = app.input_buffer.trim().to_string();
                app.input_buffer.clear();
                app.cursor_pos = 0;

                // Use setup default if empty
                if input.is_empty() {
                    if let Some(default) = app.setup_default.take() {
                        if !default.is_empty() {
                            input = default;
                        } else {
                            return Ok(());
                        }
                    } else {
                        return Ok(());
                    }
                }
                app.setup_default = None;

                // Check for slash commands
                if input.starts_with('/') {
                    let parts: Vec<&str> = input.splitn(2, ' ').collect();
                    let cmd = parts[0];
                    let args = if parts.len() > 1 { parts[1] } else { "" };

                    // Handle local commands
                    if commands::is_local_command(cmd) {
                        if let Some(output) = commands::handle_local_command(cmd, args, &app.config_name) {
                            app.messages.push(DisplayMessage::system(&output));
                        }
                    } else {
                        // Send to brain via gRPC
                        let _ = in_tx.send(ConversationRequest {
                            request_type: Some(conversation_request::RequestType::SlashCommand(
                                SlashCommand { command: cmd.to_string(), args: args.to_string() }
                            )),
                        }).await;
                    }
                } else {
                    // Regular message
                    app.messages.push(DisplayMessage::user(&input));
                    let _ = in_tx.send(ConversationRequest {
                        request_type: Some(conversation_request::RequestType::UserMessage(
                            UserMessage { content: input }
                        )),
                    }).await;
                }
            }
        }

        // Alt+Enter — newline in input
        (KeyCode::Enter, KeyModifiers::ALT) => {
            app.input_buffer.insert(app.cursor_pos, '\n');
            app.cursor_pos += 1;
        }

        // Scroll
        (KeyCode::Up, _) => {
            app.scroll_offset = app.scroll_offset.saturating_add(1);
        }
        (KeyCode::Down, _) => {
            app.scroll_offset = app.scroll_offset.saturating_sub(1);
        }
        (KeyCode::PageUp, _) => {
            app.scroll_offset = app.scroll_offset.saturating_add(10);
        }
        (KeyCode::PageDown, _) => {
            app.scroll_offset = app.scroll_offset.saturating_sub(10);
        }

        // Backspace
        (KeyCode::Backspace, _) => {
            if app.cursor_pos > 0 {
                app.cursor_pos -= 1;
                app.input_buffer.remove(app.cursor_pos);
            }
        }

        // Delete
        (KeyCode::Delete, _) => {
            if app.cursor_pos < app.input_buffer.len() {
                app.input_buffer.remove(app.cursor_pos);
            }
        }

        // Home/End
        (KeyCode::Home, _) => {
            app.cursor_pos = 0;
        }
        (KeyCode::End, _) => {
            app.cursor_pos = app.input_buffer.len();
        }

        // Left/Right cursor
        (KeyCode::Left, _) => {
            app.cursor_pos = app.cursor_pos.saturating_sub(1);
        }
        (KeyCode::Right, _) => {
            if app.cursor_pos < app.input_buffer.len() {
                app.cursor_pos += 1;
            }
        }

        // Character input
        (KeyCode::Char(c), _) => {
            app.input_buffer.insert(app.cursor_pos, c);
            app.cursor_pos += 1;
            app.scroll_offset = 0; // Auto-scroll to bottom on typing
        }

        _ => {}
    }

    Ok(())
}
