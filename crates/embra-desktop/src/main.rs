//! embra-desktop — iced GUI client for embraOS.
//!
//! Stage 4b wires a gRPC subscription bridge: a long-lived async task
//! connects to `embra-apid`, opens the bidirectional Converse stream,
//! and forwards each `ConsoleEvent` into iced as a `Message::GrpcEvent`.
//! `Message::Submit` routes input through `embra_console_core::commands`
//! (local) or sends it down the `ConversationRequest` channel (brain).
//!
//! Stages still pending:
//! - 4c: Setup wizard modal rendering for `AppMode::Setup`, multi-line
//!   mode, keyboard shortcuts beyond Enter (Ctrl+C, scroll, Alt+Enter).
//! - 4d: Theme polish.

mod subscription;
mod theme;
mod view;

use clap::Parser;
use embra_common::proto::apid::{
    conversation_request, ConversationRequest, SlashCommand, UserMessage,
};
use embra_console_core::commands;
use embra_console_core::events::handle_console_event;
use embra_console_core::grpc::ConsoleEvent;
use embra_console_core::state::{AppState, DisplayMessage};
use iced::widget::{operation, Id};
use iced::{Element, Size, Subscription, Task, Theme};
use once_cell::sync::Lazy;
use tokio::sync::mpsc;

/// Stable ID for the conversation scrollable so `operation::snap_to_end`
/// can target it from `update`. `Id::unique()` would generate a fresh
/// ID per `view()` call, breaking the link between update and the live
/// widget.
static CONVERSATION_SCROLL_ID: Lazy<Id> = Lazy::new(Id::unique);

pub fn conversation_scroll_id() -> &'static Id {
    &CONVERSATION_SCROLL_ID
}

#[derive(Parser, Debug, Clone)]
#[command(version, about = "embra-desktop — iced GUI client for embraOS")]
struct Args {
    /// embra-apid gRPC endpoint.
    #[arg(long, default_value = "http://127.0.0.1:50000")]
    apid_addr: String,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// Operator typed into the input box.
    InputChanged(String),
    /// Operator pressed Enter — submit the current input (or selector).
    Submit,
    /// gRPC subscription handshake completed; carries the request channel
    /// so `update` can later send `UserMessage` / `SlashCommand`.
    GrpcConnected(mpsc::Sender<ConversationRequest>),
    /// One brain-side event arrived over the stream.
    GrpcEvent(ConsoleEvent),
    /// Up arrow pressed. If a selector is active, navigates it; else
    /// scrolls conversation history up.
    ArrowUp,
    /// Down arrow pressed. If a selector is active, navigates it; else
    /// scrolls conversation history down.
    ArrowDown,
    /// PageUp / PageDown — scroll history by 10 lines.
    PageUp,
    PageDown,
    /// Ctrl+C / Ctrl+D — quit the application gracefully.
    Quit,
    /// 3-second tick for the expression-panel poll (Stage 4d will hook up
    /// `BrainClient::get_expression`).
    ExpressionTick,
    /// 200 ms tick for thinking-dot animation (no-op until Stage 4d
    /// renders the dots).
    AnimationTick,
}

pub struct EmbraDesktop {
    state: AppState,
    apid_addr: String,
    /// Outbound side of the Converse stream. `None` until the
    /// subscription emits `GrpcConnected`. `update()` skips brain-bound
    /// sends while it's `None` (input is buffered locally regardless).
    grpc_in: Option<mpsc::Sender<ConversationRequest>>,
}

impl EmbraDesktop {
    fn new(args: Args) -> (Self, Task<Message>) {
        let mut state = AppState::new();
        // Generous defaults — the iced window is 1280×720 by default,
        // well past the panel-show threshold (80×20). The exact values
        // are advisory; the layout uses iced's flex sizing, not these.
        state.viewport_cols = 120;
        state.viewport_rows = 40;
        state.status_message = "connecting…".to_string();
        (
            Self {
                state,
                apid_addr: args.apid_addr,
                grpc_in: None,
            },
            Task::none(),
        )
    }

