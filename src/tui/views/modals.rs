//! The modal surfaces, and the only places in the interface that draw a border.
//!
//! A dialog is the one thing that has to read as separate from the page behind
//! it, so the delete confirmation, the skill-proposal approval and the skill
//! picker keep their boxes while every view gives them up. Keeping them here —
//! rather than at the bottom of a view module — is what lets `tests/tui_chrome.rs`
//! state the rule as a file list: a `Borders::` anywhere else is a mistake, and
//! there is no exemption to hide behind. The picker's state lives in
//! [`crate::tui::views::skill_picker`]; only its panel is here, because the
//! border is what put it in this file and the border is the whole reason.

use crate::tui::theme;
use crate::tui::views::skill_picker::SkillPicker;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

/// Draws the delete confirmation over `body`. The bar and footer stay
/// readable above and below it.
///
/// The keys that answer it are the shared footer's job (`hints_for`, reached
/// through `KeyTarget::DeleteConfirm`): a dialog is not a `CurrentView`, so
/// before that target existed the answer set was spelled inside the box — which
/// is why this module is, and was, exempt from the hint scan as well as the
/// border one.
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
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Confirm Delete "),
    )
    .alignment(Alignment::Center);
    f.render_widget(dialog, horiz[1]);
}

/// Render the skill proposals awaiting approval, into `body`.
///
/// The generated bodies are shown, not just the names: the user is being asked
/// to let the model write instructions that will shape future behaviour, and
/// approving without reading would make the approval meaningless.
///
/// This used to end in a bordered row three rows deep that drew nothing — the
/// per-view footer the shared one replaced. Those rows go to the proposals
/// instead, which is what the user is reading before they decide.
///
/// `body`, like every other view's second parameter: the chrome took the bar's
/// and the footer's rows for itself, and a view that lays out the whole screen
/// writes over the bar.
pub fn render_skill_proposals(
    f: &mut Frame,
    body: Rect,
    proposals: &[crate::services::skill_generation::SkillProposal],
    selected: usize,
    results: &[String],
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(body);

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

/// The check column. `✓` for a row the agent gets, `·` for one it does not, and
/// `—` for one the analysis owns — so the three states are distinguishable in a
/// single glyph rather than by reading the row.
const PICKED: &str = "✓";
const UNPICKED: &str = "·";
const MANAGED: &str = "—";

/// Draw the skill picker over `body`, with the bar and footer left readable.
///
/// A dialog, and so one of the three places in the interface that keep a border:
/// the user is choosing from a list, and a box is what separates that list from
/// the form still sitting behind it. The keys are the shared footer's job, through
/// `KeyTarget::SkillPicker`.
pub fn render_skill_picker(f: &mut Frame, body: Rect, picker: &SkillPicker) {
    let area = centered(body, 60, body.height.saturating_sub(2).max(5));

    // The block wraps the title as well as the rows, so the inner rect is computed
    // from the border rather than from the panel. Splitting `area` instead would
    // leave the title on the border's own row, where it is overwritten — and the
    // window height one row too large, which puts the cursor's row off the bottom
    // of a full list.
    let block = Block::default().borders(Borders::ALL);
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)].as_ref())
        .split(block.inner(area));

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Skills — space to toggle, enter to confirm ",
            theme::chrome(),
        ))),
        inner[0],
    );

    // One row per skill, windowed to the panel: a catalogue longer than the screen
    // has to scroll or the bottom of the list is unreachable.
    let rows_height = inner[1].height as usize;
    let first = picker
        .cursor
        .saturating_sub(rows_height.saturating_sub(1))
        .min(picker.cursor);

    let lines: Vec<Line> = picker
        .rows
        .iter()
        .enumerate()
        .skip(first)
        .take(rows_height.max(1))
        .map(|(index, row)| {
            let mark = if row.auto {
                MANAGED
            } else if row.picked {
                PICKED
            } else {
                UNPICKED
            };
            let mark_style = if row.picked && !row.auto {
                theme::active()
            } else {
                theme::chrome()
            };
            // The cursor is a colour on the whole row, which is the same signal the
            // agent form uses for its focused field: where the next keystroke lands
            // is something the eye should find without a marker column.
            let name_style = if index == picker.cursor {
                theme::action()
            } else {
                theme::content()
            };
            let scope_style = if row.auto {
                theme::chrome()
            } else {
                theme::action()
            };
            Line::from(vec![
                Span::styled(format!(" {mark} "), mark_style),
                Span::styled(format!("{:<22}", truncate(&row.id, 22)), name_style),
                Span::styled(format!("{:<8}", truncate(&row.scope, 8)), scope_style),
                Span::styled(
                    truncate(&row.description, area.width.saturating_sub(38) as usize),
                    theme::chrome(),
                ),
            ])
        })
        .collect();

    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// The panel, centred in `body` and clamped to it.
///
/// `width.min(body.width)` and `height.min(body.height)`, because adding to a
/// small body would push the panel off the edge and ratatui would clip it into
/// something unreadable rather than reporting it.
fn centered(body: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(body.width);
    let height = height.min(body.height);
    Rect {
        x: body.x + (body.width - width) / 2,
        y: body.y + (body.height - height) / 2,
        width,
        height,
    }
}

