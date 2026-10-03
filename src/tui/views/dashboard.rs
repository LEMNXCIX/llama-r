use crate::api::handlers::AppState;
use crate::tui::{chrome, theme};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use std::sync::atomic::Ordering;

/// Blank columns between neighbouring fields of the status row.
const GAP: &str = "  ";

/// A server that is answering.
const UP: &str = "●";

/// A server that is not.
const DOWN: &str = "○";

/// Draws the dashboard inside `body`, the part of the screen the shared chrome
/// left over.
///
/// A row of status, a rule, then the log list: the three-row product banner and
/// the four bordered blocks it used to spend 13 rows on are now two. The banner
/// went because the shared bar already carries the interface's identity — which
/// view you are in, under which context, and whether the provider answers — and
/// the status row below names this gateway without a title. What is left
/// answers the three questions a user lands here with: are the servers up, how
/// much is configured, and what has happened since.
///
/// What the bar does *not* carry is the product's name: `Llama-R` appears
/// nowhere in the TUI now. Restoring it belongs to the bar itself, not to a
/// view that would otherwise repeat the banner this function deleted.
pub fn render_dashboard(f: &mut Frame, body: Rect, state: &AppState, log_scroll: usize) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ]
            .as_ref(),
        )
        .split(body);

    f.render_widget(status_line(state), chunks[0]);
    chrome::separator(f, chunks[1]);

    let log_lines = log_lines(state);
    // The clamp is the one that shipped: it holds the last line at the top of
    // the list rather than scrolling past it into an empty area. Scrolling is
    // not this task's to redesign.
    let log_lines_count = log_lines.len();
    f.render_widget(
        Paragraph::new(log_lines)
            .scroll((log_scroll.min(log_lines_count.saturating_sub(1)) as u16, 0)),
        chunks[2],
    );
}

/// Everything the two bordered panels used to show, on one row.
///
/// Servers first: a dead server is the only thing on this row the user has to
/// act on, and a row that runs out of columns drops its tail, not its head.
/// The row does not wrap — it clips — so it can never become the two-row block
/// it replaces.
fn status_line(state: &AppState) -> Line<'static> {
    let metrics = state.observability.snapshot();
    let mut spans: Vec<Span<'static>> = Vec::new();

    for (name, running) in [
        ("api", state.api_running.load(Ordering::SeqCst)),
        ("grpc", state.grpc_running.load(Ordering::SeqCst)),
    ] {
        push_gap(&mut spans);
        spans.push(Span::styled(
            if running { UP } else { DOWN },
            if running { theme::ok() } else { theme::error() },
        ));
        spans.push(Span::styled(format!(" {name}"), theme::content()));
    }

    for count in [
        count(
            state.agent_registry.list_agents().len(),
            "agente",
            "agentes",
        ),
        count(
            state.context_store.list_all_projects().len(),
            "proyecto",
            "proyectos",
        ),
        format!("{} http", metrics.http_requests),
        format!("{} chat", metrics.chat_requests),
        count(metrics.fallback_count as usize, "fallback", "fallbacks"),
        format!("{} saved", state.metrics.get_saved_tokens()),
    ] {
        push_gap(&mut spans);
        spans.push(Span::styled(count, theme::content()));
    }

    Line::from(spans)
}

/// Separates this field from the previous one. The first field has nothing
/// before it to be separated from.
fn push_gap(spans: &mut Vec<Span<'static>>) {
    if !spans.is_empty() {
        spans.push(Span::styled(GAP, theme::chrome()));
    }
}

/// `1 agente`, `3 agentes`.
fn count(total: usize, singular: &str, plural: &str) -> String {
    if total == 1 {
        format!("{total} {singular}")
    } else {
        format!("{total} {plural}")
    }
}

/// The log lines, each in the colour of the level it names.
///
/// The subscriber writes `[12:00:02] ERROR message`, so the level comes first —
/// but the test is a `contains` over the whole line, and the order of the
/// levels is the one that shipped. A message that itself names an earlier level
/// (`[ERROR] falling back to INFO ...`) therefore reads in that earlier
/// level's colour. Narrowing the test to the level's own field would fix it;
/// that changes behaviour, so it is left alone here.
fn log_lines(state: &AppState) -> Vec<Line<'static>> {
    match state.logs.lock() {
        Ok(logs) => logs
            .iter()
            .map(|line| Line::from(Span::styled(line.clone(), level_style(line))))
            .collect(),
        Err(_) => vec![Line::from(Span::styled(
            "Log buffer unavailable",
            theme::error(),
        ))],
    }
}

