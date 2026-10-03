use crate::tui::app::AnalysisState;
use crate::tui::theme;
use crate::tui::views::chat::{spinner_frame, SPINNER_INTERVAL};
use crate::tui::views::modals::render_skill_proposals;
use ratatui::{
    layout::Rect,
    style::Modifier,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

/// Draws the analysis view inside `body`, the part of the screen the shared
/// chrome left over.
///
/// Nothing frames it. The context bar already names the view, and the result is
/// a document that wants every row it can get, so the title block and the
/// footer row this view used to reserve are both gone.
///
/// [`AnalysisState::Proposals`] is the one state drawn elsewhere: approving a
/// generated skill is a modal, and [`render_skill_proposals`] keeps its border.
pub fn render_analysis(f: &mut Frame, body: Rect, analysis_state: &AnalysisState) {
    match analysis_state {
        // Idle is the state before anything has been triggered, and the key that
        // triggers it lives on the projects list. Naming it here would be
        // naming a key that does nothing on this screen; `hints_for` already
        // carries the one that does (`r`).
        AnalysisState::Idle => {
            let idle = Paragraph::new(Line::from(Span::styled(
                "No analysis running.",
                theme::chrome(),
            )));
            f.render_widget(idle, body);
        }
        AnalysisState::Loading { started_at } => {
            let elapsed = started_at.elapsed();
            // The braille glyph is the animation; a trailing "..." would sit
            // frozen next to a moving spinner and read as a glitch.
            let frame = spinner_frame(elapsed, SPINNER_INTERVAL);
            let status = Paragraph::new(Line::from(vec![
                Span::styled(frame, theme::action()),
                Span::styled(" Analyzing project ", theme::content()),
                Span::styled(format!("({}s)", elapsed.as_secs()), theme::chrome()),
            ]));
            f.render_widget(status, body);
        }
        AnalysisState::Proposals {
            proposals,
            selected,
            results,
            ..
        } => {
            render_skill_proposals(f, body, proposals, *selected, results);
        }
        AnalysisState::Loaded(content_md) => {
            let result = Paragraph::new(content_md.as_str())
                .style(theme::content())
                .wrap(Wrap { trim: true });
            f.render_widget(result, body);
        }
        AnalysisState::Error(err) => {
            let failure = Paragraph::new(Line::from(vec![
                // The one place a view adds bold on top of a `theme` token rather
                // than wearing `theme::active()`. The prefix labels a failure the
                // user has to read — the same job the delete question and the
                // approval banner have in `views/modals.rs`, which wear
                // `active` for exactly that reason.
                Span::styled("Error: ", theme::error().add_modifier(Modifier::BOLD)),
                Span::styled(err.as_str(), theme::content()),
            ]))
            .wrap(Wrap { trim: true });
            f.render_widget(failure, body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::skill_generation::SkillProposal;
    use crate::tui::views::chat::SPINNER_FRAMES;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    use super::super::test_helpers::{all_text, row_text};

    /// The analysis view on an 80x24 terminal, inside the body the chrome leaves
    /// under the bar.
    fn render(state: &AnalysisState) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render_analysis(f, Rect::new(0, 1, 80, 22), state))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn proposal(id: &str) -> SkillProposal {
        SkillProposal {
            id: id.to_string(),
            name: "Demo".to_string(),
            description: "d".to_string(),
            tags: Vec::new(),
            content: "body".to_string(),
        }
    }

    /// The regression guard for "no boxes", over every state this view draws
    /// itself. `─` is left alone: the context bar draws it and this view must
    /// not be blamed for it, but nothing here draws a rule either.
    ///
    /// The border assertion alone would also pass a view that drew nothing, so
    /// each state is checked for its own text as well.
    #[test]
    fn the_analysis_view_draws_no_box_characters() {
        let states = [
            ("idle", AnalysisState::Idle, vec!["No analysis running"]),
            (
                "loading",
                AnalysisState::Loading {
                    started_at: std::time::Instant::now(),
                },
                vec!["Analyzing project"],
            ),
            (
                "loaded",
                AnalysisState::Loaded("# Reglas\nhola".to_string()),
                vec!["# Reglas", "hola"],
            ),
            (
                "error",
                AnalysisState::Error("se rompio el análisis".to_string()),
                vec!["se rompio el análisis"],
            ),
        ];
        for (label, state, expected) in &states {
            let text = all_text(&render(state));
            for ch in ['┌', '┐', '└', '┘', '│'] {
                assert!(
                    !text.contains(ch),
                    "the {label} analysis view must not draw box char {ch:?}: {text}"
                );
            }
            for needle in expected {
                assert!(
                    text.contains(needle),
                    "the {label} analysis view must show {needle:?}: {text}"
                );
            }
        }
    }

    /// The spinner is the only feedback an analysis in flight gives, so it has
    /// to still be there and still be one of the frames the chat uses.
    #[test]
    fn the_loading_state_still_animates() {
        let state = AnalysisState::Loading {
            started_at: std::time::Instant::now(),
        };
        let row = row_text(&render(&state), 1);
        let glyph = row
            .trim_start()
            .chars()
            .next()
            .unwrap_or_else(|| panic!("the spinner must start the row: {row:?}"));
        assert!(
            SPINNER_FRAMES.contains(&glyph.to_string().as_str()),
            "unexpected glyph {glyph:?} in {row:?}"
        );
        // The shape of the counter, not its value: `started_at` is `Instant::now()`
        // and a descheduled worker can make a whole second elapse between building
        // the state and drawing it, which would fail an assertion on the number
        // for no reason a reader could act on.
        assert!(
            row.contains("s)"),
            "an analysis in flight reports how long it has been running: {row:?}"
        );
    }

    /// `Proposals` is drawn by `render_skill_proposals`, not here. Asserting the
    /// approval banner pins that delegation: restyling this state through
    /// `render_analysis` would silently drop the one line saying that nothing is
    /// written until the user says so.
    #[test]
    fn analysis_proposals_render_the_proposal_and_delegate_to_the_modal() {
        let state = AnalysisState::Proposals {
            project_path: "/tmp/p".into(),
            proposals: vec![proposal("demo")],
            selected: 0,
            results: Vec::new(),
        };
        let text = all_text(&render(&state));
        assert!(text.contains("demo"), "the proposal must be shown: {text}");
        assert!(
            text.contains("nothing is written until you approve"),
            "the approval banner must be shown: {text}"
        );
    }

    /// `body` can be a couple of columns wide on a phone-sized terminal, and
    /// every state has to survive it.
    #[test]
    fn analysis_survives_a_tiny_terminal() {
        for (width, height) in [(1u16, 1u16), (2, 2), (3, 3), (5, 4), (10, 1), (2, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|f| {
                    let body = Rect::new(0, 0, width, height);
                    for state in [
                        AnalysisState::Idle,
                        AnalysisState::Loading {
                            started_at: std::time::Instant::now(),
                        },
                        AnalysisState::Loaded("hola".to_string()),
                        AnalysisState::Error("se rompio".to_string()),
                        AnalysisState::Proposals {
                            project_path: "/tmp/p".into(),
                            proposals: vec![proposal("demo")],
                            selected: 0,
                            results: Vec::new(),
                        },
                    ] {
                        render_analysis(f, body, &state);
                    }
                })
                .unwrap();
        }
    }
}
