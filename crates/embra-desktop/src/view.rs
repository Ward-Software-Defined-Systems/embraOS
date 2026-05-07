//! Four-panel layout view for embra-desktop.
//!
//! Mirrors the TUI's region split (header / expression-or-reasoning /
//! conversation / input) but rendered with iced widgets. The view is
//! a pure function of `AppState` — no timer-driven or event-driven
//! state lives here.

use embra_console_core::state::{AppMode, AppState, DisplayMessage, SetupStep};
use iced::widget::{
    column, container, row, scrollable, text, text_input, Column, Container, Space,
};
use iced::{Background, Border, Color, Element, Length, Padding, Theme};

use crate::{conversation_scroll_id, Message};

const HEADER_BG: Color = Color::from_rgb(0.18, 0.18, 0.22);
const STATUS_BG: Color = Color::from_rgb(0.18, 0.18, 0.22);
const PANEL_BG: Color = Color::from_rgb(0.08, 0.08, 0.10);
const REASONING_FG: Color = Color::from_rgb(0.55, 0.55, 0.60);
const EXPRESSION_FG: Color = Color::from_rgb(0.70, 0.70, 0.72);
const ACCENT_CYAN: Color = Color::from_rgb(0.40, 0.85, 0.95);
const ACCENT_MAGENTA: Color = Color::from_rgb(0.90, 0.50, 0.85);
const ACCENT_GREEN: Color = Color::from_rgb(0.45, 0.90, 0.55);
const ACCENT_YELLOW: Color = Color::from_rgb(0.95, 0.85, 0.40);
const TOOL_FG: Color = Color::from_rgb(0.55, 0.85, 0.95);
const SYSTEM_FG: Color = Color::from_rgb(0.85, 0.85, 0.88);
const USER_FG: Color = Color::from_rgb(0.55, 0.75, 0.95);

pub fn draw(state: &AppState) -> Element<'_, Message> {
    column![
        draw_header(state),
        draw_expression_panel(state),
        draw_conversation(state).height(Length::Fill),
        draw_input(state),
        draw_status_bar(state),
    ]
    .spacing(0)
    .into()
}

fn draw_header(state: &AppState) -> Element<'_, Message> {
    let (label, accent) = match &state.mode {
        AppMode::Setup(s) => {
            let step_label = match s.step {
                SetupStep::Name => "Name",
                SetupStep::Provider => "Provider",
                SetupStep::ApiKey => "API Key",
                SetupStep::Endpoint => "Endpoint",
                SetupStep::BearerToken => "Bearer",
                SetupStep::ModelSelect => "Model",
                SetupStep::Timezone => "Timezone",
                SetupStep::Confirm => "Confirm",
            };
            (format!("Setup — {}", step_label), ACCENT_YELLOW)
        }
        AppMode::Learning => ("Learning Mode".to_string(), ACCENT_MAGENTA),
        AppMode::Operational { session_name } => {
            (format!("Session: {}", session_name), ACCENT_GREEN)
        }
    };

    let line = row![
        text(format!(" {} ", state.config_name))
            .color(ACCENT_CYAN)
            .size(15),
        text(format!("v{}", state.config_version)).size(13),
        text("   |   ").size(13),
        text(label).color(accent).size(13),
    ]
    .spacing(0);

    container(line)
        .padding(Padding::from([4, 8]))
        .width(Length::Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(HEADER_BG)),
            ..container::Style::default()
        })
        .into()
}

