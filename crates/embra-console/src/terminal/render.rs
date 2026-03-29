// Render utilities for terminal display
// Provides rich rendering for JSON, markdown headers, bold, and inline code.

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
            let mut end = None;
            for j in start..len {
                if chars[j] == '`' {
                    end = Some(j);
                    break;
                }
            }
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

/// Parse a JSON block for syntax highlighting.
/// Returns styled segments per line.
pub fn parse_json_line(line: &str) -> StyledLine {
    let mut segments = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut current = String::new();

    let base_style = Style::default().fg(Color::White);

    while i < len {
        let ch = chars[i];

        // String literals
        if ch == '"' {
            // Flush current as punctuation
            if !current.is_empty() {
                segments.push(StyledSegment::new(current.clone(), base_style));
                current.clear();
            }
            // Collect the full string including quotes
            let mut s = String::new();
            s.push(ch);
            i += 1;
            while i < len && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < len {
                    s.push(chars[i]);
                    i += 1;
                    s.push(chars[i]);
                } else {
                    s.push(chars[i]);
                }
                i += 1;
            }
            if i < len {
                s.push(chars[i]); // closing quote
                i += 1;
            }

            // Check if this is a key (followed by ':')
            let mut j = i;
            while j < len && chars[j] == ' ' {
                j += 1;
            }
            let is_key = j < len && chars[j] == ':';

            let color = if is_key { Color::Cyan } else { Color::Green };
            segments.push(StyledSegment::new(s, Style::default().fg(color)));
        }
        // Numbers
        else if (ch.is_ascii_digit() || ch == '-') && (current.trim().is_empty() || current.ends_with(": ") || current.ends_with(',')) {
            if !current.is_empty() {
                segments.push(StyledSegment::new(current.clone(), base_style));
                current.clear();
            }
            let mut num = String::new();
            num.push(ch);
            i += 1;
            while i < len && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == 'e' || chars[i] == 'E' || chars[i] == '+' || chars[i] == '-') {
                num.push(chars[i]);
                i += 1;
            }
            segments.push(StyledSegment::new(num, Style::default().fg(Color::Yellow)));
            continue; // don't increment i again
        }
        // Boolean/null keywords — check from chars array, not byte-indexed line slice
        else if i + 4 <= len && matches!(
            (chars[i], chars.get(i+1), chars.get(i+2), chars.get(i+3)),
            ('t', Some('r'), Some('u'), Some('e'))
            | ('n', Some('u'), Some('l'), Some('l'))
        ) {
            if !current.is_empty() {
                segments.push(StyledSegment::new(current.clone(), base_style));
                current.clear();
            }
            let word: String = chars[i..i + 4].iter().collect();
            segments.push(StyledSegment::new(word, Style::default().fg(Color::Yellow)));
            i += 4;
            continue;
        }
        else if i + 5 <= len && (chars[i], chars.get(i+1), chars.get(i+2), chars.get(i+3), chars.get(i+4))
            == ('f', Some(&'a'), Some(&'l'), Some(&'s'), Some(&'e'))
        {
            if !current.is_empty() {
                segments.push(StyledSegment::new(current.clone(), base_style));
                current.clear();
            }
            let word: String = chars[i..i + 5].iter().collect();
            segments.push(StyledSegment::new(word, Style::default().fg(Color::Yellow)));
            i += 5;
            continue;
        } else {
            current.push(ch);
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
