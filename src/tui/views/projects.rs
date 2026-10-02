use crate::api::handlers::AppState;
use crate::tui::app::AnalysisState;
use crate::tui::{chrome, theme};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};

/// Blank columns between a project's name and the fields that follow it.
const FIELD_GAP: usize = 2;

/// `project_type` of a project that has never been analysed.
const UNANALYZED: &str = "unanalyzed";

/// Draws the projects view inside `body`, the part of the screen the shared
/// chrome left over.
///
/// One row per project: the name, how many agents it has, and whether it has
/// been analysed. The counts are a column the eye compares down the list, so
/// every row's count ends in the same place.
///
/// The key hints are the chrome footer's job, not this function's.
///
/// `agent_index` stays in the signature so the call site does not change. This
/// view no longer draws the agent list — the count on each row is what it is
/// reduced to, and the context bar already carries the agent in force.
pub fn render_projects(
    f: &mut Frame,
    body: Rect,
    state: &AppState,
    project_index: usize,
    _agent_index: usize,
    active_in_project_list: bool,
) {
    let projects = state.context_store.list_all_projects();

    if projects.is_empty() {
        // A rule under a list with no rows would point at nothing.
        let empty = Paragraph::new(Line::from(Span::styled("No projects", theme::chrome())));
        f.render_widget(empty, body);
        return;
    }

    let agents = state.agent_registry.list_agents();
    let counts: Vec<String> = projects
        .iter()
        .map(|project| {
            let count = agents
                .iter()
                .filter(|agent| agent.project_id.as_deref() == Some(project.project_id.as_str()))
                .count();
            format!("{count} agentes")
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
    // One leading space on every row, the counts after the widest name, and
    // the analysed marker after the widest count.
    let count_column = 1 + widest_name + FIELD_GAP + widest_count;

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
            let pad = " ".repeat(count_column.saturating_sub(
                Line::from(name.as_str()).width() + Line::from(count.as_str()).width(),
            ));

            let mut spans = vec![
                Span::styled(name, name_style),
                Span::styled(format!("{pad}{count}"), theme::chrome()),
            ];
            if project.project_type != UNANALYZED {
                spans.push(Span::styled("  ", theme::chrome()));
                spans.push(Span::styled("●", theme::ok()));
                spans.push(Span::styled(" analizado", theme::chrome()));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    // The rule gets the last row, so a list that fills the body loses its last
    // project rather than the separator under it.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length((items.len() as u16).min(body.height.saturating_sub(1))),
            Constraint::Length(1),
        ])
        .split(body);
    f.render_widget(List::new(items), chunks[0]);
    chrome::separator(f, chunks[1]);
}

pub fn render_agent_form(
    f: &mut Frame,
    body: Rect,
    id: &str,
    name: &str,
    model: &str,
    project_id: &str,
    rules: &str,
    optimize_rules: &str,
    skills: &str,
    prompt: &str,
    field_index: usize,
) {
    let area = body;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Create/Edit Agent ")
        .style(Style::default().bg(Color::Black));

    // Center the form
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(2)
        .constraints(
            [
                Constraint::Length(3), // 0: ID
                Constraint::Length(3), // 1: Name
                Constraint::Length(3), // 2: Model
                Constraint::Length(3), // 3: Project
                Constraint::Length(3), // 4: Rules
                Constraint::Length(3), // 5: Optimization Rules
                Constraint::Length(3), // 6: Skills
                Constraint::Min(0),    // 7: Prompt
                Constraint::Length(3), // Help
            ]
            .as_ref(),
        )
        .split(block.inner(area));

    f.render_widget(block, area);

    let id_style = if field_index == 0 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let name_style = if field_index == 1 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let model_style = if field_index == 2 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let project_style = if field_index == 3 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let rules_style = if field_index == 4 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let opt_rules_style = if field_index == 5 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let skills_style = if field_index == 6 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let prompt_style = if field_index == 7 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };

    f.render_widget(
        Paragraph::new(id).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" ID ")
                .border_style(id_style),
        ),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(name).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Name ")
                .border_style(name_style),
        ),
        chunks[1],
    );
    f.render_widget(
        Paragraph::new(model).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Model ")
                .border_style(model_style),
        ),
        chunks[2],
    );
    f.render_widget(
        Paragraph::new(project_id).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Project Context (Arrows to cycle) ")
                .border_style(project_style),
        ),
        chunks[3],
    );
    f.render_widget(
        Paragraph::new(rules).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Rules (comma separated) ")
                .border_style(rules_style),
        ),
        chunks[4],
    );
    f.render_widget(
        Paragraph::new(optimize_rules).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Optimization Rules (comma separated) ")
                .border_style(opt_rules_style),
        ),
        chunks[5],
    );
    f.render_widget(
        Paragraph::new(skills).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Skills/Tools (comma separated) ")
                .border_style(skills_style),
        ),
        chunks[6],
    );
    f.render_widget(
        Paragraph::new(prompt)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" System Prompt (Enter for newline) ")
                    .border_style(prompt_style),
            )
            .wrap(Wrap { trim: true }),
        chunks[7],
    );

    // Hints come from `TuiApp::hints_for`; this row stays until the old chrome
    // is deleted.
    let help = Paragraph::new("").block(Block::default().borders(Borders::ALL));
    f.render_widget(help, chunks[8]);

    // Set cursor position based on active field
    match field_index {
        0 => f.set_cursor_position((chunks[0].x + 1 + id.chars().count() as u16, chunks[0].y + 1)),
        1 => f.set_cursor_position((
            chunks[1].x + 1 + name.chars().count() as u16,
            chunks[1].y + 1,
        )),
        2 => f.set_cursor_position((
            chunks[2].x + 1 + model.chars().count() as u16,
            chunks[2].y + 1,
        )),
        3 => {} // Project cycling, no text cursor needed
        4 => f.set_cursor_position((
            chunks[4].x + 1 + rules.chars().count() as u16,
            chunks[4].y + 1,
        )),
        5 => f.set_cursor_position((
            chunks[5].x + 1 + optimize_rules.chars().count() as u16,
            chunks[5].y + 1,
        )),
        6 => f.set_cursor_position((
            chunks[6].x + 1 + skills.chars().count() as u16,
            chunks[6].y + 1,
        )),
        7 => {
            let width = chunks[7].width.saturating_sub(2) as usize;
            if width > 0 {
                let lines: Vec<&str> = prompt.split('\n').collect();
                let mut y_offset = 0;
                let mut x_pos = 0;

                for (i, line) in lines.iter().enumerate() {
                    let line_len = line.chars().count();
                    let wrap_count = line_len / width;

                    if i < lines.len() - 1 {
                        y_offset += wrap_count + 1;
                    } else {
                        y_offset += wrap_count;
                        x_pos = line_len % width;
                    }
                }

                let final_y = (chunks[7].y + 1 + y_offset as u16)
                    .min(chunks[7].y + chunks[7].height.saturating_sub(2));
                f.set_cursor_position((chunks[7].x + 1 + x_pos as u16, final_y));
            }
        }
        _ => {}
    }
}

