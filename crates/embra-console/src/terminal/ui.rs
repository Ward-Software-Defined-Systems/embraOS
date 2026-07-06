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
    // Input height from the same char-wrap packing that renders the text and
    // places the cursor (input_layout) — the three can't disagree. Cap at 10
    // (8 content rows); draw_input scrolls to keep the cursor row visible.
    let input_lines = if app.pasted_lines.is_some() || available_width == 0 || app.input_buffer.is_empty() {
        1
    } else {
        super::input_layout::layout_input(&app.input_buffer, app.cursor_pos, available_width)
            .content_rows()
    };
    let input_height = (input_lines as u16 + 2).min(10);

    // EXPR-01: optional expression panel as a horizontal band below the header.
    // Hidden on small terminals so the conversation keeps enough rows.
    let show_panel = app.expression_panel_visible();
    let panel_height: u16 = if show_panel { 8 } else { 0 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),            // Header
            Constraint::Length(panel_height), // Expression panel (EXPR-01)
            Constraint::Min(5),               // Conversation
            Constraint::Length(input_height), // Input (dynamic)
            Constraint::Length(1),            // Status bar
        ])
        .split(f.area());

    draw_header(f, chunks[0], app);
    if show_panel {
        draw_expression_panel(f, chunks[1], app);
    }
    draw_conversation(f, chunks[2], app);
    draw_input(f, chunks[3], app);
    draw_status_bar(f, chunks[4], app);
}

fn draw_expression_panel(f: &mut Frame, area: Rect, app: &AppState) {
    // Source switch: live reasoning during an active turn, expression
    // singleton when idle. The render style differentiates the two —
    // italic dark-gray for ephemeral reasoning vs solid gray for
    // operator-set expression — without consuming any of the 6 visible
    // content rows for a header.
    //
    // Both sources render through the same tail-anchored window so
    // `expression_scroll` (Shift+Up/Down/PageUp/PageDown) works on
    // either. Long `express` content therefore shows its TAIL by
    // default now (previously it head-clipped at the panel bottom with
    // the overflow unreachable) — with scroll, all of it is reachable.
    let inner_width = area.width.saturating_sub(2) as usize; // minus borders
    let inner_rows = area.height.saturating_sub(2) as usize; // minus borders
    let skip = app.expression_scroll as usize;
    let (rendered, style) = if !app.live_reasoning.is_empty() {
        (
            window_to_visible_rows(
                &sanitize_for_render(&app.live_reasoning),
                inner_width,
                inner_rows,
                skip,
            ),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )
    } else {
        (
            window_to_visible_rows(
                &sanitize_for_render(&app.expression_content),
                inner_width,
                inner_rows,
                skip,
            ),
            Style::default().fg(Color::Gray),
        )
    };
    let para = Paragraph::new(rendered)
        .block(Block::default().borders(Borders::ALL))
        .wrap(Wrap { trim: false })
        .style(style);
    f.render_widget(para, area);
}

/// Soft-wrap `text` by `cols` and return `rows` visual lines ending
/// `skip_from_bottom` rows above the tail, joined by newlines
/// (`skip_from_bottom == 0` = the last `rows` lines — the original tail
/// window). Mirrors `draw_conversation`'s `scroll_offset` math: the
/// offset counts up from the bottom, and over-scrolling saturates at
/// the top (possibly returning fewer than `rows` lines). Pre-windowing
/// matches `draw_conversation`'s pattern of explicit wrapping
/// management and avoids `Paragraph::scroll`'s drift on edge cases
/// (CJK width, trailing `\n`). `cols == 0` or `rows == 0` returns the
/// empty string.
fn window_to_visible_rows(text: &str, cols: usize, rows: usize, skip_from_bottom: usize) -> String {
    if cols == 0 || rows == 0 {
        return String::new();
    }
    let mut wrapped: Vec<String> = Vec::new();
    for source_line in text.split('\n') {
        if source_line.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0usize;
        for ch in source_line.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_width + w > cols && !current.is_empty() {
                wrapped.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(ch);
            current_width += w;
        }
        wrapped.push(current);
    }
    let end = wrapped.len().saturating_sub(skip_from_bottom);
    let start = end.saturating_sub(rows);
    wrapped[start..end].join("\n")
}

