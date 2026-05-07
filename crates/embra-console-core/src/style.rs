//! Neutral style enums shared across UI front-ends.
//!
//! `embra-console` (TUI) maps these to `ratatui::style::Style`;
//! `embra-desktop` (iced) maps them to its widget styling. The parser
//! emits these so the styled-text logic stays UI-agnostic.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifier(u8);

impl Modifier {
    pub const NONE: Modifier = Modifier(0);
    pub const BOLD: Modifier = Modifier(1 << 0);
    pub const ITALIC: Modifier = Modifier(1 << 1);
    pub const UNDERLINE: Modifier = Modifier(1 << 2);
    pub const DIM: Modifier = Modifier(1 << 3);
    pub const SLOW_BLINK: Modifier = Modifier(1 << 4);

    pub const fn empty() -> Self {
        Modifier(0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Modifier(self.0 | other.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextStyle {
    pub fg: Color,
    pub modifier: Modifier,
}

impl TextStyle {
    pub const fn new() -> Self {
        Self {
            fg: Color::Reset,
            modifier: Modifier::empty(),
        }
    }

    pub const fn fg(mut self, fg: Color) -> Self {
        self.fg = fg;
        self
    }

    pub const fn add_modifier(mut self, m: Modifier) -> Self {
        self.modifier = self.modifier.union(m);
        self
    }
}
