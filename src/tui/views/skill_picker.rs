//! The skill picker's state: which installed skills an agent may use.
//!
//! This replaces a free-text field that took comma-separated ids, which had two
//! problems the picker is here to remove. It offered no way to see what was
//! installed, so the ids had to be known by heart; and it could not show which of
//! them were already chosen without the user remembering what they typed.
//!
//! What the picker deliberately does not do is let you edit `auto_skills`. Those
//! are chosen by `sync_project_agent_skills` from the project profile and are
//! overwritten by the next analysis, so a hand-edit is work thrown away. They are
//! listed, and listed as untouchable, because they do apply — the analyzer merges
//! them with the manual ones — and an agent that quietly has three skills you did
//! not pick is worse than one that does not.
//!
//! The panel itself is drawn by `modals::render_skill_picker`: a dialog's border
//! is only permitted in the module that holds the other dialogs, and
//! `tests/tui_chrome.rs` enforces that by file rather than by exemption.
//!
//! # Filtering
//!
//! Typing narrows the catalogue, `Backspace` widens it back, and the top row
//! shows both the needle and how much of the catalogue survived. Two rules earn
//! their keep:
//!
//! - **The filter never changes what is picked.** It changes what is on screen.
//!   A picker that saved only the visible rows would drop every pick hidden
//!   behind a mistyped needle — a choice the user made and the tool discarded
//!   without saying so.
//! - **The cursor always sits on a visible row**, unless nothing is visible. That
//!   invariant is what keeps a `Space` from ticking a row the user cannot see.
//!
//! `Space` toggles and every other character filters, so `j` and `k` type rather
//! than move. That trade is deliberate: a filter you cannot type a `k` into is
//! useless, since ids are directory names and use the whole alphabet.

use ratatui::crossterm::event::KeyCode;

/// One row: a skill that is installed, and what the agent does with it.
///
/// A row is either toggleable or not, and that is a property of the skill rather
/// than of the mode the picker is in. `auto` marks the skills the analysis chose
/// for itself; those are shown so the user knows they apply, and cannot be
/// unchecked because the next analysis puts them back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRow {
    pub id: String,
    pub name: String,
    pub description: String,
    pub scope: String,
    /// Chosen by `sync_project_agent_skills`, not by hand.
    pub auto: bool,
    /// Currently in the agent's `skills` list.
    pub picked: bool,
}

impl SkillRow {
    /// A row the user may toggle: toggling it adds or removes the id.
    pub fn toggle(&mut self) {
        if !self.auto {
            self.picked = !self.picked;
        }
    }
}

/// The picker's own state. It lives in `TuiApp` for as long as the modal is up,
/// so cancelling is "drop it" rather than "undo it".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPicker {
    pub rows: Vec<SkillRow>,
    pub cursor: usize,
    /// What the user has typed to narrow the catalogue. Never changes what is
    /// picked — only what is on screen.
    pub filter: String,
}

impl SkillPicker {
    /// Build the rows from what is installed and what the agent already has.
    ///
    /// Order is by id, so the cursor lands on the same row for the same catalogue
    /// every time — an unstable order would make `Esc` and re-open mean different
    /// things.
    pub fn for_agent(available: Vec<InstalledSkill>, skills: &[String], auto: &[String]) -> Self {
        let mut rows: Vec<SkillRow> = available
            .into_iter()
            .map(|skill| SkillRow {
                picked: skills.contains(&skill.id),
                auto: auto.contains(&skill.id),
                id: skill.id,
                name: skill.name,
                description: skill.description,
                scope: skill.scope,
            })
            .collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        Self {
            rows,
            cursor: 0,
            filter: String::new(),
        }
    }

