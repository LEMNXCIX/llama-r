//! The rule editor's state: an agent's operating rules, one per row, edited as
//! prose.
//!
//! # Why this is not the skill picker with another label
//!
//! The picker earns its shortcuts by choosing a key it does not need: `Space`
//! toggles, and a skill id contains no spaces. A rule is prose, so `Space` is the
//! most common character in it and `a`/`d` are among the most common letters in
//! Spanish. Every structural action here therefore takes a modifier, and every
//! other key types. Copying the picker's key map wholesale would have made the
//! rules unwriteable in the user's own language.
//!
//! # Why the rules are a `Vec<String>` and not a joined `String`
//!
//! They always were: `AgentConfig::rules` is a list, and `context/analyzer.rs`
//! renders each element as its own `- ` bullet. The form used to flatten that
//! list into one line joined by commas and split it again on save, which made two
//! rules impossible to express — any comma inside a rule silently became two
//! rules. The join was never the truth; it was a rendering. `form_rules` is now
//! the list, and the single line the form draws is `join(", ")` of it.
//!
//! `Esc` keeps what was typed. There is no cancel, and that is deliberate: the
//! form behind this dialog is discarded whole by its own `Esc`, so a second way
//! to discard half-typed rules would be a rule about rules.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// One rule per row, plus which one is being typed into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEditor {
    pub rules: Vec<String>,
    pub focus: usize,
    /// The first row drawn. The list is short but not bounded, and the focused row
    /// grows downwards, so a long rule near the bottom would otherwise push itself
    /// out of the panel.
    pub scroll: usize,
}

impl Default for RuleEditor {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl RuleEditor {
    pub fn new(rules: Vec<String>) -> Self {
        let mut editor = Self {
            rules: rules.into_iter().filter(|r| !r.trim().is_empty()).collect(),
            focus: 0,
            scroll: 0,
        };
        editor.normalise();
        editor
    }

    /// Put `focus` and `scroll` back into range after the list changed.
    ///
    /// The invariant is `focus` naming a row that exists, or the list being empty.
    /// It is what lets every other method index `rules[focus]` without a check, and
    /// what stops a keystroke landing on a row that is not on screen — the same
    /// failure the picker had to guard against.
    fn normalise(&mut self) {
        if self.rules.is_empty() {
            self.focus = 0;
            self.scroll = 0;
            return;
        }
        let last = self.rules.len() - 1;
        self.focus = self.focus.min(last);
        // The window starts at the focus, which is the only position guaranteed to
        // show the row being typed into — see `move_focus` for why this is not a
        // tighter scroll.
        self.scroll = self.focus;
    }

    /// The rule being typed into, if there is one.
    pub fn focused(&self) -> Option<&String> {
        self.rules.get(self.focus)
    }

    pub fn move_focus(&mut self, delta: isize) {
        if self.rules.is_empty() {
            return;
        }
        // Clamped, like the picker's: a list that wraps makes "which rule am I on"
        // unanswerable, and the count is the only thing separating the caret from a
        // static list.
        let last = self.rules.len() as isize - 1;
        self.focus = (self.focus as isize + delta).clamp(0, last) as usize;
        // The window follows the focus exactly. `move_focus` cannot know how tall
        // the focused rule will be drawn — that depends on the panel's width, which
        // lives in `modals` — so the only window position guaranteed to show the
        // focused rule's first line is the one that starts at it. With a list of a
        // handful of rules the shift is invisible, and being wrong here is the
        // worse trade: the row being typed into is the one that must be readable.
        self.normalise();
    }

    /// Type into the focused rule.
    ///
    /// A rule with no rows cannot be typed into, so typing into an empty list
    /// creates its first rule. Otherwise an editor opened on an agent with no rules
    /// would be a box that swallows every key.
    pub fn type_char(&mut self, c: char) {
        if self.rules.is_empty() {
            self.rules.push(String::new());
        }
        self.rules[self.focus].push(c);
    }

