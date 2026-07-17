//! Full TUI terminal for embra-console.
//!
//! ratatui-based terminal driven by gRPC ConsoleEvents from embra-apid.
//! Renders the full Phase 0 visual experience: styled text, JSON highlighting,
//! thinking indicator, multi-line input, selectors, and mode transitions.

mod commands;
mod input;
mod input_layout;
mod render;
pub mod state;
mod ui;

use state::*;
use crate::grpc_client::{BrainClient, ConsoleEvent};
use embra_common::proto::apid::*;

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent,
        KeyModifiers,
    },
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

    // Serial console: TIOCGWINSZ is unreliable over QEMU -nographic, so the
    // viewport is pinned to the cmdline-provided size (Viewport::Fixed).
    // Web/PTY console (EMBRA_WEB_PTY=1 — spawned by embra-web with no
    // columns/rows override): the PTY has a real, dynamic winsize driven by
    // xterm.js, so use the size-tracking backend that reflows on resize.
    let web_pty = std::env::var("EMBRA_WEB_PTY").is_ok();
    if web_pty {
        // Web/PTY only: lets crossterm coalesce the embra-web /ml editor's
        // `\x1b[200~ … \x1b[201~` blob into a single Event::Paste. The
        // serial Viewport::Fixed path deliberately never enables this, so
        // it stays bit-identical. Best-effort: if it fails, the wrapper
        // bytes arrive as ordinary keys — no worse than before.
        let _ = stdout().execute(EnableBracketedPaste);
    }
    let use_cols = if cols > 0 { cols } else { 80 };
    let use_rows = if rows > 0 { rows } else { 24 };

    let backend = CrosstermBackend::new(stdout());
    let mut terminal_tui = if web_pty {
        // ratatui's full-screen Terminal re-reads the backend size on every
        // draw() (autoresize), so it reflows automatically once the PTY
        // winsize changes and crossterm delivers Event::Resize (handled
        // in the event loop below).
        Terminal::new(backend)?
    } else {
        Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(
                    0, 0, use_cols, use_rows,
                )),
            },
        )?
    };
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
                match ev {
                    Event::Key(key) => handle_key_event(key, &mut app, &in_tx).await?,
                    // PTY/web mode: xterm.js → embra-web → TIOCSWINSZ →
                    // crossterm Event::Resize. The size-tracking backend
                    // reflows on the next draw(); we only refresh the
                    // viewport dims the renderer's manual wrapping reads.
                    // (Serial/Fixed mode never emits Resize — no-op there.)
                    Event::Resize(c, r) => {
                        app.viewport_cols = c;
                        app.viewport_rows = r;
                    }
                    // Web/PTY: a bracketed-paste blob (the embra-web /ml
                    // editor injects `\x1b[200~ … \x1b[201~`). Stage it
                    // for the existing verbatim send path — the next Enter
                    // takes pasted_lines and sends `pasted.join("\n")` as
                    // one UserMessage (no trim, no slash parse). crossterm
                    // strips CRs from paste content; split on '\n' only.
                    // Only ever produced when bracketed paste was enabled
                    // (web_pty-gated above), so the serial path is unaffected.
                    Event::Paste(s) => {
                        app.pasted_lines =
                            Some(s.split('\n').map(str::to_string).collect());
                    }
                    _ => {}
                }
            }

            // Expression panel poll (EXPR-01)
            _ = expression_tick.tick() => {
                if let Ok((content, version)) = client.get_expression().await {
                    if version != app.expression_version {
                        app.expression_content = content;
                        app.expression_version = version;
                        // New content — snap the panel scroll back to the
                        // tail so the operator isn't pinned into the old
                        // expression.
                        app.expression_scroll = 0;
                    }
                }
            }

            // Tick for animations (thinking dots)
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                // Just redraw
            }
        }

        // Out-of-band stop, consumed OUTSIDE the select! so the unary
        // call doesn't contend with the expression-poll arm for `client`
        // (two &mut client select arms would not compile). Set by the
        // /stop intercept and by Esc-while-busy.
        if app.stop_requested {
            app.stop_requested = false;
            let line = match client.stop_turn().await {
                Ok(true) => "Stop requested — interrupting the current turn…".to_string(),
                Ok(false) => "No turn in flight — nothing to stop.".to_string(),
                Err(e) => format!("Stop failed: {}", e),
            };
            app.messages.push(DisplayMessage::system_with_tz(&line, &app.config_tz));
        }
    }

    // Cleanup
    if web_pty {
        let _ = stdout().execute(DisableBracketedPaste);
    }
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
            // Live reasoning is intentionally NOT cleared here — the
            // operator can keep reading the last turn's reasoning until
            // they submit their next message (clear sites: user submit,
            // SystemMessage::Error, ModeTransition).
            app.messages.push(DisplayMessage::new_with_tz(&app.config_name, &full, &app.config_tz));
            app.scroll_offset = 0;
        }
        ConsoleEvent::SystemMessage { content, msg_type } => {
            // SYSTEM_MESSAGE_TYPE_ERROR == 3. Brain failures during a
            // turn should not leave stale reasoning on the panel.
            if msg_type == "3" {
                app.clear_live_reasoning();
            }
            app.messages.push(DisplayMessage::system_with_tz(&content, &app.config_tz));
            app.scroll_offset = 0;
        }
        ConsoleEvent::ToolExecution {
            name,
            input_json,
            result,
            is_error,
            ..
        } => {
            app.messages.push(DisplayMessage::tool_native(
                &name,
                &input_json,
                &result,
                is_error,
                &app.config_tz,
            ));
            app.scroll_offset = 0;
        }
        ConsoleEvent::ThinkingState { is_thinking, name, current_tool } => {
            app.thinking = is_thinking;
            if !name.is_empty() {
                app.thinking_name = name;
            }
            match current_tool {
                Some(tool) => {
                    // Only restart the elapsed clock if the tool name
                    // actually changed — duplicate signals during a single
                    // dispatch shouldn't reset the displayed "(Ns)".
                    if app.current_tool.as_deref() != Some(&tool) {
                        app.current_tool_started = Some(std::time::Instant::now());
                    }
                    app.current_tool = Some(tool);
                }
                None => {
                    app.current_tool = None;
                    app.current_tool_started = None;
                }
            }
        }
        ConsoleEvent::ModeTransition { from_mode: _, to_mode, message } => {
            // Mode change resets per-turn UI context — drop any pending
            // reasoning from the prior phase.
            app.clear_live_reasoning();
            // Parse name from message (format: "... — Name: <name> — ...")
            if let Some(name_part) = message.split("Name: ").nth(1) {
                let name = name_part.split(" — ").next().unwrap_or(name_part).trim().to_string();
                if !name.is_empty() {
                    app.config_name = name.clone();
                    app.thinking_name = name;
                }
            }

            // Parse timezone from message (format: "... — TZ: <tz>")
            if let Some(tz_part) = message.split("TZ: ").nth(1) {
                let tz = tz_part.split(" — ").next().unwrap_or(tz_part).trim().to_string();
                if !tz.is_empty() {
                    app.config_tz = tz;
                }
            }

            // Parse brain model from message (format: "... — Brain: <model>")
            // Added in Sprint 4 so the status bar reflects the active
            // provider after wizard / /provider switches without a
            // proto-level addition.
            if let Some(brain_part) = message.split("Brain: ").nth(1) {
                let model = brain_part.split(" — ").next().unwrap_or(brain_part).trim().to_string();
                if !model.is_empty() {
                    app.provider_model = model;
                }
            }

            // to_mode: 1=Setup, 2=Learning, 3=Operational
            match to_mode {
                1 => {
                    app.mode = AppMode::Setup(SetupState { step: SetupStep::Name });
                }
                2 => {
                    app.mode = AppMode::Learning;
                }
                3 => {
                    // Extract session name (format: "Operational — Name: <name> — Session: <name> — TZ: <tz>")
                    let session = message.split("Session: ")
                        .nth(1)
                        .and_then(|s| s.split(" — ").next())
                        .unwrap_or("main")
                        .to_string();
                    app.mode = AppMode::Operational { session_name: session };
                }
                _ => {}
            }
            // Don't display the raw ModeTransition message (it has internal metadata)
            app.scroll_offset = 0;
        }
        ConsoleEvent::SetupPrompt { field_type, prompt, options, default_value } => {
            app.messages.push(DisplayMessage::system_with_tz(&prompt, &app.config_tz));

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
        ConsoleEvent::ReasoningDelta(text) => {
            append_live_reasoning(&mut app.live_reasoning, &text);
        }
    }
}

