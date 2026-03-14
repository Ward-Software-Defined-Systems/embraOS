// Render utilities for terminal display
// Currently rendering is handled directly in ui.rs
// This module is reserved for future rich rendering (markdown, code blocks, etc.)

/// Strip ANSI escape codes from text for width calculation
pub fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut in_escape = false;
    for ch in text.chars() {
        if in_escape {
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else if ch == '\x1b' {
            in_escape = true;
        } else {
            width += 1;
        }
    }
    width
}
