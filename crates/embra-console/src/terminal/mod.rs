//! Full TUI terminal for embra-console.
//!
//! ratatui-based terminal driven by gRPC ConsoleEvents from embra-apid.
//! Renders the full Phase 0 visual experience: styled text, JSON highlighting,
//! thinking indicator, multi-line input, selectors, and mode transitions.
//!
//! All UI-agnostic logic (gRPC client, app state, console-event reduction,
//! reasoning buffer, slash-command parsing, styled-text parsers) lives in
//! `embra-console-core`. This module owns only the ratatui draw functions
//! and the crossterm-driven keyboard event loop.

mod input;
mod render;
mod ui;

use embra_console_core::commands;
use embra_console_core::events::handle_console_event;
use embra_console_core::grpc::BrainClient;
use embra_console_core::reasoning::{char_count, char_to_byte_pos};
use embra_console_core::state::{AppState, DisplayMessage};
use embra_common::proto::apid::*;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal::{self, disable_raw_mode, enable_raw_mode},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::stdout;
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
    app.viewport_cols = use_cols;
    app.viewport_rows = use_rows;

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

    // EXPR-01: 3-second polling tick for the expression panel.
    // Separate from the 200ms animation tick so the panel can change while
    // nothing else is happening.
    let mut expression_tick = tokio::time::interval(Duration::from_secs(3));
    expression_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick so the first poll happens after 3s.
    expression_tick.tick().await;

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
                        app.messages.push(DisplayMessage::system_with_tz("Disconnected from server.", &app.config_tz));
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

            // Expression panel poll (EXPR-01)
            _ = expression_tick.tick() => {
                if let Ok((content, version)) = client.get_expression().await {
                    if version != app.expression_version {
                        app.expression_content = content;
                        app.expression_version = version;
                    }
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

        // Enter — send input or selector choice (multi-line aware)
        (KeyCode::Enter, _) => {
            if let Some(selector) = app.selector.take() {
                // Send selector choice
                let choice = selector.current().to_string();
                app.live_reasoning.clear();
                let _ = in_tx.send(ConversationRequest {
                    request_type: Some(conversation_request::RequestType::UserMessage(
                        UserMessage { content: choice }
                    )),
                }).await;
            } else if let Some(pasted) = app.pasted_lines.take() {
                // Send pasted content
                let content = pasted.join("\n");
                app.messages.push(DisplayMessage::new_with_tz("user", &content, &app.config_tz));
                app.live_reasoning.clear();
                let _ = in_tx.send(ConversationRequest {
                    request_type: Some(conversation_request::RequestType::UserMessage(
                        UserMessage { content }
                    )),
                }).await;
            } else if app.multiline_mode {
                // Multi-line mode: check if the last line is "." (send terminator)
                let last_line_is_dot = app.input_buffer
                    .rsplit_once('\n')
                    .map(|(_, last)| last.trim() == ".")
                    .unwrap_or_else(|| app.input_buffer.trim() == ".");

                if last_line_is_dot {
                    // Remove the "." terminator line and send
                    if let Some(pos) = app.input_buffer.rfind('\n') {
                        app.input_buffer.truncate(pos);
                    } else {
                        // Buffer is just "." — clear it and don't send
                        app.input_buffer.clear();
                        app.cursor_pos = 0;
                        app.multiline_mode = false;
                        return Ok(());
                    }
                    let input = app.input_buffer.trim().to_string();
                    app.input_buffer.clear();
                    app.cursor_pos = 0;
                    app.multiline_mode = false;

                    if !input.is_empty() {
                        app.messages.push(DisplayMessage::new_with_tz("user", &input, &app.config_tz));
                        app.live_reasoning.clear();
                        let _ = in_tx.send(ConversationRequest {
                            request_type: Some(conversation_request::RequestType::UserMessage(
                                UserMessage { content: input }
                            )),
                        }).await;
                    }
                } else {
                    // Insert newline
                    let byte_pos = char_to_byte_pos(&app.input_buffer, app.cursor_pos);
                    app.input_buffer.insert(byte_pos, '\n');
                    app.cursor_pos += 1;
                }
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

                    // Handle /ml toggle locally (needs mutable app state)
                    if cmd == "/ml" {
                        app.multiline_mode = !app.multiline_mode;
                        let status = if app.multiline_mode {
                            "Multi-line mode ON. Type on multiple lines, then '.' on its own line to send."
                        } else {
                            "Multi-line mode OFF."
                        };
                        app.messages.push(DisplayMessage::system_with_tz(status, &app.config_tz));
                    } else if commands::is_local_command(cmd) {
                        if let Some(output) = commands::handle_local_command(cmd, args, &app.config_name) {
                            app.messages.push(DisplayMessage::system_with_tz(&output, &app.config_tz));
                        }
                    } else {
                        // Send to brain via gRPC
                        app.live_reasoning.clear();
                        let _ = in_tx.send(ConversationRequest {
                            request_type: Some(conversation_request::RequestType::SlashCommand(
                                SlashCommand { command: cmd.to_string(), args: args.to_string() }
                            )),
                        }).await;
                    }
                } else {
                    // Regular message
                    app.messages.push(DisplayMessage::new_with_tz("user", &input, &app.config_tz));
                    app.live_reasoning.clear();
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
            let byte_pos = char_to_byte_pos(&app.input_buffer, app.cursor_pos);
            app.input_buffer.insert(byte_pos, '\n');
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
                let byte_pos = char_to_byte_pos(&app.input_buffer, app.cursor_pos);
                app.input_buffer.remove(byte_pos);
            }
        }

        // Delete
        (KeyCode::Delete, _) => {
            if app.cursor_pos < char_count(&app.input_buffer) {
                let byte_pos = char_to_byte_pos(&app.input_buffer, app.cursor_pos);
                app.input_buffer.remove(byte_pos);
            }
        }

        // Home/End
        (KeyCode::Home, _) => {
            app.cursor_pos = 0;
        }
        (KeyCode::End, _) => {
            app.cursor_pos = char_count(&app.input_buffer);
        }

        // Left/Right cursor
        (KeyCode::Left, _) => {
            app.cursor_pos = app.cursor_pos.saturating_sub(1);
        }
        (KeyCode::Right, _) => {
            if app.cursor_pos < char_count(&app.input_buffer) {
                app.cursor_pos += 1;
            }
        }

        // Character input
        (KeyCode::Char(c), _) => {
            let byte_pos = char_to_byte_pos(&app.input_buffer, app.cursor_pos);
            app.input_buffer.insert(byte_pos, c);
            app.cursor_pos += 1;
            app.scroll_offset = 0; // Auto-scroll to bottom on typing
        }

        _ => {}
    }

    Ok(())
}
