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
        Self { rows, cursor: 0 }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        let next = self.cursor as isize + delta;
        // Clamped rather than wrapped: a picker that wraps from the last row to
        // the first makes "how many rows down am I" unanswerable, and the count is
        // the only thing telling the cursor apart from a static list.
        self.cursor = next.clamp(0, last) as usize;
    }

    pub fn toggle_current(&mut self) {
        if let Some(row) = self.rows.get_mut(self.cursor) {
            row.toggle();
        }
    }

    /// The ids to write back to the agent's `skills` list.
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

/// What the skill manager knows about a skill, without its body or its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub scope: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(id: &str, scope: &str) -> InstalledSkill {
        InstalledSkill {
            id: id.to_string(),
            name: id.to_string(),
            description: format!("{id} does something"),
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
}
