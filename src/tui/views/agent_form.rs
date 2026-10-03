use crate::tui::{chrome, theme};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

/// The one-line fields, in the order Tab walks them. A field's index is the
/// caller's `field_index`, so this list and the key handling in `app.rs` are the
/// same numbering.
///
/// These are the bare field names. The old border titles also spelled out the
/// keys — `(Arrows to cycle)`, `(Enter for newline)`, `(comma separated)` — and
/// none of it survives: the shared footer says what the keys do, and a value
/// column wide enough to hold those notes leaves too little room for the value.
const FIELDS: [&str; 7] = [
    "ID",
    "Name",
    "Model",
    "Project Context",
    "Rules",
    "Optimization Rules",
    "Skills/Tools",
];

/// The prompt is the only field that is more than a line, so it is not in
/// `FIELDS`; it is the field after them.
const PROMPT_INDEX: usize = FIELDS.len();

/// The project field is cycled with the arrow keys instead of being typed into,
/// so it never holds the caret. Its index is the one `app.rs` cycles on.
const CYCLED_FIELD: usize = 3;

/// Label of the prompt's own row.
const PROMPT_LABEL: &str = "System Prompt";

/// Blank columns between the label column and the values.
const GAP: usize = 2;

/// Draws the agent form inside `body`, the part of the screen the shared chrome
/// left over.
///
/// Each field is one row: a label and the value beside it, the values forming a
/// column the eye reads down. The field the next keystroke lands in wears
/// [`theme::action`], so where the user is typing is a colour rather than a
/// border. A thin rule then separates the prompt, which is the longest thing
/// typed here and takes the full width of the body.
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
    let values = [id, name, model, project_id, rules, optimize_rules, skills];

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Length(1), // 0: ID
                Constraint::Length(1), // 1: Name
                Constraint::Length(1), // 2: Model
                Constraint::Length(1), // 3: Project
                Constraint::Length(1), // 4: Rules
                Constraint::Length(1), // 5: Optimization Rules
                Constraint::Length(1), // 6: Skills/Tools
                Constraint::Length(1), // 7: the rule
                Constraint::Length(1), // 8: the prompt's label
                Constraint::Min(0),    // 9: the prompt
            ]
            .as_ref(),
        )
        .split(body);

    for (index, (label, value)) in FIELDS.iter().zip(values).enumerate() {
        let (label_style, value_style) = if index == field_index {
            (theme::action(), theme::action())
        } else {
            (theme::chrome(), theme::content())
        };
        // Padded with spaces to the value column rather than laid out with a
        // second `Layout`, because `Line::width` is display width: a label that
        // grows an accent or a wide glyph stays in the column instead of
        // shifting its own row's value right.
        let pad = " ".repeat((value_column() as usize).saturating_sub(Line::from(*label).width()));
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(*label, label_style),
                Span::styled(format!("{pad}{value}"), value_style),
            ])),
            chunks[index],
        );
    }

    chrome::separator(f, chunks[7]);

    let prompt_style = if field_index == PROMPT_INDEX {
        theme::action()
    } else {
        theme::chrome()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(PROMPT_LABEL, prompt_style))),
        chunks[8],
    );
    f.render_widget(
        Paragraph::new(prompt)
            .style(theme::content())
            .wrap(Wrap { trim: true }),
        chunks[9],
    );

    // The caret follows the active field. The project field is cycled with the
    // arrow keys rather than typed into, so it has no caret of its own.
    if field_index == PROMPT_INDEX {
        let (x, y) = prompt_cursor(prompt, chunks[9]);
        f.set_cursor_position((x, y));
    } else if field_index < FIELDS.len() && field_index != CYCLED_FIELD {
        f.set_cursor_position(value_cursor(values[field_index], chunks[field_index]));
    }
}

/// Width of the widest label, in terminal columns.
fn label_column() -> usize {
    FIELDS
        .iter()
        .map(|label| Line::from(*label).width())
        .max()
        .unwrap_or(0)
}

/// Column, measured from the left edge of the body, that every value starts at.
fn value_column() -> u16 {
    label_column() as u16 + GAP as u16
}

/// Where the caret sits after `value`, on the field's own row.
///
/// Clamped to the row, because a value longer than the row is drawn clipped and
/// a caret past the last column is not on screen at all.
fn value_cursor(value: &str, row: Rect) -> (u16, u16) {
    let x = row.x + value_column() + Line::from(value).width() as u16;
    let last_column = row.x + row.width.saturating_sub(1);
    (x.min(last_column), row.y)
}

