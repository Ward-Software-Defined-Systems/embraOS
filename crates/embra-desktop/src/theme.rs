//! Theme tokens for embra-desktop.
//!
//! Stage 4a uses iced's built-in Dark theme as a starting point. Stage 6
//! polish adds custom palette tokens that mirror the TUI's color intent
//! (cyan headers, dark-gray reasoning, magenta learning-mode, etc.).

use iced::Theme;

pub fn theme() -> Theme {
    Theme::Dark
}
