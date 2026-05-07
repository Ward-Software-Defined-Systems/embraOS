//! Reduce gRPC `ConsoleEvent`s into `AppState` mutations.
//!
//! Pure mutation, no UI calls — both the TUI and GUI clients route
//! incoming events through this function and re-render their own
//! surface afterwards.

use crate::grpc::ConsoleEvent;
use crate::reasoning::append_live_reasoning;
use crate::state::{AppMode, AppState, DisplayMessage, Selector, SetupState, SetupStep};

pub fn handle_console_event(event: ConsoleEvent, app: &mut AppState) {
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
                app.live_reasoning.clear();
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
        ConsoleEvent::ThinkingState { is_thinking, name } => {
            app.thinking = is_thinking;
            if !name.is_empty() {
                app.thinking_name = name;
            }
        }
        ConsoleEvent::ModeTransition { from_mode: _, to_mode, message } => {
            // Mode change resets per-turn UI context — drop any pending
            // reasoning from the prior phase.
            app.live_reasoning.clear();
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

#[cfg(test)]
mod tests {
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
}
