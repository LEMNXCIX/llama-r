//! The two modal surfaces, and the only places in the interface that draw a
//! border.
//!
//! A dialog is the one thing that has to read as separate from the page behind
//! it, so the delete confirmation and the skill-proposal approval keep their
//! boxes while every view gives them up. Keeping them here — rather than at the
//! bottom of a view module — is what lets `tests/tui_chrome.rs` state the rule
//! as a file list: a `Borders::` anywhere else is a mistake, and there is no
//! exemption to hide behind.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

/// Draws the delete confirmation over `body`. The bar and footer stay
/// readable above and below it.
///
/// The keys are a modal's answer set rather than a hint list: this is not a
/// `CurrentView`, so it has no row on the bar and no footer of its own to put
/// them in.
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
    .alignment(Alignment::Center);
    f.render_widget(dialog, horiz[1]);
}

/// Render the skill proposals awaiting approval.
///
/// The generated bodies are shown, not just the names: the user is being asked
/// to let the model write instructions that will shape future behaviour, and
/// approving without reading would make the approval meaningless.
///
/// The approve and discard keys are in `TuiApp::hints_for`'s `Proposals` scope:
/// they are live while this is on screen, so the shared footer has to name them,
/// which is why the bordered row below the list is empty.
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

    let footer = Paragraph::new("").block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::chrome;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    use super::super::test_helpers::{all_text, row_text};

    fn proposal(id: &str) -> crate::services::skill_generation::SkillProposal {
        crate::services::skill_generation::SkillProposal {
            id: id.to_string(),
            name: "Demo".to_string(),
            description: "d".to_string(),
            tags: Vec::new(),
            content: "body".to_string(),
        }
    }

    fn render_delete(confirm_type: &str, confirm_id: &str) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render_confirm_delete(f, Rect::new(0, 1, 80, 22), confirm_type, confirm_id))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn render_proposals(sel: &[&str], selected: usize) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let proposals: Vec<_> = sel.iter().map(|id| proposal(id)).collect();
        terminal
            .draw(|f| render_skill_proposals(f, Rect::new(0, 1, 80, 22), &proposals, selected, &[]))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// The two surfaces that keep their borders, and why: a dialog has to read as
    /// separate from the page behind it. Both are also what
    /// `tests/tui_chrome.rs` exempts from the no-boxes rule, so if a box here
    /// stopped meaning "this is a dialog", that test's exemption would be
    /// pointing at nothing.
    #[test]
    fn the_modals_draw_their_border_and_their_keys() {
        let text = all_text(&render_delete("project", "fudi"));
        assert!(text.contains('┌'), "a dialog is framed: {text}");
        assert!(
            text.contains("Delete project: \"fudi\"?"),
            "the dialog names what it deletes: {text}"
        );
        // Its answer set is the one key list a view may not own: there is no
        // `CurrentView` for a dialog, so it has no row on the bar and no footer.
        assert!(
            text.contains("[y] Yes") && text.contains("[n] No"),
            "a dialog states the keys that answer it: {text}"
        );

        let proposals = all_text(&render_proposals(&["demo"], 0));
        assert!(proposals.contains('┌'), "the proposal panel is framed");
        assert!(
            proposals.contains("nothing is written until you approve"),
            "the approval banner must survive: {proposals}"
        );
    }

    /// The selected proposal wears `>`, and it is the row the approve key acts
    /// on, so the marker has to move with `selected`.
    #[test]
    fn the_selected_proposal_is_the_only_one_marked() {
        let buffer = render_proposals(&["uno", "dos", "tres"], 1);
        let marked: Vec<String> = (0..buffer.area.height)
            .filter(|y| row_text(&buffer, *y).contains('>'))
            .map(|y| row_text(&buffer, y))
            .collect();
        assert_eq!(marked.len(), 1, "exactly one proposal is marked: {marked:?}");
        assert!(
            marked[0].contains("dos"),
            "the marked proposal is the selected one: {marked:?}"
        );
    }

    /// A modal drawn into a body a couple of columns wide must not panic.
    #[test]
    fn the_modals_survive_a_tiny_terminal() {
        for (width, height) in [(1u16, 1u16), (2, 2), (3, 3), (10, 1), (2, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let body = chrome::layout(Rect::new(0, 0, width, height)).body;
            terminal
                .draw(|f| {
                    render_confirm_delete(f, body, "project", "fudi");
                    render_skill_proposals(f, body, &[proposal("demo")], 0, &[]);
                })
                .unwrap();
        }
    }
}