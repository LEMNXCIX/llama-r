use crate::api::handlers::AppState;
use crate::tui::theme;
use ratatui::{
    layout::Rect,
    widgets::{Paragraph, Wrap},
    Frame,
};

/// Shown for a project that has never been analysed.
const NO_CONTEXT: &str = "No context analyzed yet. Press 'a' to analyze.";

/// Shown when `project_index` points past the end of the project list.
const NO_PROJECT: &str = "No project selected";

/// Draws the saved context of the selected project inside `body`, the part of
/// the screen the shared chrome left over.
///
/// The context is a document, so it takes the whole body and scrolls: `scroll`
/// is the row the caller wants at the top.
pub fn render_context(
    f: &mut Frame,
    body: Rect,
    state: &AppState,
    project_index: usize,
    scroll: usize,
) {
    let projects = state.context_store.list_all_projects();
    let context_md = match projects.get(project_index) {
        Some(project) => state
            .context_store
            .get_context(&project.project_id)
            .map(|context| context.context_md)
            .unwrap_or_else(|| NO_CONTEXT.to_string()),
        None => NO_PROJECT.to_string(),
    };

    f.render_widget(
        Paragraph::new(context_md)
            .style(theme::content())
            .wrap(Wrap { trim: true })
            .scroll((scroll as u16, 0)),
        body,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::store::ProjectContext;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    use super::super::test_helpers::{all_text, row_text, state_with};

    /// The context view on an 80x24 terminal, inside the body the chrome leaves
    /// under the bar.
    fn render(state: &AppState, project_index: usize, scroll: usize) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render_context(f, Rect::new(0, 1, 80, 22), state, project_index, scroll))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// An `AppState` holding one project whose saved context is `context_md`.
    ///
    /// The shared fixture saves empty contexts, which is all the projects list
    /// needs and nothing this view can show, so the context is written after.
    fn state_with_context(context_md: &str) -> (tempfile::TempDir, std::sync::Arc<AppState>) {
        let (dir, state) = state_with(&[("fudi", "rust")], &[]);
        state
            .context_store
            .save_context(ProjectContext {
                project_id: "fudi".to_string(),
                path: dir.path().join("fudi").display().to_string(),
                context_md: context_md.to_string(),
                project_type: "rust".to_string(),
                skills_injected: Vec::new(),
                last_analyzed: chrono::Utc::now(),
                custom_rules: String::new(),
            })
            .unwrap();
        (dir, state)
    }

    /// Absence of borders alone would also pass a view that drew nothing, so
    /// the saved context itself has to be on screen.
    #[test]
    fn the_context_view_shows_the_saved_context_without_borders() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with_context("# Reglas\n- hablar en español\n- no inventar");
        let text = all_text(&render(&state, 0, 0));
        for ch in ['┌', '┐', '└', '┘', '│'] {
            assert!(
                !text.contains(ch),
                "the context view must not draw box char {ch:?}: {text}"
            );
        }
        assert!(
            text.contains("# Reglas") && text.contains("hablar en español"),
            "the saved context must be shown: {text}"
        );
    }

    /// A context longer than the body has to be reachable, not clipped at the
    /// first screenful.
    #[test]
    fn the_context_view_scrolls() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with_context("uno\ndos\ntres\ncuatro");
        let top = row_text(&render(&state, 0, 0), 1);
        assert!(top.contains("uno"), "the first row shows the top: {top:?}");
        let scrolled = row_text(&render(&state, 0, 2), 1);
        assert!(
            scrolled.contains("tres") && !scrolled.contains("uno"),
            "scrolling two rows must start at the third line: {scrolled:?}"
        );
    }

    /// A project that exists only because it has agents, with no context saved,
    /// is a state `list_all_projects` really produces — it is what an
    /// unanalysed project looks like — and the view has to say what fills it in.
    #[test]
    fn a_project_without_a_saved_context_says_so() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("clinica", "rust")], &[("solo-agentes", 1)]);
        let projects = state.context_store.list_all_projects();
        let index = projects
            .iter()
            .position(|p| p.project_id == "solo-agentes")
            .expect("the project with agents must be in the list");
        assert!(
            state.context_store.get_context("solo-agentes").is_none(),
            "the fixture must have no saved context for this to test anything"
        );
        let text = all_text(&render(&state, index, 0));
        assert!(
            text.contains("No context analyzed yet"),
            "a project with no context must say so: {text}"
        );
    }

    /// `project_index` is the app's selection, which can point at a project the
    /// store no longer has.
    #[test]
    fn the_context_view_survives_an_out_of_range_project_index() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust")], &[]);
        let text = all_text(&render(&state, 99, 0));
        assert!(text.contains("No project selected"), "{text}");
    }

    /// A context longer than the terminal and a `scroll` past its end both have
    /// to be survivable: `scroll` is a `usize` the caller clamps, not a promise.
    #[test]
    fn the_context_view_survives_a_tiny_terminal_and_a_scroll_past_the_end() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with_context("hola");
        for (width, height) in [(1u16, 1u16), (2, 2), (3, 3), (10, 1), (2, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let body = Rect::new(0, 0, width, height);
            terminal
                .draw(|f| render_context(f, body, &state, 0, 99))
                .unwrap();
        }
    }
}
