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
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use crate::tui::{chrome, theme};

/// The prompt marker: it says the row is the input and that Enter sends.
const PROMPT: &str = "> ";

/// Marks text that did not fit.
const ELLIPSIS: char = '…';

/// What the input row shows of a possibly multi-line buffer.
///
/// The row is one line, so the last line is what shows: it is the one being
/// typed. `Shift+Enter` puts the earlier lines in the buffer, and they go out
/// with the message, so nothing is lost by not drawing them here.
///
/// When even that line is wider than the row, its **tail** is what shows. The
/// user is typing at the end and the cursor sits there, so cutting from the
/// left keeps the text being written visible and reviewable — cutting from the
/// right would leave the row frozen on a prefix that never changes again. A
/// leading `…` says something was cut.
pub fn input_display(buffer: &str, width: u16) -> String {
    let last = buffer.rsplit('\n').next().unwrap_or_default();
    let budget = width as usize;
    if Line::from(last).width() <= budget {
        return last.to_string();
    }
    if budget == 0 {
        return String::new();
    }

    // Walk back from the end, whole characters only, until one more would not
    // fit beside the ellipsis. Measuring each candidate suffix from the left
    // would be quadratic in the line's length; this is bounded by the budget.
    let keep = budget - 1;
    let mut used = 0;
    let mut start = last.len();
    for (index, ch) in last.char_indices().rev() {
        let w = Line::from(&last[index..index + ch.len_utf8()]).width();
        if used + w > keep {
            break;
        }
        used += w;
        start = index;
    }
    format!("{ELLIPSIS}{}", &last[start..])
}