/// Cut to `max` display columns, with an ellipsis when anything was cut.
///
/// `Line::width` for the measure and `char_indices` for the walk, so a name with
/// an accent or a CJK glyph is cut on a character boundary rather than splitting
/// one — the same reason the chat input's tail is cut the way it is.
fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if Line::from(text).width() <= max {
        return text.to_string();
    }
    let mut cut = String::new();
    let mut width = 0;
    for (index, ch) in text.char_indices() {
        let ch_width = Line::from(ch.to_string()).width();
        if width + ch_width + 1 > max {
            break;
        }
        cut.push_str(&text[index..index + ch.len_utf8()]);
        width += ch_width;
    }
    cut.push('…');
    cut
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
    ///
    /// The answer set is *not* asserted here: it lives in `hints_for`, and
    /// `tests/tui_chrome.rs` scans this module for it like any other. A copy here
    /// would be a second place to update and a second thing to go stale.
    #[test]
    fn the_modals_draw_their_border_and_say_what_they_are() {
        let text = all_text(&render_delete("project", "fudi"));
        assert!(text.contains('┌'), "a dialog is framed: {text}");
        assert!(
            text.contains("Delete project: \"fudi\"?"),
            "the dialog names what it deletes: {text}"
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
    /// `active`" — the banner above and the delete question wear it too, which is
    /// why the count below is scoped to the proposal rows.
    #[test]
    fn the_selected_proposal_is_the_active_one_among_the_proposal_rows() {
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
                    render_skill_picker(f, body, &picker(&[("a", false)]));
                })
                .unwrap();
        }
    }

    fn picker(rows: &[(&str, bool)]) -> SkillPicker {
        use crate::tui::views::skill_picker::SkillRow;
        SkillPicker {
            rows: rows
                .iter()
                .map(|(id, picked)| SkillRow {
                    id: id.to_string(),
                    name: id.to_string(),
                    description: format!("{id} does something"),
                    scope: "LlamaR".to_string(),
                    auto: false,
                    picked: *picked,
                })
                .collect(),
            cursor: 0,
        }
    }

    /// The three states a row can be in, and the glyph that tells them apart.
    /// This is the picker's whole reason for existing: before it, a user could not
    /// tell which ids the agent already had without remembering what they typed.
    #[test]
    fn the_three_row_states_are_distinguishable() {
        let mut modal = picker(&[("chosen", true), ("managed", false)]);
        modal.rows[1].auto = true;
        let text = all_text(&render_picker(&modal));

        assert!(text.contains(PICKED), "a picked row is ticked: {text}");
        assert!(text.contains(MANAGED), "a managed row is not: {text}");
        assert!(
            !text.contains(UNPICKED),
            "neither row is unpicked, so the dot must be absent: {text}"
        );
    }

    /// A catalogue longer than the panel still shows the cursor's row. Without the
    /// window, `↓` walks off the bottom and the last skills are unreachable.
    #[test]
    fn a_long_catalogue_scrolls_to_keep_the_cursor_visible() {
        let mut modal = picker(&[("skill-00", false), ("skill-01", false)]);
        modal.rows = (0..40)
            .map(|n| crate::tui::views::skill_picker::SkillRow {
                id: format!("skill-{n:02}"),
                name: format!("skill-{n:02}"),
                description: "does something".to_string(),
                scope: "LlamaR".to_string(),
                auto: false,
                picked: false,
            })
            .collect();
        modal.cursor = 35;

        let text = all_text(&render_picker(&modal));
        assert!(
            text.contains("skill-35"),
            "the row under the cursor must be drawn: {text}"
        );
        assert!(
            !text.contains("skill-00"),
            "and rows above the window must not be, or it is not scrolling"
        );
    }

    /// The dialog keeps a border, because the user is choosing from a list and the
    /// box is what separates that list from the form behind it. It lives in this
    /// module for the same reason, and `only_the_modals_draw_borders` is what makes
    /// that mechanical.
    #[test]
    fn the_picker_is_bordered() {
        let text = all_text(&render_picker(&picker(&[("a", false)])));

        for corner in ['┌', '┐', '└', '┘'] {
            assert!(text.contains(corner), "the dialog keeps a border: {text}");
        }
    }

    /// Cutting to width must not split a character: a CJK glyph is two cells, so a
    /// byte- or char-index cut lands the ellipsis mid-glyph.
    #[test]
    fn truncate_cuts_on_a_character_boundary() {
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("abc", 10), "abc", "short enough is untouched");
        assert_eq!(truncate("anything", 0), "", "no room is no room");

        let wide = truncate("日本語プロジェクト", 7);
        assert!(
            Line::from(wide.as_str()).width() <= 7,
            "a wide glyph must not overshoot the budget: {wide:?}"
        );
    }

    /// The panel must not be drawn over the bar's row, or the header goes the way
    /// it used to before `body` existed.
    #[test]
    fn the_bar_row_survives_the_picker() {
        let modal = picker(&[("a", false)]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        terminal
            .draw(|f| {
                let c = chrome::layout(f.area());
                chrome::render_bar(
                    f,
                    c.bar,
                    &["Dashboard", "Projects", "Chat"],
                    1,
                    &chrome::Context::default(),
                    true,
                );
                render_skill_picker(f, c.body, &modal);
            })
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        assert!(
            row_text(&buffer, 0).contains("Dashboard"),
            "the bar row must survive the dialog: {:?}",
            row_text(&buffer, 0)
        );
    }

    fn render_picker(modal: &SkillPicker) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render_skill_picker(f, Rect::new(0, 1, 80, 22), modal))
            .unwrap();
        terminal.backend().buffer().clone()
    }
}