pub fn render_analysis(f: &mut Frame, body: Rect, analysis_state: &AnalysisState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints(
            [
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(3),
            ]
            .as_ref(),
        )
        .split(body);

    // Title
    let title = Paragraph::new(Line::from(Span::styled(
        "Project Analysis ",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    // Content
    match analysis_state {
        AnalysisState::Idle => {
            let content = Paragraph::new("Press 'a' on a project to start analysis")
                .block(Block::default().borders(Borders::ALL).title(" Analysis "))
                .wrap(Wrap { trim: true });
            f.render_widget(content, chunks[1]);
        }
        AnalysisState::Loading { started_at } => {
            let elapsed_secs = started_at.elapsed().as_secs();
            // The braille glyph is the animation; a trailing "..." would sit
            // frozen next to a moving spinner and read as a glitch.
            let frame = crate::tui::views::chat::spinner_frame(
                started_at.elapsed(),
                crate::tui::views::chat::SPINNER_INTERVAL,
            );
            let content = Paragraph::new(Line::from(vec![
                Span::styled(frame, Style::default().fg(Color::Cyan)),
                Span::raw(" Analyzing project "),
                Span::styled(
                    format!("({elapsed_secs}s)"),
                    Style::default().fg(Color::Gray),
                ),
            ]))
            .block(Block::default().borders(Borders::ALL).title(" Analysis "))
            .wrap(Wrap { trim: true });
            f.render_widget(content, chunks[1]);
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
            let content = Paragraph::new(content_md.as_str())
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Analysis Result "),
                )
                .wrap(Wrap { trim: true })
                .scroll((0, 0));
            f.render_widget(content, chunks[1]);
        }
        AnalysisState::Error(err) => {
            let content = Paragraph::new(Line::from(vec![
                Span::styled(
                    "Error: ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(err),
            ]))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Analysis Error "),
            )
            .wrap(Wrap { trim: true });
            f.render_widget(content, chunks[1]);
        }
    }

    // Footer. The hints come from `TuiApp::hints_for` now; this bordered row
    // stays until the old chrome is deleted.
    let footer = Paragraph::new("").block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}

pub fn render_context(
    f: &mut Frame,
    body: Rect,
    state: &AppState,
    project_index: usize,
    scroll: usize,
) {
    let projects = state.context_store.list_all_projects();
    let project = projects.get(project_index);

    let (project_id, context_md) = match project {
        Some(p) => {
            let ctx = state.context_store.get_context(&p.project_id);
            let md = ctx
                .map(|c| c.context_md)
                .unwrap_or_else(|| "No context analyzed yet. Press 'a' to analyze.".to_string());
            (p.project_id.clone(), md)
        }
        None => ("Unknown".to_string(), "No project selected".to_string()),
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints(
            [
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(3),
            ]
            .as_ref(),
        )
        .split(body);

    // Title
    let title = Paragraph::new(Line::from(Span::styled(
        format!(" Context: {project_id} "),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    // Content
    let content = Paragraph::new(context_md)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Project Context "),
        )
        .wrap(Wrap { trim: true })
        .scroll((scroll as u16, 0));
    f.render_widget(content, chunks[1]);

    // Footer. The hints come from `TuiApp::hints_for` now; this bordered row
    // stays until the old chrome is deleted.
    let footer = Paragraph::new("").block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}

/// Draws the delete confirmation over `body`. The bar and footer stay
/// readable above and below it.
pub fn render_confirm_delete(f: &mut Frame, body: Rect, confirm_type: &str, confirm_id: &str) {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Percentage(40),
                Constraint::Length(5),
                Constraint::Percentage(40),
            ]
            .as_ref(),
        )
        .split(body);
    let horiz = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(
            [
                Constraint::Percentage(25),
                Constraint::Length(50),
                Constraint::Percentage(25),
            ]
            .as_ref(),
        )
        .split(vert[1]);

    let dialog = Paragraph::new(vec![
        Line::from(Span::styled(
            format!(" Delete {confirm_type}: \"{confirm_id}\"?"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            " [y] Yes   [n] No   [Esc] Cancel",
            Style::default().fg(Color::Gray),
        )),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Confirm Delete "),
    )
    .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(dialog, horiz[1]);
}

/// Render the skill proposals awaiting approval.
///
/// The generated bodies are shown, not just the names: the user is being asked
/// to let the model write instructions that will shape future behaviour, and
/// approving without reading would make the approval meaningless.
pub fn render_skill_proposals(
    f: &mut Frame,
    area: Rect,
    proposals: &[crate::services::skill_generation::SkillProposal],
    selected: usize,
    results: &[String],
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    let title = Paragraph::new(Line::from(Span::styled(
        " Skill proposals — nothing is written until you approve ",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    let mut lines: Vec<Line> = Vec::new();
    if proposals.is_empty() {
        lines.push(Line::from("No skill proposals."));
    }
    for (index, proposal) in proposals.iter().enumerate() {
        let marker = if index == selected { ">" } else { " " };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), Style::default().fg(Color::Cyan)),
            Span::styled(
                proposal.id.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" — {}", proposal.description),
                Style::default().fg(Color::Gray),
            ),
        ]));
        for line in proposal.content.lines().take(6) {
            lines.push(Line::from(format!("    {line}")));
        }
        lines.push(Line::from(""));
    }
    for result in results {
        lines.push(Line::from(Span::styled(
            result.clone(),
            Style::default().fg(Color::Green),
        )));
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Proposals "))
            .wrap(Wrap { trim: true }),
        chunks[1],
    );

    // The approve/discard keys are in `TuiApp::hints_for` for the analysis
    // view; this bordered row stays until the old chrome is deleted.
    let footer = Paragraph::new("").block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::mcp::StaticMcpRegistry;
    use crate::context::store::{ContextStore, ProjectContext};
    use crate::providers::ollama::OllamaProvider;
    use crate::runtime::build_app_state;
    use crate::services::agent_registry::AgentRegistry;
    use crate::services::skill_manager::SkillManager;
    use crate::tui::chrome::{self, Context};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect()
    }

    fn all_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| row_text(buffer, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The row `needle` was drawn on.
    fn row_of(buffer: &Buffer, needle: &str) -> u16 {
        (0..buffer.area.height)
            .find(|y| row_text(buffer, *y).contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} is not on screen: {}", all_text(buffer)))
    }

    /// The terminal column each character of row `y` was drawn in.
    ///
    /// Comparing character offsets instead of columns is wrong exactly where
    /// this view is easiest to get wrong: `日本語` is three characters and six
    /// columns, so a row carrying one puts every later character several
    /// columns left of where the screen actually puts it. What makes the two
    /// differ is that a wide glyph and the blank cell beside it are one
    /// character between two columns.
    fn char_columns(buffer: &Buffer, y: u16) -> Vec<usize> {
        let mut columns = Vec::new();
        let mut written = 0;
        for x in 0..buffer.area.width {
            columns.push(written);
            written += buffer[(x, y)].symbol().chars().count();
        }
        columns
    }

    /// The terminal column where `needle` starts on row `y`.
    ///
    /// `str::find` answers in bytes and a CJK glyph is three of them, so the
    /// offset is converted to a character index before being read as a column.
    /// Left in bytes it would move the CJK row's count *with* its name, which
    /// is the opposite of what this asserts.
    fn column_of(buffer: &Buffer, y: u16, needle: &str) -> Option<usize> {
        let row = row_text(buffer, y);
        let characters = row[..row.find(needle)?].chars().count();
        char_columns(buffer, y).get(characters).copied()
    }

    /// The projects view on an 80x24 terminal, inside the body the chrome
    /// leaves under the bar.
    fn render(state: &AppState, project_index: usize, active_in_project_list: bool) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                render_projects(
                    f,
                    Rect::new(0, 1, 80, 22),
                    state,
                    project_index,
                    0,
                    active_in_project_list,
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// An `AppState` reading a throwaway data dir holding `projects` (id,
    /// project type) and, per project, how many agents it has.
    ///
    /// Agent ids do not matter to this view, only how many there are.
    /// `LLAMA_R_DIR` is process-wide, so every caller holds the env lock.
    fn state_with(projects: &[(&str, &str)], agents: &[(&str, usize)]) -> (TempDir, Arc<AppState>) {
        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_var("LLAMA_R_DIR", temp_dir.path());

        let context_store = Arc::new(ContextStore::new());
        for (project_id, project_type) in projects {
            context_store
                .save_context(ProjectContext {
                    project_id: (*project_id).to_string(),
                    path: temp_dir.path().join(project_id).display().to_string(),
                    context_md: String::new(),
                    project_type: (*project_type).to_string(),
                    skills_injected: Vec::new(),
                    last_analyzed: chrono::Utc::now(),
                    custom_rules: String::new(),
                })
                .unwrap();
        }

        let agent_registry = Arc::new(AgentRegistry::new());
        for (project_id, count) in agents {
            let dir = crate::core::paths::get_project_agents_dir(project_id);
            std::fs::create_dir_all(&dir).unwrap();
            for index in 0..*count {
                std::fs::write(
                    dir.join(format!("agent-{index}.toml")),
                    "name = \"agente\"\nmodel = \"llama3\"\nsystem_prompt = \"hola\"\n",
                )
                .unwrap();
            }
        }
        agent_registry.reload_all(&[]).unwrap();

        let state = build_app_state(
            Arc::new(OllamaProvider::new("http://localhost:11434".to_string())),
            agent_registry,
            Arc::new(SkillManager::new()),
            context_store,
            "llama3".to_string(),
            Arc::new(Mutex::new(VecDeque::new())),
            Vec::new(),
            Arc::new(StaticMcpRegistry::new()),
            None,
            None,
            None,
            None,
            None,
        );
        (temp_dir, state)
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
        let columns: Vec<usize> = (0..buffer.area.height)
            .filter_map(|y| column_of(&buffer, y, "agentes"))
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

    /// The bug this whole refactor exists to fix: a view laid out over
    /// `f.area()` covers the bar's row, and the bar disappears.
    #[test]
    fn the_agent_form_leaves_the_bar_row_alone() {
        // Derived from the chrome rather than restated, so a change to `layout`
        // is caught here too.
        let area = Rect::new(0, 0, 80, 24);
        let c = chrome::layout(area);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|f| {
                chrome::render_bar(
                    f,
                    c.bar,
                    &["Dashboard", "Projects", "Agent"],
                    2,
                    &Context::default(),
                    true,
                );
                render_agent_form(f, c.body, "ops", "ops", "llama3", "fudi", "", "", "", "", 0);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let bar_row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, c.bar.y)].symbol().to_string())
            .collect();
        // Only the bar writes these names; the form's own border title is
        // " Create/Edit Agent ", so matching on "Agent" would pass by accident.
        assert!(
            bar_row.contains("Dashboard") && bar_row.contains("Projects"),
            "the bar's row must survive the view: {bar_row:?}"
        );
        let first_body_row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, c.body.y)].symbol().to_string())
            .collect();
        assert!(
            first_body_row.contains('┌'),
            "the form must start on the body's first row: {first_body_row:?}"
        );
    }
}
