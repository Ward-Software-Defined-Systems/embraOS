use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::render::{self, StyledLine, StyledSegment};
use super::state::{AppMode, AppState, SetupStep};

pub fn draw(f: &mut Frame, app: &AppState) {
    let available_width = f.area().width.saturating_sub(2) as usize; // minus borders
    let input_lines = if app.pasted_lines.is_some() {
        1
    } else if available_width == 0 {
        1
    } else {
        // Count visual lines including wrapping
        app.input_buffer.split('\n').map(|line| {
            let w = UnicodeWidthStr::width(line);
            ((w / available_width) + 1).max(1)
        }).sum::<usize>()
    };
    let input_height = (input_lines as u16 + 2).min(10);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),            // Header
            Constraint::Min(5),              // Conversation
            Constraint::Length(input_height), // Input (dynamic)
            Constraint::Length(1),            // Status bar
        ])
        .split(f.area());

    draw_header(f, chunks[0], app);
    draw_conversation(f, chunks[1], app);
    draw_input(f, chunks[2], app);
    draw_status_bar(f, chunks[3], app);
}

fn draw_header(f: &mut Frame, area: Rect, app: &AppState) {
    let spans = match &app.mode {
        AppMode::Setup(setup) => {
            let step_label = match setup.step {
                SetupStep::Name => "Name",
                SetupStep::ApiKey => "API Key",
                SetupStep::Timezone => "Timezone",
                SetupStep::Confirm => "Confirm",
            };
            vec![
                Span::styled(
                    " embraOS ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("v{}", app.config_version)),
                Span::raw(" | "),
                Span::styled("Setup", Style::default().fg(Color::Yellow)),
                Span::raw(format!(" | Step: {}", step_label)),
            ]
        }
        AppMode::Learning => {
            vec![
                Span::styled(
                    format!(" {} ", app.config_name),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("v{}", app.config_version)),
                Span::raw(" | "),
                Span::styled("Learning Mode", Style::default().fg(Color::Magenta)),
            ]
        }
        AppMode::Operational { session_name } => {
            vec![
                Span::styled(
                    format!(" {} ", app.config_name),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("v{}", app.config_version)),
                Span::raw(" | Session: "),
                Span::styled(session_name, Style::default().fg(Color::Yellow)),
                Span::raw(" | "),
                Span::styled("● Online", Style::default().fg(Color::Green)),
            ]
        }
    };

    let header = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray));
    f.render_widget(header, area);
}

