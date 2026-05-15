//! Four-panel layout view for embra-desktop.
//!
//! Mirrors the TUI's region split (header / expression-or-reasoning /
//! conversation / input) but rendered with iced widgets. The view is
//! a pure function of `AppState` — no timer-driven or event-driven
//! state lives here.

use embra_console_core::render as core_render;
use embra_console_core::state::{AppMode, AppState, DisplayMessage, SetupStep};
use embra_console_core::style::{Color as CoreColor, Modifier as CoreModifier};
use iced::widget::text::Span as TextSpan;
use iced::widget::{
    button, center, column, container, mouse_area, opaque, pin, rich_text, row, scrollable, span,
    stack, text, text_editor, text_input, Column, Container, Space,
};
use iced::{Background, Border, Color, Element, Font, Length, Padding, Shadow, Theme};

use crate::editor::{EditorState, EditorSyntax};
use crate::menu::{Action, MenuItem, MenuPanel, MenuState, ModalState};
use crate::{conversation_scroll_id, editor_id, modal_input_id, Message};

const HEADER_BG: Color = Color::from_rgb(0.18, 0.18, 0.22);
const STATUS_BG: Color = Color::from_rgb(0.18, 0.18, 0.22);
const PANEL_BG: Color = Color::from_rgb(0.08, 0.08, 0.10);
const REASONING_FG: Color = Color::from_rgb(0.55, 0.55, 0.60);
const ACCENT_CYAN: Color = Color::from_rgb(0.40, 0.85, 0.95);
const ACCENT_CYAN_DIM: Color = Color::from_rgb(0.25, 0.45, 0.50);
const ACCENT_MAGENTA: Color = Color::from_rgb(0.90, 0.50, 0.85);
const ACCENT_GREEN: Color = Color::from_rgb(0.45, 0.90, 0.55);
const ACCENT_YELLOW: Color = Color::from_rgb(0.95, 0.85, 0.40);
const SYSTEM_FG: Color = Color::from_rgb(0.85, 0.85, 0.88);
const USER_FG: Color = Color::from_rgb(0.55, 0.75, 0.95);
const SEPARATOR_FG: Color = Color::from_rgb(0.25, 0.25, 0.30);
const BACKDROP: Color = Color {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 0.55,
};

const MENU_BAR_HEIGHT: f32 = 26.0;

pub fn draw<'a>(
    state: &'a AppState,
    menu: &'a MenuState,
    modal: Option<&'a ModalState>,
    editor: Option<&'a EditorState>,
) -> Element<'a, Message> {
    let base: Element<'a, Message> = column![
        draw_menu_bar(menu),
        draw_header(state),
        draw_expression_panel(state),
        draw_conversation(state).height(Length::Fill),
        draw_input(state),
        draw_status_bar(state),
    ]
    .spacing(0)
    .into();

    let mut layers: Vec<Element<'a, Message>> = vec![base];

    if let Some(panel) = menu.open {
        layers.push(draw_menu_dropdown(panel, menu));
    }
    if let Some(m) = modal {
        layers.push(draw_backdrop(Message::ModalCancel));
        layers.push(draw_modal(m));
    }
    // Editor is the topmost overlay; it's mutually exclusive with
    // menu/modal via the update guards, but push last regardless.
    if let Some(e) = editor {
        layers.push(draw_backdrop(Message::EditorCancel));
        layers.push(draw_editor_overlay(e));
    }

    stack(layers)
        .width(Length::Fill)
        .height(Length::Fill)
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

    // Match the TUI: live reasoning is italic dark-gray, the operator
    // expression is solid gray; both ANSI/control-stripped via the
    // shared sanitizer.
    let (raw, core_fg, italic) = if !state.live_reasoning.is_empty() {
        (state.live_reasoning.as_str(), CoreColor::DarkGray, true)
    } else {
        (state.expression_content.as_str(), CoreColor::Gray, false)
    };

    let body = core_render::sanitize_for_render(raw);
    let mut t = text(body).color(core_color_to_iced(core_fg)).size(12);
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
        let lines = core_render::render_streaming_lines(&state.thinking_name, streaming);
        col = col.push(styled_lines_to_column(&lines));
    } else if state.thinking {
        let line = core_render::render_thinking_line(&state.thinking_name);
        col = col.push(styled_lines_to_column(&[line]));
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

/// One neutral styled segment → an iced rich-text span (color + bold /
/// italic / underline). Text is owned, so the span isn't tied to `seg`.
fn seg_to_span<'a>(seg: &core_render::StyledSegment) -> TextSpan<'a, ()> {
    let mut font = Font::DEFAULT;
    if seg.style.modifier.contains(CoreModifier::BOLD) {
        font.weight = iced::font::Weight::Bold;
    }
    if seg.style.modifier.contains(CoreModifier::ITALIC) {
        font.style = iced::font::Style::Italic;
    }
    let mut s = span(seg.text.clone())
        .color(core_color_to_iced(seg.style.fg))
        .font(font);
    if seg.style.modifier.contains(CoreModifier::UNDERLINE) {
        s = s.underline(true);
    }
    s
}

/// Neutral styled lines → a column of rich-text rows. Shared by message,
/// streaming, and thinking rendering so they all style identically.
fn styled_lines_to_column<'a>(lines: &[core_render::StyledLine]) -> Column<'a, Message> {
    let mut col: Column<'a, Message> = Column::new().spacing(1);
    for line in lines {
        let spans: Vec<TextSpan<'_, ()>> = line.iter().map(seg_to_span).collect();
        col = col.push(rich_text(spans).size(13));
    }
    col
}

fn render_message(msg: &DisplayMessage) -> Element<'_, Message> {
    // Styled lines come from the shared core (same parser the TUI uses):
    // role/header, ```json fences, markdown bold/headers/inline-code.
    let lines = core_render::render_message_lines(&msg.role, &msg.content, &msg.timestamp);
    styled_lines_to_column(&lines).into()
}

