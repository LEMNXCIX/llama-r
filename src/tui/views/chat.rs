use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

pub fn render_chat(
    f: &mut Frame,
    messages: &[(String, String)],
    input: &str,
    loading: bool,
    selected_agent: &Option<String>,
    _available_agents: &[String],
    _agent_index: usize,
    scroll: usize,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints(
            [
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(3),
            ]
            .as_ref(),
        )
        .split(f.area());

    // ── Header ────────────────────────────────────────────────────────────
    let agent_label = match selected_agent {
        Some(id) => format!(" Agent: {id} "),
        None => " Direct (no agent) ".to_string(),
    };
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            "Chat ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(agent_label, Style::default().fg(Color::Cyan)),
        Span::styled(
            "  [←/→] Agent  [Tab] Next View  [Esc] Back",
            Style::default().fg(Color::Gray),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    // ── Build text lines ───────────────────────────────────────────────────
    let mut lines: Vec<Line> = Vec::new();
    for (role, content) in messages {
        let (label, color) = if role == "user" {
            (" You", Color::Yellow)
        } else {
            (" AI", Color::Cyan)
        };
        lines.push(Line::from(Span::styled(
            format!("{label}:"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
        for line in content.split('\n') {
            lines.push(Line::from(Span::styled(
                format!(" {line}"),
                Style::default().fg(Color::White),
            )));
        }
        lines.push(Line::from(""));
    }

    if loading {
        lines.push(Line::from(Span::styled(
            "  ⠋ Thinking...",
            Style::default().fg(Color::Gray),
        )));
    }

    // ── Compute wrap-aware scroll_max ──────────────────────────────────────
    // The inner width of the messages block (subtract 2 for borders).
    let inner_width = chunks[1].width.saturating_sub(2) as usize;
    let inner_height = chunks[1].height.saturating_sub(2) as usize;

    // Count how many terminal rows each logical line will occupy after wrapping.
    let total_wrapped_rows: usize = lines
        .iter()
        .map(|l| {
            let raw_len = l.spans.iter().map(|s| s.content.chars().count()).sum::<usize>();
            if inner_width == 0 || raw_len == 0 {
                1
            } else {
                raw_len.div_ceil(inner_width).max(1)
            }
        })
        .sum();

    let scroll_max = total_wrapped_rows.saturating_sub(inner_height);
    let clamped_scroll = scroll.min(scroll_max);

    // ── Scroll position indicator ──────────────────────────────────────────
    let scroll_hint = if scroll_max > 0 {
        let pct = if scroll_max == 0 {
            100u16
        } else {
            ((clamped_scroll * 100) / scroll_max).min(100) as u16
        };
        format!(" Messages  ↑↓ PgUp/PgDn │ {pct}% ")
    } else {
        " Messages ".to_string()
    };

    let messages_widget = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(scroll_hint)
                .title_style(
                    Style::default()
                        .fg(if scroll_max > 0 {
                            Color::Yellow
                        } else {
                            Color::Gray
                        })
                        .add_modifier(Modifier::BOLD),
                ),
        )
        .wrap(Wrap { trim: false })
        .scroll((clamped_scroll as u16, 0));
    f.render_widget(messages_widget, chunks[1]);

    // ── Input ──────────────────────────────────────────────────────────────
    let input_widget = Paragraph::new(input)
        .block(Block::default().borders(Borders::ALL).title(" Input "))
        .style(Style::default().fg(Color::White));
    f.render_widget(input_widget, chunks[2]);

    // ── Footer ────────────────────────────────────────────────────────────
    let footer = Paragraph::new(
        " [←/→] Switch Agent  [Enter] Send  [↑↓] Scroll  [PgUp/PgDn] Fast Scroll  [Tab] Next View",
    )
    .block(Block::default().borders(Borders::ALL))
    .style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[3]);

    // ── Cursor ────────────────────────────────────────────────────────────
    let x = chunks[2].x + 1 + input.chars().count() as u16;
    let y = chunks[2].y + 1;
    f.set_cursor_position((x.min(chunks[2].x + chunks[2].width.saturating_sub(2)), y));
}
