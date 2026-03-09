use crate::api::handlers::AppState;
use ratatui::style::Stylize;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

pub fn render_projects(
    f: &mut Frame,
    state: &AppState,
    project_index: usize,
    agent_index: usize,
    active_in_project_list: bool,
) {
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
        .split(f.area());

    // Title
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            "Projects Management ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("(Tab to switch view)", Style::default().fg(Color::Gray)),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    // Middle Content: Project List (Left) | Details & Specialized Agents (Right)
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)].as_ref())
        .split(chunks[1]);

    // 1. Project List
    let projects = state.context_store.list_contexts();
    let project_items: Vec<ListItem> = projects
        .iter()
        .enumerate()
        .map(|(i, ctx)| {
            let style = if active_in_project_list && i == project_index {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(format!(" {} ", ctx.project_id)).style(style)
        })
        .collect();

    let project_list = List::new(project_items)
        .block(Block::default().borders(Borders::ALL).title(" Projects "))
        .highlight_style(Style::default().bg(Color::DarkGray));
    f.render_widget(project_list, main_chunks[0]);

    // 2. Right Side: Details & Agents
    if let Some(selected_project) = projects.get(project_index) {
        let right_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(8), Constraint::Min(0)].as_ref())
            .split(main_chunks[1]);

        // Details Panel
        let details = Paragraph::new(vec![
            Line::from(vec!["ID: ".bold(), Span::raw(&selected_project.project_id)]),
            Line::from(vec!["Path: ".bold(), Span::raw(&selected_project.path)]),
            Line::from(vec![
                "Analyzed: ".bold(),
                Span::styled("YES", Style::default().fg(Color::Green)),
            ]),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Project Details "),
        );
        f.render_widget(details, right_chunks[0]);

        // Agents Panel
        let project_id = &selected_project.project_id;
        let all_agents = state.agent_manager.list_agents();
        let project_agents: Vec<_> = all_agents
            .iter()
            .filter(|a| {
                // Check if it belongs to this project
                // In our new structure, we can check its path or just the context_project field
                a.config.context_project.as_ref() == Some(project_id)
            })
            .collect();

        let agent_items: Vec<ListItem> = project_agents
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let style = if !active_in_project_list && i == agent_index {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                ListItem::new(format!(" {} (Model: {})", a.id, a.config.model)).style(style)
            })
            .collect();

        let agent_list = List::new(agent_items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Specialized Agents "),
            )
            .highlight_style(Style::default().bg(Color::DarkGray));
        f.render_widget(agent_list, right_chunks[1]);
    } else {
        let empty = Paragraph::new("No projects found. Use 'llama-r analyze' to add one.")
            .block(Block::default().borders(Borders::ALL).title(" Details "));
        f.render_widget(empty, main_chunks[1]);
    }

    // Footer
    let footer = Paragraph::new(
        " [Tab] Switch List | [Arrows] Navigate | [a] Analyze | [n] New Agent | [e] Edit | [d] Delete | [q] Quit"
    )
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}

pub fn render_agent_form(
    f: &mut Frame,
    id: &str,
    name: &str,
    model: &str,
    prompt: &str,
    field_index: usize,
) {
    let area = f.area();
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
                Constraint::Length(3), // ID
                Constraint::Length(3), // Name
                Constraint::Length(3), // Model
                Constraint::Min(0),    // Prompt
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
    let prompt_style = if field_index == 3 {
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
        Paragraph::new(prompt).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" System Prompt ")
                .border_style(prompt_style),
        ),
        chunks[3],
    );

    let help = Paragraph::new(" [Tab/Arrows] Next Field | [Enter] Save | [Esc] Cancel ")
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(help, chunks[4]);
}