    /// The rows the filter leaves on screen, as indices into `rows`.
    ///
    /// Indices rather than a second `Vec<SkillRow>`: picking is state on the row,
    /// and a copy the filter could not write back to is a copy that goes stale the
    /// moment a row is ticked.
    pub fn visible(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| self.matches(row))
            .map(|(index, _)| index)
            .collect()
    }

    /// Case-insensitive substring match on the id and the description.
    ///
    /// The id alone would be enough to find anything whose name you remember, and
    /// that is the case that matters when you are looking for one specific skill.
    /// The description is included because it is the only other thing on screen —
    /// a list you can only search by its own ids is searchable only by people who
    /// already know the ids, which is what the free-text field already forced.
    fn matches(&self, row: &SkillRow) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let needle = self.filter.to_lowercase();
        row.id.to_lowercase().contains(&needle) || row.description.to_lowercase().contains(&needle)
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        // Clamped rather than wrapped: a picker that wraps from the last row to
        // the first makes "how many rows down am I" unanswerable, and the count is
        // the only thing telling the cursor apart from a static list.
        let last = visible.len() as isize - 1;
        // An off-screen cursor is not a state this struct should be able to hold
        // (`reanchor` prevents it), but a move while the filter matches nothing
        // has to be a no-op rather than a panic.
        let position = match visible.iter().position(|&index| index == self.cursor) {
            Some(position) => position as isize,
            None => return,
        };
        self.cursor = visible[(position + delta).clamp(0, last) as usize];
    }

    /// Put the cursor back on a row the filter leaves visible.
    ///
    /// The invariant this maintains — *the cursor is on a visible row, unless
    /// nothing is visible* — is what lets `toggle_current` skip the visibility
    /// check and what stops a keystroke from acting on a row the user cannot see.
    fn reanchor(&mut self) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        if !visible.contains(&self.cursor) {
            self.cursor = visible[0];
        }
    }

    /// Whether `c` is something a filter can be typed from.
    ///
    /// Letters, digits and the punctuation a directory name tends to carry.
    /// Space is deliberately excluded: it is the toggle, and a filter is worth
    /// less than a way to pick things.
    pub fn filter_char(c: char) -> Option<char> {
        if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '+') {
            Some(c.to_ascii_lowercase())
        } else {
            None
        }
    }

    pub fn push_filter_char(&mut self, c: char) {
        if let Some(c) = Self::filter_char(c) {
            self.filter.push(c);
            self.reanchor();
        }
    }

    pub fn pop_filter_char(&mut self) {
        self.filter.pop();
        self.reanchor();
    }

    pub fn toggle_current(&mut self) {
        // With the filter matching nothing every row is hidden, and a toggle then
        // would change a pick the user cannot see and did not aim at.
        if !self.visible().contains(&self.cursor) {
            return;
        }
        if let Some(row) = self.rows.get_mut(self.cursor) {
            row.toggle();
        }
    }

    /// The ids to write back to the agent's `skills` list.
    ///
    /// The filter is not consulted: it changes what is on screen, not what is
    /// chosen. A picker that saved only what the filter happened to leave visible
    /// would silently drop every pick hidden behind a mistyped needle, which is a
    /// selection the user made and the tool then threw away without saying so.
    ///
    /// Only the picked, non-auto rows: an auto skill is applied by the analyzer
    /// regardless of what lands here, and writing it into `skills` as well would
    /// make it survive as a manual entry the user never chose and cannot see the
    /// origin of.
    pub fn chosen_ids(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|row| row.picked && !row.auto)
            .map(|row| row.id.clone())
            .collect()
    }
}

/// What a keystroke asked of the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerAction {
    /// Handled inside the picker; nothing to apply outside it.
    Applied,
    /// Confirm: these ids are the agent's new `skills` list.
    Save(Vec<String>),
    /// Cancel: drop the picker, changing nothing.
    Cancel,
}

