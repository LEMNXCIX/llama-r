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

use crate::tui::chrome;
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
/// the form still sitting behind it.
///
/// `hints` is the *same* slice `KeyTarget::SkillPicker` feeds the chrome footer,
/// and it is laid out by the same `chrome::hints_line` — not by
/// `chrome::render_footer`, which the chrome's own row is the one call site of.
/// The panel carries its own keys because it is centred in the screen while the
/// chrome's footer is pinned to the last row, and a legend nine lines below the
/// list it describes is a legend nobody reads. Nothing here names a key:
/// `hints_for` stays the one place the list is written down, so the panel and the
/// footer cannot disagree about what a key does. The caller silences the chrome
/// footer while the picker is up, so the keys appear once, next to what they act
/// on.
pub fn render_skill_picker(
    f: &mut Frame,
    body: Rect,
    picker: &SkillPicker,
    hints: &[(&str, &str)],
) {
    // Wide enough for the legend it has to show, never wider than the screen. A
    // fixed width truncates the tail of the hints, and the tail is where `esc`
    // lives: a dialog whose way out is the part that got cut is worse than a
    // dialog that is slightly too wide.
    let width = chrome::hints_width(hints)
        .saturating_add(4)
        .max(60)
        .min(body.width);
    let area = centered(body, width, body.height.saturating_sub(2).max(7));

    // The block wraps the title and the keys as well as the rows, so the inner
    // rect is computed from the border rather than from the panel. Splitting
    // `area` instead would leave the title on the border's own row, where it is
    // overwritten — and the window height one row too large, which puts the
    // cursor's row off the bottom of a full list.
    let block = Block::default().borders(Borders::ALL);
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ]
            .as_ref(),
        )
        .split(block.inner(area));

    // The block first, then the title and the rows on top of it. Drawn the other
    // way round the border wipes the title, because the block paints every cell
    // of `area` — including the row the title was written to. `Clear` matters for
    // the same reason: without it the panel shows whatever was behind it through
    // the gaps between rows.
    f.render_widget(ratatui::widgets::Clear, area);
    f.render_widget(Paragraph::new(Vec::<Line>::new()).block(block), area);

    f.render_widget(Paragraph::new(picker_title(picker)), inner[0]);

    // One row per visible skill, windowed to the panel: a catalogue longer than
    // the screen has to scroll or the bottom of the list is unreachable. The
    // window is over `visible`, not over `rows`, so the cursor's position within
    // what is on screen is what decides the offset.
    let visible = picker.visible();
    let rows_height = inner[1].height as usize;
    let position = visible
        .iter()
        .position(|&index| index == picker.cursor)
        .unwrap_or(0);
    let first = position.saturating_sub(rows_height.saturating_sub(1));

    let lines: Vec<Line> = if visible.is_empty() {
        vec![Line::from(Span::styled(
            " nothing matches ",
            theme::chrome(),
        ))]
    } else {
        visible
            .iter()
            .skip(first)
            .take(rows_height)
            .map(|&index| {
                let row = &picker.rows[index];
                let mark = if row.auto {
                    MANAGED
                } else if row.picked {
                    PICKED
                } else {
                    UNPICKED
                };
                // The unpicked mark is `content`, not `chrome`: a one-pixel middle
                // dot in `DarkGray` on a dark terminal is close to invisible,
                // which is how the column shipped looking empty. The three states
                // have to be distinguishable at a glance, and the difference
                // between "not chosen" and "chosen" is the whole reason the column
                // exists.
                let mark_style = if row.auto {
                    theme::chrome()
                } else if row.picked {
                    theme::active()
                } else {
                    theme::content()
                };
                // The cursor is a colour on the whole row, which is the same signal
                // the agent form uses for its focused field: where the next
                // keystroke lands is something the eye should find without a
                // marker column.
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
            .collect()
    };

    f.render_widget(Paragraph::new(lines), inner[1]);
    // The panel's own legend. It is a `hints_line`, not a second `render_footer`:
    // `only_the_dispatch_draws_the_footer` counts call sites, and a dialog that
    // reaches for the footer renderer *is* the second footer it exists to catch.
    // Same list, same formatter, and `app.rs` silences the chrome row while the
    // picker is up, so exactly one of the two is ever on screen.
    f.render_widget(Paragraph::new(chrome::hints_line(hints)), inner[2]);
}

/// The panel's top row: the filter being typed, and how much of the catalogue is
/// still on screen.
///
/// The count is what makes the filter trustworthy. Without it a narrowed list is
/// indistinguishable from a short catalogue, and the user cannot tell whether the
/// skill they wanted is missing or merely spelled differently.
fn picker_title(picker: &SkillPicker) -> Line<'static> {
    let total = picker.rows.len();
    let shown = picker.visible().len();
    let mut line = Line::from(Span::styled(" Skills", theme::chrome()));
    if picker.filter.is_empty() {
        line.push_span(Span::styled(
            format!(" — {total} installed "),
            theme::chrome(),
        ));
        return line;
    }
    line.push_span(Span::styled(" / ", theme::chrome()));
    line.push_span(Span::styled(picker.filter.clone(), theme::content()));
    line.push_span(Span::styled(
        format!(" — {shown} of {total} "),
        theme::chrome(),
    ));
    line
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
                    render_skill_picker(f, body, &picker(&[("a", false)]), &[]);
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
            filter: String::new(),
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
                render_skill_picker(f, c.body, &modal, &[]);
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
        render_picker_with(modal, PICKER_HINTS)
    }

    /// The keys the panel is handed. In the app these are the very slice
    /// `KeyTarget::SkillPicker` produces; the panel names no key of its own.
    const PICKER_HINTS: &[(&str, &str)] = &[
        ("↑/↓", "move"),
        ("space", "toggle"),
        ("type", "filter"),
        ("enter", "save"),
        ("esc", "cancel"),
    ];

    fn render_picker_with(modal: &SkillPicker, hints: &[(&str, &str)]) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render_skill_picker(f, Rect::new(0, 1, 80, 22), modal, hints))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// The panel's own keys must reach the screen, and must land inside the border.
    ///
    /// Two failures were stacked here. The keys used to be a title the module
    /// hard-coded, and a bordered `Block` rendered *after* it wiped the row it had
    /// been written to — so the panel had no legend at all. Now the hints arrive
    /// from `hints_for` and are drawn by `chrome::render_footer` into the panel's
    /// last inner row, which is why this asserts the whole line rather than one
    /// word: a legend with its keys on the border is not a legend.
    #[test]
    fn the_panel_keeps_its_own_instructions() {
        let text = all_text(&render_picker(&picker(&[("a", false)])));

        for (key, what) in PICKER_HINTS {
            assert!(
                text.contains(&format!("{key}: {what}")),
                "{key:?} must be legible inside the panel: {text}"
            );
        }
    }

    /// The legend must be *whole* inside the border, not merely present in the
    /// buffer.
    ///
    /// The panel used to be a fixed 60 columns and the hint list is 64, so
    /// `esc: cancel` fell off the right edge — inside a dialog, the one key that
    /// gets you out of it. Checking that the words appear would have passed: a
    /// truncated hint is still in the buffer, just outside the box.
    #[test]
    fn the_whole_legend_fits_inside_the_border() {
        let buffer = render_picker(&picker(&[("a", false)]));
        // The legend is on the last row before the border, so the first pair that
        // fits is the marker for "this row holds the whole thing".
        let legend = row_of(
            &buffer,
            &format!("{}: {}", PICKER_HINTS[0].0, PICKER_HINTS[0].1),
        );
        let row = row_text(&buffer, legend);
        let right_border = row
            .rfind('│')
            .unwrap_or_else(|| panic!("the panel's right border is gone: {row:?}"));
        let inside = &row[..right_border];

        for (key, what) in PICKER_HINTS {
            let hint = format!("{key}: {what}");
            assert!(
                inside.contains(&hint),
                "{hint:?} was cut at the border; the panel is too narrow for its \
                 own legend: {inside:?}"
            );
        }
    }

    /// A filter narrow enough to match nothing must say so on screen. A blank list
    /// under a needle reads as "the catalogue is empty", which is a different and
    /// much more alarming thing than "your spelling is wrong".
    #[test]
    fn a_filter_matching_nothing_says_so() {
        let mut modal = picker(&[("alpha", false), ("beta", false)]);
        modal.push_filter_char('z');

        let text = all_text(&render_picker(&modal));
        assert!(
            text.contains("nothing matches"),
            "an empty list under a needle must be explained: {text}"
        );
        assert!(
            text.contains("0 of 2"),
            "and the count must say the catalogue itself is not empty: {text}"
        );
    }

    /// The title row carries the filter and how much survived. Without the count a
    /// narrowed list is indistinguishable from a short catalogue.
    #[test]
    fn the_title_reports_how_much_of_the_catalogue_survived() {
        let mut modal = picker(&[("alpha", false), ("beta", false), ("gamma", false)]);
        assert!(all_text(&render_picker(&modal)).contains("3 installed"));

        modal.push_filter_char('a');
        modal.push_filter_char('l');
        modal.push_filter_char('p');
        let text = all_text(&render_picker(&modal));
        assert!(
            text.contains("alp") && text.contains("1 of 3"),
            "the needle and the count share the title row: {text}"
        );
    }

    /// The panel's legend must not be the title row's job any more: the title now
    /// belongs to the filter, and a legend sharing a row with the filter would put
    /// two unrelated things in one line.
    #[test]
    fn the_legend_is_not_the_title() {
        let buffer = render_picker(&picker(&[("a", false)]));

        assert_ne!(
            row_of(&buffer, "type: filter"),
            row_of(&buffer, " Skills"),
            "the filter and the legend need their own rows"
        );
    }

    /// Every one of the three states must be *legible*, not merely present.
    ///
    /// The unpicked mark shipped in `DarkGray` and a one-pixel middle dot on a
    /// dark terminal is close to invisible: the column was in the buffer and on
    /// screen the user reported the list as carrying no indication at all. A
    /// mark that is emitted but cannot be read is not a mark, so this asserts the
    /// styles differ rather than only that the glyphs are there.
    #[test]
    fn the_mark_column_is_legible_in_every_state() {
        let mut modal = picker(&[("plain", false), ("ticked", true)]);
        modal.rows[0].auto = false;
        modal.rows[1].auto = false;
        let buffer = render_picker(&modal);

        let mark_column = 11; // the column the marks are written to
        let style_at = |y: u16| buffer[(mark_column, y)].fg;

        // Each row's mark must differ in colour from a plain description in
        // `chrome`, which is what made the dot unreadable.
        let rows: Vec<u16> = (0..buffer.area.height)
            .filter(|y| {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, *y)].symbol().to_string())
                    .collect();
                row.contains("plain") || row.contains("ticked")
            })
            .collect();
        assert_eq!(rows.len(), 2, "both rows must be drawn");

        assert_ne!(
            style_at(rows[0]),
            ratatui::style::Color::DarkGray,
            "the unpicked mark must not be `chrome`; it was invisible that way"
        );
        assert_ne!(
            style_at(rows[1]),
            style_at(rows[0]),
            "a ticked row must not look like an unticked one"
        );
    }
}
