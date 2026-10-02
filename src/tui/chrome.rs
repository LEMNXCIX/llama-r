//! The furniture every view shares: the one-row context bar, the key-hint
//! footer, and thin separators.
//!
//! Views are handed the `body` rect from [`layout`] instead of calling
//! `f.area()` themselves. That is the whole point of this module: when a view
//! lays out the whole screen it writes over the bar's row, which is how the
//! tab bar came to disappear without anyone noticing.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::theme;

/// Rows the context bar takes at the top.
pub const BAR_HEIGHT: u16 = 1;

/// Rows the key hints take at the bottom.
pub const FOOTER_HEIGHT: u16 = 1;

/// Blank columns between neighbouring parts of a line.
const GAP: &str = "  ";

/// Punctuation between view names. Not a border: it is the bar's own wording.
const VIEW_JOIN: &str = " │ ";

/// Marks text that did not fit.
const ELLIPSIS: char = '…';

/// Where the chrome sits inside one screen.
#[derive(Debug, Clone, Copy)]
pub struct Chrome {
    /// The context bar row.
    pub bar: Rect,
    /// Everything the current view is allowed to draw on.
    pub body: Rect,
    /// The key-hint row, pinned to the last line.
    pub footer: Rect,
}

/// The project and agent in force for this request.
#[derive(Debug, Default, Clone)]
pub struct Context {
    pub project: Option<String>,
    pub agent: Option<String>,
}

/// Splits `area` into bar, body, and footer.
///
/// Every subtraction saturates: a terminal too small for the two fixed rows
/// gets an empty body rather than a wrapped-around one.
pub fn layout(area: Rect) -> Chrome {
    let body_height = area
        .height
        .saturating_sub(BAR_HEIGHT)
        .saturating_sub(FOOTER_HEIGHT);
    Chrome {
        bar: Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: BAR_HEIGHT,
        },
        body: Rect {
            x: area.x,
            y: area.y.saturating_add(BAR_HEIGHT),
            width: area.width,
            height: body_height,
        },
        footer: Rect {
            x: area.x,
            y: area
                .y
                .saturating_add(area.height.saturating_sub(FOOTER_HEIGHT)),
            width: area.width,
            height: FOOTER_HEIGHT,
        },
    }
}

/// Draws the context bar: view names, the active one bold, then the context
/// and the provider's health.
///
/// View names are never dropped. When the line is too long it is the context
/// that shrinks, from the right, ending in `…`.
pub fn render_bar(
    f: &mut Frame,
    rect: Rect,
    views: &[&str],
    active: usize,
    ctx: &Context,
    healthy: bool,
) {
    if rect.is_empty() {
        return;
    }

    let mut line = Line::default();
    for (index, name) in views.iter().enumerate() {
        if index > 0 {
            line.push_span(Span::styled(VIEW_JOIN, theme::chrome()));
        }
        let style = if index == active {
            theme::active()
        } else {
            theme::chrome()
        };
        line.push_span(Span::styled(*name, style));
    }
    line.push_span(GAP);

    let health = if healthy {
        Span::styled("●", theme::ok())
    } else {
        Span::styled("○", theme::error())
    };
    let budget = (rect.width as usize)
        .saturating_sub(line.width())
        .saturating_sub(width_of(GAP))
        .saturating_sub(health.width());

    line.push_span(Span::styled(
        fit(&context_label(ctx), budget),
        theme::chrome(),
    ));
    line.push_span(GAP);
    line.push_span(health);

    f.render_widget(Paragraph::new(line), rect);
}

/// Draws the key hints for the current view: each key in `action`, its
/// meaning in `chrome`.
pub fn render_footer(f: &mut Frame, rect: Rect, hints: &[(&str, &str)]) {
    if rect.is_empty() {
        return;
    }

    let mut line = Line::default();
    for (index, (key, hint)) in hints.iter().enumerate() {
        if index > 0 {
            line.push_span(Span::styled(GAP, theme::chrome()));
        }
        line.push_span(Span::styled(*key, theme::action()));
        line.push_span(Span::styled(format!(": {hint}"), theme::chrome()));
    }

    f.render_widget(Paragraph::new(line), rect);
}

/// Draws a thin horizontal rule across `rect.width`.
pub fn separator(f: &mut Frame, rect: Rect) {
    if rect.is_empty() {
        return;
    }

    let rule = "─".repeat(rect.width as usize);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(rule, theme::chrome()))),
        rect,
    );
}

