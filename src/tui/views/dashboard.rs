use crate::api::handlers::AppState;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use std::sync::atomic::Ordering;

pub fn render_dashboard(f: &mut Frame, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints(
            [
                Constraint::Length(3),
                Constraint::Length(6),
                Constraint::Min(0),
                Constraint::Length(3),
            ]
            .as_ref(),
        )
        .split(f.area());

    let title = Paragraph::new(vec![Line::from(vec![
        Span::styled(
            "Llama-R ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "High-Performance AI Gateway",
            Style::default().fg(Color::White),
        ),
    ])])
    .block(Block::default().borders(Borders::ALL).title(" Title "));

    f.render_widget(title, chunks[0]);

    let api_status = if state.api_running.load(Ordering::SeqCst) {
        Span::styled("● ONLINE", Style::default().fg(Color::Green))
    } else {
        Span::styled("○ OFFLINE", Style::default().fg(Color::Red))
    };

    let grpc_status = if state.grpc_running.load(Ordering::SeqCst) {
        Span::styled("● ONLINE", Style::default().fg(Color::Green))
    } else {
        Span::styled("○ OFFLINE", Style::default().fg(Color::Red))
    };

    let main_content = Paragraph::new(vec![
        Line::from(vec![
            Span::raw("API Server: "),
            api_status,
            Span::raw(" | gRPC Server: "),
            grpc_status,
        ]),
        Line::from(""),
        Line::from(format!(
            "Tokens Saved: {} | Agents Configured: {}",
            state.metrics.get_saved_tokens(),
            state.agent_manager.list_agents().len()
        )),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" System Status "),
    );

    f.render_widget(main_content, chunks[1]);

    let logs_data = state.logs.lock().unwrap();
    let logs_lines: Vec<Line> = logs_data
        .iter()
        .map(|l| {
            let color = if l.contains("INFO") {
                Color::Cyan
            } else if l.contains("WARN") {
                Color::Yellow
            } else if l.contains("ERROR") {
                Color::Red
            } else {
                Color::Gray
            };
            Line::from(Span::styled(l, Style::default().fg(color)))
        })
        .collect();

    let logs_panel = Paragraph::new(logs_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Activity Logs "),
        )
        .style(Style::default().fg(Color::Gray));

    f.render_widget(logs_panel, chunks[2]);

    let footer = Paragraph::new("Press 'q' to exit").block(Block::default().borders(Borders::ALL));

    f.render_widget(footer, chunks[3]);
}
