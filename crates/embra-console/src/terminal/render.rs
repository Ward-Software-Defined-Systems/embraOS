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

pub fn parse_styled_line(line: &str, base_style: Style) -> StyledLine {
    let base_core = ratatui_to_core(base_style);
    core_render::parse_styled_line(line, base_core)
        .into_iter()
        .map(|seg| StyledSegment {
            text: seg.text,
            style: core_to_ratatui(seg.style),
        })
        .collect()
}

pub fn parse_json_line(line: &str) -> StyledLine {
    core_render::parse_json_line(line)
        .into_iter()
        .map(|seg| StyledSegment {
            text: seg.text,
            style: core_to_ratatui(seg.style),
        })
        .collect()
}

fn ratatui_to_core(s: Style) -> TextStyle {
    let fg = s.fg.map(map_color_to_core).unwrap_or(CoreColor::Reset);
    let mut m = CoreModifier::empty();
    if s.add_modifier.contains(Modifier::BOLD) {
        m = m.union(CoreModifier::BOLD);
    }
    if s.add_modifier.contains(Modifier::ITALIC) {
        m = m.union(CoreModifier::ITALIC);
    }
    if s.add_modifier.contains(Modifier::UNDERLINED) {
        m = m.union(CoreModifier::UNDERLINE);
    }
    if s.add_modifier.contains(Modifier::DIM) {
        m = m.union(CoreModifier::DIM);
    }
    if s.add_modifier.contains(Modifier::SLOW_BLINK) {
        m = m.union(CoreModifier::SLOW_BLINK);
    }
    TextStyle { fg, modifier: m }
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

fn map_color_to_core(c: Color) -> CoreColor {
    match c {
        Color::Reset => CoreColor::Reset,
        Color::Black => CoreColor::Black,
        Color::Red => CoreColor::Red,
        Color::Green => CoreColor::Green,
        Color::Yellow => CoreColor::Yellow,
        Color::Blue => CoreColor::Blue,
        Color::Magenta => CoreColor::Magenta,
        Color::Cyan => CoreColor::Cyan,
        Color::Gray => CoreColor::Gray,
        Color::DarkGray => CoreColor::DarkGray,
        Color::LightRed => CoreColor::LightRed,
        Color::LightGreen => CoreColor::LightGreen,
        Color::LightYellow => CoreColor::LightYellow,
        Color::LightBlue => CoreColor::LightBlue,
        Color::LightMagenta => CoreColor::LightMagenta,
        Color::LightCyan => CoreColor::LightCyan,
        Color::White => CoreColor::White,
        _ => CoreColor::Reset,
    }
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