/// Map the shared neutral palette to the GUI's dark theme. Base text
/// colors reuse the existing role constants so non-styled output looks
/// unchanged; the accents light up markdown / JSON spans.
fn core_color_to_iced(c: CoreColor) -> Color {
    match c {
        CoreColor::Reset => SYSTEM_FG,
        CoreColor::Black => Color::from_rgb(0.0, 0.0, 0.0),
        CoreColor::Red => Color::from_rgb(0.90, 0.45, 0.45),
        CoreColor::Green => ACCENT_GREEN,
        CoreColor::Yellow => ACCENT_YELLOW,
        CoreColor::Blue => Color::from_rgb(0.45, 0.65, 0.95),
        CoreColor::Magenta => ACCENT_MAGENTA,
        CoreColor::Cyan => ACCENT_CYAN,
        CoreColor::Gray => Color::from_rgb(0.60, 0.60, 0.65),
        CoreColor::DarkGray => REASONING_FG,
        CoreColor::LightRed => Color::from_rgb(0.95, 0.55, 0.55),
        CoreColor::LightGreen => Color::from_rgb(0.60, 0.95, 0.65),
        CoreColor::LightYellow => Color::from_rgb(0.97, 0.92, 0.55),
        CoreColor::LightBlue => USER_FG,
        CoreColor::LightMagenta => Color::from_rgb(0.95, 0.65, 0.92),
        CoreColor::LightCyan => Color::from_rgb(0.60, 0.90, 0.97),
        CoreColor::White => Color::from_rgb(0.92, 0.92, 0.94),
    }
}

fn draw_input(state: &AppState) -> Element<'_, Message> {
    let placeholder = state.input_placeholder();
    let input = text_input(placeholder, &state.input_buffer)
        .on_input(Message::InputChanged)
        .on_submit(Message::Submit)
        .padding(Padding::from([8, 10]))
        .size(14);

    // Bottom-right escalation to the structured-input editor overlay
    // (multi-line, JSON / Markdown highlighting). Also reachable via /ml.
    let editor_btn = button(text("⊞ Structured").size(13))
        .on_press(Message::EditorOpen)
        .padding(Padding::from([8, 12]))
        .style(|_: &Theme, _status| button::Style {
            background: Some(Background::Color(HEADER_BG)),
            text_color: ACCENT_CYAN,
            border: Border {
                color: ACCENT_CYAN_DIM,
                width: 1.0,
                radius: 4.0.into(),
            },
            shadow: Shadow::default(),
            snap: false,
        });

    container(
        row![input, editor_btn]
            .spacing(6)
            .align_y(iced::alignment::Vertical::Center),
    )
    .padding(Padding::from([6, 10]))
    .width(Length::Fill)
    .into()
}

