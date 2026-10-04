//! The optimization-tool picker's state: which of the built-in optimizations an
//! agent runs.
//!
//! This replaces a free-text field that took comma-separated names, and it removed
//! the same failure the skills field had: a name that matched no tool was a silent
//! no-op. Here the catalog is [`crate::optimizer::tools::TOOLS`], so the picker can
//! only ever offer what exists, and a name already in a TOML that no longer matches
//! is reported when the agent loads instead of quietly doing nothing.
//!
//! Unlike the rule editor, `Space` is the toggle and letters filter. The entries
//! are ids, not prose — `space` is in `compress_code` — so there is nothing here
//! worth protecting a letter for.
//!
//! The panel is drawn by `modals::render_opt_tools`, for the reason every dialog's
//! is: a border belongs to the module that holds the other dialogs.

use ratatui::crossterm::event::KeyCode;

use crate::optimizer::tools::{is_known, OptTool, TOOLS};

/// One selectable optimization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRow {
    pub id: String,
    pub summary: String,
    pub picked: bool,
    /// In the agent's `optimize.rules`, but not a tool this build knows.
    ///
    /// Shown, and not toggleable, for the same reason `auto` skills are: it is in
    /// the configuration, it is being ignored, and an agent that silently carries
    /// one is worse than one that says so.
    pub unknown: bool,
}

/// The picker's state, alive for as long as the modal is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptTools {
    pub rows: Vec<ToolRow>,
    pub cursor: usize,
    pub filter: String,
}

impl OptTools {
    /// The rows, from the catalog plus anything already configured.
    pub fn for_agent(configured: &[String]) -> Self {
        let rows = TOOLS
            .iter()
            .map(|tool: &OptTool| ToolRow {
                id: tool.id.to_string(),
                summary: tool.summary.to_string(),
                picked: configured.iter().any(|c| c == tool.id),
                unknown: false,
            })
            .chain(
                configured
                    .iter()
                    .filter(|c| !is_known(c))
                    .map(|id| ToolRow {
                        id: id.clone(),
                        summary: "not a tool in this build".to_string(),
                        picked: true,
                        unknown: true,
                    }),
            )
            .collect();
        let mut picker = Self {
            rows,
            cursor: 0,
            filter: String::new(),
        };
        picker.reanchor();
        picker
    }

    /// Indices of the rows the filter leaves on screen.
    pub fn visible(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.rows.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row.id.to_lowercase().contains(&needle)
                    || row.summary.to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn reanchor(&mut self) {
        let visible = self.visible();
        if !visible.is_empty() && !visible.contains(&self.cursor) {
            self.cursor = visible[0];
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let Some(position) = visible.iter().position(|&index| index == self.cursor) else {
            return;
        };
        let last = visible.len() as isize - 1;
        self.cursor = visible[(position as isize + delta).clamp(0, last) as usize];
    }

    pub fn toggle_current(&mut self) {
        if !self.visible().contains(&self.cursor) {
            return;
        }
        let Some(row) = self.rows.get_mut(self.cursor) else {
            return;
        };
        // An unknown name is already in the configuration and is being ignored.
        // Un-ticking it is the one thing a user can usefully do about it, so unlike
        // an `auto` skill this one *is* removable.
        row.picked = !row.picked;
    }

    /// The tool names to write back.
    ///
    /// Unknown names are dropped rather than carried: the catalog is what this build
    /// can run, and writing back a name it cannot run would restore the silent
    /// no-op the picker exists to remove. They are shown before that happens.
    pub fn chosen_ids(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|row| row.picked && !row.unknown)
            .map(|row| row.id.clone())
            .collect()
    }

    /// Whether the agent should have `optimize.enabled`.
    ///
    /// Derived rather than asked for: the form wrote `enabled = true` whether or
    /// not anything was selected, which produced a config that claimed to be
    /// optimising while doing nothing at all.
    pub fn enabled(&self) -> bool {
        !self.chosen_ids().is_empty()
    }

    pub fn push_filter_char(&mut self, c: char) {
        self.filter.push(c.to_ascii_lowercase());
        self.reanchor();
    }

    pub fn pop_filter_char(&mut self) {
        self.filter.pop();
        self.reanchor();
    }
}

/// What a keystroke asked of the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptToolsAction {
    Applied,
    /// Confirm: these tools, and whether the optimizer runs at all.
    Save {
        tools: Vec<String>,
        enabled: bool,
    },
    Cancel,
}

