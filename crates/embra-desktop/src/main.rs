//! embra-desktop — iced GUI client for embraOS.
//!
//! Stage 4a scaffold: opens a window, renders the four-panel layout
//! (header, expression/reasoning, conversation, input) against a static
//! `AppState`. Stage 4b wires the gRPC subscription bringing in real
//! `ConsoleEvent`s from `embra-apid` via `embra-console-core::grpc`.

mod theme;
mod view;

use clap::Parser;
use embra_console_core::state::AppState;
use iced::{Element, Size, Subscription, Task, Theme};

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
    /// Operator pressed Enter — submit current input.
    Submit,
    /// 3-second tick for expression-panel poll (Stage 4b — currently no-op).
    ExpressionTick,
    /// 200 ms tick for thinking-dot animation.
    AnimationTick,
}

pub struct EmbraDesktop {
    state: AppState,
    /// gRPC endpoint — used by the subscription in Stage 4b.
    #[allow(dead_code)]
    apid_addr: String,
}

impl EmbraDesktop {
    fn new(args: Args) -> (Self, Task<Message>) {
        let mut state = AppState::new();
        state.viewport_cols = 120;
        state.viewport_rows = 40;
        // Static seed messages so 4a renders something visible before
        // the gRPC subscription lands in 4b.
        state.messages.push(
            embra_console_core::state::DisplayMessage::system_with_tz(
                "embra-desktop scaffold (Stage 4a) — gRPC stream not yet wired",
                &state.config_tz,
            ),
        );
        (
            Self {
                state,
                apid_addr: args.apid_addr,
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
                let input = self.state.input_buffer.trim().to_string();
                self.state.input_buffer.clear();
                self.state.cursor_pos = 0;
                if !input.is_empty() {
                    // Stage 4b: route to embra_console_core::commands +
                    // gRPC stream. For 4a, just append to the conversation
                    // as a local echo so the input UI is self-evidently
                    // alive.
                    self.state.live_reasoning.clear();
                    self.state.messages.push(
                        embra_console_core::state::DisplayMessage::new_with_tz(
                            "user",
                            &input,
                            &self.state.config_tz,
                        ),
                    );
                }
            }
            Message::ExpressionTick => {
                // Stage 4b: poll BrainClient::get_expression
            }
            Message::AnimationTick => {
                // No-op for 4a; Stage 4b advances the thinking-dot phase
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        view::draw(&self.state)
    }

    fn subscription(&self) -> Subscription<Message> {
        // Stage 4b: combine grpc-event channel + iced::time::every ticks.
        Subscription::none()
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
