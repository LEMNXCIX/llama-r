use crate::api::handlers::AppState;
use crate::tui::{chrome, theme};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{List, ListItem, Paragraph},
    Frame,
};
use std::ops::Range;

/// Blank columns between a project's name and the fields that follow it.
const FIELD_GAP: usize = 2;

/// `project_type` of a project that has never been analysed.
const UNANALYZED: &str = "unanalyzed";

/// The `rows` rows ending at `index`, extended forwards when `index` is near the
/// top so the window is full.
///
/// A window, not a slice of the head: which rows a list shows is *derived* from
/// which one is selected, so the two cannot disagree however the caller moves the
/// index. That is the property the keys need — `↑/↓`, `e` and `d` all address
/// `index`, and the row `index` names is in the window by construction.
///
/// Trailing rather than leading, so `↓` walks into the list without moving it and
/// `↑` scrolls only once the selection reaches the top. That is the direction
/// `less` and `vi` scroll in, and the one where the row you just moved onto is the
/// row your eye was already following.
fn window(len: usize, index: usize, rows: usize) -> Range<usize> {
    if len == 0 || rows == 0 {
        return 0..0;
    }
    let index = index.min(len - 1);
    let start = (index + 1).saturating_sub(rows);
    start..(start + rows).min(len)
}

