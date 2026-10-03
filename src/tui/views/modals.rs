//! The two modal surfaces, and the only places in the interface that draw a
//! border.
//!
//! A dialog is the one thing that has to read as separate from the page behind
//! it, so the delete confirmation and the skill-proposal approval keep their
//! boxes while every view gives them up. Keeping them here — rather than at the
//! bottom of a view module — is what lets `tests/tui_chrome.rs` state the rule
//! as a file list: a `Borders::` anywhere else is a mistake, and there is no
//! exemption to hide behind.

use crate::tui::theme;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
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
            theme::active(),
        )),
        Line::from(""),
        // A dialog's answer set, not a view's key hints: this is not a
        // `CurrentView`, so it has no row on the bar and no footer of its own to
        // put them in. The keys are spelled out where the question is.
        Line::from(Span::styled(
            " [y] Yes   [n] No   [Esc] Cancel",
            theme::chrome(),
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
/// This used to end in a bordered row three rows deep that drew nothing — the
/// per-view footer the shared one replaced. Those rows go to the proposals
/// instead, which is what the user is reading before they decide.
pub fn render_skill_proposals(
    f: &mut Frame,
    area: Rect,
    proposals: &[crate::services::skill_generation::SkillProposal],
    selected: usize,
    results: &[String],
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    let title = Paragraph::new(Line::from(Span::styled(
        " Skill proposals — nothing is written until you approve ",
        theme::active(),
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
            Span::styled(format!("{marker} "), theme::action()),
            // The selected proposal wears `active`, the only bold token
            // in the interface. In a list that token marks the row the next keystroke
            // acts on — something the marker alone could not carry here, being one
            // character.
            Span::styled(
                proposal.id.clone(),
                if index == selected {
                    theme::active()
                } else {
                    theme::content()
                },
            ),
            Span::styled(format!(" — {}", proposal.description), theme::chrome()),
        ]));
        for line in proposal.content.lines().take(6) {
            lines.push(Line::from(format!("    {line}")));
        }
        lines.push(Line::from(""));
    }
    for result in results {
        lines.push(Line::from(Span::styled(result.clone(), theme::ok())));
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Proposals "))
            .wrap(Wrap { trim: true }),
        chunks[1],
    );
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

    /// The selected proposal is the one `Enter` acts on, so it has to be marked by
    /// more than the `>` glyph: it wears the bold-and-yellow `active` token the
    /// list rows use for "the next keystroke lands here". An unselected proposal
    /// wears `content`, so the two cannot be confused. Not "the only one wearing
    /// `active`": the banner above and the delete question wear it too, which is
    /// why the count below is scoped to the proposal rows.
    #[test]
    fn the_selected_proposal_is_the_only_one_wearing_active() {
        let buffer = render_proposals(&["uno", "dos", "tres"], 1);
        // Scoped to the proposal rows: the banner above them is bold too, and a
        // screen-wide count would be counting the banner.
        let first = row_of(&buffer, "nothing is written until you approve") + TITLE_ROWS;
        let wearing: Vec<String> = (first..buffer.area.height)
            .filter(|y| wears(&buffer, *y, theme::active()))
            .map(|y| row_text(&buffer, y))
            .collect();
        assert_eq!(
            wearing.len(),
            1,
            "exactly one proposal wears active: {wearing:?}"
        );
        assert!(
            wearing[0].contains("dos"),
            "the active proposal is the selected one: {wearing:?}"
        );
        assert!(
            wearing[0].contains('>'),
            "the marker must still agree with it: {wearing:?}"
        );

        for id in ["uno", "tres"] {
            assert!(
                wears(&buffer, row_of(&buffer, id), theme::content()),
                "{id} is not selected and must wear content: {:?}",
                row_text(&buffer, row_of(&buffer, id))
            );
        }
    }

    /// The row `needle` is on.
    /// Rows the bordered title takes: the text plus the rule above and below it.
    const TITLE_ROWS: u16 = 3;

    /// Bordered boxes this panel draws: the banner and the proposal list.
    const BOXES: usize = 2;

    /// The row `needle` is on.
    fn row_of(buffer: &Buffer, needle: &str) -> u16 {
        (0..buffer.area.height)
            .find(|y| row_text(&buffer, *y).contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} is not on screen: {}", all_text(buffer)))
    }

    /// Does row `y` wear `token` anywhere? A proposal's name starts after the
    /// `>` and its spaces, so the token is read off the row's first non-blank
    /// cell rather than a hardcoded column.
    fn wears(buffer: &Buffer, y: u16, token: ratatui::style::Style) -> bool {
        (0..buffer.area.width)
            .filter(|x| buffer[(*x, y)].symbol() != " ")
            .any(|x| {
                let cell = &buffer[(x, y)];
                Some(cell.fg) == token.fg && cell.modifier == token.add_modifier
            })
    }

    /// Every row the panel spends below the title goes to the proposals. The
    /// empty bordered footer this used to end in drew nothing and took three
    /// rows of a body that is already short — on the one screen where the user is
    /// reading a generated skill before approving it.
    #[test]
    fn the_proposals_get_every_row_but_the_title() {
        let buffer = render_proposals(&["uno", "dos", "tres"], 0);
        let rules: Vec<String> = (0..buffer.area.height)
            .filter(|y| row_text(&buffer, *y).contains('─'))
            .map(|y| row_text(&buffer, y))
            .collect();
        // Two bordered boxes, so four rules: the banner's and the panel's. The old
        // layout drew a third, empty box for its footer, which made six — three
        // rows of the body spent on nothing, on the one screen where the user is
        // reading a generated skill before approving it.
        assert_eq!(
            rules.len(),
            2 * BOXES,
            "the banner and the panel are the only two bordered boxes: {rules:?}"
        );
        // …and those rows go to the proposals. Three at six content lines each is
        // more than fits, which is the point: the empty footer used to eat into
        // the one that did.
        let held = all_text(&buffer).matches("body").count();
        assert_eq!(
            held,
            3,
            "every proposal's body must be readable in the panel: {}",
            all_text(&buffer)
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
