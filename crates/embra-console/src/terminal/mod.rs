//! Minimal terminal for embra-console Phase 1.
//!
//! Connects to embra-brain via embra-apid gRPC and renders a basic
//! conversational experience over serial/TTY. Full Phase 0 TUI
//! adaptation (styled rendering, JSON highlighting, etc.) is a follow-up.

// Phase 0 modules — available for future full TUI adaptation
// mod commands;
// mod input;
// mod render;
// mod ui;

use crate::grpc_client::{BrainClient, ConsoleEvent};
use embra_common::proto::apid::*;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{self, disable_raw_mode, enable_raw_mode};
use std::io::{self, Write};
use tracing::info;

pub async fn run(mut client: BrainClient, _device: Option<String>) -> Result<()> {
    info!("Terminal starting");

    // Open conversation stream
    let (in_tx, mut out_rx) = client.open_conversation("").await?;

    // Use raw mode for better input handling
    enable_raw_mode()?;
    let _raw_guard = RawModeGuard;

    // Get terminal size with fallback for serial
    let (cols, _rows) = terminal::size().unwrap_or((80, 24));

    print!("\x1b[2J\x1b[H"); // Clear screen
    print!("\r\n  embraOS Phase 1 — Serial Console\r\n");
    print!("  Type a message and press Enter. Ctrl-C to exit.\r\n");
    print!("{}\r\n", "─".repeat(cols as usize));
    print!("\r\n> ");
    io::stdout().flush()?;

    let mut input_buffer = String::new();
    let mut setup_default: Option<String> = None;
    let mut is_thinking = false;
    let mut in_response = false;

    loop {
        tokio::select! {
            // Handle gRPC events
            event = out_rx.recv() => {
                match event {
                    Some(ConsoleEvent::Token(text)) => {
                        if is_thinking {
                            print!("\r\x1b[K"); // Clear thinking line
                            is_thinking = false;
                        }
                        if !in_response {
                            print!("\r\n\x1b[32m"); // Green for AI
                            in_response = true;
                        }
                        print!("{}", text);
                        io::stdout().flush()?;
                    }
                    Some(ConsoleEvent::ResponseDone(_full)) => {
                        if in_response {
                            print!("\x1b[0m\r\n"); // Reset color
                            in_response = false;
                        }
                        print!("\r\n> ");
                        io::stdout().flush()?;
                    }
                    Some(ConsoleEvent::SystemMessage { content, .. }) => {
                        if is_thinking {
                            print!("\r\x1b[K");
                            is_thinking = false;
                        }
                        print!("\r\n\x1b[33m{}\x1b[0m\r\n", content); // Yellow for system
                        if !in_response {
                            print!("> ");
                        }
                        io::stdout().flush()?;
                    }
                    Some(ConsoleEvent::ToolExecution { name, result, success, .. }) => {
                        let color = if success { "36" } else { "31" }; // Cyan or red
                        print!("\r\n\x1b[{}m[{}] {}\x1b[0m\r\n", color, name, result);
                        io::stdout().flush()?;
                    }
                    Some(ConsoleEvent::ThinkingState { is_thinking: thinking, name }) => {
                        if thinking {
                            print!("\r\x1b[90m{} is thinking...\x1b[0m", name);
                            io::stdout().flush()?;
                            is_thinking = true;
                        } else if is_thinking {
                            print!("\r\x1b[K");
                            is_thinking = false;
                        }
                    }
                    Some(ConsoleEvent::ModeTransition { message, .. }) => {
                        print!("\r\n\x1b[35m══ {} ══\x1b[0m\r\n\r\n> ", message);
                        io::stdout().flush()?;
                    }
                    Some(ConsoleEvent::SetupPrompt { field_type, prompt, options, default_value }) => {
                        print!("\r\n\x1b[33m{}\x1b[0m", prompt);
                        if !default_value.is_empty() {
                            print!(" \x1b[90m[default: {}]\x1b[0m", default_value);
                        }
                        print!("\r\n");
                        if !options.is_empty() {
                            for (i, opt) in options.iter().enumerate() {
                                print!("  {}. {}\r\n", i + 1, opt);
                            }
                        }
                        print!("> ");
                        io::stdout().flush()?;
                        // Store default so Enter with empty input uses it
                        setup_default = Some(default_value);
                    }
                    None => {
                        print!("\r\n\x1b[31m[Disconnected from server]\x1b[0m\r\n");
                        io::stdout().flush()?;
                        break;
                    }
                }
            }
            // Handle keyboard input (polled)
            _ = tokio::task::spawn_blocking(|| event::poll(std::time::Duration::from_millis(50))) => {
                if event::poll(std::time::Duration::from_millis(0))? {
                    if let Event::Key(key) = event::read()? {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('c'), KeyModifiers::CONTROL) |
                            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                                print!("\r\n");
                                break;
                            }
                            (KeyCode::Enter, _) => {
                                let mut input = input_buffer.trim().to_string();
                                input_buffer.clear();
                                print!("\r\n");
                                io::stdout().flush()?;

                                // Use setup default if input is empty and a default is set
                                if input.is_empty() {
                                    if let Some(default) = setup_default.take() {
                                        if !default.is_empty() {
                                            input = default;
                                        } else {
                                            print!("> ");
                                            io::stdout().flush()?;
                                            continue;
                                        }
                                    } else {
                                        print!("> ");
                                        io::stdout().flush()?;
                                        continue;
                                    }
                                }
                                setup_default = None;

                                // Check for slash commands
                                if input.starts_with('/') {
                                    let parts: Vec<&str> = input.splitn(2, ' ').collect();
                                    let cmd = parts[0];
                                    let args = if parts.len() > 1 { parts[1] } else { "" };
                                    let _ = in_tx.send(ConversationRequest {
                                        request_type: Some(conversation_request::RequestType::SlashCommand(
                                            SlashCommand { command: cmd.to_string(), args: args.to_string() }
                                        )),
                                    }).await;
                                } else {
                                    let _ = in_tx.send(ConversationRequest {
                                        request_type: Some(conversation_request::RequestType::UserMessage(
                                            UserMessage { content: input }
                                        )),
                                    }).await;
                                }
                            }
                            (KeyCode::Backspace, _) => {
                                if !input_buffer.is_empty() {
                                    input_buffer.pop();
                                    print!("\x08 \x08"); // Erase character
                                    io::stdout().flush()?;
                                }
                            }
                            (KeyCode::Char(c), _) => {
                                input_buffer.push(c);
                                print!("{}", c);
                                io::stdout().flush()?;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// RAII guard to restore terminal state on exit
struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        print!("\r\n");
    }
}