/// Append a reasoning shard to the live buffer, hard-capping at
/// `MAX_LIVE_REASONING_BYTES` (64 KiB). When the cap would be
/// exceeded we drop the oldest content (UTF-8 boundary safe via
/// `char_indices`). The buffer is transient — cleared at turn end —
/// so the cap exists only to prevent pathological streams from
/// growing console memory unbounded.
fn append_live_reasoning(buffer: &mut String, shard: &str) {
    const MAX_LIVE_REASONING_BYTES: usize = 64 * 1024;
    if shard.is_empty() {
        return;
    }
    if shard.len() >= MAX_LIVE_REASONING_BYTES {
        // Single shard already exceeds cap — keep only its tail.
        let tail_start = shard.len() - MAX_LIVE_REASONING_BYTES;
        let safe_start = shard
            .char_indices()
            .find(|(i, _)| *i >= tail_start)
            .map(|(i, _)| i)
            .unwrap_or(shard.len());
        buffer.clear();
        buffer.push_str(&shard[safe_start..]);
        return;
    }
    let needed = buffer.len() + shard.len();
    if needed > MAX_LIVE_REASONING_BYTES {
        // Drop oldest bytes to make room. Walk forward to a UTF-8
        // boundary >= drop_amount so we never split a char.
        let drop_amount = needed - MAX_LIVE_REASONING_BYTES;
        let safe_drop = buffer
            .char_indices()
            .find(|(i, _)| *i >= drop_amount)
            .map(|(i, _)| i)
            .unwrap_or(buffer.len());
        buffer.replace_range(..safe_drop, "");
    }
    buffer.push_str(shard);
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
                app.clear_live_reasoning();
                let _ = in_tx.send(ConversationRequest {
                    request_type: Some(conversation_request::RequestType::UserMessage(
                        UserMessage { content: choice }
                    )),
                }).await;
            } else if let Some(pasted) = app.pasted_lines.take() {
                let content = pasted.join("\n");
                if app.guardian_capture {
                    // Guardian capture: deliver the pasted module to the
                    // brain's /guardian define path, not a model turn.
                    app.guardian_capture = false;
                    app.multiline_mode = false;
                    app.messages.push(DisplayMessage::system_with_tz("Submitting Guardian tool module…", &app.config_tz));
                    app.clear_live_reasoning();
                    let _ = in_tx.send(ConversationRequest {
                        request_type: Some(conversation_request::RequestType::SlashCommand(
                            SlashCommand { command: "/guardian".to_string(), args: format!("define\n{content}") }
                        )),
                    }).await;
                } else {
                    // Send pasted content
                    app.messages.push(DisplayMessage::new_with_tz("user", &content, &app.config_tz));
                    app.clear_live_reasoning();
                    let _ = in_tx.send(ConversationRequest {
                        request_type: Some(conversation_request::RequestType::UserMessage(
                            UserMessage { content }
                        )),
                    }).await;
                }
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
                        app.guardian_capture = false;
                        return Ok(());
                    }
                    let input = app.input_buffer.trim().to_string();
                    app.input_buffer.clear();
                    app.cursor_pos = 0;
                    app.multiline_mode = false;
                    let guardian = app.guardian_capture;
                    app.guardian_capture = false;

                    if !input.is_empty() {
                        if guardian {
                            // Guardian capture: deliver the typed module to
                            // the brain's /guardian define path.
                            app.messages.push(DisplayMessage::system_with_tz("Submitting Guardian tool module…", &app.config_tz));
                            app.clear_live_reasoning();
                            let _ = in_tx.send(ConversationRequest {
                                request_type: Some(conversation_request::RequestType::SlashCommand(
                                    SlashCommand { command: "/guardian".to_string(), args: format!("define\n{input}") }
                                )),
                            }).await;
                        } else {
                            app.messages.push(DisplayMessage::new_with_tz("user", &input, &app.config_tz));
                            app.clear_live_reasoning();
                            let _ = in_tx.send(ConversationRequest {
                                request_type: Some(conversation_request::RequestType::UserMessage(
                                    UserMessage { content: input }
                                )),
                            }).await;
                        }
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
                    } else if cmd == "/guardian-define" {
                        // Enter Guardian capture (reuses multi-line accumulation +
                        // bracketed paste). The next submitted buffer is sent as
                        // SlashCommand{"/guardian","define\n<module>"}.
                        app.guardian_capture = true;
                        app.multiline_mode = true;
                        app.messages.push(DisplayMessage::system_with_tz(
                            "Guardian: paste your Rust tool module (marker + GUARDIAN_* + fn run), then send with a lone '.' on its own line (serial) or paste-and-Enter (web). It will be validated and built in the background.",
                            &app.config_tz,
                        ));
                    } else if cmd == "/stop" {
                        // Not sent in-band: the brain's Converse loop is
                        // parked inside the running turn, so a streamed
                        // SlashCommand would queue behind it. The main
                        // loop consumes this flag and fires the unary
                        // StopTurn RPC out-of-band (get_expression
                        // precedent — same channel, separate RPC).
                        app.stop_requested = true;
                    } else if commands::is_local_command(cmd) {
                        if let Some(output) = commands::handle_local_command(cmd, args, &app.config_name) {
                            app.messages.push(DisplayMessage::system_with_tz(&output, &app.config_tz));
                        }
                    } else {
                        // Send to brain via gRPC
                        app.clear_live_reasoning();
                        let _ = in_tx.send(ConversationRequest {
                            request_type: Some(conversation_request::RequestType::SlashCommand(
                                SlashCommand { command: cmd.to_string(), args: args.to_string() }
                            )),
                        }).await;
                    }
                } else {
                    // Regular message
                    app.messages.push(DisplayMessage::new_with_tz("user", &input, &app.config_tz));
                    app.clear_live_reasoning();
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

        // Expression-panel scroll (Shift chords). MUST sit before the
        // bare `(KeyCode::Up, _)` conversation-scroll arms below — the
        // `_` modifier wildcard would swallow Shift too (the exact
        // shadowing that makes the Alt+Enter arm above unreachable).
        // Guarded with `.contains` so SHIFT combined with other
        // modifiers still routes here. No-op while the panel is hidden
        // (small terminals) so invisible state can't drift. Offsets
        // count rows from the bottom, mirroring `scroll_offset`.
        (KeyCode::Up, m) if m.contains(KeyModifiers::SHIFT) => {
            if app.expression_panel_visible() {
                app.expression_scroll = app.expression_scroll.saturating_add(1);
            }
        }
        (KeyCode::Down, m) if m.contains(KeyModifiers::SHIFT) => {
            if app.expression_panel_visible() {
                app.expression_scroll = app.expression_scroll.saturating_sub(1);
            }
        }
        (KeyCode::PageUp, m) if m.contains(KeyModifiers::SHIFT) => {
            if app.expression_panel_visible() {
                app.expression_scroll = app.expression_scroll.saturating_add(5);
            }
        }
        (KeyCode::PageDown, m) if m.contains(KeyModifiers::SHIFT) => {
            if app.expression_panel_visible() {
                app.expression_scroll = app.expression_scroll.saturating_sub(5);
            }
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

        // Esc = operator stop, only while a turn is busy (thinking or
        // streaming) — an idle Esc stays a no-op so it can't grow
        // surprising meanings. The main loop consumes the flag and fires
        // the out-of-band StopTurn unary.
        (KeyCode::Esc, _) => {
            if app.thinking || app.streaming_text.is_some() {
                app.stop_requested = true;
            }
        }

        _ => {}
    }

    Ok(())
}