/// Where the caret sits inside the prompt, once it has been wrapped into `area`.
///
/// The prompt is the only field long enough to wrap, and the only one anyone
/// types prose into, so the caret has to land on the character last typed rather
/// than at the top of the buffer.
fn prompt_cursor(prompt: &str, area: Rect) -> (u16, u16) {
    let width = area.width as usize;
    if width == 0 {
        return (area.x, area.y);
    }

    let lines: Vec<&str> = prompt.split('\n').collect();
    let mut rows = 0usize;
    let mut column = 0usize;
    for (index, line) in lines.iter().enumerate() {
        // Widths, not character counts: an accented letter is one column and a
        // CJK one is two, and counting characters puts the caret in the wrong
        // place on exactly the prompts that have them.
        let line_width = Line::from(*line).width();
        if index < lines.len() - 1 {
            // Every row this line occupies, and nothing more: the hard newline
            // does not spend a row of its own, it only puts the next line on the
            // row after this one's last. Adding one here detaches the caret by a
            // row for every newline already typed.
            rows += line_width.div_ceil(width);
        } else {
            // The caret goes *after* the last character, and floor is what puts
            // it there: `ceil - 1` is only the same when the line does not end
            // exactly on a row boundary, and one that does — an 80-column system
            // prompt on an 80-column terminal — would leave the caret sitting on
            // the last character instead of past it.
            rows += line_width / width;
            column = line_width % width;
        }
    }

    let last_row = area.y + area.height.saturating_sub(1);
    (
        area.x + column.min(width - 1) as u16,
        (area.y + rows as u16).min(last_row),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::chrome::{self, Context};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Style;
    use ratatui::Terminal;

    use super::super::test_helpers::{all_text, column_of, row_text};

    /// Row of the form `index`'s field. The body starts under the bar, and the
    /// fields come before the rule and the prompt.
    fn row_of(index: usize) -> u16 {
        1 + index as u16
    }

    /// A distinguishable value per one-line field, so a test can tell one row's
    /// value from another's.
    const VALUES: [&str; 7] = [
        "ops",
        "hola",
        "llama3",
        "fudi",
        "habla español",
        "sé breve",
        "demo",
    ];

    /// The form on an 80x24 terminal, inside the body the chrome leaves under
    /// the bar, with the active field `field_index`.
    ///
    /// Returns the buffer and the caret the terminal ended up with: a caret is
    /// not drawn into the buffer, so it can only be read back off the terminal.
    fn draw_form(prompt: &str, field_index: usize) -> (Buffer, (u16, u16)) {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                render_agent_form(
                    f,
                    Rect::new(0, 1, 80, 22),
                    VALUES[0],
                    VALUES[1],
                    VALUES[2],
                    VALUES[3],
                    VALUES[4],
                    VALUES[5],
                    VALUES[6],
                    prompt,
                    field_index,
                )
            })
            .unwrap();
        let caret = terminal.backend().cursor_position();
        (terminal.backend().buffer().clone(), (caret.x, caret.y))
    }

    /// The form with a one-line prompt, on field `field_index`.
    fn render(field_index: usize) -> Buffer {
        draw_form("hola", field_index).0
    }

    /// The terminal column where `needle` starts on row `y`, or a panic naming
    /// the row: a value missing from its own row is a failure, not a row to skip.
    fn column_of_or_panic(buffer: &Buffer, y: u16, needle: &str) -> usize {
        column_of(buffer, y, needle)
            .unwrap_or_else(|| panic!("{needle:?} is not on row {y}: {:?}", row_text(buffer, y)))
    }

    /// Does the field label on row `y` wear `token`?
    ///
    /// Only the fields a token sets: a buffer cell always carries a colour and a
    /// modifier set, so the whole `Style` never compares equal to a token.
    fn wears(buffer: &Buffer, y: u16, token: Style) -> bool {
        let cell = &buffer[(0, y)];
        Some(cell.fg) == token.fg && cell.modifier == token.add_modifier
    }

    /// Absence of borders alone would also pass a form that drew nothing, so
    /// every field has to be on screen too.
    #[test]
    fn the_form_renders_without_borders_and_shows_every_field() {
        let text = all_text(&render(0));
        for ch in ['┌', '┐', '└', '┘', '│'] {
            assert!(
                !text.contains(ch),
                "the agent form must not draw box char {ch:?}: {text}"
            );
        }
        for label in FIELDS.iter().chain([&PROMPT_LABEL]) {
            assert!(text.contains(*label), "{label:?} must be labelled: {text}");
        }
        for value in VALUES {
            assert!(text.contains(value), "{value:?} must be shown: {text}");
        }
        assert!(
            text.contains('─'),
            "the rule above the prompt must render: {text}"
        );
    }

    /// `CYCLED_FIELD` is a bare index, and `app.rs` hardcodes the same numbering
    /// against `PROMPT_INDEX` and `% 8`, so this is the pin that keeps the two
    /// sides describing the same form. Reordering `FIELDS` without it would put
    /// the caret in the wrong row, which nothing else here would notice.
    #[test]
    fn the_cycled_field_is_the_project_one() {
        assert_eq!(FIELDS[CYCLED_FIELD], "Project Context");
    }

    /// The values are a column: the eye reads them down, so they all have to
    /// start in the same place whatever the label above them is.
    ///
    /// The column is pinned against the widest label and the gap rather than
    /// against `value_column`, so a helper that returned something else — a
    /// column at 40, clipping every value — fails here instead of agreeing with
    /// itself.
    #[test]
    fn the_field_values_line_up_in_a_column() {
        let buffer = render(0);
        let columns: Vec<usize> = VALUES
            .iter()
            .enumerate()
            .map(|(index, value)| column_of_or_panic(&buffer, row_of(index), value))
            .collect();
        let expected = Line::from("Optimization Rules").width() + 2;
        assert_eq!(
            columns,
            vec![expected; FIELDS.len()],
            "every value must start at the widest label plus the gap, column {expected}: {columns:?}"
        );
    }

    /// The active field is the one the next keystroke lands in, and colour is
    /// how it says so. Every other label has to stay in `chrome`, or nothing on
    /// screen tells the two apart.
    #[test]
    fn only_the_active_field_wears_the_action_style() {
        for field_index in 0..=FIELDS.len() {
            let buffer = render(field_index);
            for index in 0..FIELDS.len() {
                let token = if index == field_index {
                    theme::action()
                } else {
                    theme::chrome()
                };
                assert!(
                    wears(&buffer, row_of(index), token),
                    "field {index} while the form is on field {field_index} must wear {token:?}"
                );
            }
            let prompt_label_row = row_of(PROMPT_INDEX + 1);
            assert_eq!(
                wears(&buffer, prompt_label_row, theme::action()),
                field_index == PROMPT_INDEX,
                "the prompt label must read as active only while the prompt is the field being edited"
            );
        }
    }

    /// The caret has to sit after the value being typed, not at the start of the
    /// field. The project field is skipped: it is cycled, and it has its own
    /// test below.
    #[test]
    fn the_cursor_sits_after_the_active_field() {
        for field_index in (0..VALUES.len()).filter(|index| *index != CYCLED_FIELD) {
            let (_, caret) = draw_form("hola", field_index);
            let value = VALUES[field_index];
            assert_eq!(
                caret,
                (
                    value_column() + Line::from(value).width() as u16,
                    row_of(field_index),
                ),
                "the caret must sit after {value:?}"
            );
        }
    }

    /// The project field is cycled with the arrow keys, so it must not claim a
    /// caret the user cannot see move. The terminal starts at (0,0) and no field
    /// puts a caret there, so an untouched (0,0) is the frame having set none.
    #[test]
    fn the_cycled_project_field_leaves_the_cursor_alone() {
        let (_, caret) = draw_form("hola", CYCLED_FIELD);
        assert_eq!(caret, (0, 0), "a cycled field has no caret to place");
    }

    /// A prompt long enough to wrap puts the caret on the row the text wrapped
    /// to. The bug this guards is the caret jumping back to the top of the
    /// prompt as soon as a line wraps.
    ///
    /// The CJK case is the one that bites: `日` is one character and two
    /// columns, so counting characters instead of columns puts the caret a whole
    /// row higher and far to the right. Both are asserted, because either alone
    /// would pass a caret that is wrong in only one of them.
    #[test]
    fn the_cursor_follows_the_wrapped_prompt() {
        let prompt_row = row_of(PROMPT_INDEX + 2);
        let cases = [
            // 163 columns on an 80-column row: two full rows and three left over.
            (format!("{}fin", "x".repeat(160)), 3, 2),
            // 41 CJK glyphs are 82 columns: two rows and five left over. Counted
            // as characters it is 44, which is one row and never wraps.
            (format!("{}fin", "日".repeat(41)), 5, 1),
        ];
        for (prompt, expected_column, expected_rows) in cases {
            let (buffer, caret) = draw_form(&prompt, PROMPT_INDEX);
            assert_eq!(
                caret,
                (expected_column, prompt_row + expected_rows),
                "the caret must sit after `fin`, on the row {prompt:?} wrapped onto"
            );
            // And the prompt really did wrap, or the caret arithmetic is untested.
            // `trim_end` rather than `starts_with`: a wide glyph owns its cell and
            // the blank beside it, so the row reads as one entry per column.
            let tail = row_text(&buffer, prompt_row + expected_rows);
            assert!(
                tail.trim_end().ends_with("fin"),
                "the tail must be on row {expected_rows}: {tail:?}"
            );
        }
    }

    /// Each hard newline puts the next line on the row after the previous one, so
    /// the caret follows the text down one row per newline and not one more.
    ///
    /// The rendered rows are read back as well: without them the caret could be
    /// asserted against arithmetic that is wrong in the same direction as the
    /// implementation and the test would agree with the bug.
    #[test]
    fn the_cursor_follows_the_newlines_in_the_prompt() {
        let (buffer, caret) = draw_form("uno\ndos\ntres\ncuatro", PROMPT_INDEX);
        let prompt_row = row_of(PROMPT_INDEX + 2);
        for (offset, line) in ["uno", "dos", "tres", "cuatro"].iter().enumerate() {
            let row = row_text(&buffer, prompt_row + offset as u16);
            assert_eq!(
                row.trim_end(),
                *line,
                "`{line}` must be on row {offset} of the prompt: {row:?}"
            );
        }
        assert_eq!(
            caret,
            (6, prompt_row + 3),
            "the caret must sit after `cuatro`, on the row it rendered on"
        );
    }

    /// A final line that exactly fills the row leaves the caret on the next row
    /// at column 0: "after the last character" is the start of the row below, and
    /// that is where a terminal puts it too.
    ///
    /// The rows are read back first so the `+1` means something: the prompt
    /// occupies exactly one row, and the caret is the row past it.
    #[test]
    fn the_cursor_sits_after_a_final_line_that_exactly_fills_the_row() {
        let full_row = "x".repeat(80);
        let (buffer, caret) = draw_form(&full_row, PROMPT_INDEX);
        let prompt_row = row_of(PROMPT_INDEX + 2);
        assert_eq!(
            row_text(&buffer, prompt_row).trim_end(),
            full_row,
            "the prompt must fill the row exactly"
        );
        assert_eq!(
            row_text(&buffer, prompt_row + 1).trim_end(),
            "",
            "and stop there: nothing wrapped onto the next row"
        );
        assert_eq!(
            caret,
            (0, prompt_row + 1),
            "the caret must sit past the last character, not on it"
        );
    }

    /// The bug the shared body rect exists to fix: a form laid out over
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
                render_agent_form(f, c.body, VALUES[0], "", "", "", "", "", "", "", 0);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let bar_row = row_text(&buffer, c.bar.y);
        // Only the bar writes these names now that the form has no title block,
        // so matching on "Agent" cannot pass by accident either.
        assert!(
            bar_row.contains("Dashboard") && bar_row.contains("Projects"),
            "the bar's row must survive the form: {bar_row:?}"
        );
        let first_body_row = row_text(&buffer, c.body.y);
        assert!(
            first_body_row.contains("ID") && first_body_row.contains("ops"),
            "the form must start on the body's first row: {first_body_row:?}"
        );
    }

    /// A body narrower than the labels cannot hold the value column, and the
    /// prompt's wrap arithmetic divides by the row width. Nothing may panic on a
    /// terminal too small to draw the form, whichever field is active.
    #[test]
    fn the_form_survives_a_tiny_terminal() {
        for (width, height) in [(1u16, 1u16), (2, 2), (3, 3), (5, 4), (10, 1), (2, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|f| {
                    let body = Rect::new(0, 0, width, height);
                    for field_index in 0..=FIELDS.len() {
                        render_agent_form(
                            f,
                            body,
                            VALUES[0],
                            VALUES[1],
                            VALUES[2],
                            VALUES[3],
                            VALUES[4],
                            VALUES[5],
                            VALUES[6],
                            "hola\nmundo",
                            field_index,
                        );
                    }
                })
                .unwrap();
        }
    }
}