    pub fn backspace(&mut self) {
        if self.rules.is_empty() {
            return;
        }
        self.rules[self.focus].pop();
    }

    /// Newline into the focused rule.
    ///
    /// Not indented: `context/analyzer.rs` renders a rule as `format!("- {rule}")`
    /// and a continuation line without leading spaces is a CommonMark lazy
    /// continuation, so the bullet still reads as one item. Indenting would put
    /// characters into the rule's own text, and a rule is the user's text.
    pub fn newline(&mut self) {
        if self.rules.is_empty() {
            self.rules.push(String::new());
        }
        self.rules[self.focus].push('\n');
    }

    /// Add a rule after the focused one and type into it.
    pub fn insert_rule(&mut self) {
        let at = (self.focus + 1).min(self.rules.len());
        self.rules.insert(at, String::new());
        self.focus = at;
        self.scroll = at.min(self.scroll);
        self.normalise();
    }

    /// Remove the focused rule.
    ///
    /// Deleting the last remaining rule leaves an empty list rather than one
    /// blank row: an agent with no rules is a real state, and a blank row would
    /// save as a rule of nothing.
    pub fn delete_rule(&mut self) {
        if self.rules.is_empty() {
            return;
        }
        self.rules.remove(self.focus);
        self.normalise();
    }

    /// The rules to write back to the agent.
    ///
    /// Empty and whitespace-only rows are dropped: a row the user started and did
    /// not finish is not a rule, and `agent_api.rs` rejects empty entries outright.
    /// The remaining rules are trimmed, which is what the old comma split did.
    pub fn rules_to_save(&self) -> Vec<String> {
        self.rules
            .iter()
            .map(|rule| rule.trim().to_string())
            .filter(|rule| !rule.is_empty())
            .collect()
    }
}

/// What a keystroke asked of the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleAction {
    /// Handled inside the editor; nothing to apply outside it.
    Applied,
    /// Close, keeping the rules.
    Done(Vec<String>),
}