fn draw_conversation(f: &mut Frame, area: Rect, app: &AppState) {
    let mut styled_lines: Vec<StyledLine> = Vec::new();

    for msg in &app.messages {
        let (color, prefix) = match msg.role.as_str() {
            "user" | "You" => (Color::LightBlue, "You"),
            "system" => (Color::White, ""),
            "tool" => (Color::Cyan, ""),
            _ => (Color::Green, msg.role.as_str()),
        };

        let base_style = Style::default().fg(color);

        if msg.role == "system" || msg.role == "tool" {
            let style = Style::default().fg(color);
            for line in msg.content.lines() {
                styled_lines.push(render::parse_styled_line(&format!("  {}", line), style));
            }
        } else {
            // Header line
            let header = format!("[{}] {}: ", msg.timestamp, prefix);
            styled_lines.push(vec![StyledSegment::new(
                header,
                base_style.add_modifier(Modifier::BOLD),
            )]);

            // Content lines with rich rendering
            let mut in_json_block = false;
            for line in msg.content.lines() {
                let prefixed = format!("  {}", line);
                if line.trim_start().starts_with("```json") || line.trim_start().starts_with("```JSON") {
                    in_json_block = true;
                    styled_lines.push(vec![StyledSegment::new(prefixed, Style::default().fg(Color::Gray))]);
                    continue;
                }
                if line.trim_start().starts_with("```") && in_json_block {
                    in_json_block = false;
                    styled_lines.push(vec![StyledSegment::new(prefixed, Style::default().fg(Color::Gray))]);
                    continue;
                }
                if in_json_block {
                    let mut json_segments = vec![StyledSegment::new("  ".to_string(), base_style)];
                    json_segments.extend(render::parse_json_line(line));
                    styled_lines.push(json_segments);
                } else {
                    styled_lines.push(render::parse_styled_line(&prefixed, base_style));
                }
            }
        }
        styled_lines.push(vec![StyledSegment::new(String::new(), Style::default())]);
    }

    // Show selector if active
    if let Some(ref selector) = app.selector {
        for (i, option) in selector.options.iter().enumerate() {
            let is_selected = i == selector.selected;
            let (marker, style) = if is_selected {
                (
                    "› ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("  ", Style::default().fg(Color::Gray))
            };
            styled_lines.push(vec![StyledSegment::new(format!("  {}{}", marker, option), style)]);
        }
        styled_lines.push(vec![StyledSegment::new(String::new(), Style::default())]);
    }

    // Show thinking indicator
    if app.thinking && app.streaming_text.is_none() {
        let elapsed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let dots = match (elapsed / 500) % 3 {
            0 => ".",
            1 => "..",
            _ => "...",
        };
        styled_lines.push(vec![StyledSegment::new(
            format!("  {} is thinking{}", app.thinking_name, dots),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )]);
    }

    // Show streaming text if present
    if let Some(streaming) = &app.streaming_text {
        styled_lines.push(vec![StyledSegment::new(
            format!("{}: ", app.thinking_name),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )]);
        let style = Style::default().fg(Color::Green);
        for line in streaming.lines() {
            styled_lines.push(render::parse_styled_line(&format!("  {}", line), style));
        }
        styled_lines.push(vec![StyledSegment::new(
            "  ▊".to_string(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::SLOW_BLINK),
        )]);
    }

    // Manually wrap lines and render from the bottom up
    let content_width = area.width.saturating_sub(2) as usize;
    let visible_rows = area.height as usize;

    if content_width == 0 || visible_rows == 0 {
        return;
    }

    let mut visual_rows: Vec<Vec<Span>> = Vec::new();
    for styled_line in &styled_lines {
        let total_len: usize = styled_line.iter().map(|s| UnicodeWidthStr::width(s.text.as_str())).sum();

        if total_len == 0 {
            visual_rows.push(vec![Span::raw("")]);
        } else if total_len <= content_width {
            let spans: Vec<Span> = styled_line
                .iter()
                .map(|seg| Span::styled(seg.text.clone(), seg.style))
                .collect();
            visual_rows.push(spans);
        } else {
            let mut flat_chars: Vec<(char, Style)> = Vec::new();
            for seg in styled_line {
                for ch in seg.text.chars() {
                    flat_chars.push((ch, seg.style));
                }
            }

            let mut i = 0;
            while i < flat_chars.len() {
                let mut row_width = 0usize;
                let mut row_chars: Vec<(char, Style)> = Vec::new();
                while i < flat_chars.len() {
                    let (ch, style) = flat_chars[i];
                    let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                    if row_width + char_width > content_width && !row_chars.is_empty() {
                        break;
                    }
                    row_chars.push((ch, style));
                    row_width += char_width;
                    i += 1;
                }

                if row_chars.is_empty() {
                    break;
                }
                let mut spans: Vec<Span> = Vec::new();
                let mut current_text = String::new();
                let mut current_style = row_chars[0].1;
                for &(ch, style) in &row_chars {
                    if style == current_style {
                        current_text.push(ch);
                    } else {
                        if !current_text.is_empty() {
                            spans.push(Span::styled(current_text.clone(), current_style));
                            current_text.clear();
                        }
                        current_style = style;
                        current_text.push(ch);
                    }
                }
                if !current_text.is_empty() {
                    spans.push(Span::styled(current_text, current_style));
                }
                visual_rows.push(spans);
            }
        }
    }

    let total = visual_rows.len();
    let skip_from_bottom = app.scroll_offset as usize;
    let end = total.saturating_sub(skip_from_bottom);
    let start = end.saturating_sub(visible_rows);

    let visible_slice = &visual_rows[start..end];

    let lines: Vec<Line> = visible_slice
        .iter()
        .map(|spans| Line::from(spans.clone()))
        .collect();

    let paragraph = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::LEFT | Borders::RIGHT));

    f.render_widget(paragraph, area);
}