fn draw_expression_panel(state: &AppState) -> Element<'_, Message> {
    let show = state.viewport_cols >= 80 && state.viewport_rows >= 20;
    if !show {
        return container(text("")).height(Length::Fixed(0.0)).into();
    }

    let (body, body_color, italic) = if !state.live_reasoning.is_empty() {
        (state.live_reasoning.as_str(), REASONING_FG, true)
    } else {
        (state.expression_content.as_str(), EXPRESSION_FG, false)
    };

    let mut t = text(body.to_string()).color(body_color).size(12);
    if italic {
        t = t.font(iced::Font {
            style: iced::font::Style::Italic,
            ..iced::Font::DEFAULT
        });
    }

    container(t)
        .padding(Padding::from([6, 10]))
        .width(Length::Fill)
        .height(Length::Fixed(120.0))
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(PANEL_BG)),
            border: Border {
                color: Color::from_rgb(0.2, 0.2, 0.25),
                width: 1.0,
                radius: 0.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

fn draw_conversation(state: &AppState) -> Container<'_, Message> {
    let mut col: Column<Message> = Column::new().spacing(6).padding(Padding::from([8, 12]));

    for msg in &state.messages {
        col = col.push(render_message(msg));
    }

    if let Some(streaming) = &state.streaming_text {
        let chunk = column![
            text(format!("{}: ", state.thinking_name))
                .color(ACCENT_GREEN)
                .size(13),
            text(streaming.clone()).color(ACCENT_GREEN).size(13),
        ]
        .spacing(2);
        col = col.push(chunk);
    } else if state.thinking {
        col = col.push(
            text(format!("  {} is thinking…", state.thinking_name))
                .color(REASONING_FG)
                .size(12)
                .font(iced::Font {
                    style: iced::font::Style::Italic,
                    ..iced::Font::DEFAULT
                }),
        );
    }

    if let Some(selector) = &state.selector {
        col = col.push(Space::new().height(Length::Fixed(4.0)));
        for (i, option) in selector.options.iter().enumerate() {
            let marker = if i == selector.selected { "› " } else { "  " };
            let line = text(format!("  {}{}", marker, option))
                .color(if i == selector.selected {
                    ACCENT_CYAN
                } else {
                    Color::from_rgb(0.65, 0.65, 0.65)
                })
                .size(13);
            col = col.push(line);
        }
    }

    container(
        scrollable(col)
            .id(conversation_scroll_id().clone())
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
}

fn render_message(msg: &DisplayMessage) -> Element<'_, Message> {
    let (color, prefix) = match msg.role.as_str() {
        "user" | "You" => (USER_FG, "You"),
        "system" => (SYSTEM_FG, ""),
        "tool" => (TOOL_FG, ""),
        _ => (ACCENT_GREEN, msg.role.as_str()),
    };

    if msg.role == "system" || msg.role == "tool" {
        return text(format!("  {}", msg.content))
            .color(color)
            .size(13)
            .into();
    }

    column![
        text(format!("[{}] {}: ", msg.timestamp, prefix))
            .color(color)
            .size(13),
        text(msg.content.clone()).color(color).size(13),
    ]
    .spacing(2)
    .into()
}

fn draw_input(state: &AppState) -> Element<'_, Message> {
    let placeholder = state.input_placeholder();
    let input = text_input(placeholder, &state.input_buffer)
        .on_input(Message::InputChanged)
        .on_submit(Message::Submit)
        .padding(Padding::from([8, 10]))
        .size(14);

    container(input)
        .padding(Padding::from([6, 10]))
        .width(Length::Fill)
        .into()
}

fn draw_status_bar(state: &AppState) -> Element<'_, Message> {
    let mut spans: Vec<Element<'_, Message>> = Vec::new();
    spans.push(
        text(format!(" Brain: {} ", state.provider_model))
            .color(SYSTEM_FG)
            .size(12)
            .into(),
    );
    if state.multiline_mode {
        spans.push(
            text(" [ML] ")
                .color(ACCENT_CYAN)
                .size(12)
                .into(),
        );
    }
    spans.push(
        text(format!(" Status: {} ", &state.status_message))
            .color(if state.status_message == "OK" {
                ACCENT_GREEN
            } else {
                ACCENT_YELLOW
            })
            .size(12)
            .into(),
    );

    container(row(spans).spacing(8))
        .padding(Padding::from([3, 8]))
        .width(Length::Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(STATUS_BG)),
            ..container::Style::default()
        })
        .into()
}