/// How the current scope reads on the bar: `fudi/ops`, `fudi`, `ops`, or
/// `direct` when nothing is scoped.
pub fn context_label(ctx: &Context) -> String {
    match (&ctx.project, &ctx.agent) {
        (Some(project), Some(agent)) => format!("{project}/{agent}"),
        (Some(project), None) => project.clone(),
        (None, Some(agent)) => agent.clone(),
        (None, None) => "direct".to_string(),
    }
}

/// Width of `text` in terminal columns.
///
/// `Line::width` measures display width, so an accented letter counts as one
/// column and a CJK one as two — which `chars().count()` gets wrong in both
/// directions.
fn width_of(text: &str) -> usize {
    Line::from(text).width()
}

/// Shortens `text` from the right until it fits `budget` columns, marking the
/// cut with `…`. Truncation never splits a character.
fn fit(text: &str, budget: usize) -> String {
    if width_of(text) <= budget {
        return text.to_string();
    }
    if budget == 0 {
        return String::new();
    }

    let budget = budget - 1; // the ellipsis takes the last column
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let next = index + ch.len_utf8();
        if width_of(&text[..next]) > budget {
            break;
        }
        end = next;
    }
    format!("{}{ELLIPSIS}", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // `row_text` from Test Helpers.
    fn row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect()
    }

    #[tokio::test]
    async fn context_bar_renders_into_a_single_row() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let ctx = Context {
            project: Some("fudi".into()),
            agent: Some("ops".into()),
        };
        terminal
            .draw(|f| {
                render_bar(
                    f,
                    f.area(),
                    &["Dashboard", "Projects", "Chat"],
                    2,
                    &ctx,
                    true,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row = row_text(&buffer, 0);
        assert!(
            row.contains("Chat"),
            "active view must be on the bar: {row:?}"
        );
        assert!(
            row.contains("fudi/ops"),
            "context must be on the bar: {row:?}"
        );
        // `│` is the bar's own view separator, not a border (the spec's mockup shows
        // it), so the border check looks for the characters a border would draw.
        assert!(
            !row.contains('┌') && !row.contains('┐') && !row.contains('└') && !row.contains('┘'),
            "the bar must not draw a border: {row:?}"
        );
    }

    #[test]
    fn layout_reserves_bar_and_footer_rows() {
        let c = layout(Rect::new(0, 0, 80, 24));
        assert_eq!(c.bar.height, BAR_HEIGHT);
        assert_eq!(c.footer.height, FOOTER_HEIGHT);
        assert_eq!(c.body.y, 1);
        assert_eq!(c.body.height, 22);
    }

    #[test]
    fn layout_clamps_instead_of_underflowing_on_a_tiny_terminal() {
        let c = layout(Rect::new(0, 0, 4, 1));
        assert_eq!(c.body.height, 0, "must clamp, not wrap around");
    }

    #[tokio::test]
    async fn long_context_is_truncated_and_view_names_survive() {
        let mut terminal = Terminal::new(TestBackend::new(50, 10)).unwrap();
        let ctx = Context {
            project: Some("un-proyecto-con-nombre-realmente-larguísimo".into()),
            agent: Some("un-agente-igual-de-largo-y-que-no-cabe".into()),
        };
        terminal
            .draw(|f| {
                render_bar(
                    f,
                    f.area(),
                    &["Dashboard", "Projects", "Chat"],
                    0,
                    &ctx,
                    true,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row = row_text(&buffer, 0);
        assert!(
            row.contains("Dashboard"),
            "view names must survive: {row:?}"
        );
        assert!(
            row.contains('…'),
            "the long context must be truncated: {row:?}"
        );
        assert_eq!(row.chars().count(), 50, "the bar must stay on one row");
    }

    #[tokio::test]
    async fn tiny_terminal_renders_without_panicking() {
        for (w, h) in [(1u16, 1u16), (3, 2), (10, 1), (2, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            let c = layout(Rect::new(0, 0, w, h));
            terminal
                .draw(|f| {
                    render_bar(f, c.bar, &["Dashboard"], 0, &Context::default(), false);
                    render_footer(f, c.footer, &[("q", "quit")]);
                })
                .unwrap();
        }
    }
}