/// Route one keystroke to what it means in the picker.
///
/// This is the whole key map, lifted out of `TuiApp::run` for the same reason
/// `chat_input_action` is: the event loop reads stdin directly, so a rule that
/// lives only inside its `match` cannot be tested at all. That is not
/// hypothetical — the picker shipped with its entire key map unreachable, because
/// the arm sat below the agent form's own guard. `j` and `k` moving the cursor
/// used to be untested for the same structural reason, and are now a plain
/// function over a `KeyCode`.
///
/// `Space` toggles and every other character filters, which is why `j` and `k`
/// type instead of moving. One key does one thing: the alternative is a filter
/// you cannot type a `k` into, and skill ids contain every letter.
pub fn picker_action(picker: &mut SkillPicker, code: KeyCode) -> PickerAction {
    match code {
        KeyCode::Down => picker.move_cursor(1),
        KeyCode::Up => picker.move_cursor(-1),
        KeyCode::Char(' ') => picker.toggle_current(),
        KeyCode::Char(c) => picker.push_filter_char(c),
        KeyCode::Backspace => picker.pop_filter_char(),
        KeyCode::Enter => return PickerAction::Save(picker.chosen_ids()),
        KeyCode::Esc => return PickerAction::Cancel,
        // Anything else — `Tab`, `Home`, a function key — belongs to no rule here.
        // It is swallowed rather than passed through to the form underneath, which
        // is the same thing the dialog did before it had a filter.
        _ => {}
    }
    PickerAction::Applied
}