/// Render-side ANSI and control-char strip. Second defence layer behind
/// the brain-side sanitize in the `express` tool — the tool should already
/// keep these out of WardSONDB, but we never want a stray escape sequence
/// to corrupt the rest of the TUI.
fn sanitize_for_render(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek() {
                Some(&'[') => {
                    chars.next();
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        let cv = c as u32;
                        if (0x40..=0x7e).contains(&cv) {
                            break;
                        }
                    }
                }
                Some(&']') => {
                    chars.next();
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if c == '\x07' {
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        match ch {
            '\n' => out.push(ch),
            c if (c as u32) < 0x20 => continue,
            '\u{7f}' => continue,
            c if (c as u32) >= 0x80 && (c as u32) < 0xA0 => continue,
            c => out.push(c),
        }
    }
    out
}

fn draw_header(f: &mut Frame, area: Rect, app: &AppState) {
    let spans = match &app.mode {
        AppMode::Setup(setup) => {
            let step_label = match setup.step {
                SetupStep::Name => "Name",
                SetupStep::Provider => "Provider",
                SetupStep::ApiKey => "API Key",
                SetupStep::Endpoint => "Endpoint",
                SetupStep::BearerToken => "Bearer",
                SetupStep::ModelSelect => "Model",
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

    // Show thinking / tool-execution indicator
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
        let label = match (&app.current_tool, app.current_tool_started) {
            (Some(tool), Some(started)) => {
                let secs = started.elapsed().as_secs();
                format!("  {} is running {} ({}s){}", app.thinking_name, tool, secs, dots)
            }
            _ => format!("  {} is thinking{}", app.thinking_name, dots),
        };
        styled_lines.push(vec![StyledSegment::new(
            label,
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

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(Style::default().fg(Color::LightBlue));

    // Pasted-preview and placeholder branches: single synthetic line, no
    // cursor — unchanged rendering.
    if let Some(ref pasted) = app.pasted_lines {
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
        let input = Paragraph::new(preview)
            .style(Style::default().fg(Color::Yellow))
            .wrap(Wrap { trim: false })
            .block(block);
        f.render_widget(input, area);
        return;
    }
    if app.input_buffer.is_empty() {
        let input = Paragraph::new(placeholder.to_string())
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: false })
            .block(block);
        f.render_widget(input, area);
        return;
    }

    // Non-empty buffer: text rows, box height, and cursor all come from ONE
    // char-wrap packing (input_layout) — the render is pre-wrapped, so no
    // Paragraph::wrap here. The old code word-wrapped the render while
    // char-wrapping the cursor math, landing the cursor inside the text on
    // any soft-wrapped line. Scroll keeps the cursor row visible once the
    // input outgrows the box's height cap.
    let inner_width = area.width.saturating_sub(2) as usize; // minus borders
    let visible_rows = area.height.saturating_sub(2) as usize;
    let layout = super::input_layout::layout_input(&app.input_buffer, app.cursor_pos, inner_width);
    let scroll = super::input_layout::scroll_offset(layout.cursor_row, visible_rows);
    let end = (scroll + visible_rows).min(layout.rows.len());
    let start = scroll.min(end);
    let lines: Vec<Line> = layout.rows[start..end]
        .iter()
        .map(|row| Line::from(row.clone()))
        .collect();
    let input = Paragraph::new(Text::from(lines))
        .style(Style::default().fg(Color::White))
        .block(block);
    f.render_widget(input, area);

    if visible_rows > 0 {
        f.set_cursor_position((
            area.x + 1 + layout.cursor_col,
            area.y + 1 + (layout.cursor_row - scroll) as u16,
        ));
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
            Span::raw(format!(" | Brain: {}", app.provider_model)),
        ],
        AppMode::Operational { session_name } => vec![
            Span::raw(" Sessions: ["),
            Span::styled(
                format!("{}*", session_name),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(format!("] | Brain: {}", app.provider_model)),
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

#[cfg(test)]
mod expression_window_tests {
    //! Tail-anchored windowing for the expression panel, now with a
    //! from-bottom scroll offset (Shift+Up/Down/PageUp/PageDown). Pure
    //! fn — same test seam discipline as `input_layout`.
    use super::window_to_visible_rows;

    fn ten_lines() -> String {
        (1..=10).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn zero_offset_keeps_the_original_tail_window() {
        let out = window_to_visible_rows(&ten_lines(), 80, 6, 0);
        assert_eq!(out, "l5\nl6\nl7\nl8\nl9\nl10");
    }

    #[test]
    fn offset_scrolls_up_by_visual_rows() {
        let out = window_to_visible_rows(&ten_lines(), 80, 6, 2);
        assert_eq!(out, "l3\nl4\nl5\nl6\nl7\nl8");
        let out = window_to_visible_rows(&ten_lines(), 80, 6, 4);
        assert_eq!(out, "l1\nl2\nl3\nl4\nl5\nl6");
    }

    #[test]
    fn over_scroll_saturates_at_the_top() {
        // skip > total-rows pins to the head and may return fewer than
        // `rows` lines — the conversation pane's exact semantics.
        let out = window_to_visible_rows(&ten_lines(), 80, 6, 8);
        assert_eq!(out, "l1\nl2");
        let out = window_to_visible_rows(&ten_lines(), 80, 6, 100);
        assert_eq!(out, "");
    }

    #[test]
    fn offset_counts_wrapped_visual_rows_not_source_lines() {
        // 10 chars at cols=4 wraps into 3 visual rows: "aaaa","bbbb","cc".
        let out = window_to_visible_rows("aaaabbbbcc", 4, 2, 0);
        assert_eq!(out, "bbbb\ncc");
        let out = window_to_visible_rows("aaaabbbbcc", 4, 2, 1);
        assert_eq!(out, "aaaa\nbbbb");
    }

    #[test]
    fn zero_cols_or_rows_returns_empty() {
        assert_eq!(window_to_visible_rows("x", 0, 6, 0), "");
        assert_eq!(window_to_visible_rows("x", 80, 0, 3), "");
    }
}