/// How many rows of each list the projects view actually drew.
///
/// The two lists share one body, and the agent list is the one four keys act on,
/// so it is given the rows first and the project list takes what is left. At some
/// project counts that leaves a list with none — and a key that drives a row
/// nobody can see is the defect this view exists to avoid — so the counts are
/// reported back for the handlers to measure against.
///
/// This is the same render-time feedback [`crate::tui::views::chat::render_chat`]
/// returns for its scroll bound, and for the same reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListBounds {
    /// Rows drawn for the project list.
    pub project_rows: usize,
    /// Rows drawn for the agent list.
    pub agent_rows: usize,
}

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
///
/// Returns the rows each list got, for the reason given on [`ListBounds`]: the
/// two share a body, so a list can end up with none of it and the keys that drive
/// it have to know.
pub fn render_projects(
    f: &mut Frame,
    body: Rect,
    state: &AppState,
    project_index: usize,
    agent_index: usize,
    active_in_project_list: bool,
) -> ListBounds {
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
        return ListBounds::default();
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
    // `12 agentes` with the words a column apart. `1 agente`, `3 agentes` — the
    // spec's own mockup, in `docs/superpowers/specs/2026-10-02-tui-redesign-design.md`
    // § Per-view layout.
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

    let selected = projects.get(project_index);
    let agents = agents_of_project(state, selected.map(|p| p.project_id.as_str()));

    // Four regions: the project rows, the rule, the agent list's label, then the
    // agent rows.
    //
    // The agent rows are sized first and the project rows take what is left,
    // because four keys act on the agent list and one (`a`) acts on a project.
    // But the agent list is not given *all* of them: a project list with no rows
    // would point `a`, `d` and `n` at a project nobody can see, which is the same
    // defect one list over. So each list keeps at least one row whenever the
    // other has something to show, and when the body is too short even for that
    // the counts reported back are zero and the keys consult them.
    //
    // Both lists are then drawn as a window on their own selection, so a list
    // that did not get room for all of its rows still shows the selected one and
    // every row stays reachable. That is what makes "no key drives invisible
    // state" hold at every project count rather than only while the body
    // outgrows both lists.
    let fixed = 1 + 1; // the rule, and the label
    let available = body.height.saturating_sub(fixed);
    let both_have_rows = !agents.is_empty() && !projects.is_empty();
    let agent_rows = (agents.len() as u16).min(if both_have_rows {
        available.saturating_sub(1)
    } else {
        available
    });
    let project_rows = (projects.len() as u16).min(available.saturating_sub(agent_rows));

    // Each window is read twice: once to walk the rows it names, once for its
    // length in the bounds reported back. `Range` is two `usize`s, so the copy
    // costs nothing — and it is the copy that lets the window, rather than a
    // count of the rows built from it, be what both sides read.
    let project_window = window(projects.len(), project_index, project_rows as usize);
    let visible_items: Vec<ListItem> = project_window
        .clone()
        .map(|index| {
            let project = &projects[index];
            project_row(
                &project.project_id,
                &counts[index],
                count_column,
                widest_count,
                project.project_type != UNANALYZED,
                active_in_project_list && index == project_index,
            )
        })
        .collect();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(project_rows),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(agent_rows),
        ])
        .split(body);
    f.render_widget(List::new(visible_items), chunks[0]);
    chrome::separator(f, chunks[1]);
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
    f.render_widget(label, chunks[2]);

    let agent_window = window(agents.len(), agent_index, agent_rows as usize);
    let agent_items: Vec<ListItem> = agent_window
        .clone()
        .map(|index| {
            let agent = &agents[index];
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
    f.render_widget(List::new(agent_items), chunks[3]);

    ListBounds {
        project_rows: project_window.len(),
        agent_rows: agent_window.len(),
    }
}

/// One project row: its name, how many agents it has, whether it has been
/// analysed.
///
/// Built once per project per frame. Two copies of this existed — one over every
/// project to measure the list, one over the window that is drawn — and they had
/// to stay byte-identical or the rows would change as the selection moved, so
/// the padding, the marker and the singular/plural all lived twice.
///
/// The count starts in the same column on every row: the name is padded to
/// `count_column`, and `count` arrives already right-aligned in a field as wide
/// as the widest one, so ` 2 agentes` sits beside `12 agentes` with the words in
/// one column. The analysed marker is padded to `widest_count` for the same
/// reason. Widths are display widths throughout (`Line::width`): a name with
/// accents or CJK in it takes more columns than it has characters, and counting
/// characters would push that row's count off the column every other row lines
/// up on.
///
/// `selected` is not read from any state: in a list, bold marks the row the next
/// keystroke acts on, and only while this list has focus — and bold is not
/// exclusive to that, since the bar's current view and two rows in `modals.rs`
/// wear it too.
fn project_row<'a>(
    project_id: &'a str,
    count: &'a str,
    count_column: usize,
    widest_count: usize,
    analysed: bool,
    selected: bool,
) -> ListItem<'a> {
    let name_style = if selected {
        theme::active()
    } else {
        theme::content()
    };
    let name = format!(" {project_id}");
    let pad = " ".repeat(count_column.saturating_sub(Line::from(name.as_str()).width()));
    let mut spans = vec![
        Span::styled(name, name_style),
        Span::styled(format!("{pad}{count}"), theme::chrome()),
    ];
    if analysed {
        let tail = " ".repeat(widest_count.saturating_sub(Line::from(count).width()));
        spans.push(Span::styled(format!("{tail}  "), theme::chrome()));
        spans.push(Span::styled("●", theme::ok()));
        spans.push(Span::styled(" analizado", theme::chrome()));
    }
    ListItem::new(Line::from(spans))
}

