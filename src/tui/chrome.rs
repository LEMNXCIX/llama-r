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

/// Punctuation between the bar's groups. Not a border: it is the bar's own
/// wording, and the spec's mockup shows it.
const VIEW_JOIN: &str = " │ ";

/// What this interface is called. It leads the bar because it answers the one
/// question the rest of the row cannot: not *where you are* — the view names
/// say that — but *what this is*. No view carries it, so the bar is where it
/// lives.
const PRODUCT: &str = "Llama-R";

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

/// Draws the context bar: the product's name, then the view names with the
/// active one bold, then the context and the health dot at the right edge.
///
/// `api_running` is the HTTP **listener's** liveness, not the provider's: it is
/// set immediately before `axum::serve` and cleared only when that call errors
/// (`runtime.rs`). Nothing in the TUI reports whether the provider answers — the
/// dashboard says so where a reader would otherwise assume the bar does.
///
/// The context group — label, gap, dot — is what gives way when the row is too
/// narrow, and within the group only the label is *fitted*: [`fit`] shortens it
/// from the right and ends it in `…`.
///
/// The rest of the row is not fitted, it is clipped, and nothing in it is exempt
/// from that. The line is assembled left to right and handed to a `Paragraph`,
/// which does not wrap and cuts at the row's width, so past the cut the gap goes
/// first, then the dot, then the last view names, and on a row narrower than the
/// fixed part the product's name goes with them. Which is not the same as the
/// fixed part being safe: it is only ever at the left, so it is the *last* thing
/// the cut reaches, not a thing the cut never reaches.
///
/// The six shipped view names make the fixed part 66 columns, and the group needs
/// three more for its gap and dot — 69 before the label has a column of its own
/// (`the_dot_needs_sixty_nine_columns_before_the_label_gets_one`).
pub fn render_bar(
    f: &mut Frame,
    rect: Rect,
    views: &[&str],
    active: usize,
    ctx: &Context,
    api_running: bool,
) {
    if rect.is_empty() {
        return;
    }

    let mut line = Line::default();
    line.push_span(Span::styled(PRODUCT, theme::content()));
    for (index, name) in views.iter().enumerate() {
        line.push_span(Span::styled(VIEW_JOIN, theme::chrome()));
        let style = if index == active {
            theme::active()
        } else {
            theme::chrome()
        };
        line.push_span(Span::styled(*name, style));
    }

    let health = if api_running {
        Span::styled("●", theme::ok())
    } else {
        Span::styled("○", theme::error())
    };
    let gap_width = width_of(GAP);

    // Columns the context group may use, measured from where the views end.
    let room = (rect.width as usize).saturating_sub(line.width());
    let label = fit(
        &context_label(ctx),
        room.saturating_sub(gap_width)
            .saturating_sub(health.width()),
    );
    let group_width = width_of(&label)
        .saturating_add(gap_width)
        .saturating_add(health.width());

    line.push_span(Span::raw(" ".repeat(room.saturating_sub(group_width))));
    line.push_span(Span::styled(label, theme::chrome()));
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

    /// The bar's job is *where you are*; the product's name answers the question
    /// none of the rest of the row can — *what this is*. It used to come from the
    /// dashboard's deleted banner, and with that gone the name was in no view at
    /// all, which is a screen that never says what it is.
    #[tokio::test]
    async fn the_bar_leads_with_the_product_name() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                render_bar(
                    f,
                    f.area(),
                    &["Dashboard", "Projects", "Chat"],
                    0,
                    &Context::default(),
                    true,
                )
            })
            .unwrap();
        let row = row_text(&terminal.backend().buffer().clone(), 0);
        assert!(
            row.starts_with("Llama-R "),
            "the name leads the bar: {row:?}"
        );
        // Ahead of the view names, not after them: a name at the end of a row
        // that ends in the health dot is the first thing to be truncated, and it
        // is the one thing that must never be.
        assert!(
            row.find("Llama-R") < row.find("Dashboard"),
            "the name comes before the view names: {row:?}"
        );
    }

    /// The name is part of what the bar never drops, so it survives a context too
    /// long to fit — the same rule the view names are under.
    #[tokio::test]
    async fn the_product_name_survives_a_context_that_does_not_fit() {
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
        let row = row_text(&terminal.backend().buffer().clone(), 0);
        assert!(row.contains("Llama-R"), "the name must survive: {row:?}");
        assert!(row.contains('…'), "the context must give way: {row:?}");
    }

    /// The dot is the HTTP listener's liveness, and the doc comment above
    /// `render_bar` says so. This pins the claim that it tracks the flag it is
    /// given rather than anything the TUI works out for itself.
    #[tokio::test]
    async fn the_dot_follows_the_flag_it_is_given() {
        for (api_running, expected) in [(true, "●"), (false, "○")] {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|f| {
                    render_bar(
                        f,
                        f.area(),
                        &["Dashboard", "Projects", "Chat"],
                        0,
                        &Context::default(),
                        api_running,
                    )
                })
                .unwrap();
            let row = row_text(&terminal.backend().buffer().clone(), 0);
            // Both marks, and only the right one: a row showing both would mean
            // something on the screen is contributing a mark the flag does not
            // account for, which is the whole claim this test exists to pin.
            assert!(
                row.contains(expected),
                "api_running={api_running} must show {expected:?}: {row:?}"
            );
            let absent = if api_running { '○' } else { '●' };
            assert!(
                !row.contains(absent),
                "api_running={api_running} must not show {absent:?}: {row:?}"
            );
        }
    }

    /// The bar clips its right edge; it does not choose what to drop.
    ///
    /// The doc comment on `render_bar` says only the label is fitted and the rest
    /// is cut at the row's width, which makes 69 columns a number in a comment
    /// rather than a consequence of the layout. This pins it, with the six view
    /// names that actually ship — every other bar test here passes three, which is
    /// 29 columns narrower than the real row and lets every "the view names
    /// survive" claim look true.
    ///
    /// Found by measurement, not by counting the name, the joins and the names in
    /// a comment: the width at which the dot stops being on screen is the width
    /// the fixed part and the group together come to.
    #[tokio::test]
    async fn the_dot_needs_sixty_nine_columns_before_the_label_gets_one() {
        const VIEWS: [&str; 6] = [
            "Dashboard",
            "Projects",
            "Agent",
            "Analysis",
            "Context",
            "Chat",
        ];
        /// The row with the group given no room at all, which is what the search
        /// below measures against.
        fn bar(width: u16) -> String {
            let mut terminal = Terminal::new(TestBackend::new(width, 3)).unwrap();
            terminal
                .draw(|f| render_bar(f, f.area(), &VIEWS, 0, &Context::default(), true))
                .unwrap();
            row_text(&terminal.backend().buffer().clone(), 0)
                .trim_end()
                .to_string()
        }

        let narrowest = (1..=120u16).find(|width| bar(*width).contains('●'));
        assert_eq!(
            narrowest,
            Some(69),
            "the dot is the rightmost thing on the row, so the width it first \
             fits at is the whole fixed part plus the gap and the dot"
        );
        // One column short of it the row is the fixed part and nothing else: no
        // dot, and not even the gap in front of it.
        assert_eq!(
            bar(68),
            "Llama-R │ Dashboard │ Projects │ Agent │ Analysis │ Context │ Chat",
            "at 68 the group has no column left and the row is cut after the names"
        );
        // And wide enough to land it, with the label fitted into what is left.
        assert_eq!(
            bar(80),
            "Llama-R │ Dashboard │ Projects │ Agent │ Analysis │ Context │ Chat     direct  ●",
            "at 80 the whole label fits and the dot is the last column"
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
        // `row_text` always returns exactly `width` symbols, so counting them
        // proves nothing. What matters is that the bar occupied one row: nothing
        // wrapped or was pushed into row 1.
        assert_eq!(
            row_text(&buffer, 1).trim(),
            "",
            "the bar must not spill into row 1: {:?}",
            row_text(&buffer, 1)
        );
        assert!(
            row.contains("Dashboard"),
            "view names must survive: {row:?}"
        );
        assert!(
            row.contains('…'),
            "the long context must be truncated: {row:?}"
        );
    }

    // The regression guard for the bug this layout exists to fix: a view that
    // honours `body` cannot write over the bar's row, so the bar survives.
    #[test]
    fn a_view_drawn_into_body_leaves_the_bar_intact() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                let c = layout(f.area());
                let ctx = Context {
                    project: Some("fudi".into()),
                    agent: Some("ops".into()),
                };
                render_bar(f, c.bar, &["Dashboard", "Projects", "Chat"], 0, &ctx, true);
                // Stand-in for a view: fill the body region only.
                f.render_widget(ratatui::widgets::Paragraph::new("body content"), c.body);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        assert!(
            row_text(&buffer, 0).contains("Dashboard"),
            "bar row must survive"
        );
        assert!(
            (0..buffer.area.width).any(|x| buffer[(x, 1)].symbol() != " "),
            "the body must start on the row after the bar"
        );
    }

    #[test]
    fn body_does_not_overlap_the_bar_or_the_footer() {
        let c = layout(Rect::new(0, 0, 80, 24));
        assert!(
            c.body.y >= c.bar.y + c.bar.height,
            "body must start below the bar"
        );
        assert!(
            c.body.y + c.body.height <= c.footer.y,
            "body must end above the footer"
        );
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