/// Returns `(scroll_max, messages_rect)` so the caller can drive scroll
/// state and hit-test mouse events on the messages region.
///
/// `body` is the region the shared chrome left over; `messages_rect` is
/// therefore relative to it, and callers hit-test against the same rect.
///
/// The agent and project are deliberately not drawn here: the context bar
/// already carries them on every view, so repeating them inside chat is the
/// duplication the redesign is removing. The parameters stay so the call
/// signature is unchanged across the redesign.
pub fn render_chat(
    f: &mut Frame,
    body: Rect,
    messages: &[(String, String)],
    input: &str,
    loading: bool,
    loading_since: Option<std::time::Instant>,
    _selected_agent: &Option<String>,
    _selected_project: &Option<String>,
    _available_agents: &[String],
    _agent_index: usize,
    scroll: usize,
) -> (usize, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
            ]
            .as_ref(),
        )
        .split(body);

    // ── Messages ───────────────────────────────────────────────────────────
    let mut lines: Vec<Line> = Vec::new();
    for (role, content) in messages {
        // Labels recede; the message text is the content. Distinguishing the
        // speakers by colour was the only thing the label colours did, and the
        // words themselves already say who is speaking.
        let label = if role == "user" { " You:" } else { " AI:" };
        lines.push(Line::from(Span::styled(label, theme::chrome())));
        for line in content.split('\n') {
            lines.push(Line::from(Span::styled(
                format!(" {line}"),
                theme::content(),
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
            theme::chrome(),
        )));
    }

    // ── Compute wrap-aware scroll_max ──────────────────────────────────────
    // No borders, so the messages rect is already the text area.
    let inner_width = chunks[0].width as usize;
    let inner_height = chunks[0].height as usize;

    // Count how many terminal rows each logical line will occupy after wrapping.
    let total_wrapped_rows: usize = lines
        .iter()
        .map(|l| {
            let raw_len = l.width();
            if inner_width == 0 || raw_len == 0 {
                1
            } else {
                raw_len.div_ceil(inner_width).max(1)
            }
        })
        .sum();

    let scroll_max = total_wrapped_rows.saturating_sub(inner_height);
    let clamped_scroll = scroll.min(scroll_max);

    let messages_widget = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((clamped_scroll as u16, 0));
    f.render_widget(messages_widget, chunks[0]);

    // ── Separator ──────────────────────────────────────────────────────────
    chrome::separator(f, chunks[1]);

    // ── Input ──────────────────────────────────────────────────────────────
    let prompt_width = Line::from(PROMPT).width() as u16;
    let shown = input_display(input, chunks[2].width.saturating_sub(prompt_width));
    let input_widget = Paragraph::new(Line::from(vec![
        Span::styled(PROMPT, theme::action()),
        Span::styled(shown.clone(), theme::content()),
    ]));
    f.render_widget(input_widget, chunks[2]);

    // ── Cursor ────────────────────────────────────────────────────────────
    // Measured from the text the row actually shows, not the whole buffer, so a
    // multi-line buffer leaves the cursor at the end of its last line, and a
    // line too long for the row leaves it where the typing is.
    let x = chunks[2].x + prompt_width + Line::from(shown.as_str()).width() as u16;
    let last_column = chunks[2].x + chunks[2].width.saturating_sub(1);
    f.set_cursor_position((x.min(last_column), chunks[2].y));

    (scroll_max, chunks[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::chrome;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::time::Duration;

    fn row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect()
    }

    fn all_text(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| row_text(buffer, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn input_display_shows_the_last_line_of_a_multiline_buffer() {
        assert_eq!(input_display("one\ntwo\nthree", 40), "three");
        assert_eq!(input_display("single", 40), "single");
        assert_eq!(input_display("", 40), "");
    }

    #[test]
    fn input_display_truncates_to_the_available_width() {
        let shown = input_display("abcdefghij", 5);
        assert!(shown.chars().count() <= 5, "{shown:?}");
    }

    /// The end of the line is the text being typed, so that is what must
    /// survive. Cutting from the left instead would freeze the row on a prefix
    /// the user can no longer read or correct.
    #[test]
    fn input_display_keeps_the_end_of_a_line_that_does_not_fit() {
        assert_eq!(input_display("abcdefghij", 5), "…ghij");
        assert_eq!(input_display("abcdefghij", 2), "…j");
        assert_eq!(input_display("abc", 3), "abc", "an exact fit needs no cut");
    }

    #[test]
    fn input_display_never_splits_a_wide_character() {
        // Each glyph is two columns, so only one fits beside the ellipsis, and
        // with no room at all the ellipsis is all that is left.
        assert_eq!(input_display("你好世界", 4), "…界");
        assert_eq!(input_display("你好世界", 3), "…界");
        assert_eq!(input_display("你好世界", 2), "…");
        for width in 0..8 {
            let shown = input_display("你好世界", width);
            assert!(
                Line::from(shown.as_str()).width() <= width as usize,
                "{shown:?} is wider than {width}"
            );
        }
    }

    #[test]
    fn input_display_returns_nothing_when_there_is_no_room() {
        assert_eq!(input_display("texto", 0), "");
        assert_eq!(input_display("", 0), "");
    }

    /// The regression guard for tail truncation, at the level the user sees
    /// it: a long draft must keep showing what was just typed.
    #[test]
    fn a_long_input_row_shows_its_tail_not_its_head() {
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
        terminal
            .draw(|f| {
                render_chat(
                    f,
                    Rect::new(0, 0, 20, 6),
                    &[],
                    "abcdefghijklmnopqrstuvwxyz",
                    false,
                    None,
                    &None,
                    &None,
                    &[],
                    0,
                    0,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row = row_text(&buffer, 5);
        assert!(
            row.contains('…'),
            "the row must say it was cut from the left: {row:?}"
        );
        assert!(
            row.ends_with("yz"),
            "the text being typed must stay visible: {row:?}"
        );
    }

    #[test]
    fn chat_renders_without_any_bordered_block() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                render_chat(
                    f,
                    Rect::new(0, 1, 80, 22),
                    &[("user".to_string(), "hola".to_string())],
                    "",
                    false,
                    None,
                    &Some("soporte".to_string()),
                    &Some("fudi".to_string()),
                    &[],
                    0,
                    0,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text = all_text(&buffer);
        for ch in ['┌', '┐', '└', '┘', '│'] {
            assert!(!text.contains(ch), "chat must not draw box char {ch:?}");
        }
        assert!(text.contains("hola"), "the message must render");
        // Absence of borders alone would also pass a view that drew nothing but
        // whitespace, so pin the two things that must be there: the thin rule
        // that separates the conversation from the input, and the input row.
        assert!(
            text.contains('─'),
            "the thin separator must render: {text:?}"
        );
        assert!(
            text.contains("> "),
            "the input row and its prompt must render: {text:?}"
        );
    }

    /// Renders chat into `body` and returns the messages rect it reports back
    /// for scroll and mouse handling.
    fn messages_rect_in(body: Rect) -> Rect {
        let messages = vec![("user".to_string(), "hola".to_string())];
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut reported = Rect::default();
        terminal
            .draw(|f| {
                reported =
                    render_chat(f, body, &messages, "", false, None, &None, &None, &[], 0, 0).1;
            })
            .unwrap();
        reported
    }

    /// The bar's row must survive the chat view drawn after it.
    #[test]
    fn chat_leaves_the_bar_row_alone() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                let c = chrome::layout(f.area());
                chrome::render_bar(
                    f,
                    c.bar,
                    &["Dashboard", "Projects", "Chat"],
                    2,
                    &chrome::Context::default(),
                    true,
                );
                render_chat(
                    f,
                    c.body,
                    &[("user".to_string(), "hola".to_string())],
                    "",
                    false,
                    None,
                    &None,
                    &None,
                    &[],
                    0,
                    0,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let bar_row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            bar_row.contains("Dashboard") && bar_row.contains("Chat"),
            "the bar's row must survive the chat view: {bar_row:?}"
        );
    }

    /// The rect chat reports back must follow `body`. A view laying out the
    /// whole screen instead leaves it pinned to the screen, and the mouse wheel
    /// then scrolls rows that belong to the bar.
    #[test]
    fn the_reported_messages_rect_follows_the_body() {
        let at_top = messages_rect_in(Rect::new(0, 1, 80, 22));
        let lower = messages_rect_in(Rect::new(0, 4, 80, 14));
        assert_eq!(
            lower.y - at_top.y,
            3,
            "moving the body down 3 rows must move the messages rect down 3: \
             {at_top:?} then {lower:?}"
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

    /// The layout arithmetic subtracts and clamps against `chunks[2]`, which can
    /// be smaller than the prompt marker on a tiny terminal. Nothing may panic
    /// or underflow; `scroll` past the end must clamp too.
    #[test]
    fn chat_renders_on_a_tiny_terminal_without_panicking() {
        for (w, h) in [(1u16, 1u16), (2, 2), (3, 3), (5, 4), (10, 1), (2, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            let c = chrome::layout(Rect::new(0, 0, w, h));
            terminal
                .draw(|f| {
                    render_chat(
                        f,
                        c.body,
                        &[("user".to_string(), "hola".to_string())],
                        "un texto mas largo que la fila",
                        true,
                        Some(std::time::Instant::now()),
                        &None,
                        &None,
                        &[],
                        0,
                        99,
                    );
                })
                .unwrap();
        }
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