/// Convert a char index to a byte index in a string.
/// cursor_pos is a character offset; String::insert/remove need byte offsets.
fn char_to_byte_pos(s: &str, char_pos: usize) -> usize {
    s.char_indices()
        .nth(char_pos)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(s.len())
}

/// Return the number of characters in a string (not bytes).
fn char_count(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
mod reasoning_tests {
    use super::*;

    #[test]
    fn reasoning_delta_appends_to_live_buffer() {
        let mut app = AppState::new();
        handle_console_event(ConsoleEvent::ReasoningDelta("first ".to_string()), &mut app);
        handle_console_event(ConsoleEvent::ReasoningDelta("second".to_string()), &mut app);
        assert_eq!(app.live_reasoning, "first second");
    }

    #[test]
    fn response_done_preserves_live_reasoning() {
        // Lifecycle: live reasoning persists past ResponseDone so the
        // operator can keep reading the last turn's reasoning between
        // turns. Cleared only on the next user submit (or error /
        // mode change).
        let mut app = AppState::new();
        app.live_reasoning = "kept across the gap".to_string();
        handle_console_event(ConsoleEvent::ResponseDone("ok".to_string()), &mut app);
        assert_eq!(app.live_reasoning, "kept across the gap");
    }

    #[test]
    fn error_system_message_clears_live_reasoning() {
        let mut app = AppState::new();
        app.live_reasoning = "in flight".to_string();
        handle_console_event(
            ConsoleEvent::SystemMessage {
                content: "boom".to_string(),
                msg_type: "3".to_string(), // SYSTEM_MESSAGE_TYPE_ERROR
            },
            &mut app,
        );
        assert!(app.live_reasoning.is_empty());
    }

    #[test]
    fn info_system_message_does_not_clear_live_reasoning() {
        let mut app = AppState::new();
        app.live_reasoning = "in flight".to_string();
        handle_console_event(
            ConsoleEvent::SystemMessage {
                content: "fyi".to_string(),
                msg_type: "1".to_string(), // SYSTEM_MESSAGE_TYPE_INFO
            },
            &mut app,
        );
        assert_eq!(app.live_reasoning, "in flight");
    }

    #[test]
    fn mode_transition_clears_live_reasoning() {
        let mut app = AppState::new();
        app.live_reasoning = "old phase".to_string();
        handle_console_event(
            ConsoleEvent::ModeTransition {
                from_mode: 2,
                to_mode: 3,
                message: "Operational — Name: Embra — Session: main — TZ: UTC".to_string(),
            },
            &mut app,
        );
        assert!(app.live_reasoning.is_empty());
    }

    #[test]
    fn append_live_reasoning_caps_at_64_kib_with_tail_keep() {
        // Pathological single-shard input larger than the cap should
        // truncate to the tail.
        let mut buf = String::new();
        let big = "a".repeat(64 * 1024 + 100);
        append_live_reasoning(&mut buf, &big);
        assert_eq!(buf.len(), 64 * 1024);
        // Tail of the original input is what remains.
        assert!(buf.ends_with(&"a".repeat(10)));
    }

    #[test]
    fn append_live_reasoning_drops_oldest_to_fit_within_cap() {
        let mut buf = "x".repeat(64 * 1024 - 5);
        append_live_reasoning(&mut buf, "yyyyyyyyyy"); // 10 bytes
        assert_eq!(buf.len(), 64 * 1024 - 5 + 10 - 5);
        // Note: 5 bytes dropped from the front; the new shard's full
        // 10 bytes are appended.
        assert!(buf.ends_with("yyyyyyyyyy"));
    }

    #[test]
    fn append_live_reasoning_handles_utf8_drop_safely() {
        // Multi-byte chars at the drop boundary must not be split.
        let mut buf = "é".repeat(32 * 1024); // 2 bytes per é = 64 KiB exactly
        let original_byte_len = buf.len();
        append_live_reasoning(&mut buf, "abc");
        assert!(buf.len() <= 64 * 1024);
        assert!(buf.ends_with("abc"));
        // Ensure no panic from splitting a multi-byte char — buffer
        // remains valid UTF-8 (which String guarantees structurally
        // anyway, but the test crashes if `replace_range` were called
        // on a non-boundary).
        assert!(std::str::from_utf8(buf.as_bytes()).is_ok());
        assert!(buf.len() < original_byte_len + 3 + 1); // grew by ≤ shard+1
    }

    #[test]
    fn empty_shard_is_no_op() {
        let mut buf = "kept".to_string();
        append_live_reasoning(&mut buf, "");
        assert_eq!(buf, "kept");
    }

    #[test]
    fn clear_live_reasoning_resets_panel_scroll() {
        // The clear helper is the single seam every clear site routes
        // through — reasoning and scroll offset must drop together so
        // the operator is never scrolled into vanished content.
        let mut app = AppState::new();
        app.live_reasoning = "some reasoning".to_string();
        app.expression_scroll = 7;
        app.clear_live_reasoning();
        assert!(app.live_reasoning.is_empty());
        assert_eq!(app.expression_scroll, 0);
    }

    #[test]
    fn error_system_message_resets_panel_scroll() {
        let mut app = AppState::new();
        app.live_reasoning = "reasoning".to_string();
        app.expression_scroll = 3;
        handle_console_event(
            ConsoleEvent::SystemMessage {
                content: "Brain error: boom".to_string(),
                msg_type: "3".to_string(),
            },
            &mut app,
        );
        assert!(app.live_reasoning.is_empty());
        assert_eq!(app.expression_scroll, 0);

        // Non-error system frames keep both.
        let mut app = AppState::new();
        app.live_reasoning = "reasoning".to_string();
        app.expression_scroll = 3;
        handle_console_event(
            ConsoleEvent::SystemMessage {
                content: "info".to_string(),
                msg_type: "1".to_string(),
            },
            &mut app,
        );
        assert_eq!(app.live_reasoning, "reasoning");
        assert_eq!(app.expression_scroll, 3);
    }

    #[test]
    fn mode_transition_resets_panel_scroll_but_response_done_keeps_it() {
        let mut app = AppState::new();
        app.expression_scroll = 4;
        handle_console_event(
            ConsoleEvent::ModeTransition {
                from_mode: 3,
                to_mode: 3,
                message: "Operational — Name: Embra — Session: main — TZ: UTC — Brain: opus-4.8"
                    .to_string(),
            },
            &mut app,
        );
        assert_eq!(app.expression_scroll, 0);

        // ResponseDone deliberately preserves reasoning AND the scroll
        // position — the operator may be mid-review when the turn ends.
        let mut app = AppState::new();
        app.live_reasoning = "kept".to_string();
        app.expression_scroll = 4;
        handle_console_event(ConsoleEvent::ResponseDone("ok".to_string()), &mut app);
        assert_eq!(app.live_reasoning, "kept");
        assert_eq!(app.expression_scroll, 4);
    }
}

#[cfg(test)]
mod paste_tests {
    use super::*;

    // Mirrors the Event::Paste dispatch arm and the pasted_lines consume
    // path (the next Enter sends `pasted.join("\n")` as one UserMessage,
    // no trim, no slash parse). A bracketed-paste blob is split on '\n'
    // into pasted_lines; this `stage` fn is byte-identical to the arm.
    fn stage(s: &str) -> Vec<String> {
        s.split('\n').map(str::to_string).collect()
    }

    #[test]
    fn paste_stages_pasted_lines() {
        let mut app = AppState::new();
        app.pasted_lines = Some(stage("a\nb\n."));
        // The lone "." line is preserved verbatim — the property the
        // /ml dot-terminator path could not guarantee.
        assert_eq!(
            app.pasted_lines,
            Some(vec!["a".to_string(), "b".to_string(), ".".to_string()])
        );
    }

    #[test]
    fn pasted_lines_join_roundtrips_verbatim() {
        // Leading '/', a lone '.' line, and surrounding whitespace all
        // survive the split→join round-trip — the core correctness claim.
        let staged = stage("/status\nline 2\n.\n  trailing  ");
        assert_eq!(staged.join("\n"), "/status\nline 2\n.\n  trailing  ");
    }

    #[test]
    fn empty_paste_stages_single_empty_line() {
        // "".split('\n') yields [""]; join is "". The web-ui empty-guard
        // (trim_end_matches('\n') + is_empty) is what prevents an empty
        // UserMessage being sent — this documents the console side.
        let staged = stage("");
        assert_eq!(staged, vec![String::new()]);
        assert_eq!(staged.join("\n"), "");
    }

    #[test]
    fn guardian_capture_wraps_module_as_define_slash() {
        // Documents the console side: with guardian_capture set, the
        // staged module is delivered to the brain as
        // SlashCommand{"/guardian","define\n<verbatim module>"} — not a
        // UserMessage. The split→join is verbatim (same property the
        // pasted_lines path guarantees), so the module reaches the
        // validator byte-for-byte.
        let module = "// guardian-tool: web_search\nconst GUARDIAN_NAME: &str = \"web_search\";\nfn run(i: &str) -> String { String::new() }";
        let staged = stage(module);
        let args = format!("define\n{}", staged.join("\n"));
        assert_eq!(args, format!("define\n{module}"));
        assert!(args.starts_with("define\n// guardian-tool: web_search"));
    }

    #[test]
    fn guardian_capture_defaults_off() {
        // The serial path stays byte-identical: the flag is false unless
        // /guardian-define explicitly sets it.
        assert!(!AppState::new().guardian_capture);
    }
}