/// Route one keystroke to what it means in the rule editor.
///
/// Lifted out of `TuiApp::run` for the reason `chat_input_action` and
/// `picker_action` are: the event loop reads stdin directly, so a rule that lives
/// only inside its `match` cannot be tested at all. That is how the skill picker
/// shipped inert — its whole key map was unreachable and no test could have said
/// so.
pub fn rule_action(editor: &mut RuleEditor, key: KeyEvent) -> RuleAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter => editor.newline(),
        KeyCode::Backspace => editor.backspace(),
        KeyCode::Up | KeyCode::BackTab => editor.move_focus(-1),
        KeyCode::Down | KeyCode::Tab => editor.move_focus(1),
        KeyCode::Char('n') if ctrl => editor.insert_rule(),
        KeyCode::Char('d') if ctrl => editor.delete_rule(),
        // Two arms, not one: `A | B if ctrl` guards the whole pattern, so
        // `Esc | Char('s') if ctrl` would have made plain `Esc` fall through to the
        // catch-all and silently do nothing. The keys close the editor on their own.
        KeyCode::Esc => return RuleAction::Done(editor.rules_to_save()),
        KeyCode::Char('s') if ctrl => return RuleAction::Done(editor.rules_to_save()),
        KeyCode::Char(c) => editor.type_char(c),
        // Anything else belongs to no rule here and is swallowed: the dialog owns
        // every key while it is up.
        _ => {}
    }
    RuleAction::Applied
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn type_str(editor: &mut RuleEditor, text: &str) {
        for c in text.chars() {
            rule_action(editor, key(KeyCode::Char(c)));
        }
    }

    /// The whole point of the editor: a rule that contains a comma. Under the old
    /// comma-joined field this became two rules the moment the agent was saved,
    /// with nothing on screen saying so.
    #[test]
    fn a_rule_may_contain_a_comma() {
        let mut editor = RuleEditor::default();
        type_str(&mut editor, "usa comas, no puntos");

        assert_eq!(editor.rules, vec!["usa comas, no puntos".to_string()]);

        let RuleAction::Done(saved) = rule_action(&mut editor, key(KeyCode::Esc)) else {
            panic!("Esc closes the editor");
        };
        assert_eq!(
            saved,
            vec!["usa comas, no puntos".to_string()],
            "one rule in, one rule out"
        );
    }

    /// A rule may span lines, which the one-line field made impossible. `Enter`
    /// inserting a newline is also why `Esc` closes rather than `Enter`: the key a
    /// text editor needs for a line break cannot also be the key that saves.
    #[test]
    fn a_rule_may_span_lines() {
        let mut editor = RuleEditor::default();
        type_str(&mut editor, "primera");
        rule_action(&mut editor, key(KeyCode::Enter));
        type_str(&mut editor, "segunda");

        assert_eq!(editor.rules, vec!["primera\nsegunda".to_string()]);
    }

    /// Every unshifted key types. This is the constraint that separates this
    /// dialog from the skill picker, and it is the one a future shortcut would
    /// quietly break: prose in Spanish is dense with `a`, `d`, `n` and `s`.
    #[test]
    fn no_bare_letter_is_stolen_from_the_text() {
        let mut editor = RuleEditor::default();

        for c in "las reglas de siempre dan detalle".chars() {
            rule_action(&mut editor, key(KeyCode::Char(c)));
        }

        assert_eq!(
            editor.rules,
            vec!["las reglas de siempre dan detalle".to_string()],
            "every letter reached the text, including a, d, n and s"
        );
        assert_eq!(editor.rules.len(), 1, "and none of them created a rule");
    }

    /// `Space` types here, where it toggles in the picker. A rule of prose without
    /// spaces is not a rule of prose.
    #[test]
    fn space_is_a_character_and_not_a_command() {
        let mut editor = RuleEditor::default();
        type_str(&mut editor, "usa espacios");

        assert_eq!(editor.rules, vec!["usa espacios".to_string()]);
    }

    /// The structural actions are all modified, so they are unambiguous.
    #[test]
    fn the_structural_keys_all_take_control() {
        let mut editor = RuleEditor::new(vec!["primera".to_string()]);
        type_str(&mut editor, " editada");
        rule_action(&mut editor, ctrl(KeyCode::Char('n')));

        assert_eq!(editor.rules.len(), 2, "Control-N adds a rule");
        type_str(&mut editor, "nueva");
        rule_action(&mut editor, ctrl(KeyCode::Char('d')));

        assert_eq!(
            editor.rules,
            vec!["primera editada".to_string()],
            "Control-D removes the focused one, which is the new empty rule"
        );
    }

    /// Deleting the last rule leaves no rules, not one blank row. An agent with an
    /// empty rule is rejected by `agent_api.rs` and rendered as a bullet with
    /// nothing in it.
    #[test]
    fn deleting_the_last_rule_leaves_no_rules() {
        let mut editor = RuleEditor::new(vec!["única".to_string()]);
        rule_action(&mut editor, ctrl(KeyCode::Char('d')));

        assert!(editor.rules.is_empty(), "no blank row is left behind");

        // And the editor still works afterwards: typing makes the first rule.
        type_str(&mut editor, "otra");
        assert_eq!(editor.rules, vec!["otra".to_string()]);
    }

    /// A rule the user started and did not finish is not a rule.
    #[test]
    fn an_unfinished_row_is_not_saved() {
        let mut editor = RuleEditor::new(vec!["buena".to_string()]);
        rule_action(&mut editor, ctrl(KeyCode::Char('n')));
        rule_action(&mut editor, key(KeyCode::Char(' ')));
        rule_action(&mut editor, key(KeyCode::Char(' ')));

        let RuleAction::Done(saved) = rule_action(&mut editor, key(KeyCode::Esc)) else {
            panic!("Esc closes the editor");
        };
        assert_eq!(saved, vec!["buena".to_string()], "the blank row is dropped");
    }

    /// Focus clamps, and the panel follows it.
    #[test]
    fn focus_clamps_at_both_ends() {
        let mut editor = RuleEditor::new(vec!["a".to_string(), "b".to_string()]);

        rule_action(&mut editor, key(KeyCode::Up));
        assert_eq!(editor.focus, 0, "up from the first stays put");

        rule_action(&mut editor, key(KeyCode::Down));
        rule_action(&mut editor, key(KeyCode::Down));
        assert_eq!(editor.focus, 1, "down past the last stays put");
    }

    /// The focused row grows, so a long rule low in the list has to bring itself
    /// into view rather than expanding off the bottom of the panel.
    #[test]
    fn the_focused_row_stays_in_the_window() {
        let editor = RuleEditor::new((0..5).map(|i| format!("regla {i}")).collect());

        let mut scrolled = editor.clone();
        for _ in 0..4 {
            scrolled.move_focus(1);
        }
        assert_eq!(scrolled.focus, 4);
        assert_eq!(
            scrolled.scroll, 4,
            "the window followed the focus downwards"
        );

        let mut back = scrolled.clone();
        for _ in 0..4 {
            back.move_focus(-1);
        }
        assert_eq!(back.scroll, 0, "and back up again");
    }

    /// `Ctrl+S` closes as well as `Esc`, because `Ctrl+S` is what the form it sits
    /// on top of already means by "save" — and both keep what was typed.
    #[test]
    fn both_ways_out_keep_the_rules() {
        for closer in [key(KeyCode::Esc), ctrl(KeyCode::Char('s'))] {
            let mut editor = RuleEditor::default();
            type_str(&mut editor, "escrita");
            assert_eq!(
                rule_action(&mut editor, closer),
                RuleAction::Done(vec!["escrita".to_string()])
            );
        }
    }

    /// A bare `s` is an `s`, not a save. If it closed the dialog, "responde siempre
    /// en español" would have ended the sentence.
    #[test]
    fn a_bare_s_is_not_the_save_key() {
        let mut editor = RuleEditor::default();

        assert_eq!(
            rule_action(&mut editor, key(KeyCode::Char('s'))),
            RuleAction::Applied,
            "a bare `s` is a letter, not a save"
        );
        assert_eq!(editor.rules, vec!["s".to_string()]);

        // Two in a row, so "s" is a word of its own rather than the tail of one.
        type_str(&mut editor, "sí");
        assert_eq!(editor.rules, vec!["ssí".to_string()]);
    }

    /// Opening the editor on an agent that has rules shows them; opening it on one
    /// with none starts from nothing rather than from one blank row.
    #[test]
    fn it_opens_on_what_the_agent_already_has() {
        let editor = RuleEditor::new(vec!["primera".into(), "segunda".into()]);
        assert_eq!(editor.rules.len(), 2);

        let empty = RuleEditor::new(Vec::new());
        assert!(
            empty.rules.is_empty(),
            "no blank row to type into or delete"
        );
    }

    /// A whole session through the public key map only: two rules, a newline in the
    /// first, a comma in the second, one added and then deleted.
    ///
    /// Every other test here checks one method and one fact. This drives the order
    /// the user actually types in, because the bug that shipped was never a wrong
    /// fact about a method — it was which method the keystroke reached.
    #[test]
    fn a_whole_editing_session_behaves() {
        let mut editor = RuleEditor::default();

        type_str(&mut editor, "usa comas,");
        rule_action(&mut editor, key(KeyCode::Enter));
        type_str(&mut editor, "no puntos");
        rule_action(&mut editor, ctrl(KeyCode::Char('n')));

        type_str(&mut editor, "cita la fuente");
        rule_action(&mut editor, key(KeyCode::Down));
        rule_action(&mut editor, ctrl(KeyCode::Char('d')));

        assert_eq!(editor.rules.len(), 1);
        assert_eq!(
            rule_action(&mut editor, key(KeyCode::Esc)),
            RuleAction::Done(vec!["usa comas,\nno puntos".to_string()])
        );
    }
}