/// Route one keystroke.
///
/// `Space` toggles and every other character filters, as in the skill picker: the
/// rows are ids, so a sacrificed space costs nothing.
pub fn opt_tools_action(picker: &mut OptTools, code: KeyCode) -> OptToolsAction {
    match code {
        KeyCode::Down => picker.move_cursor(1),
        KeyCode::Up => picker.move_cursor(-1),
        KeyCode::Char(' ') => picker.toggle_current(),
        KeyCode::Char(c) => picker.push_filter_char(c),
        KeyCode::Backspace => picker.pop_filter_char(),
        KeyCode::Enter => {
            return OptToolsAction::Save {
                tools: picker.chosen_ids(),
                enabled: picker.enabled(),
            }
        }
        KeyCode::Esc => return OptToolsAction::Cancel,
        _ => {}
    }
    OptToolsAction::Applied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_is_what_it_offers() {
        let picker = OptTools::for_agent(&[]);

        assert_eq!(picker.rows.len(), TOOLS.len());
        for (row, tool) in picker.rows.iter().zip(TOOLS) {
            assert_eq!(row.id, tool.id);
            assert_eq!(
                row.summary, tool.summary,
                "{} is listed without saying what it does",
                tool.id
            );
        }
    }

    /// The regression: a name that matches no tool used to be stored and ignored.
    /// It has to be *shown*, and saying so is the only way the user finds out.
    #[test]
    fn a_configured_name_that_is_not_a_tool_is_shown_and_marked() {
        let picker =
            OptTools::for_agent(&["compress_code".to_string(), "compres_code".to_string()]);

        let unknown = picker
            .rows
            .iter()
            .find(|row| row.id == "compres_code")
            .expect("the typo must be on screen");
        assert!(unknown.unknown);
        assert!(
            unknown.summary.contains("not a tool"),
            "and it must say why: {:?}",
            unknown.summary
        );
    }

    /// It is dropped on save, not carried: writing back a name this build cannot run
    /// would restore exactly the silent no-op being removed.
    #[test]
    fn saving_drops_the_names_it_cannot_run() {
        let picker = OptTools::for_agent(&["compres_code".to_string()]);

        assert!(picker.chosen_ids().is_empty());
        assert!(
            !picker.enabled(),
            "and nothing to run means nothing enabled"
        );
    }

    /// What the user can do about an unknown name is delete it, which is why this
    /// row is toggleable where an `auto` skill is not.
    #[test]
    fn an_unknown_row_can_be_ticked_off() {
        let mut picker = OptTools::for_agent(&["compres_code".to_string()]);
        picker.cursor = picker.rows.len() - 1;
        assert!(picker.rows[picker.cursor].unknown, "precondition");

        picker.toggle_current();

        assert!(!picker.rows[picker.cursor].picked, "the user can remove it");
    }

    #[test]
    fn what_the_agent_has_comes_back_ticked() {
        let picker = OptTools::for_agent(&["strip_ansi".to_string(), "compress_code".to_string()]);

        let chosen = picker.chosen_ids();
        assert!(chosen.contains(&"strip_ansi".to_string()));
        assert!(chosen.contains(&"compress_code".to_string()));
        assert!(picker.enabled());
    }

    /// `enabled` follows the selection, so the config can no longer claim to be
    /// optimising while the list is empty.
    #[test]
    fn enabled_follows_the_selection() {
        let empty = OptTools::for_agent(&["compres_code".to_string()]);
        assert!(
            !empty.enabled(),
            "a list of names it cannot run is not enabled"
        );

        let mut chosen = OptTools::for_agent(&[]);
        chosen.toggle_current();
        assert!(chosen.enabled());
        assert_eq!(chosen.chosen_ids().len(), 1);

        chosen.toggle_current();
        assert!(
            !chosen.enabled(),
            "and unticking the last one turns it off again"
        );
    }

    #[test]
    fn filtering_narrows_by_id_or_summary() {
        let mut picker = OptTools::for_agent(&[]);
        let total = picker.rows.len();

        for c in "minify".chars() {
            picker.push_filter_char(c);
        }
        let json = TOOLS.iter().position(|t| t.id == "minify_json").unwrap();
        assert_eq!(picker.visible(), vec![json], "by id prefix");

        while !picker.filter.is_empty() {
            picker.pop_filter_char();
        }
        assert_eq!(picker.visible().len(), total);

        // A word from a summary, which no id contains.
        for c in "terminal".chars() {
            picker.push_filter_char(c);
        }
        let shown = picker.visible();
        assert_eq!(shown.len(), 1, "only strip_ansi mentions terminals");
        assert_eq!(picker.rows[shown[0]].id, "strip_ansi");
    }

    /// Filtering changes the screen and nothing else, as in the skill picker: a tool
    /// ticked before a needle hid it is still ticked after.
    #[test]
    fn filtering_never_touches_the_selection() {
        let mut picker = OptTools::for_agent(&[]);
        picker.toggle_current();
        let before = picker.chosen_ids();
        assert_eq!(before, vec!["compress_code".to_string()], "precondition");

        // "removes" is in the summaries of strip_ansi and dedent_code and in
        // neither of the other three, so the pick above is hidden while two rows
        // stay — a needle leaving one row would prove nothing, because the cursor
        // would have nowhere to move to.
        for c in "removes".chars() {
            picker.push_filter_char(c);
        }
        assert_eq!(
            picker.visible().len(),
            2,
            "two rows, neither of them the pick"
        );

        picker.move_cursor(1);
        picker.toggle_current();

        assert_eq!(
            before,
            picker.chosen_ids()[..1],
            "the pick made before the filter is still the first one",
        );
        assert_eq!(
            picker.chosen_ids().len(),
            2,
            "and one more was added after it"
        );
    }

    #[test]
    fn the_cursor_never_sits_on_a_hidden_row() {
        let mut picker = OptTools::for_agent(&[]);
        picker.move_cursor(2);
        assert!(picker.visible().contains(&picker.cursor));

        for c in "strip".chars() {
            picker.push_filter_char(c);
        }
        assert!(
            picker.visible().contains(&picker.cursor),
            "pulled onto something still on screen"
        );

        picker.move_cursor(9);
        picker.move_cursor(-9);
        assert!(picker.visible().contains(&picker.cursor), "moves clamp");
    }

    /// Every tool id has to be reachable by typing, or the filter hides it and the
    /// only way to pick it is the arrow keys.
    #[test]
    fn every_tool_is_findable_by_its_own_name() {
        for tool in TOOLS {
            let mut picker = OptTools::for_agent(&[]);
            for c in tool.id.chars() {
                picker.push_filter_char(c);
            }
            assert_eq!(
                picker.visible(),
                vec![TOOLS.iter().position(|t| t.id == tool.id).unwrap()],
                "{} is not findable by typing its name",
                tool.id
            );
        }
    }

    /// The whole key map in order. Every other test here calls one method; this one
    /// drives what a user types, because the failure this replaces was not a wrong
    /// fact about a method but which method the keystroke reached.
    #[test]
    fn a_whole_picking_session_behaves() {
        let mut picker = OptTools::for_agent(&["strip_ansi".to_string()]);

        // Filter to one, tick it, keep the pre-existing pick.
        for c in "minify".chars() {
            picker.push_filter_char(c);
        }
        assert_eq!(picker.visible().len(), 1);
        picker.toggle_current();
        assert_eq!(picker.chosen_ids().len(), 2);

        // Widen back; both picks are still there.
        while !picker.filter.is_empty() {
            picker.pop_filter_char();
        }
        assert_eq!(picker.chosen_ids().len(), 2);

        assert_eq!(
            opt_tools_action(&mut picker, KeyCode::Enter),
            OptToolsAction::Save {
                tools: picker.chosen_ids(),
                enabled: true
            }
        );
    }

    #[test]
    fn enter_saves_and_esc_cancels() {
        let mut picker = OptTools::for_agent(&["strip_ansi".to_string()]);
        assert_eq!(
            opt_tools_action(&mut picker, KeyCode::Esc),
            OptToolsAction::Cancel
        );

        picker.toggle_current();
        let OptToolsAction::Save { tools, enabled } = opt_tools_action(&mut picker, KeyCode::Enter)
        else {
            panic!("Enter saves");
        };
        assert!(enabled);
        assert!(tools.contains(&"strip_ansi".to_string()));
    }
}