fn draw_menu_bar<'a>(menu: &'a MenuState) -> Element<'a, Message> {
    let mut bar = row![].spacing(0);
    for &panel in MenuPanel::ALL {
        let active = menu.open == Some(panel);
        let msg = if active {
            Message::MenuClose
        } else {
            Message::MenuOpen(panel)
        };
        let btn = button(text(panel.label()).size(13))
            .on_press(msg)
            .padding(Padding::from([4, 10]))
            .style(move |_: &Theme, _status| button::Style {
                background: Some(Background::Color(if active {
                    ACCENT_CYAN_DIM
                } else {
                    HEADER_BG
                })),
                text_color: if active { Color::BLACK } else { SYSTEM_FG },
                border: Border::default(),
                shadow: Shadow::default(),
                snap: false,
            });
        bar = bar.push(btn);
    }
    container(bar)
        .width(Length::Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(HEADER_BG)),
            ..container::Style::default()
        })
        .into()
}

fn panel_x_offset(panel: MenuPanel) -> f32 {
    // Approximate cumulative widths for the menu-bar buttons. Tunable
    // visually; labels are ASCII and the font is fixed.
    match panel {
        MenuPanel::File => 0.0,
        MenuPanel::View => 50.0,
        MenuPanel::Provider => 104.0,
        MenuPanel::Settings => 184.0,
        MenuPanel::Setup => 268.0,
        MenuPanel::Help => 328.0,
    }
}

fn draw_menu_dropdown<'a>(panel: MenuPanel, menu: &MenuState) -> Element<'a, Message> {
    let parent_col = build_menu_items(panel.items(), menu.selected, false);

    let content: Element<'a, Message> = if menu.submenu_open {
        let sub_items: &'static [MenuItem] = match panel.items().get(menu.selected) {
            Some(MenuItem::Action {
                action: Action::OpenSubmenu(sub),
                ..
            }) => sub,
            _ => &[],
        };
        let sub_col = build_menu_items(sub_items, menu.submenu_selected, true);
        container(row![parent_col, sub_col].spacing(0))
            .style(|_: &Theme| container::Style {
                background: Some(Background::Color(PANEL_BG)),
                border: Border {
                    color: ACCENT_CYAN,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..container::Style::default()
            })
            .into()
    } else {
        container(parent_col)
            .style(|_: &Theme| container::Style {
                background: Some(Background::Color(PANEL_BG)),
                border: Border {
                    color: ACCENT_CYAN,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..container::Style::default()
            })
            .into()
    };

    pin(content)
        .x(panel_x_offset(panel))
        .y(MENU_BAR_HEIGHT)
        .into()
}

fn build_menu_items<'a>(
    items: &'static [MenuItem],
    selected: usize,
    is_submenu: bool,
) -> Element<'a, Message> {
    let mut col: Column<Message> = Column::new().spacing(0).width(Length::Fixed(220.0));
    for (i, item) in items.iter().enumerate() {
        match item {
            MenuItem::Separator => {
                col = col.push(
                    container(Space::new().height(Length::Fixed(1.0)).width(Length::Fill))
                        .padding(Padding::from([3, 0]))
                        .style(|_: &Theme| container::Style {
                            background: Some(Background::Color(SEPARATOR_FG)),
                            ..container::Style::default()
                        }),
                );
            }
            MenuItem::Action { label, .. } => {
                let highlight = i == selected;
                let bg = if highlight { ACCENT_CYAN } else { PANEL_BG };
                let fg = if highlight { Color::BLACK } else { SYSTEM_FG };
                let row_widget = container(text(*label).size(13).color(fg))
                    .padding(Padding::from([4, 10]))
                    .width(Length::Fill)
                    .style(move |_: &Theme| container::Style {
                        background: Some(Background::Color(bg)),
                        ..container::Style::default()
                    });
                let interactive = mouse_area(row_widget)
                    .on_enter(Message::MenuHover {
                        sub: is_submenu,
                        index: i,
                    })
                    .on_press(Message::MenuActivate);
                col = col.push(interactive);
            }
        }
    }
    col.into()
}

fn draw_backdrop<'a>(on_press: Message) -> Element<'a, Message> {
    let backdrop = container(Space::new().width(Length::Fill).height(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(BACKDROP)),
            ..container::Style::default()
        });
    mouse_area(backdrop).on_press(on_press).into()
}

