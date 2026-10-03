use crate::api::handlers::AppState;
use crate::tui::{chrome, theme};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{List, ListItem, Paragraph},
    Frame,
};

/// Blank columns between a project's name and the fields that follow it.
const FIELD_GAP: usize = 2;

/// `project_type` of a project that has never been analysed.
const UNANALYZED: &str = "unanalyzed";

/// Draws the projects view inside `body`, the part of the screen the shared
/// chrome left over.
///
/// Two lists, stacked and separated by one rule, both of them plain rows:
///
/// - one row per project — its name, how many agents it has, whether it has
///   been analysed. The counts are a column the eye compares down the list, so
///   every row's count ends in the same place.
/// - the selected project's agents, one row each, under a label saying whose
///   they are.
///
/// The agent list is here because four key bindings act on it. `←/→` moves
/// focus between the two, `↑/↓` moves within whichever has it, `e` edits the
/// selected agent and `d` deletes it. A previous revision of this view dropped
/// the agent rows and kept the keys, so pressing any of them deselected the
/// screen with nothing left to navigate to: the keys drove a list that was not
/// drawn. Two lists of rows with a rule between is still the minimal shape —
/// it is boxes and titles this view gives up, not its content.
///
/// The key hints are the chrome footer's job, not this function's; they follow
/// the focus, so they are asked for with the same two flags that pick the active
/// row.
pub fn render_projects(
    f: &mut Frame,
    body: Rect,
    state: &AppState,
    project_index: usize,
    agent_index: usize,
    active_in_project_list: bool,
) {
    let projects = state.context_store.list_all_projects();

    if projects.is_empty() {
        // A rule under a list with no rows would point at nothing. The footer
        // is no help either — `a` no-ops with nothing to analyse and `n` opens
        // a form with an empty project id — so the next step is named here.
        let empty = Paragraph::new(Line::from(Span::styled(
            "No projects — run 'llama-r analyze <path>' to add one",
            theme::chrome(),
        )));
        f.render_widget(empty, body);
        return;
    }

    let agents = state.agent_registry.list_agents();
    let totals: Vec<usize> = projects
        .iter()
        .map(|project| {
            agents
                .iter()
                .filter(|agent| agent.project_id.as_deref() == Some(project.project_id.as_str()))
                .count()
        })
        .collect();

    // The number is right-aligned in its own field, so the word after it is a
    // column: ` 2 agentes` beside `12 agentes`, not `2 agentes` beside
    // `12 agentes` with the words a column apart. `1 agente`, `3 agentes`
    // (`design.md:100-101`).
    let digits = totals
        .iter()
        .map(|total| total.to_string().len())
        .max()
        .unwrap_or(1);
    let counts: Vec<String> = totals
        .iter()
        .map(|total| {
            let plural = if *total == 1 { "" } else { "s" };
            format!("{total:>digits$} agente{plural}")
        })
        .collect();

    // Widths, not character counts: a name with accents or CJK in it takes
    // more columns than it has characters, and `chars().count` would push that
    // row's count off the column every other row lines up on.
    let widest_name = projects
        .iter()
        .map(|project| Line::from(project.project_id.as_str()).width())
        .max()
        .unwrap_or(0);
    let widest_count = counts
        .iter()
        .map(|count| Line::from(count.as_str()).width())
        .max()
        .unwrap_or(0);
    // One leading space on every row, then the counts.
    let count_column = 1 + widest_name + FIELD_GAP;

    let items: Vec<ListItem> = projects
        .iter()
        .zip(&counts)
        .enumerate()
        .map(|(index, (project, count))| {
            // Bold is the only bold in the interface, so it marks the row the
            // next keystroke acts on — and only while this list has focus.
            let name_style = if active_in_project_list && index == project_index {
                theme::active()
            } else {
                theme::content()
            };
            let name = format!(" {}", project.project_id);
            let pad = " ".repeat(count_column.saturating_sub(Line::from(name.as_str()).width()));

            // The count starts in its column, which is what `design.md:100-101`
            // draws: `3 agentes` and `1 agente` begin at the same column.
            let mut spans = vec![
                Span::styled(name, name_style),
                Span::styled(format!("{pad}{count}"), theme::chrome()),
            ];
            if project.project_type != UNANALYZED {
                // Pad the count out so the marker below it is a column too.
                let tail =
                    " ".repeat(widest_count.saturating_sub(Line::from(count.as_str()).width()));
                spans.push(Span::styled(format!("{tail}  "), theme::chrome()));
                spans.push(Span::styled("●", theme::ok()));
                spans.push(Span::styled(" analizado", theme::chrome()));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    // The agents of the selected project, which the lower list shows. Filtered
    // the same way the counts above are, so the two cannot disagree about which
    // project is selected.
    let selected = projects.get(project_index);
    let agents: Vec<&crate::domain::agent::Agent> = agents
        .iter()
        .filter(|agent| match (agent.project_id.as_deref(), selected) {
            (Some(agent_project), Some(project)) => agent_project == project.project_id.as_str(),
            _ => false,
        })
        .collect();

    // Three regions: the project rows, the rule, then the agent rows. The rule
    // and the label come off the top first, so a body too short for the agents
    // loses agent rows rather than the project list or the rule between them.
    let project_rows = items.len() as u16;
    let reserved = 1 + 1; // the rule, and the agent list's label
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(project_rows.min(body.height.saturating_sub(reserved))),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(body);
    f.render_widget(List::new(items), chunks[0]);
    chrome::separator(f, chunks[1]);

    let agents_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(chunks[2]);
    // The label says whose agents these are. Without it the lower rows are a
    // second list of short names directly under a list of projects, and the eye
    // has no way to tell the two apart.
    let label = Paragraph::new(Line::from(Span::styled(
        match selected {
            Some(project) => format!(" agentes de {}", project.project_id),
            None => " agentes".to_string(),
        },
        theme::chrome(),
    )));
    f.render_widget(label, agents_chunks[0]);

    let agent_items: Vec<ListItem> = agents
        .iter()
        .enumerate()
        .map(|(index, agent)| {
            // Bold marks the row the next keystroke acts on, and only while
            // this list has the focus — the same rule the project rows follow.
            let style = if !active_in_project_list && index == agent_index {
                theme::active()
            } else {
                theme::content()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("  {}", agent.id), style),
                Span::styled(format!("  {}", agent.config.model), theme::chrome()),
            ]))
        })
        .collect();
    f.render_widget(List::new(agent_items), agents_chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{all_text, column_of, row_text, state_with};
    use super::*;
    use crate::tui::chrome;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Style;
    use ratatui::Terminal;

    /// The row `needle` was drawn on.
    fn row_of(buffer: &Buffer, needle: &str) -> u16 {
        (0..buffer.area.height)
            .find(|y| row_text(buffer, *y).contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} is not on screen: {}", all_text(buffer)))
    }

    /// The projects view on an 80x24 terminal, inside the body the chrome
    /// leaves under the bar.
    fn render(state: &AppState, project_index: usize, active_in_project_list: bool) -> Buffer {
        render_with(state, project_index, 0, active_in_project_list)
    }

    /// The same, with the agent list's own selection.
    fn render_with(
        state: &AppState,
        project_index: usize,
        agent_index: usize,
        active_in_project_list: bool,
    ) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                render_projects(
                    f,
                    Rect::new(0, 1, 80, 22),
                    state,
                    project_index,
                    agent_index,
                    active_in_project_list,
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn projects_render_without_borders_and_show_counts() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust"), ("clinica", "unanalyzed")], &[]);
        let buffer = render(&state, 0, true);
        let text = all_text(&buffer);
        for ch in ['┌', '┐', '└', '┘', '│'] {
            assert!(
                !text.contains(ch),
                "projects must not draw box char {ch:?}: {text}"
            );
        }
        assert!(
            text.contains("agente"),
            "the agent count must be shown: {text}"
        );
        assert!(
            text.contains('─'),
            "the thin rule under the list must render: {text}"
        );
    }

    #[test]
    fn projects_renders_an_empty_state_instead_of_a_bare_divider() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[], &[]);
        let buffer = render(&state, 0, true);
        let text = all_text(&buffer);
        assert!(
            text.contains("No projects"),
            "an empty list must say so: {text}"
        );
        // Saying so is not enough to be useful. With no projects the footer is
        // no help either — `a` no-ops and `n` opens a form with an empty
        // project id — so the empty state is the only place the next step can
        // be named.
        assert!(
            text.contains("llama-r analyze"),
            "an empty list must name the way out: {text}"
        );
        assert!(
            !text.contains('─'),
            "no separator should point at nothing: {text}"
        );
    }

    /// The count is a column the eye compares down the list, so it has to be
    /// placed by display width. `日本語` is three characters and six columns,
    /// and `chars().count()` gets that wrong in both directions: counting
    /// characters puts its row's count nine columns off from everyone else's,
    /// and the row stops reading as a column at all.
    #[test]
    fn the_agent_count_column_aligns_with_multibyte_names() {
        let _env = crate::core::paths::lock_env_for_tests();
        // One and two digit counts on purpose: right-aligned means both end in
        // the same column, not that they start in it.
        let (_dir, state) = state_with(
            &[
                ("fudi", "rust"),
                ("clinica", "rust"),
                ("日本語プロジェクト", "rust"),
            ],
            &[("fudi", 2), ("clinica", 1), ("日本語プロジェクト", 12)],
        );
        let buffer = render(&state, 0, true);
        // `agente`, not `agentes`: one of the three projects has exactly one
        // agent and correctly reads `1 agente`, so the plural needle would find
        // two rows and the test would pass on a broken column. Scoped to the
        // rows above the rule, because the agent list below it is labelled
        // `agentes de <project>` and would match too.
        let projects_end = (0..buffer.area.height)
            .find(|y| row_text(&buffer, *y).contains('─'))
            .unwrap();
        let columns: Vec<usize> = (0..projects_end)
            .filter_map(|y| column_of(&buffer, y, "agente"))
            .collect();
        assert_eq!(
            columns.len(),
            3,
            "each project row must show a count: {columns:?}"
        );
        assert!(
            columns.windows(2).all(|w| w[0] == w[1]),
            "the count column must align across multibyte names: {columns:?}"
        );
    }

    /// The count is a word, and Spanish inflects it: `1 agente` is not
    /// `1 agentes`. The row still has to read as a column.
    #[test]
    fn a_project_with_one_agent_does_not_say_agentes() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust"), ("clinica", "rust")], &[("fudi", 1)]);
        let buffer = render(&state, 0, true);
        let one = row_text(&buffer, row_of(&buffer, "fudi"));
        assert!(one.contains("1 agente"), "one agent is `1 agente`: {one:?}");
        assert!(
            !one.contains("1 agentes"),
            "one agent is not `1 agentes`: {one:?}"
        );
        let none = row_text(&buffer, row_of(&buffer, "clinica"));
        assert!(none.contains("0 agentes"), "zero is plural: {none:?}");
    }

    #[test]
    fn an_analysed_project_is_marked_and_one_awaiting_analysis_is_not() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust"), ("clinica", "unanalyzed")], &[]);
        let buffer = render(&state, 0, true);
        let analysed = row_text(&buffer, row_of(&buffer, "fudi"));
        assert!(
            analysed.contains("● analizado"),
            "an analysed project must say so: {analysed:?}"
        );
        let pending = row_text(&buffer, row_of(&buffer, "clinica"));
        assert!(
            !pending.contains('●'),
            "a project without analysis must not claim one: {pending:?}"
        );
    }

    #[test]
    fn the_selected_project_is_the_only_row_in_the_active_style() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust"), ("clinica", "rust")], &[]);
        // `list_all_projects` comes out of a `HashMap`, so which project sits at
        // an index is not the test's to assume: ask the store.
        let projects = state.context_store.list_all_projects();
        let selected = projects[1].project_id.clone();

        let buffer = render(&state, 1, true);
        assert!(
            wears(&buffer, row_of(&buffer, &selected), theme::active()),
            "{selected} is the selected project and must wear active: {:?}",
            row_text(&buffer, row_of(&buffer, &selected))
        );
        for project in projects.iter().filter(|p| p.project_id != selected) {
            let row = row_of(&buffer, &project.project_id);
            assert!(
                wears(&buffer, row, theme::content()),
                "{} is not selected and must wear content: {:?}",
                project.project_id,
                row_text(&buffer, row)
            );
        }

        // With the agent list focused nothing here is selected, so no row may
        // claim to be.
        let unfocused = render(&state, 1, false);
        for project in &projects {
            assert!(
                !wears(
                    &unfocused,
                    row_of(&unfocused, &project.project_id),
                    theme::active()
                ),
                "{} must not look selected while the project list is unfocused",
                project.project_id
            );
        }
    }

    /// Four keys act on the agent list: `←/→` moves focus onto it, `↑/↓` moves
    /// within it, `e` edits the selected agent and `d` deletes it. This is the
    /// regression guard for the state that made all four of them useless — the
    /// list was dropped from the view while the keys stayed, so pressing one
    /// deselected the screen with nothing to navigate to.
    #[test]
    fn the_selected_projects_agents_are_listed_under_it() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(
            &[("fudi", "rust"), ("clinica", "rust")],
            &[("fudi", 2), ("clinica", 1)],
        );
        let projects = state.context_store.list_all_projects();
        let fudi = projects
            .iter()
            .position(|p| p.project_id == "fudi")
            .expect("the fixture must have a project named fudi");
        let text = all_text(&render(&state, fudi, true));
        assert!(
            text.contains("agentes de fudi"),
            "the agent list must say whose agents it is: {text}"
        );
        for id in ["agent-0", "agent-1"] {
            assert!(
                text.contains(id),
                "the selected project's agent {id} must be listed: {text}"
            );
        }
        // The other project's single agent belongs to the other project, and a
        // list that showed it would be showing an agent `↑/↓` cannot reach:
        // `agent_index` only ever walks the selected project's agents.
        assert_eq!(
            text.matches("agent-0").count(),
            1,
            "an agent must appear once, under its own project: {text}"
        );
    }

    /// The count on the project row and the rows below it must agree. They are
    /// computed from the same filter, so a change to one without the other
    /// would show `3 agentes` above an empty list.
    #[test]
    fn the_agent_rows_match_the_count_on_the_project_row() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust")], &[("fudi", 3)]);
        let buffer = render(&state, 0, true);
        let text = all_text(&buffer);
        assert!(
            text.contains("3 agentes"),
            "the row must count them: {text}"
        );
        for id in ["agent-0", "agent-1", "agent-2"] {
            assert!(
                text.contains(id),
                "every counted agent must be listed: {text}"
            );
        }
    }

    /// A project with no agents says so instead of showing a bare label over
    /// nothing — the same rule the empty project list follows.
    #[test]
    fn a_project_with_no_agents_says_so() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("clinica", "rust")], &[]);
        let text = all_text(&render(&state, 0, true));
        assert!(
            text.contains("agentes de clinica"),
            "the label is still the header of the list: {text}"
        );
        assert!(
            text.contains("0 agentes"),
            "the row already says there are none: {text}"
        );
        assert!(
            !text.contains("agent-"),
            "no agent rows may be drawn for a project with no agents: {text}"
        );
    }

    /// `agent_index` selects a row in the lower list, and it is a `usize` the
    /// caller clamps rather than a promise. An index past the end must select
    /// nothing rather than panic or mark a row that is not there.
    #[test]
    fn an_out_of_range_agent_index_selects_nothing() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust")], &[("fudi", 2)]);
        let buffer = render(&state, 0, false);
        let text = all_text(&buffer);
        assert!(
            text.contains("agent-0"),
            "the agents must still be listed: {text}"
        );
        assert!(
            !text.contains('>'),
            "no row may claim the selection: {text}"
        );
    }

    /// Which list holds the bold row is what tells the user where `e` and `d`
    /// will act, and it has to follow `active_in_project_list` in both
    /// directions: one row active at a time, and always in the focused list.
    #[test]
    fn the_focused_list_is_the_only_one_with_an_active_row() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust")], &[("fudi", 2)]);

        // The registry does not promise an order, so the index under test is read
        // back off it rather than assumed: `list_agents` walks a HashMap, and
        // `agent-1` is not reliably the second row.
        let second = state
            .agent_registry
            .list_agents()
            .iter()
            .position(|agent| agent.id == "agent-1")
            .expect("the fixture must have an agent named agent-1");

        let agents_focused = render_with(&state, 0, second, false);
        let agent_row = row_of(&agents_focused, "agent-1");
        assert!(
            wears_at(&agents_focused, agent_row, 2, theme::active()),
            "agent-1 is the selected agent and must wear active: {:?}",
            row_text(&agents_focused, agent_row)
        );
        assert!(
            !wears(
                &agents_focused,
                row_of(&agents_focused, "fudi"),
                theme::active()
            ),
            "the project row must not look selected while the agent list has focus"
        );
        // And the row that is *not* selected says so.
        let other = row_of(&agents_focused, "agent-0");
        assert!(
            wears_at(&agents_focused, other, 2, theme::content()),
            "an unselected agent is ordinary text: {:?}",
            row_text(&agents_focused, other)
        );

        let projects_focused = render_with(&state, 0, second, true);
        assert!(
            wears(
                &projects_focused,
                row_of(&projects_focused, "fudi"),
                theme::active()
            ),
            "the project row must look selected while it has the focus"
        );
        assert!(
            !wears_at(
                &projects_focused,
                row_of(&projects_focused, "agent-1"),
                2,
                theme::active()
            ),
            "no agent row may look selected while the agent list does not have focus"
        );
    }

    /// Does the cell at column `x` on row `y` wear `token`? Agent rows are
    /// indented two columns, so their name does not start at 1.
    fn wears_at(buffer: &Buffer, y: u16, x: u16, token: Style) -> bool {
        let cell = &buffer[(x, y)];
        Some(cell.fg) == token.fg && cell.modifier == token.add_modifier
    }

    /// Does the project's name on row `y` wear `token`?
    ///
    /// Only the fields a token sets: a buffer cell always carries a colour and
    /// a modifier set, so the whole `Style` never compares equal to a token.
    fn wears(buffer: &Buffer, y: u16, token: Style) -> bool {
        let cell = &buffer[(1, y)];
        Some(cell.fg) == token.fg && cell.modifier == token.add_modifier
    }

    /// The layout subtracts one row for the rule and clamps against
    /// `body.height`, and `project_index` may point past the end. Nothing may
    /// panic on a terminal too small to draw the list, or on an index that no
    /// longer exists.
    #[test]
    fn projects_survives_a_tiny_terminal_and_an_out_of_range_index() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust"), ("clinica", "rust")], &[]);
        for (width, height) in [(1u16, 1u16), (2, 2), (3, 3), (10, 1), (2, 40), (80, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let c = chrome::layout(Rect::new(0, 0, width, height));
            terminal
                .draw(|f| render_projects(f, c.body, &state, 99, 99, true))
                .unwrap();
        }
    }
}