/// What the skill manager knows about a skill, without its body or its path.
///
/// A thin alias of the API's `SkillSummary`: both answer "what is installed and
/// what is it called", and they used to be hand-mapped from `Skill` separately,
/// which is how one gains a column and the other does not.
pub type InstalledSkill = crate::api::skill_api::SkillSummary;

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(id: &str, scope: &str) -> InstalledSkill {
        InstalledSkill {
            id: id.to_string(),
            name: id.to_string(),
            description: format!("{id} does something"),
            tags: Vec::new(),
            scope: scope.to_string(),
        }
    }

    /// `auto_skills` are shown but not editable: they are rewritten by the next
    /// analysis, so a hand-edit is work thrown away.
    #[test]
    fn an_auto_skill_cannot_be_toggled() {
        let mut picker = SkillPicker::for_agent(
            vec![
                installed("auto-one", "LlamaR"),
                installed("mine", "Project"),
            ],
            &[],
            &["auto-one".to_string()],
        );

        picker.move_cursor(0);
        picker.toggle_current();
        assert!(
            !picker.rows.iter().any(|row| row.auto && row.picked),
            "an auto row must not become picked"
        );

        picker.toggle_current();
        assert!(
            !picker.rows.iter().any(|row| row.picked),
            "still not picked"
        );

        // And the hand-editable one still works, so the rule is about `auto` and
        // not about the first row being broken.
        picker.move_cursor(1);
        picker.toggle_current();
        assert_eq!(picker.chosen_ids(), vec!["mine".to_string()]);
    }

    /// The cursor clamps rather than wraps. A wrapping picker makes "which row am
    /// I on" unanswerable from the count alone, and the count is the only thing
    /// separating the cursor from a static list.
    #[test]
    fn the_cursor_clamps_at_both_ends() {
        let mut picker = SkillPicker::for_agent(
            vec![
                installed("a", "LlamaR"),
                installed("b", "LlamaR"),
                installed("c", "LlamaR"),
            ],
            &[],
            &[],
        );

        picker.move_cursor(-1);
        assert_eq!(picker.cursor, 0, "up from the first row stays put");

        picker.move_cursor(99);
        assert_eq!(picker.cursor, 2, "down past the last row stays put");
    }

    /// An empty catalogue is a real state — nothing installed yet — and it must not
    /// panic on a move or a toggle.
    #[test]
    fn an_empty_catalogue_survives_the_keys() {
        let mut picker = SkillPicker::for_agent(vec![], &[], &[]);

        picker.move_cursor(-1);
        picker.move_cursor(1);
        picker.toggle_current();

        assert_eq!(picker.chosen_ids(), Vec::<String>::new());
    }

    /// Confirming writes back the picked ids in catalogue order, so the agent's
    /// `skills` list does not churn when a user re-picks the same set. The rows are
    /// sorted by id, so that order is alphabetical whatever order the manager
    /// handed them over in.
    #[test]
    fn confirming_writes_the_picked_ids_in_order() {
        let mut picker = SkillPicker::for_agent(
            vec![
                installed("zebra", "LlamaR"),
                installed("apple", "LlamaR"),
                installed("mango", "LlamaR"),
            ],
            &[],
            &[],
        );

        assert_eq!(
            picker.rows[0].id, "apple",
            "sorted, so the cursor starts there"
        );

        picker.toggle_current();
        picker.move_cursor(2);
        picker.toggle_current();

        assert_eq!(
            picker.chosen_ids(),
            vec!["apple".to_string(), "zebra".to_string()]
        );
    }

    /// What the agent already has comes back ticked, which is the difference
    /// between a picker and a blank form.
    #[test]
    fn the_agents_existing_skills_come_back_picked() {
        let picker = SkillPicker::for_agent(
            vec![installed("already", "LlamaR"), installed("fresh", "LlamaR")],
            &["already".to_string()],
            &[],
        );

        assert_eq!(picker.chosen_ids(), vec!["already".to_string()]);
    }

    /// A three-row catalogue with descriptions that are not the id again.
    ///
    /// The filter cannot hold a space — `Space` is the toggle — so every needle in
    /// these tests is a single word. That only bites if the descriptions differ from
    /// the ids: with a description of `"<id> does something"` the whole catalogue
    /// shares the substring `"does something"` and nothing can be narrowed by it.
    fn catalogue() -> SkillPicker {
        let rows = [
            ("billing-glossary", "Project", "Terms used on invoices"),
            (
                "safe-refactor",
                "Harness",
                "Restructure code and keep it green",
            ),
            ("writing-shape", "LlamaR", "Prose that reads as breath"),
        ];
        SkillPicker::for_agent(
            rows.iter()
                .map(|(id, scope, description)| InstalledSkill {
                    id: id.to_string(),
                    name: id.to_string(),
                    description: description.to_string(),
                    tags: Vec::new(),
                    scope: scope.to_string(),
                })
                .collect(),
            &[],
            &[],
        )
    }

    fn typed(picker: &mut SkillPicker, needle: &str) {
        for c in needle.chars() {
            picker.push_filter_char(c);
        }
    }

    /// Widen the needle back to empty the way the user would: one `Backspace` per
    /// character, so this also exercises the re-anchor on every step.
    fn clear(picker: &mut SkillPicker) {
        while !picker.filter.is_empty() {
            picker.pop_filter_char();
        }
    }

    /// A filter that only searched ids would be searchable only by people who
    /// already know the ids — which is the free-text field's old failure. The
    /// description is on screen, so it has to be searchable too.
    #[test]
    fn the_filter_narrows_by_id_or_description() {
        let mut picker = catalogue();

        typed(&mut picker, "billing");
        assert_eq!(
            picker.visible(),
            vec![0],
            "an id substring narrows to that row"
        );

        clear(&mut picker);
        assert_eq!(picker.visible(), vec![0, 1, 2], "back to everything");

        // "invoices" is in row 0's description and in no id anywhere, so an id-only
        // search finds nothing here.
        typed(&mut picker, "invoices");
        assert_eq!(
            picker.visible(),
            vec![0],
            "the description is searchable too"
        );
    }

    /// The rule the whole feature hangs on: typing changes what is on screen and
    /// nothing else. A pick hidden behind a needle is still a pick.
    #[test]
    fn filtering_never_touches_the_selection() {
        let mut picker = catalogue();

        picker.toggle_current(); // "billing-glossary"
        assert_eq!(picker.chosen_ids(), vec!["billing-glossary".to_string()]);

        // A needle that excludes it, and a move around while it is excluded.
        typed(&mut picker, "writing");
        picker.move_cursor(1);
        picker.move_cursor(1);
        picker.toggle_current();

        assert_eq!(
            picker.chosen_ids(),
            vec!["billing-glossary".to_string(), "writing-shape".to_string()],
            "the pick made before the filter and the one made after it both stand"
        );
    }

    /// Confirming with a filter on saves everything, not just what is on screen.
    /// The failure this prevents is a silent one: the user ticks two skills,
    /// types a needle to check a third, and `Enter` quietly un-ticks the first.
    #[test]
    fn confirming_under_a_filter_saves_the_whole_selection() {
        let mut picker = catalogue();
        picker.toggle_current();
        picker.move_cursor(2);
        picker.toggle_current();
        typed(&mut picker, "refactor");

        let action = picker_action(&mut picker, KeyCode::Enter);

        assert_eq!(
            action,
            PickerAction::Save(vec![
                "billing-glossary".to_string(),
                "writing-shape".to_string()
            ]),
            "the filtered-out pick is still saved"
        );
    }

    /// The cursor must land on something the user can see. Without this, a `Space`
    /// typed right after narrowing ticks whichever row happened to be under the
    /// cursor — a row that is not on screen.
    #[test]
    fn the_cursor_never_sits_on_a_hidden_row() {
        let mut picker = catalogue();
        picker.move_cursor(1); // "safe-refactor"
        assert_eq!(picker.cursor, 1);

        // A needle that leaves only row 0 visible.
        typed(&mut picker, "billing");
        assert_eq!(
            picker.cursor, 0,
            "the cursor was pulled onto the row that is still on screen"
        );

        // A move steps through *visible* rows rather than through raw indices. "b" is in
        // "billing" and in "breath" but in neither the id nor the description of
        // "safe-refactor", so the visible set is rows 0 and 2 — non-contiguous,
        // and stepping by index would land on row 1, which is not on screen.
        clear(&mut picker);
        typed(&mut picker, "b");
        assert_eq!(picker.visible(), vec![0, 2], "a non-contiguous pair");
        picker.move_cursor(1);
        assert_eq!(
            picker.cursor, 2,
            "down went to the next visible row, not to index 1"
        );

        // Widening back must not strand it either.
        clear(&mut picker);
        assert_eq!(picker.cursor, 2, "the same row is still there to point at");
        picker.move_cursor(1);
        assert_eq!(picker.cursor, 2, "clamped at the end, and still visible");
    }

    /// A needle matching nothing is a real state, and it must be inert rather than
    /// act on whatever row the cursor was left on.
    #[test]
    fn a_filter_matching_nothing_is_inert() {
        let mut picker = catalogue();
        picker.toggle_current(); // "billing-glossary" is picked
        typed(&mut picker, "zzzz");
        assert!(picker.visible().is_empty());

        picker.move_cursor(1);
        picker.move_cursor(-1);
        picker.toggle_current();

        assert_eq!(
            picker.chosen_ids(),
            vec!["billing-glossary".to_string()],
            "nothing may be ticked while every row is hidden"
        );
    }

    /// `Space` is the toggle, so it cannot also be a character in the needle — and
    /// a needle cannot hold a space, which is the same statement.
    #[test]
    fn space_toggles_and_never_types() {
        assert_eq!(
            SkillPicker::filter_char(' '),
            None,
            "a filter cannot contain a space, or `Space` would do both"
        );
    }

    /// Ids are directory names, so the filter has to take a capital and keep it
    /// findable: the match is case-insensitive, which means folding on the way in.
    #[test]
    fn the_filter_folds_case_on_the_way_in() {
        assert_eq!(SkillPicker::filter_char('R'), Some('r'));

        let mut picker = catalogue();
        typed(&mut picker, "SAFE");
        assert_eq!(picker.visible(), vec![1], "typed in caps, found anyway");
    }

    /// `j` and `k` were the cursor keys before there was a filter. They type now.
    /// Pinned because it is a trade, not an accident: the arrows still navigate, and
    /// the panel's legend says so.
    #[test]
    fn letters_type_and_only_the_arrows_move_the_cursor() {
        let mut picker = catalogue();

        assert_eq!(
            picker_action(&mut picker, KeyCode::Char('j')),
            PickerAction::Applied
        );
        assert_eq!(
            picker.cursor, 0,
            "`j` types instead of moving; that is the cost of a filter"
        );
        assert_eq!(picker.filter, "j");

        picker_action(&mut picker, KeyCode::Char('k'));
        assert_eq!(picker.filter, "jk");

        // Neither letter moved the cursor, and with `"jk"` matching nothing there is
        // no row for them to have moved it onto.
        picker_action(&mut picker, KeyCode::Backspace);
        assert_eq!(picker.filter, "j");
        assert!(
            picker.visible().is_empty(),
            "`j` is in no id and no description, so there was nothing to select"
        );
        assert_eq!(picker.cursor, 0);

        // The arrows are the only way down.
        clear(&mut picker);
        picker_action(&mut picker, KeyCode::Down);
        assert_eq!(picker.cursor, 1, "and they do move it");
    }

    /// A key that belongs to no rule is swallowed, not forwarded to the form
    /// underneath: the dialog owns every key while it is up, which is what
    /// `a_dialog_is_intercepted_before_the_view_it_sits_over` is about.
    #[test]
    fn an_unrouted_key_is_swallowed_and_changes_nothing() {
        let mut picker = catalogue();
        let before = picker.clone();

        assert_eq!(
            picker_action(&mut picker, KeyCode::Tab),
            PickerAction::Applied
        );
        assert_eq!(picker, before);
    }

    /// Cancel is the negative case of the whole dialog: it must change nothing,
    /// including the picks made while it was up.
    #[test]
    fn cancelling_reports_cancel_and_commits_nothing() {
        let mut picker = catalogue();
        picker.toggle_current();

        assert_eq!(
            picker_action(&mut picker, KeyCode::Esc),
            PickerAction::Cancel
        );
        assert_eq!(
            picker.chosen_ids(),
            vec!["billing-glossary".to_string()],
            "the caller drops the picker; this is why the state lives in it"
        );
    }

    /// A whole session, in order, through the public key map only.
    ///
    /// Every other test here calls one method and checks one fact. This one drives
    /// the sequence the user actually types — narrow, move, tick, widen, tick,
    /// save — because the bug that shipped was never a wrong fact about a method,
    /// it was the *order* in which the methods were reached.
    #[test]
    fn a_whole_typing_session_behaves() {
        let mut picker = catalogue();

        // Narrow to one row and tick it.
        for c in "bill".chars() {
            picker_action(&mut picker, KeyCode::Char(c));
        }
        assert_eq!(picker.visible(), vec![0]);
        picker_action(&mut picker, KeyCode::Char(' '));

        // The arrows clamp at the only visible row rather than walking off it.
        picker_action(&mut picker, KeyCode::Down);
        picker_action(&mut picker, KeyCode::Down);
        assert_eq!(picker.cursor, 0);

        // Widen back a character at a time; the tick survives every step.
        for expected in ["bil", "bi", "b", ""] {
            picker_action(&mut picker, KeyCode::Backspace);
            assert_eq!(picker.filter, expected);
        }
        assert_eq!(picker.visible().len(), 3, "the whole catalogue is back");

        // Move and tick a second one, then save both.
        picker_action(&mut picker, KeyCode::Down);
        picker_action(&mut picker, KeyCode::Char(' '));
        assert_eq!(
            picker_action(&mut picker, KeyCode::Enter),
            PickerAction::Save(vec![
                "billing-glossary".to_string(),
                "safe-refactor".to_string()
            ])
        );
    }
}
