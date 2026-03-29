use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use super::render::{self, StyledLine, StyledSegment};
use super::{AppMode, AppState, SetupStep};

pub fn draw(f: &mut Frame, app: &AppState) {
    // Dynamic input height: 3 rows base, grows with newlines (max 10)
    let input_lines = if app.pasted_lines.is_some() {
        1
    } else {
        app.input_buffer.chars().filter(|c| *c == '\n').count() + 1
    };
    let input_height = (input_lines as u16 + 2).min(10); // +2 for border

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
                Span::raw(format!("v{}", env!("CARGO_PKG_VERSION"))),
                Span::raw(" | "),
                Span::styled("Setup", Style::default().fg(Color::Yellow)),
                Span::raw(format!(" | Step: {}", step_label)),
            ]
        }
        AppMode::Learning(lm) => {
            let phase_label = crate::learning::phase_label(&lm.state.phase);
            let name = app
                .config
                .as_ref()
                .map(|c| c.name.as_str())
                .unwrap_or("embraOS");
            vec![
                Span::styled(
                    format!(" {} ", name),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("v{}", env!("CARGO_PKG_VERSION"))),
                Span::raw(" | "),
                Span::styled("Learning Mode", Style::default().fg(Color::Magenta)),
                Span::raw(format!(" | Phase: {}", phase_label)),
            ]
        }
        AppMode::Operational { session_name } => {
            let name = app
                .config
                .as_ref()
                .map(|c| c.name.as_str())
                .unwrap_or("Embra");
            vec![
                Span::styled(
                    format!(" {} ", name),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    "v{}",
                    app.config
                        .as_ref()
                        .map(|c| c.version.as_str())
                        .unwrap_or(env!("CARGO_PKG_VERSION"))
                )),
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
    // Collect all logical lines as multi-segment styled lines (DESIGN-004)
    let mut styled_lines: Vec<StyledLine> = Vec::new();

    for msg in &app.messages {
        let (color, prefix) = match msg.role.as_str() {
            "You" => (Color::LightBlue, "You"),
            "system" => (Color::White, ""),
            _ => (Color::Green, msg.role.as_str()),
        };

        let base_style = Style::default().fg(color);

        if msg.role == "system" {
            let style = Style::default().fg(Color::White);
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
                    // JSON syntax highlighting
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

    // Show thinking indicator (before first token arrives)
    if app.thinking && app.streaming_text.is_none() {
        let name = app
            .config
            .as_ref()
            .map(|c| c.name.as_str())
            .unwrap_or("Embra");
        // Animate dots based on elapsed time (~3 states cycling)
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
            format!("  {} is thinking{}", name, dots),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )]);
    }

    // Show streaming text if present
    if let Some(streaming) = &app.streaming_text {
        let name = app
            .config
            .as_ref()
            .map(|c| c.name.as_str())
            .unwrap_or("Embra");
        styled_lines.push(vec![StyledSegment::new(
            format!("{}: ", name),
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

    // Manually wrap lines and render from the bottom up.
    let content_width = area.width.saturating_sub(2) as usize; // 1px border each side
    let visible_rows = area.height as usize;

    if content_width == 0 || visible_rows == 0 {
        return;
    }

    // Wrap multi-segment lines into visual rows
    let mut visual_rows: Vec<Vec<Span>> = Vec::new();
    for styled_line in &styled_lines {
        // Calculate total display width for this line (unicode-aware)
        let total_len: usize = styled_line.iter().map(|s| UnicodeWidthStr::width(s.text.as_str())).sum();

        if total_len == 0 {
            visual_rows.push(vec![Span::raw("")]);
        } else if total_len <= content_width {
            // Fits in one row — convert segments to spans directly
            let spans: Vec<Span> = styled_line
                .iter()
                .map(|seg| Span::styled(seg.text.clone(), seg.style))
                .collect();
            visual_rows.push(spans);
        } else {
            // Need to wrap: flatten all chars with styles, then chunk by display width
            let mut flat_chars: Vec<(char, Style)> = Vec::new();
            for seg in styled_line {
                for ch in seg.text.chars() {
                    flat_chars.push((ch, seg.style));
                }
            }

            // Break into rows respecting character display widths
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

                // Group consecutive chars with same style into spans
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

    // Determine which rows to show (from the bottom, adjusted by scroll_offset)
    let total = visual_rows.len();
    let skip_from_bottom = app.scroll_offset as usize;
    let end = total.saturating_sub(skip_from_bottom);
    let start = end.saturating_sub(visible_rows);

    let visible_slice = &visual_rows[start..end];

    // Build ratatui Lines from our pre-wrapped rows
    let lines: Vec<Line> = visible_slice
        .iter()
        .map(|spans| Line::from(spans.clone()))
        .collect();

    // Render without Wrap (we already wrapped manually) and without scroll
    let paragraph = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::LEFT | Borders::RIGHT));

    f.render_widget(paragraph, area);
}

fn draw_input(f: &mut Frame, area: Rect, app: &AppState) {
    // If selector is active, show selection hint instead of text input
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

    let (placeholder, title) = match &app.mode {
        AppMode::Setup(setup) => {
            let ph = match setup.step {
                SetupStep::Name => "Enter a custom name...",
                SetupStep::ApiKey => "Enter API key...",
                SetupStep::Timezone => "Enter timezone (e.g. America/New_York)...",
                SetupStep::Confirm => "Type yes or no...",
            };
            (ph, " Input ")
        }
        AppMode::Learning(_) => ("Type a message...", " You "),
        AppMode::Operational { .. } => ("Type a message...", " You "),
    };

    // Show paste indicator or normal input (FEATURE-001: improved previews)
    let (input_text, style) = if let Some(ref pasted) = app.pasted_lines {
        let total_chars: usize = pasted.iter().map(|l| l.len()).sum::<usize>() + pasted.len().saturating_sub(1);
        let preview = if pasted.len() == 1 && total_chars > 200 {
            // Long single-line paste
            format!("[{} chars pasted] press Enter to send", total_chars)
        } else if pasted.len() > 2 {
            // Multi-line: show first 2 lines + count
            let first_two: String = pasted.iter().take(2).map(|l| {
                if l.len() > 60 { format!("{}...", &l[..57]) } else { l.clone() }
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

    let input = Paragraph::new(input_text).style(style).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .title_style(Style::default().fg(Color::LightBlue)),
    );

    f.render_widget(input, area);

    // Position cursor (only for text input, not paste or selector)
    if app.pasted_lines.is_none() && !app.input_buffer.is_empty() {
        // For multi-line input, find which line the cursor is on and the offset within that line
        let before_cursor: String = app.input_buffer.chars().take(app.cursor_pos).collect();
        let cursor_line = before_cursor.chars().filter(|c| *c == '\n').count() as u16;
        let last_newline_pos = before_cursor.rfind('\n').map(|p| p + 1).unwrap_or(0);
        let line_text = &before_cursor[last_newline_pos..];
        let display_offset: u16 = line_text
            .chars()
            .map(|c| if c.is_ascii() { 1u16 } else { 2u16 })
            .sum();
        f.set_cursor_position((area.x + display_offset + 1, area.y + 1 + cursor_line));
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
        AppMode::Learning(lm) => {
            let phase_num = match lm.state.phase {
                crate::learning::LearningPhase::UserConfiguration => 1,
                crate::learning::LearningPhase::IdentityFormation => 2,
                crate::learning::LearningPhase::SoulDefinition => 3,
                crate::learning::LearningPhase::InitialToolset => 4,
                crate::learning::LearningPhase::Confirmation => 5,
                crate::learning::LearningPhase::Complete => 5,
            };
            vec![
                Span::raw(format!(" Phase {}/5", phase_num)),
                Span::raw(" | Brain: opus-4.6"),
            ]
        }
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
    spans.push(Span::raw(" | Status: "));
    spans.push(Span::styled(
        &app.status_message,
        Style::default().fg(status_color),
    ));

    let status = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray));
    f.render_widget(status, area);
}