fn draw_modal<'a>(m: &'a ModalState) -> Element<'a, Message> {
    let panel = container(
        column![
            text(m.title.as_str()).color(ACCENT_CYAN).size(15),
            text_input("", &m.input)
                .id(modal_input_id().clone())
                .on_input(Message::ModalInputChanged)
                .on_submit(Message::ModalSubmit)
                .padding(Padding::from([6, 8]))
                .size(13),
            row![
                button(text("Cancel").size(13))
                    .on_press(Message::ModalCancel)
                    .padding(Padding::from([4, 12])),
                Space::new().width(Length::Fill),
                button(text("OK").size(13))
                    .on_press(Message::ModalSubmit)
                    .padding(Padding::from([4, 12])),
            ]
            .spacing(8),
        ]
        .spacing(10),
    )
    .padding(Padding::from([14, 18]))
    .width(Length::Fixed(420.0))
    .style(|_: &Theme| container::Style {
        background: Some(Background::Color(HEADER_BG)),
        border: Border {
            color: ACCENT_CYAN,
            width: 1.0,
            radius: 4.0.into(),
        },
        ..container::Style::default()
    });

    center(opaque(panel)).into()
}

fn draw_editor_overlay<'a>(e: &'a EditorState) -> Element<'a, Message> {
    let mut syntax_row = row![].spacing(6);
    for &s in EditorSyntax::ALL {
        let active = e.syntax == s;
        syntax_row = syntax_row.push(
            button(text(s.label()).size(12))
                .on_press(Message::EditorSyntaxSet(s))
                .padding(Padding::from([3, 10]))
                .style(move |_: &Theme, _status| button::Style {
                    background: Some(Background::Color(if active {
                        ACCENT_CYAN
                    } else {
                        PANEL_BG
                    })),
                    text_color: if active { Color::BLACK } else { SYSTEM_FG },
                    border: Border {
                        color: ACCENT_CYAN_DIM,
                        width: 1.0,
                        radius: 3.0.into(),
                    },
                    shadow: Shadow::default(),
                    snap: false,
                }),
        );
    }

    let editor_widget = text_editor(&e.content)
        .placeholder("Structured input — multi-line, JSON, Markdown…")
        .on_action(Message::EditorAction)
        .height(Length::Fill)
        .padding(Padding::from([8, 10]))
        .size(13)
        .font(Font::MONOSPACE)
        .id(editor_id().clone())
        .highlight(e.syntax.token(), iced::highlighter::Theme::Base16Ocean);

    let panel = container(
        column![
            row![
                text("Structured Input").color(ACCENT_CYAN).size(15),
                Space::new().width(Length::Fill),
                syntax_row,
            ]
            .spacing(8)
            .align_y(iced::alignment::Vertical::Center),
            container(editor_widget)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_: &Theme| container::Style {
                    background: Some(Background::Color(PANEL_BG)),
                    border: Border {
                        color: ACCENT_CYAN_DIM,
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    ..container::Style::default()
                }),
            row![
                text("Ctrl+Enter to send · Esc to cancel")
                    .color(REASONING_FG)
                    .size(11),
                Space::new().width(Length::Fill),
                button(text("Cancel").size(13))
                    .on_press(Message::EditorCancel)
                    .padding(Padding::from([4, 14])),
                button(text("Send").size(13))
                    .on_press(Message::EditorSubmit)
                    .padding(Padding::from([4, 14])),
            ]
            .spacing(8)
            .align_y(iced::alignment::Vertical::Center),
        ]
        .spacing(10),
    )
    .padding(Padding::from([14, 18]))
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_: &Theme| container::Style {
        background: Some(Background::Color(HEADER_BG)),
        border: Border {
            color: ACCENT_CYAN,
            width: 1.0,
            radius: 4.0.into(),
        },
        ..container::Style::default()
    });

    // ~85% centered via a FillPortion sandwich (adaptive to window size).
    column![
        Space::new().height(Length::FillPortion(1)),
        row![
            Space::new().width(Length::FillPortion(1)),
            container(opaque(panel))
                .width(Length::FillPortion(12))
                .height(Length::Fill),
            Space::new().width(Length::FillPortion(1)),
        ]
        .height(Length::FillPortion(10)),
        Space::new().height(Length::FillPortion(1)),
    ]
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
