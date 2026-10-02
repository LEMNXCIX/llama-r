/// Build the chat header's agent label.
///
/// Two projects can register an agent with the same id, so showing the project
/// is what makes the selection unambiguous.
pub fn agent_header_label(agent: Option<&str>, project: Option<&str>) -> String {
    match (agent, project) {
        (None, _) => " Direct (no agent) ".to_string(),
        (Some(id), None) => format!(" Agent: {id} · Project: — "),
        (Some(id), Some(project)) => format!(" Agent: {id} · Project: {project} "),
    }
}

/// Braille spinner frames, shared by the chat and analysis loaders.
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Spinner frame for a given elapsed duration.
///
/// `frame_interval` controls the animation speed. The frame is derived from
/// elapsed time rather than a counter so the animation stays smooth regardless
/// of how often the view is redrawn.
pub fn spinner_frame(
    elapsed: std::time::Duration,
    frame_interval: std::time::Duration,
) -> &'static str {
    let interval = frame_interval.as_millis().max(1) as u128;
    let index = (elapsed.as_millis() / interval) as usize % SPINNER_FRAMES.len();
    SPINNER_FRAMES[index]
}

/// The "thinking" status line.
///
/// The braille glyph is the animation; a trailing "..." would be redundant and
/// looks broken next to a moving spinner, so it is not used.
pub fn thinking_line(elapsed: std::time::Duration) -> String {
    format!("  {} Thinking", spinner_frame(elapsed, SPINNER_INTERVAL))
}

/// Animation speed for both loaders: one frame every 120ms (~8 fps), fast
/// enough to read as motion, slow enough not to be distracting.
pub const SPINNER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(120);

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

/// Returns `(scroll_max, messages_rect)` so the caller can drive scroll
/// state and hit-test mouse events on the messages region.
pub fn render_chat(
    f: &mut Frame,
    messages: &[(String, String)],
    input: &str,
    loading: bool,
    loading_since: Option<std::time::Instant>,
    selected_agent: &Option<String>,
    selected_project: &Option<String>,
    _available_agents: &[String],
    _agent_index: usize,
    scroll: usize,
) -> (usize, Rect) {
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
    let agent_label = agent_header_label(selected_agent.as_deref(), selected_project.as_deref());
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
        let elapsed = loading_since
            .map(|start| start.elapsed())
            .unwrap_or_default();
        lines.push(Line::from(Span::styled(
            thinking_line(elapsed),
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
            let raw_len = l
                .spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>();
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

    (scroll_max, chunks[1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn header_shows_project_next_to_agent() {
        let label = agent_header_label(Some("soporte"), Some("fudi"));
        assert!(label.contains("soporte"), "{label}");
        assert!(
            label.contains("fudi"),
            "the project must be shown so same-named agents are distinguishable: {label}"
        );
    }

    #[test]
    fn header_marks_global_agents() {
        let label = agent_header_label(Some("notificador"), None);
        assert!(label.contains("notificador"), "{label}");
        assert!(
            label.contains("Project"),
            "a global agent still needs a project slot, marked as none: {label}"
        );
    }

    #[test]
    fn header_without_agent_says_so() {
        assert!(agent_header_label(None, Some("fudi")).contains("no agent"));
    }

    #[test]
    fn header_distinguishes_same_named_agents_from_different_projects() {
        let a = agent_header_label(Some("soporte"), Some("fudi"));
        let b = agent_header_label(Some("soporte"), Some("clinica"));
        assert_ne!(
            a, b,
            "two projects with an agent of the same id must render differently"
        );
    }

    fn ms(total: u128) -> Duration {
        Duration::from_millis(total as u64)
    }

    #[test]
    fn spinner_advances_over_time() {
        let first = spinner_frame(ms(0), SPINNER_INTERVAL);
        let later = spinner_frame(ms(400), SPINNER_INTERVAL);
        assert_ne!(first, later, "the spinner must actually move");
    }

    #[test]
    fn spinner_cycles_through_every_frame() {
        let interval = SPINNER_INTERVAL.as_millis() as u64;
        let mut seen = std::collections::HashSet::new();
        for step in 0..SPINNER_FRAMES.len() {
            seen.insert(spinner_frame(
                ms(interval as u128 * step as u128),
                SPINNER_INTERVAL,
            ));
        }
        assert_eq!(
            seen.len(),
            SPINNER_FRAMES.len(),
            "frames must not repeat early"
        );
    }

    #[test]
    fn spinner_is_stable_within_a_frame_interval() {
        // The redraw loop runs every 50ms; several redraws inside one 120ms
        // frame must not jitter the glyph.
        let a = spinner_frame(ms(100), SPINNER_INTERVAL);
        let b = spinner_frame(ms(110), SPINNER_INTERVAL);
        assert_eq!(a, b);
    }

    #[test]
    fn thinking_line_animates_and_has_no_trailing_dots() {
        let first = thinking_line(ms(0));
        let later = thinking_line(ms(400));
        assert_ne!(first, later, "the thinking dots must move");
        assert!(
            !first.contains("..."),
            "redundant dots must be gone: {first}"
        );
        assert!(first.contains("Thinking"), "label must remain: {first}");
    }

    #[test]
    fn thinking_line_only_uses_spinner_glyphs() {
        for step in 0..40 {
            let line = thinking_line(ms(SPINNER_INTERVAL.as_millis() as u128 * step));
            let glyph = line
                .trim_start()
                .chars()
                .next()
                .expect("line must start with the spinner glyph");
            assert!(
                SPINNER_FRAMES.contains(&glyph.to_string().as_str()),
                "unexpected glyph {glyph:?} in {line:?}"
            );
        }
    }
}
