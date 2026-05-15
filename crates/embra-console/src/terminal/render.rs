//! TUI adapter for `embra-console-core`'s neutral styled-text parsers.
//!
//! The core crate's parsers emit `StyledSegment`s carrying neutral
//! `TextStyle` values; this layer translates them into ratatui `Style`s
//! so the rendering code (`ui.rs`) can treat them as native ratatui
//! types. The TUI module imports `parse_styled_line`, `parse_json_line`,
//! `StyledSegment`, and `StyledLine` through this adapter, so adding or
//! changing core parsers requires no churn in `ui.rs`.

use embra_console_core::render as core_render;
use embra_console_core::style::{
    Color as CoreColor, Modifier as CoreModifier, TextStyle,
};
use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone)]
pub struct StyledSegment {
    pub text: String,
    pub style: Style,
}

impl StyledSegment {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

pub type StyledLine = Vec<StyledSegment>;

fn map_core_lines(lines: Vec<core_render::StyledLine>) -> Vec<StyledLine> {
    lines
        .into_iter()
        .map(|line| {
            line.into_iter()
                .map(|seg| StyledSegment {
                    text: seg.text,
                    style: core_to_ratatui(seg.style),
                })
                .collect()
        })
        .collect()
}

/// Whole-message styling (header, ```json fences, trailing blank),
/// delegated to the shared core so the TUI and GUI render identically.
pub fn render_message_lines(role: &str, content: &str, timestamp: &str) -> Vec<StyledLine> {
    map_core_lines(core_render::render_message_lines(role, content, timestamp))
}

/// In-progress streamed response (shared with the GUI).
pub fn render_streaming_lines(thinking_name: &str, streaming: &str) -> Vec<StyledLine> {
    map_core_lines(core_render::render_streaming_lines(thinking_name, streaming))
}

/// "Thinking…" indicator line with time-cycled dots (shared with the GUI).
pub fn render_thinking_line(thinking_name: &str) -> StyledLine {
    core_render::render_thinking_line(thinking_name)
        .into_iter()
        .map(|seg| StyledSegment {
            text: seg.text,
            style: core_to_ratatui(seg.style),
        })
        .collect()
}

/// ANSI / control-char strip (shared with the GUI).
pub fn sanitize_for_render(s: &str) -> String {
    core_render::sanitize_for_render(s)
}

fn core_to_ratatui(ts: TextStyle) -> Style {
    let mut s = Style::default();
    if ts.fg != CoreColor::Reset {
        s = s.fg(map_color_from_core(ts.fg));
    }
    if ts.modifier.contains(CoreModifier::BOLD) {
        s = s.add_modifier(Modifier::BOLD);
    }
    if ts.modifier.contains(CoreModifier::ITALIC) {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if ts.modifier.contains(CoreModifier::UNDERLINE) {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    if ts.modifier.contains(CoreModifier::DIM) {
        s = s.add_modifier(Modifier::DIM);
    }
    if ts.modifier.contains(CoreModifier::SLOW_BLINK) {
        s = s.add_modifier(Modifier::SLOW_BLINK);
    }
    s
}

fn map_color_from_core(c: CoreColor) -> Color {
    match c {
        CoreColor::Reset => Color::Reset,
        CoreColor::Black => Color::Black,
        CoreColor::Red => Color::Red,
        CoreColor::Green => Color::Green,
        CoreColor::Yellow => Color::Yellow,
        CoreColor::Blue => Color::Blue,
        CoreColor::Magenta => Color::Magenta,
        CoreColor::Cyan => Color::Cyan,
        CoreColor::Gray => Color::Gray,
        CoreColor::DarkGray => Color::DarkGray,
        CoreColor::LightRed => Color::LightRed,
        CoreColor::LightGreen => Color::LightGreen,
        CoreColor::LightYellow => Color::LightYellow,
        CoreColor::LightBlue => Color::LightBlue,
        CoreColor::LightMagenta => Color::LightMagenta,
        CoreColor::LightCyan => Color::LightCyan,
        CoreColor::White => Color::White,
    }
}