/// The agents of `project_id`, in the order the projects view lists them.
///
/// Both sides of `agent_index` must agree on that order: the view draws this
/// order and the key handlers index into it, so a second filter — or an unsorted
/// one — would have `e` and `d` act on an agent other than the highlighted row.
/// [`crate::api::handlers::AppState::agent_registry`] is a `HashMap` walk, so the
/// order is not stable on its own.
///
/// Sorted by id, which is also what the user reads the list as, and the rows come
/// from here rather than from a filter of the view's own so the two cannot
/// disagree. The per-project counts above filter the registry directly instead:
/// a count does not depend on the order, and `e`/`d` never read it.
pub fn agents_of_project(
    state: &AppState,
    project_id: Option<&str>,
) -> Vec<crate::domain::agent::Agent> {
    let Some(project_id) = project_id else {
        return Vec::new();
    };
    let mut agents: Vec<crate::domain::agent::Agent> = state
        .agent_registry
        .list_agents()
        .into_iter()
        .filter(|agent| agent.project_id.as_deref() == Some(project_id))
        .collect();
    agents.sort_by(|a, b| a.id.cmp(&b.id));
    agents
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
        bounds_with(state, project_index, agent_index, active_in_project_list).1
    }

    /// The same render, returning both the buffer and the [`ListBounds`] it
    /// reported — which is how a test reads the rows drawn rather than counting
    /// them off the screen.
    fn bounds_with(
        state: &AppState,
        project_index: usize,
        agent_index: usize,
        active_in_project_list: bool,
    ) -> (ListBounds, Buffer) {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        // `Terminal::draw` returns a `CompletedFrame`, not the closure's value, so
        // the bounds come back through a cell — the same shape `render_chat`'s
        // caller uses.
        let reported = std::cell::Cell::new(ListBounds::default());
        terminal
            .draw(|f| {
                reported.set(render_projects(
                    f,
                    Rect::new(0, 1, 80, 22),
                    state,
                    project_index,
                    agent_index,
                    active_in_project_list,
                ));
            })
            .unwrap();
        (reported.get(), terminal.backend().buffer().clone())
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

    /// The window a list shows always contains the selected row, whatever the
    /// list's length, the selection, or how many rows it was given.
    ///
    /// Pinned as a table rather than through the renderer because the renderer's
    /// own case — a list too long for the body — needs 20+ rows to reach, and this
    /// says the same thing about every input at once.
    #[test]
    fn the_window_always_contains_the_selected_row() {
        for len in 0..8usize {
            for rows in 0..8usize {
                for index in 0..12usize {
                    let w = window(len, index, rows);
                    assert!(
                        w.start <= w.end && w.end <= len,
                        "window {w:?} is not inside a list of {len} (index {index}, rows {rows})"
                    );
                    if len > 0 && rows > 0 {
                        let selected = index.min(len - 1);
                        assert!(
                            w.contains(&selected),
                            "index {selected} is outside its window {w:?} \
                             (len {len}, rows {rows})"
                        );
                    }
                }
            }
        }
    }

    /// The window shows `rows` rows wherever the list has that many, so a short
    /// list is not left showing one row because the selection sits at its head.
    #[test]
    fn the_window_is_full_wherever_the_list_has_the_rows() {
        assert_eq!(window(2, 0, 19), 0..2, "a 2-item list in a 19-row body");
        assert_eq!(window(20, 0, 3), 0..3, "at the top it fills forwards");
        assert_eq!(
            window(20, 19, 3),
            17..20,
            "at the bottom it fills backwards"
        );
        assert_eq!(
            window(20, 5, 3),
            3..6,
            "in the middle it trails the selection"
        );
        assert_eq!(window(20, 5, 1), 5..6, "one row is the selection itself");
        assert_eq!(window(20, 99, 3), 17..20, "a stale index shows the tail");
        assert_eq!(window(0, 0, 3), 0..0, "an empty list shows nothing");
        assert_eq!(window(20, 5, 0), 0..0, "no rows means no window");
    }

    /// **The guard for item 4's promise.** Twenty-five projects and twenty-four
    /// agents in twenty-two body rows: neither list can be shown whole, and both
    /// have keys aimed at them — four at the agent list, three at the project.
    ///
    /// So: each list keeps at least one row; the selected row of each is on
    /// screen and is the row wearing `active`, at the head, the middle and the
    /// tail; and the window is a window rather than a clip. The regression this
    /// pins is the layout as it was, where `Min(0)` left the agent region
    /// *empty* at this project count — a footer advertising `e: edit  d: delete`
    /// over a list never drawn.
    #[test]
    fn a_body_too_short_for_both_lists_still_shows_the_selected_row() {
        let _env = crate::core::paths::lock_env_for_tests();
        let projects: Vec<(&str, &str)> = (0..25)
            .map(|i| (leak(format!("p{i:02}")), "rust"))
            .collect();
        let (_dir, state) = state_with(&projects, &[("p00", 24)]);

        // `list_all_projects` is a HashMap walk, so the index of the project that
        // owns the twenty-four agents is looked up rather than assumed to be 0.
        let p00 = state
            .context_store
            .list_all_projects()
            .iter()
            .position(|project| project.project_id == "p00")
            .expect("the fixture must have a project named p00");

        // Which agent sits at which index is the view's own order, read back from
        // the function that defines it. Sorting is by id and `agent-10` sorts
        // before `agent-2`, so a test that assumed `agent-19` was last would be
        // reading an accident of the fixture rather than the list.
        let ordered: Vec<String> = agents_of_project(&state, Some("p00"))
            .iter()
            .map(|agent| agent.id.clone())
            .collect();
        assert_eq!(ordered.len(), 24, "the fixture has twenty-four agents");

        // The agent list is drawn at all, and it is a window rather than the whole
        // list — otherwise this test would pass without exercising anything.
        let head = render_with(&state, p00, 0, false);
        let head_text = all_text(&head);
        assert!(
            head_text.contains("agentes de p00"),
            "the agent list keeps its label: {head_text}"
        );
        assert!(
            head_text.contains(&ordered[0]),
            "the first agent is drawn: {head_text}"
        );
        assert!(
            !head_text.contains(&ordered[23]),
            "a list that showed all twenty-four would not prove the window: {head_text}"
        );

        // The last agent is reachable, and it is the row that wears `active` — so
        // `e` and `d` act on a row the user can see.
        let tail = render_with(&state, p00, 23, false);
        let tail_text = all_text(&tail);
        assert!(
            tail_text.contains(&ordered[23]),
            "the last agent must be reachable: {tail_text}"
        );
        assert!(
            wears_at(&tail, row_of(&tail, &ordered[23]), 2, theme::active()),
            "the selected agent is the active one: {:?}",
            row_text(&tail, row_of(&tail, &ordered[23]))
        );

        // The middle too, so the window is not merely its two ends.
        let middle = render_with(&state, p00, 12, false);
        assert!(
            wears_at(&middle, row_of(&middle, &ordered[12]), 2, theme::active()),
            "the middle agent must be reachable and selected: {}",
            all_text(&middle)
        );

        // The project list is a window as well, so its own keys — `a`, `d`, `n` —
        // cannot be aimed at a row that scrolled off the top.
        let all_projects = state.context_store.list_all_projects();
        let last = all_projects.len() - 1;
        let selected_id = all_projects[last].project_id.clone();
        let projects_focused = render_with(&state, last, 0, true);
        assert!(
            wears(
                &projects_focused,
                row_of(&projects_focused, &selected_id),
                theme::active()
            ),
            "{selected_id} is the selected project and must be drawn and active: {}",
            all_text(&projects_focused)
        );
        // …and neither list was emptied by the other, which is the swap that
        // "the agents get the rows first" would have caused.
        let (bounds, _) = bounds_with(&state, p00, 23, false);
        assert!(
            bounds.project_rows > 0,
            "the project list must not be emptied by the agent list: {bounds:?}"
        );
        assert!(
            bounds.agent_rows > 0,
            "nor the agent list by the projects: {bounds:?}"
        );
        assert!(
            bounds.agent_rows < 24,
            "and the agents are a window: {bounds:?}"
        );
        assert!(bounds.project_rows < 25, "as are the projects: {bounds:?}");
    }

    /// A body too short for either list — three rows, where the rule and the label
    /// take two and one row is left — reports zero for the list that got none.
    /// That zero is what the four agent keys consult to decide they may not act on
    /// a row that was never drawn.
    #[test]
    fn a_body_with_no_room_reports_no_rows_rather_than_pretending() {
        let _env = crate::core::paths::lock_env_for_tests();
        let projects: Vec<(&str, &str)> = (0..25)
            .map(|i| (leak(format!("p{i:02}")), "rust"))
            .collect();
        let (_dir, state) = state_with(&projects, &[("p00", 24)]);
        let p00 = state
            .context_store
            .list_all_projects()
            .iter()
            .position(|project| project.project_id == "p00")
            .expect("the fixture must have a project named p00");

        let mut terminal = Terminal::new(TestBackend::new(80, 3)).unwrap();
        let reported = std::cell::Cell::new(ListBounds::default());
        terminal
            .draw(|f| {
                reported.set(render_projects(
                    f,
                    Rect::new(0, 0, 80, 3),
                    &state,
                    p00,
                    0,
                    false,
                ));
            })
            .unwrap();
        let bounds = reported.get();
        assert_eq!(
            bounds.agent_rows, 0,
            "three rows cannot hold a project row, a rule, a label and an agent: {bounds:?}"
        );
        assert_eq!(
            bounds.project_rows, 1,
            "the one row left goes to the project list, and the agent keys then defer: {bounds:?}"
        );

        // Four rows is the threshold the spec's Per-view layout section names:
        // rule, label, one project row and one agent row. Pinned from both sides
        // so that sentence has a test behind it rather than a number that could
        // drift — cited by section, not by line: the document is amended by this
        // branch, and a number into it is stale the moment it grows.
        let mut four = Terminal::new(TestBackend::new(80, 4)).unwrap();
        let reported = std::cell::Cell::new(ListBounds::default());
        four.draw(|f| {
            reported.set(render_projects(
                f,
                Rect::new(0, 0, 80, 4),
                &state,
                p00,
                0,
                false,
            ));
        })
        .unwrap();
        let bounds = reported.get();
        assert_eq!(bounds.agent_rows, 1, "four rows fit one agent: {bounds:?}");
        assert_eq!(bounds.project_rows, 1, "and one project: {bounds:?}");
    }
    /// The agent list's order is a guarantee, not an accident of the registry.
    ///
    /// `agent_index` addresses this list from the key handlers and points at a row in
    /// the rendered one, so the two have to be the same list. They are one function —
    /// [`agents_of_project`] is what both call — which is stronger than sorting twice
    /// and hoping. What is left to pin is that it is *sorted*, since that is the part
    /// the registry does not provide.
    #[test]
    fn the_agent_list_is_sorted_so_the_index_means_the_same_row_every_time() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust")], &[("fudi", 12)]);
        let ids: Vec<String> = agents_of_project(&state, Some("fudi"))
            .into_iter()
            .map(|agent| agent.id)
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(
            ids, sorted,
            "`agent_index` walks this order, so it must not be a HashMap's"
        );
        // Sorted by id as a string, which is not the numeric order a reader
        // assumes: `agent-10` comes before `agent-2`. Worth saying out loud,
        // because a test that assumed otherwise would be reading the fixture.
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "and strictly, so no index names two rows: {ids:?}"
        );
        assert!(ids.iter().any(|id| id == "agent-10"));
        assert!(ids.iter().any(|id| id == "agent-2"));
    }

    /// A project with no agents, or an index naming no project, yields an empty
    /// list rather than every agent: the filter is on the project's id and
    /// nothing else.
    #[test]
    fn the_agent_list_is_empty_for_a_project_that_is_not_selected() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(
            &[("fudi", "rust"), ("clinica", "rust")],
            &[("fudi", 2), ("clinica", 1)],
        );
        assert_eq!(
            agents_of_project(&state, Some("clinica")).len(),
            1,
            "another project's agents are not this project's"
        );
        assert!(
            agents_of_project(&state, None).is_empty(),
            "no project selected means no agents"
        );
        assert!(
            agents_of_project(&state, Some("nope")).is_empty(),
            "a project that does not exist has no agents"
        );
    }

    /// A `&'static str` for a generated project name. `state_with` borrows its
    /// names and a `Vec<(String, _)>`, which is the one way to name 25 projects
    /// without 25 lines of fixture.
    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
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
    /// nothing — which is a claim about the *style*, so it is checked against the
    /// style: no agent row may wear `active`. An earlier version of this test
    /// looked for a `>` marker, which agent rows never draw, so it passed for
    /// every implementation.
    #[test]
    fn an_out_of_range_agent_index_selects_nothing() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust")], &[("fudi", 2)]);
        let buffer = render_with(&state, 0, 99, false);
        let text = all_text(&buffer);
        assert!(
            text.contains("agent-0"),
            "the agents must still be listed: {text}"
        );
        for id in ["agent-0", "agent-1"] {
            assert!(
                !wears_at(&buffer, row_of(&buffer, id), 2, theme::active()),
                "an index past the end must mark no row, and {id} is marked: {:?}",
                row_text(&buffer, row_of(&buffer, id))
            );
        }
    }

    /// Which list holds the bold row is what tells the user where `e` and `d`
    /// will act, and it has to follow `active_in_project_list` in both
    /// directions: one row active at a time, and always in the focused list.
    #[test]
    fn the_focused_list_is_the_only_one_with_an_active_row() {
        let _env = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(&[("fudi", "rust")], &[("fudi", 2)]);

        // The index under test is read off the same list the view draws, which is the
        // only order the assertion can be about: the registry itself is a HashMap
        // walk and promises nothing.
        let second = agents_of_project(&state, Some("fudi"))
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

    /// Project rows drawn the way [`render_projects`] draws them: the text of
    /// each row with the trailing blanks trimmed, and whether its name wears
    /// `active`.
    ///
    /// Drawn through a `List` rather than read off the spans, because the row a
    /// user sees is the one the widget lays out — and a `List` is what turns a
    /// `ListItem` into columns.
    fn draw_rows(rows: Vec<ListItem<'_>>) -> (Vec<String>, Vec<bool>, Buffer) {
        let height = rows.len() as u16;
        let mut terminal = Terminal::new(TestBackend::new(80, height)).unwrap();
        terminal
            .draw(|f| f.render_widget(List::new(rows), f.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text = (0..buffer.area.height)
            .map(|y| row_text(&buffer, y).trim_end().to_string())
            .collect();
        let bold = (0..buffer.area.height)
            .map(|y| wears(&buffer, y, theme::active()))
            .collect();
        (text, bold, buffer)
    }

    /// **The guard for [`project_row`] being the only place a project row is
    /// built.** It used to be written twice — once over every project to measure
    /// the list, once over the window that is drawn — so the padding, the marker
    /// and the selection all had to stay byte-identical across two copies and
    /// nothing held them there.
    ///
    /// Asserted as exact text because the weaker forms pass for a broken row:
    /// `contains` finds the marker wherever it lands, and a padding computed
    /// from `chars().count()` lines up perfectly on ASCII names and only drifts
    /// once a name is multibyte — which is the third row below, and the one that
    /// fails if the width rule goes back to counting characters.
    #[test]
    fn a_project_row_is_the_name_the_count_and_the_marker_in_those_columns() {
        // Two names, four and eight columns wide, so the short one is the row
        // whose padding has to absorb the difference. `count_column` is
        // `1 + widest_name + FIELD_GAP`; `widest_count` is the width of the
        // counts, which `render_projects` right-aligns to the widest total so
        // they are all the same width.
        let count_column = 1 + 8 + FIELD_GAP;
        let (text, bold, _) = draw_rows(vec![
            project_row("fudi", "3 agentes", count_column, 9, true, false),
            project_row("clinica", "1 agente", count_column, 9, false, true),
        ]);

        assert_eq!(
            text[0], " fudi      3 agentes  ● analizado",
            "an analysed row is name, count, marker and its label"
        );
        assert_eq!(
            text[1], " clinica   1 agente",
            "a row with no analysis stops after its count"
        );
        // The words `agentes` and `agente` are the column the eye compares down
        // the list, so they have to end in the same place.
        assert_eq!(
            text[0].find("agentes"),
            text[1].find("agente"),
            "the count's word is a column: {text:?}"
        );
        assert!(
            !bold[0] && bold[1],
            "bold follows `selected`, not the row's position: {bold:?}"
        );

        // `日本語` is three characters and six columns. Padded by characters
        // instead of by columns it would sit at column 7, and its count with it —
        // three columns right of every other row's, which is the mistake
        // `Line::width` is here to prevent. Read back as a column rather than as
        // a string: `row_text` writes one symbol per terminal column, so a wide
        // glyph comes back interleaved with the blank cell behind it and is not
        // the row's text.
        let (_, _, buffer) = draw_rows(vec![project_row(
            "日本語",
            "3 agentes",
            count_column,
            9,
            true,
            false,
        )]);
        assert_eq!(
            column_of(&buffer, 0, "3 agentes"),
            Some(count_column),
            "a multibyte name must not push its own row's count off the column"
        );
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
                .draw(|f| {
                    render_projects(f, c.body, &state, 99, 99, true);
                })
                .unwrap();
        }
    }
}
