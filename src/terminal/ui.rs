use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::{AppMode, AppState, SetupStep};

pub fn draw(f: &mut Frame, app: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // Header
            Constraint::Min(5),    // Conversation
            Constraint::Length(3), // Input
            Constraint::Length(1), // Status bar
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
    // Collect all logical lines with their styles
    let mut styled_lines: Vec<(String, Style)> = Vec::new();

    for msg in &app.messages {
        let (color, prefix) = match msg.role.as_str() {
            "You" => (Color::LightBlue, "You"),
            "system" => (Color::White, ""),
            _ => (Color::Green, msg.role.as_str()),
        };

        if msg.role == "system" {
            let style = Style::default().fg(Color::White);
            for line in msg.content.lines() {
                styled_lines.push((format!("  {}", line), style));
            }
        } else {
            let header = format!("[{}] {}: ", msg.timestamp, prefix);
            styled_lines.push((
                header,
                Style::default()
                    .fg(color)
                    .add_modifier(Modifier::BOLD),
            ));
            let style = Style::default().fg(color);
            for line in msg.content.lines() {
                styled_lines.push((format!("  {}", line), style));
            }
        }
        styled_lines.push((String::new(), Style::default()));
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
            styled_lines.push((format!("  {}{}", marker, option), style));
        }
        styled_lines.push((String::new(), Style::default()));
    }

    // Show streaming text if present
    if let Some(streaming) = &app.streaming_text {
        let name = app
            .config
            .as_ref()
            .map(|c| c.name.as_str())
            .unwrap_or("Embra");
        styled_lines.push((
            format!("{}: ", name),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
        let style = Style::default().fg(Color::Green);
        for line in streaming.lines() {
            styled_lines.push((format!("  {}", line), style));
        }
        styled_lines.push((
            "  ▊".to_string(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::SLOW_BLINK),
        ));
    }

    // Manually wrap lines and render from the bottom up.
    // This avoids any mismatch with ratatui's internal scroll calculation.
    let content_width = area.width.saturating_sub(2) as usize; // 1px border each side
    let visible_rows = area.height as usize;

    if content_width == 0 || visible_rows == 0 {
        return;
    }

    // Wrap all logical lines into visual rows: (text_segment, style)
    let mut visual_rows: Vec<(String, Style)> = Vec::new();
    for (text, style) in &styled_lines {
        if text.is_empty() {
            visual_rows.push((String::new(), *style));
        } else {
            // Character-level wrapping
            let chars: Vec<char> = text.chars().collect();
            let mut pos = 0;
            while pos < chars.len() {
                let end = (pos + content_width).min(chars.len());
                let segment: String = chars[pos..end].iter().collect();
                visual_rows.push((segment, *style));
                pos = end;
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
        .map(|(text, style)| Line::from(Span::styled(text.clone(), *style)))
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

    // Show paste indicator or normal input
    let (input_text, style) = if let Some(ref pasted) = app.pasted_lines {
        let line_count = pasted.len();
        let preview = format!("[{} lines pasted] press Enter to send", line_count);
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
        // Calculate display width of chars before cursor for correct positioning
        let display_offset: u16 = app
            .input_buffer
            .chars()
            .take(app.cursor_pos)
            .map(|c| if c.is_ascii() { 1u16 } else { 2u16 })
            .sum();
        f.set_cursor_position((area.x + display_offset + 1, area.y + 1));
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
