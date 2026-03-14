use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::AppState;

pub fn draw(f: &mut Frame, app: &AppState, session_name: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // Header
            Constraint::Min(5),    // Conversation
            Constraint::Length(3), // Input
            Constraint::Length(1), // Status bar
        ])
        .split(f.area());

    draw_header(f, chunks[0], app, session_name);
    draw_conversation(f, chunks[1], app);
    draw_input(f, chunks[2], app);
    draw_status_bar(f, chunks[3], app, session_name);
}

fn draw_header(f: &mut Frame, area: Rect, app: &AppState, session_name: &str) {
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" {} ", app.config.name),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("v{}", app.config.version)),
        Span::raw(" | Session: "),
        Span::styled(session_name, Style::default().fg(Color::Yellow)),
        Span::raw(" | "),
        Span::styled("● Online", Style::default().fg(Color::Green)),
    ]))
    .style(Style::default().bg(Color::DarkGray));

    f.render_widget(header, area);
}

fn draw_conversation(f: &mut Frame, area: Rect, app: &AppState) {
    let mut lines: Vec<Line> = Vec::new();

    for msg in &app.messages {
        let (color, prefix) = match msg.role.as_str() {
            "You" => (Color::Blue, "You"),
            "system" => (Color::DarkGray, "sys"),
            _ => (Color::Green, msg.role.as_str()),
        };

        lines.push(Line::from(vec![
            Span::styled(
                format!("[{}] ", msg.timestamp),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("{}: ", prefix),
                Style::default()
                    .fg(color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        // Wrap message content into lines
        for line in msg.content.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {}", line),
                Style::default().fg(color),
            )));
        }
        lines.push(Line::from(""));
    }

    // Show streaming text if present
    if let Some(streaming) = &app.streaming_text {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{}: ", app.config.name),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        for line in streaming.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {}", line),
                Style::default().fg(Color::Green),
            )));
        }
        lines.push(Line::from(Span::styled(
            "  ▊",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::SLOW_BLINK),
        )));
    }

    // Calculate scroll: show bottom of conversation
    let total_lines = lines.len() as u16;
    let visible_height = area.height.saturating_sub(2);
    let scroll = if total_lines > visible_height {
        total_lines - visible_height - app.scroll_offset.min(total_lines.saturating_sub(visible_height))
    } else {
        0
    };

    let conversation = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::LEFT | Borders::RIGHT))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    f.render_widget(conversation, area);
}

fn draw_input(f: &mut Frame, area: Rect, app: &AppState) {
    let input_text = if app.input_buffer.is_empty() {
        "Type a message...".to_string()
    } else {
        app.input_buffer.clone()
    };

    let style = if app.input_buffer.is_empty() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    };

    let input = Paragraph::new(input_text)
        .style(style)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" You ")
                .title_style(Style::default().fg(Color::Blue)),
        );

    f.render_widget(input, area);

    // Position cursor
    if !app.input_buffer.is_empty() {
        f.set_cursor_position((
            area.x + app.cursor_pos as u16 + 1,
            area.y + 1,
        ));
    }
}

fn draw_status_bar(f: &mut Frame, area: Rect, app: &AppState, session_name: &str) {
    let status_color = if app.status_message == "OK" {
        Color::Green
    } else if app.status_message.starts_with("Error") {
        Color::Red
    } else {
        Color::Yellow
    };

    let status = Paragraph::new(Line::from(vec![
        Span::raw(" Sessions: ["),
        Span::styled(
            format!("{}*", session_name),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("] | Brain: opus-4.6 | Status: "),
        Span::styled(&app.status_message, Style::default().fg(status_color)),
    ]))
    .style(Style::default().bg(Color::DarkGray));

    f.render_widget(status, area);
}