/// The colour a log line reads at. A line naming no level is chrome, so it
/// recedes behind the ones that do.
fn level_style(line: &str) -> Style {
    if line.contains("INFO") {
        theme::ok()
    } else if line.contains("WARN") {
        theme::warn()
    } else if line.contains("ERROR") {
        theme::error()
    } else {
        theme::chrome()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use ratatui::Terminal;

    use super::super::test_helpers::{all_text, column_of, row_text, state_with};

    /// The dashboard on an 80x24 terminal, inside the body the chrome leaves
    /// under the bar.
    fn render(state: &AppState, log_scroll: usize) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render_dashboard(f, Rect::new(0, 1, 80, 22), state, log_scroll))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// Row the status line takes, the row the rule under it takes, and the row
    /// the log list starts on.
    const STATUS_ROW: u16 = 1;
    const RULE_ROW: u16 = 2;
    const FIRST_LOG_ROW: u16 = 3;

    /// Fills the state's log buffer with `lines`, in order.
    fn with_logs(state: &AppState, lines: &[&str]) {
        let mut buffer = state.logs.lock().unwrap();
        for line in lines {
            buffer.push_back((*line).to_string());
        }
    }

    /// The mark standing in front of `name` on row `y`, and the colour it was
    /// drawn in.
    ///
    /// Searched backwards from the name so the gap in between does not have to
    /// be a particular width for the mark to be found.
    fn mark_of(buffer: &Buffer, y: u16, name: &str) -> (String, Color) {
        let name_at = column_of(buffer, y, name)
            .unwrap_or_else(|| panic!("{name:?} is not on row {y}: {:?}", row_text(buffer, y)));
        let column = (0..name_at)
            .rev()
            .find(|x| buffer[(*x as u16, y)].symbol() != " ")
            .unwrap_or_else(|| panic!("no mark in front of {name:?} on row {y}"))
            as u16;
        (
            buffer[(column, y)].symbol().to_string(),
            buffer[(column, y)].fg,
        )
    }

    /// The design's central claim about this view: the title banner and every
    /// box are gone. `─` is deliberately left legal — it is the rule the
    /// redesign puts in their place — but a `Block` with borders draws the
    /// corners and the verticals, which nothing here may draw.
    #[test]
    fn dashboard_drops_the_title_banner_and_the_boxes() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust")], &[("fudi", 2)]);
        let text = all_text(&render(&state, 0));
        for ch in ['┌', '┐', '└', '┘', '│'] {
            assert!(
                !text.contains(ch),
                "dashboard must not draw box char {ch:?}: {text}"
            );
        }
        assert!(
            !text.contains("High-Performance"),
            "the title banner must be gone: {text}"
        );
        assert!(
            !text.contains("Llama-R"),
            "the product banner must be gone: {text}"
        );
        // A view that draws nothing passes every absence check above, so the
        // status content and the rule that replaces the boxes are pinned too.
        assert!(
            text.contains("api") && text.contains("grpc"),
            "both servers must be reported: {text}"
        );
        assert!(
            text.contains('─'),
            "the rule under the status must render: {text}"
        );
    }

    /// The status is *one* row. Every field the two bordered panels used to
    /// carry has to land on that single row, with the rule directly beneath it
    /// and the logs below the rule — a view that spread the same fields over a
    /// four-row panel would still pass the border check above.
    #[test]
    fn the_status_is_one_row_above_the_rule() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust")], &[("fudi", 2)]);
        let buffer = render(&state, 0);
        let status = row_text(&buffer, STATUS_ROW);
        // Stems, not whole words: the counts inflect (`1 proyecto`,
        // `2 proyectos`) and this is about the field being there at all.
        for field in [
            "api", "grpc", "agent", "proyect", "http", "chat", "fallback", "saved",
        ] {
            assert!(
                status.contains(field),
                "{field:?} must be on the status row: {status:?}"
            );
        }
        let rule = row_text(&buffer, RULE_ROW);
        assert!(
            rule.chars().all(|ch| ch == '─'),
            "row {RULE_ROW} must be nothing but the rule: {rule:?}"
        );
        assert_eq!(
            row_text(&buffer, FIRST_LOG_ROW).trim(),
            "",
            "the log list must start under the rule"
        );
    }

    /// The counts have to carry the right *numbers* behind their words. Every
    /// needle above is a stem, and two numbers side by side satisfy every stem
    /// even when they are swapped — the fixture has one project and two agents,
    /// so `1 agentes  2 proyectos` reads the same way through the list above.
    /// The project count is also the only information this view adds, so it
    /// gets a number of its own rather than a word.
    ///
    /// A fresh fixture's request counters are all zero, which would leave a
    /// hardcoded `0` indistinguishable from the real figure, so they are moved
    /// off zero first. Every number the row shows is then pinned to the state
    /// field it comes from.
    #[test]
    fn the_status_numbers_come_from_the_state() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust")], &[("fudi", 2)]);
        for _ in 0..3 {
            state.observability.record_http_request();
        }
        state.observability.record_chat_request(800);
        state.observability.record_fallback();
        // 400 characters in, 200 out: the metric's own one-token-per-four-
        // characters heuristic makes that 50 saved tokens.
        state.metrics.record_optimization(400, 200);

        let status = row_text(&render(&state, 0), STATUS_ROW);

        let agents = state.agent_registry.list_agents().len();
        assert!(
            status.contains(&format!("{agents} agentes")),
            "the agent count must be the number of agents, not the project's: {status:?}"
        );
        let projects = state.context_store.list_all_projects().len();
        assert!(
            status.contains(&format!("{projects} proyecto")),
            "the project count must be the number of projects: {status:?}"
        );

        let metrics = state.observability.snapshot();
        for number in [
            format!("{} http", metrics.http_requests),
            format!("{} chat", metrics.chat_requests),
            format!("{} fallback", metrics.fallback_count),
            format!("{} saved", state.metrics.get_saved_tokens()),
        ] {
            assert!(
                status.contains(&number),
                "{number:?} must be the state's own figure: {status:?}"
            );
        }
    }

    /// `count` inflects, and nothing above can see it happen: those needles are
    /// stems on purpose. This view's first draft read `1 proyectos`, which no
    /// stem-based assertion would have caught.
    #[test]
    fn the_counts_inflect_with_the_number() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, one) = state_with(&[("fudi", "rust")], &[]);
        let one_project = row_text(&render(&one, 0), STATUS_ROW);
        // The plural check is the one with teeth: `1 proyectos` contains
        // `1 proyecto`, so only forbidding it tells the two apart.
        assert!(
            one_project.contains("1 proyecto") && !one_project.contains("1 proyectos"),
            "one project is singular: {one_project:?}"
        );

        let (_dir, two) = state_with(&[("fudi", "rust"), ("clinica", "rust")], &[]);
        let two_projects = row_text(&render(&two, 0), STATUS_ROW);
        assert!(
            two_projects.contains("2 proyectos"),
            "two projects are plural: {two_projects:?}"
        );
    }

    /// `●` is a server that answers and `○` one that does not, so the marks have
    /// to follow the two atomics. The old view spelled the state out in words;
    /// this one carries it in the mark and the colour, which is what makes the
    /// two servers readable at a glance.
    #[test]
    fn the_server_marks_follow_the_runtime_state() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[], &[]);
        state.api_running.store(true, Ordering::SeqCst);
        state.grpc_running.store(false, Ordering::SeqCst);
        let buffer = render(&state, 0);
        let (api_mark, api_colour) = mark_of(&buffer, STATUS_ROW, "api");
        let (grpc_mark, grpc_colour) = mark_of(&buffer, STATUS_ROW, "grpc");
        assert_eq!(api_mark, "●", "a server that answers is a filled mark");
        assert_eq!(grpc_mark, "○", "a server that does not is a hollow mark");
        assert_eq!(
            api_colour,
            theme::ok().fg.unwrap(),
            "a live server reads as ok"
        );
        assert_eq!(
            grpc_colour,
            theme::error().fg.unwrap(),
            "a dead server reads as error"
        );
    }

    /// The level in a log line decides its colour. This is the one mapping the
    /// redesign changes: INFO was cyan (the `action` token, for something the
    /// user can act on) and now reads as ordinary healthy output.
    #[test]
    fn log_lines_take_the_colour_of_their_level() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[], &[]);
        with_logs(
            &state,
            &[
                "[12:00:00] INFO server listening",
                "[12:00:01] WARN slow response",
                "[12:00:02] ERROR provider unreachable",
                "a line that names no level",
            ],
        );
        let buffer = render(&state, 0);
        for (needle, token) in [
            ("INFO", theme::ok()),
            ("WARN", theme::warn()),
            ("ERROR", theme::error()),
            ("names no level", theme::chrome()),
        ] {
            let y = (0..buffer.area.height)
                .find(|y| row_text(&buffer, *y).contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} is not on screen"));
            let x = column_of(&buffer, y, needle).unwrap() as u16;
            assert_eq!(
                buffer[(x, y)].fg,
                token.fg.unwrap(),
                "{needle:?} must be drawn in its level's colour"
            );
        }
    }

    /// The scroll clamp is the list's contract: scrolling moves the top of the
    /// list, and a scroll past the end holds the last line at the top instead of
    /// scrolling off into an empty area. The clamp is deliberately the one that
    /// shipped — `count - 1`, not viewport-aware — so this pins that too.
    #[test]
    fn the_log_list_scrolls_and_clamps_at_the_end() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[], &[]);
        with_logs(&state, &["primera", "segunda", "tercera"]);

        let at_top = row_text(&render(&state, 0), FIRST_LOG_ROW);
        assert!(
            at_top.contains("primera"),
            "the first log is the top: {at_top:?}"
        );

        let one_down = row_text(&render(&state, 1), FIRST_LOG_ROW);
        assert!(
            one_down.contains("segunda") && !one_down.contains("primera"),
            "one row of scroll moves the top to the second log: {one_down:?}"
        );

        let past_the_end = row_text(&render(&state, 99), FIRST_LOG_ROW);
        assert!(
            past_the_end.contains("tercera"),
            "a scroll past the end holds the last line at the top: {past_the_end:?}"
        );
    }

    /// The status row and the rule take two rows between them, so a terminal with
    /// fewer than three rows leaves the log area with none — which has to be
    /// survivable rather than a subtraction that wraps.
    #[test]
    fn the_dashboard_survives_a_tiny_terminal() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust")], &[("fudi", 1)]);
        with_logs(&state, &["una linea de log"]);
        for (width, height) in [(1u16, 1u16), (2, 2), (3, 1), (10, 1), (2, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|f| {
                    render_dashboard(f, Rect::new(0, 0, width, height), &state, 99);
                })
                .unwrap();
        }
    }
}