    fn title(&self) -> String {
        format!("embra-desktop — {}", self.state.config_name)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::InputChanged(text) => {
                self.state.input_buffer = text;
                self.state.cursor_pos = self.state.input_buffer.chars().count();
            }
            Message::Submit => {
                self.handle_submit();
                return Self::scroll_to_end_task();
            }
            Message::ArrowUp => {
                if let Some(sel) = self.state.selector.as_mut() {
                    sel.up();
                } else {
                    self.state.scroll_offset = self.state.scroll_offset.saturating_add(1);
                }
            }
            Message::ArrowDown => {
                if let Some(sel) = self.state.selector.as_mut() {
                    sel.down();
                } else {
                    self.state.scroll_offset = self.state.scroll_offset.saturating_sub(1);
                }
            }
            Message::PageUp => {
                self.state.scroll_offset = self.state.scroll_offset.saturating_add(10);
            }
            Message::PageDown => {
                self.state.scroll_offset = self.state.scroll_offset.saturating_sub(10);
            }
            Message::Quit => {
                tracing::info!("ctrl-c received, exiting");
                return iced::exit();
            }
            Message::GrpcConnected(tx) => {
                self.grpc_in = Some(tx);
                self.state.status_message = "OK".to_string();
            }
            Message::GrpcEvent(event) => {
                let scrollable_event = matches!(
                    event,
                    ConsoleEvent::ResponseDone(_)
                        | ConsoleEvent::SystemMessage { .. }
                        | ConsoleEvent::ToolExecution { .. }
                        | ConsoleEvent::SetupPrompt { .. }
                );
                handle_console_event(event, &mut self.state);
                if scrollable_event {
                    return operation::snap_to_end(conversation_scroll_id().clone());
                }
            }
            Message::ExpressionTick => {
                // Stage 4c: BrainClient::get_expression poll. Currently
                // no-op — the expression panel updates only when the
                // stream emits a fresh value (which doesn't happen yet
                // for the existing protocol; the TUI used a 3s REST poll
                // through apid).
            }
            Message::AnimationTick => {
                // No visual animation yet (Stage 4c).
            }
        }
        Task::none()
    }

    fn handle_submit(&mut self) {
        // Selector takes priority — if a selector is active, Enter sends
        // the highlighted choice to the brain.
        if let Some(selector) = self.state.selector.take() {
            let choice = selector.current().to_string();
            self.state.live_reasoning.clear();
            self.send_user_message(choice);
            return;
        }

        let input = self.state.input_buffer.trim().to_string();
        self.state.input_buffer.clear();
        self.state.cursor_pos = 0;

        // Honor setup defaults — empty Enter on a wizard step uses the
        // pre-filled value. Same logic as the TUI handle_key_event.
        let input = if input.is_empty() {
            match self.state.setup_default.take() {
                Some(d) if !d.is_empty() => d,
                _ => return,
            }
        } else {
            self.state.setup_default = None;
            input
        };

        if let Some(stripped) = input.strip_prefix('/') {
            // Slash command: split at first space, then either local-handle
            // or forward to brain.
            let parts: Vec<&str> = stripped.splitn(2, ' ').collect();
            let cmd_word = parts[0];
            let args = if parts.len() > 1 { parts[1] } else { "" };
            let cmd = format!("/{}", cmd_word);

            if commands::is_local_command(&cmd) {
                if let Some(out) =
                    commands::handle_local_command(&cmd, args, &self.state.config_name)
                {
                    self.state
                        .messages
                        .push(DisplayMessage::system_with_tz(&out, &self.state.config_tz));
                }
                return;
            }

            self.state.live_reasoning.clear();
            self.send(ConversationRequest {
                request_type: Some(conversation_request::RequestType::SlashCommand(
                    SlashCommand {
                        command: cmd,
                        args: args.to_string(),
                    },
                )),
            });
            return;
        }

        // Regular message — echo into history then forward to brain.
        self.state
            .messages
            .push(DisplayMessage::new_with_tz("user", &input, &self.state.config_tz));
        self.state.live_reasoning.clear();
        self.send_user_message(input);
    }

    fn scroll_to_end_task() -> Task<Message> {
        operation::snap_to_end(conversation_scroll_id().clone())
    }

    fn send_user_message(&mut self, content: String) {
        self.send(ConversationRequest {
            request_type: Some(conversation_request::RequestType::UserMessage(
                UserMessage { content },
            )),
        });
    }

    fn send(&mut self, req: ConversationRequest) {
        let Some(tx) = self.grpc_in.as_ref() else {
            self.state.messages.push(DisplayMessage::system_with_tz(
                "(not connected — message dropped)",
                &self.state.config_tz,
            ));
            return;
        };
        // try_send keeps update() synchronous. The channel is sized 32
        // by `BrainClient::open_conversation`; backpressure beyond that
        // is a brain-handler stall, surfaced as a system message.
        if tx.try_send(req).is_err() {
            self.state.messages.push(DisplayMessage::system_with_tz(
                "(brain stream backpressure — message dropped)",
                &self.state.config_tz,
            ));
        }
    }

    fn view(&self) -> Element<'_, Message> {
        view::draw(&self.state)
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch(vec![
            subscription::grpc(self.apid_addr.clone()),
            subscription::keyboard(),
            iced::time::every(std::time::Duration::from_secs(3))
                .map(|_| Message::ExpressionTick),
            iced::time::every(std::time::Duration::from_millis(200))
                .map(|_| Message::AnimationTick),
        ])
    }

    fn theme(&self) -> Theme {
        theme::theme()
    }
}

fn main() -> iced::Result {
    init_logging();
    let args = Args::parse();
    tracing::info!(apid_addr = %args.apid_addr, "starting embra-desktop");

    iced::application(
        move || EmbraDesktop::new(args.clone()),
        EmbraDesktop::update,
        EmbraDesktop::view,
    )
    .subscription(EmbraDesktop::subscription)
    .theme(EmbraDesktop::theme)
    .title(EmbraDesktop::title)
    .window_size(Size::new(1280.0, 720.0))
    .run()
}

fn init_logging() {
    let env = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env).init();
}