fn draw_input(f: &mut Frame, area: Rect, app: &AppState) {
    if app.selector.is_some() {
        let hint = Paragraph::new("  Use ↑↓ arrows to select, Enter to confirm")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Select ")
                    .title_style(Style::default().fg(Color::Cyan)),
            );
        f.render_widget(hint, area);
        return;
    }

    let placeholder = app.input_placeholder();
    let title = match &app.mode {
        AppMode::Setup(_) => " Input ",
        _ => " You ",
    };

    let (input_text, style) = if let Some(ref pasted) = app.pasted_lines {
        let total_chars: usize = pasted.iter().map(|l| l.chars().count()).sum::<usize>() + pasted.len().saturating_sub(1);
        let preview = if pasted.len() == 1 && total_chars > 200 {
            format!("[{} chars pasted] press Enter to send", total_chars)
        } else if pasted.len() > 2 {
            let first_two: String = pasted.iter().take(2).map(|l| {
                if l.chars().count() > 60 {
                    let truncated: String = l.chars().take(57).collect();
                    format!("{}...", truncated)
                } else {
                    l.clone()
                }
            }).collect::<Vec<_>>().join(" | ");
            format!("{} ... and {} more lines", first_two, pasted.len() - 2)
        } else {
            format!("[{} lines pasted] press Enter to send", pasted.len())
        };
        (preview, Style::default().fg(Color::Yellow))
    } else if app.input_buffer.is_empty() {
        (placeholder.to_string(), Style::default().fg(Color::DarkGray))
    } else {
        (app.input_buffer.clone(), Style::default().fg(Color::White))
    };

    let input = Paragraph::new(input_text).style(style)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_style(Style::default().fg(Color::LightBlue)),
        );

    f.render_widget(input, area);

    if app.pasted_lines.is_none() && !app.input_buffer.is_empty() {
        let inner_width = area.width.saturating_sub(2) as usize; // minus borders
        let before_cursor: String = app.input_buffer.chars().take(app.cursor_pos).collect();

        // Calculate visual row and column accounting for wrapping
        let mut visual_row: u16 = 0;
        let mut visual_col: u16 = 0;
        for (i, line) in before_cursor.split('\n').enumerate() {
            let is_last = i == before_cursor.matches('\n').count();
            let w = UnicodeWidthStr::width(line);
            if is_last {
                // Cursor is on this line — compute wrapped position
                if inner_width > 0 {
                    visual_row += (w / inner_width) as u16;
                    visual_col = (w % inner_width) as u16;
                } else {
                    visual_col = w as u16;
                }
            } else {
                // Full line — count its visual lines
                if inner_width > 0 {
                    visual_row += ((w / inner_width) + 1) as u16;
                } else {
                    visual_row += 1;
                }
            }
        }
        f.set_cursor_position((area.x + visual_col + 1, area.y + 1 + visual_row));
    }
}

fn draw_status_bar(f: &mut Frame, area: Rect, app: &AppState) {
    let status_color = if app.status_message == "OK" {
        Color::Green
    } else if app.status_message.starts_with("Error") {
        Color::Red
    } else {
        Color::Yellow
    };

    let left_spans = match &app.mode {
        AppMode::Setup(_) => vec![Span::raw(" Setup in progress")],
        AppMode::Learning => vec![
            Span::raw(" Learning Mode"),
            Span::raw(" | Brain: opus-4.6"),
        ],
        AppMode::Operational { session_name } => vec![
            Span::raw(" Sessions: ["),
            Span::styled(
                format!("{}*", session_name),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw("] | Brain: opus-4.6"),
        ],
    };

    let mut spans = left_spans;
    if app.multiline_mode {
        spans.push(Span::styled(" [ML]", Style::default().fg(Color::Cyan)));
    }
    spans.push(Span::raw(" | Status: "));
    spans.push(Span::styled(
        &app.status_message,
        Style::default().fg(status_color),
    ));

    let status = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray));
    f.render_widget(status, area);
}
