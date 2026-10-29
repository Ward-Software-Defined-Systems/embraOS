// Render utilities for terminal display
// Provides rich rendering for JSON, markdown headers, bold, and inline code.

use embra_tools_core::{JsonTokenKind, tokenize_line};
use ratatui::style::{Color, Modifier, Style};

/// A single styled segment within a line
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

/// A line composed of multiple styled segments
pub type StyledLine = Vec<StyledSegment>;

/// Parse a line of text into styled segments, handling markdown-like formatting.
/// base_style is the default style for unformatted text.
pub fn parse_styled_line(line: &str, base_style: Style) -> StyledLine {
    let trimmed = line.trim_start();

    // Markdown headers
    if trimmed.starts_with("### ") {
        return vec![StyledSegment::new(
            line.to_string(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )];
    }
    if trimmed.starts_with("## ") {
        return vec![StyledSegment::new(
            line.to_string(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )];
    }
    if trimmed.starts_with("# ") {
        return vec![StyledSegment::new(
            line.to_string(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )];
    }

    // Parse inline formatting: **bold** and `code`
    let mut segments = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Bold: **text**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            // Flush current
            if !current.is_empty() {
                segments.push(StyledSegment::new(current.clone(), base_style));
                current.clear();
            }
            // Find closing **
            let start = i + 2;
            let mut end = None;
            let mut j = start;
            while j + 1 < len {
                if chars[j] == '*' && chars[j + 1] == '*' {
                    end = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end_pos) = end {
                let bold_text: String = chars[start..end_pos].iter().collect();
                segments.push(StyledSegment::new(
                    bold_text,
                    base_style.add_modifier(Modifier::BOLD),
                ));
                i = end_pos + 2;
            } else {
                current.push(chars[i]);
                i += 1;
            }
        }
        // Inline code: `text`
        else if chars[i] == '`' {
            if !current.is_empty() {
                segments.push(StyledSegment::new(current.clone(), base_style));
                current.clear();
            }
            let start = i + 1;
            let end = chars[start..].iter().position(|&c| c == '`').map(|p| start + p);
            if let Some(end_pos) = end {
                let code_text: String = chars[start..end_pos].iter().collect();
                segments.push(StyledSegment::new(
                    code_text,
                    Style::default().fg(Color::Gray),
                ));
                i = end_pos + 1;
            } else {
                current.push(chars[i]);
                i += 1;
            }
        } else {
            current.push(chars[i]);
            i += 1;
        }
    }

    if !current.is_empty() {
        segments.push(StyledSegment::new(current, base_style));
    }

    if segments.is_empty() {
        vec![StyledSegment::new(String::new(), base_style)]
    } else {
        segments
    }
}

/// A JSON line as styled segments, through the one tokenizer the web
/// console shares (`embra_tools_core::tokenize_line`), each kind in the
/// console's colour: keys cyan, strings green, numbers and the three
/// literals yellow, the structure and anything else the base white (#40).
pub fn parse_json_line(line: &str) -> StyledLine {
    let base_style = Style::default().fg(Color::White);
    let segments: StyledLine = tokenize_line(line)
        .into_iter()
        .map(|t| StyledSegment::new(t.text(line), json_style(t.kind, base_style)))
        .collect();
    if segments.is_empty() {
        vec![StyledSegment::new(String::new(), base_style)]
    } else {
        segments
    }
}

/// The console's colour for each token kind; every kind is named here.
fn json_style(kind: JsonTokenKind, base: Style) -> Style {
    match kind {
        JsonTokenKind::Key => Style::default().fg(Color::Cyan),
        JsonTokenKind::Str => Style::default().fg(Color::Green),
        JsonTokenKind::Number | JsonTokenKind::Bool | JsonTokenKind::Null => Style::default().fg(Color::Yellow),
        JsonTokenKind::Punct | JsonTokenKind::Text => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every token kind of the shared tokenizer has its colour here, and
    /// the two rules the console lacked hold: a number after `[`, and a
    /// keyword at word boundaries only (`null` inside `nullable` is text).
    #[test]
    fn the_two_renderers_cover_every_token_kind() {
        let base = Style::default().fg(Color::White);
        for (kind, color) in [
            (JsonTokenKind::Key, Color::Cyan),
            (JsonTokenKind::Str, Color::Green),
            (JsonTokenKind::Number, Color::Yellow),
            (JsonTokenKind::Bool, Color::Yellow),
            (JsonTokenKind::Null, Color::Yellow),
            (JsonTokenKind::Punct, Color::White),
            (JsonTokenKind::Text, Color::White),
        ] {
            assert_eq!(json_style(kind, base).fg, Some(color), "{kind:?}");
        }
        let line = r#"{"k": [1, null], "s": "v", "x": nullable}"#;
        let segs: Vec<(String, Option<Color>)> =
            parse_json_line(line).into_iter().map(|s| (s.text, s.style.fg)).collect();
        assert_eq!(
            segs,
            vec![
                ("{".to_string(), Some(Color::White)),
                ("\"k\"".to_string(), Some(Color::Cyan)),
                (": [".to_string(), Some(Color::White)),
                ("1".to_string(), Some(Color::Yellow)),
                (", ".to_string(), Some(Color::White)),
                ("null".to_string(), Some(Color::Yellow)),
                ("], ".to_string(), Some(Color::White)),
                ("\"s\"".to_string(), Some(Color::Cyan)),
                (": ".to_string(), Some(Color::White)),
                ("\"v\"".to_string(), Some(Color::Green)),
                (", ".to_string(), Some(Color::White)),
                ("\"x\"".to_string(), Some(Color::Cyan)),
                (": nullable}".to_string(), Some(Color::White)),
            ]
        );
        assert_eq!(parse_json_line("").len(), 1, "an empty line is one empty base segment");
    }

    fn texts(line: &str) -> Vec<(String, bool)> {
        let code = Style::default().fg(Color::Gray);
        parse_styled_line(line, Style::default())
            .into_iter()
            .map(|s| (s.text, s.style == code))
            .collect()
    }

    fn seg(text: &str, code: bool) -> (String, bool) {
        (text.to_string(), code)
    }

    #[test]
    fn inline_code_is_its_own_segment() {
        assert_eq!(
            texts("run `cargo test` now"),
            vec![seg("run ", false), seg("cargo test", true), seg(" now", false)]
        );
        // At the line's edges, and empty.
        assert_eq!(texts("`a`"), vec![seg("a", true)]);
        assert_eq!(texts("x``y"), vec![seg("x", false), seg("", true), seg("y", false)]);
        // Two spans: the search for a closing tick starts after the opening one.
        assert_eq!(
            texts("`a` and `b`"),
            vec![seg("a", true), seg(" and ", false), seg("b", true)]
        );
    }

    #[test]
    fn an_unclosed_backtick_is_text() {
        assert_eq!(texts("a ` b"), vec![seg("a ", false), seg("` b", false)]);
        // As the last character: nothing to search, nothing to index past.
        assert_eq!(texts("tail `"), vec![seg("tail ", false), seg("`", false)]);
    }

    #[test]
    fn multibyte_text_around_code_is_kept() {
        assert_eq!(
            texts("café `é` ünï"),
            vec![seg("café ", false), seg("é", true), seg(" ünï", false)]
        );
    }
}
