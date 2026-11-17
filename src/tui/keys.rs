//! The screens, the keys they answer to, and the footer's hints.
//!
//! Everything here moved out of `app.rs` unchanged: `TuiApp::run` keeps the
//! event loop and the key-dispatch arms, and this module owns what a key
//! means for each screen. `key_target` resolves a screen to a `KeyTarget`,
//! and `hints_for` is the one place a key and its meaning are written down.

use crate::tui::views::projects::ListBounds;
use crate::tui::views::rule_editor::RuleEditor;
use crossterm::event::{self, KeyCode};
use std::time::Instant;

/// The skills field in the agent form, by its index in the form's field table.
/// Named because the picker key and the summary both key off it, and a bare `6`
/// would be the kind of thing a reordered table changes silently.
pub(super) const SKILLS_FIELD: usize = 6;

/// The rules field, by its index in the form's field table.
///
/// Named for the same reason as `SKILLS_FIELD`: `Ctrl+R` opens the rule editor
/// over this field, and a bare `4` would be the kind of thing a reordered table
/// changes silently.
pub(super) const RULES_FIELD: usize = 4;

/// The optimization-tools field, by its index in the form's field table.
///
/// `Ctrl+O` opens the tool picker over it, so the same reasoning as `RULES_FIELD`
/// applies: a bare `5` would be the kind of thing a reordered table changes
/// silently.
pub(super) const OPTIMIZE_FIELD: usize = 5;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum CurrentView {
    Dashboard,
    Projects,
    AgentForm,
    Analysis,
    ContextView,
    Chat,
}

impl CurrentView {
    /// Position of this view in the context bar's view list, in declaration
    /// order. Must stay within `VIEW_NAMES`.
    pub(super) fn index(&self) -> usize {
        match self {
            CurrentView::Dashboard => 0,
            CurrentView::Projects => 1,
            CurrentView::AgentForm => 2,
            CurrentView::Analysis => 3,
            CurrentView::ContextView => 4,
            CurrentView::Chat => 5,
        }
    }
}

/// Names the context bar shows, in the same order as [`CurrentView::index`].
pub(super) const VIEW_NAMES: [&str; 6] = [
    "Dashboard",
    "Projects",
    "Agent",
    "Analysis",
    "Context",
    "Chat",
];

/// Which keys the shared footer describes.
///
/// The `CurrentView` alone is not enough: three screens answer to keys that
/// belong to no view of their own — the agent list inside the projects view, the
/// proposal modal inside the analysis view, and the delete confirmation, which is
/// not a view at all. `hints_for` is keyed on `CurrentView`, so without these it
/// cannot see any of them, which is how the modals lost the only mention of the
/// keys that answer them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyTarget {
    /// The view's own body.
    View(CurrentView),
    /// The selected project's agent list, inside the projects view.
    ProjectAgents,
    /// Skill proposals awaiting a decision, inside the analysis view.
    Proposals,
    /// The delete confirmation, drawn over the projects view.
    DeleteConfirm,
    /// The skill picker, drawn over the agent form.
    ///
    /// Another screen whose keys belong to no `CurrentView`: `Space` toggles a row
    /// and means nothing anywhere else, so without this arm the form's footer
    /// would keep naming `Enter: next field` while the picker is up.
    SkillPicker,
    /// The rule editor, drawn over the agent form.
    ///
    /// A fifth screen whose keys belong to no `CurrentView`. Unlike the picker,
    /// every unshifted key here types — `Space` is a character and `a`, `d`
    /// and `n` are letters — so every action in it is a modified key.
    RuleEditor,
    /// The optimization-tool picker, drawn over the agent form.
    ///
    /// A sixth screen, and the only one that is *not* also a form field: it has no
    /// caret and every key does one of three things.
    OptTools,
    /// The agent form, at a given field. Two of its keys mean different things
    /// field by field, so the field index is part of what the row says.
    AgentForm(usize),
}

#[derive(Debug, Clone)]
pub enum AnalysisState {
    Idle,
    Loading {
        started_at: Instant,
    },
    Loaded(String),
    Error(String),
    /// Skills the analysis proposed, awaiting the user's decision.
    ///
    /// Nothing is written until a proposal is approved here.
    Proposals {
        project_path: String,
        proposals: Vec<crate::services::skill_generation::SkillProposal>,
        selected: usize,
        /// One line per proposal: what was written, or why it was refused.
        results: Vec<String>,
    },
}

/// Which keys the footer shows for the screen about to be drawn.
///
/// A free function rather than a method so its inputs can be named and tested
/// without an `AppState`: nothing here reads app state beyond the view and the
/// three facts about it the view alone cannot supply.
///
/// **Every axis, never a subset.** Each one is a screen whose keys belong to no
/// `CurrentView`, and each was added after the footer was found naming keys
/// that did something else entirely. `Esc` out of the proposal modal returns to
/// the projects list and leaves `analysis_state` on `Proposals` — nothing else
/// writes it — so a check on the state alone would keep the modal's `Enter:
/// approve  a: approve all  d: discard` on the projects footer, where `a`
/// re-triggers an analysis and `d` opens the delete confirmation. The delete
/// confirmation is the same story with no view to go back to: it takes *every*
/// key while it is up, so the row underneath names keys that do nothing.
/// Everything the footer row depends on that is not the `CurrentView` alone.
///
/// Seven positional `bool`/`usize` arguments was what this replaces. The problem
/// was never the count — it was that `key_target(form, true, &idle, drawn, 0,
/// false, false)` reads as a row of flags with no way to tell two `false`s apart,
/// and swapping two of them compiles and silently changes which screen owns the
/// keys. Named fields cannot be swapped by accident, and a new axis is one field
/// here rather than one argument at every call site.
#[derive(Clone)]
pub(super) struct ScreenState<'a> {
    pub(super) view: &'a CurrentView,
    /// Whether focus is on the project list rather than its agent list.
    pub(super) active_in_project_list: bool,
    pub(super) analysis: &'a AnalysisState,
    /// What the projects view actually drew last frame. A key may only act on a
    /// row that was on screen.
    pub(super) projects_bounds: ListBounds,
    pub(super) form_field_index: usize,
    /// The delete confirmation, up over the projects view.
    pub(super) confirm_delete: bool,
    /// The skill picker, up over the agent form.
    pub(super) skill_picker: bool,
    /// The rule editor, up over the agent form.
    pub(super) rule_editor: bool,
    /// The optimization-tool picker, up over the agent form.
    pub(super) opt_tools: bool,
}

pub(super) fn key_target(state: &ScreenState<'_>) -> KeyTarget {
    // Checked first because it outranks every axis below it: the handler in the
    // event loop takes every key while the confirmation is up and `continue`s, so
    // no keypress reaches the screen any other target here describes.
    if state.confirm_delete {
        return KeyTarget::DeleteConfirm;
    }
    // Same story as the confirmation: the picker takes every key while it is up,
    // and the form underneath it is still the current view, so without this the
    // form's own row keeps naming `Enter: next field` under a list where `Enter`
    // confirms.
    if state.skill_picker {
        return KeyTarget::SkillPicker;
    }
    // The rule editor opens from the rules field like the picker does from the
    // skills one, and the same reasoning applies: while it is up the form is still
    // the current view, so its own row would name `Enter: next field` under an
    // editor where `Enter` writes a newline.
    if state.rule_editor {
        return KeyTarget::RuleEditor;
    }
    if state.opt_tools {
        return KeyTarget::OptTools;
    }
    if matches!(state.view, CurrentView::Analysis)
        && matches!(state.analysis, AnalysisState::Proposals { .. })
    {
        return KeyTarget::Proposals;
    }
    // The agent list's keys, and only when the agent list has a row to aim at.
    // `ProjectAgents` is reached by focusing it, and focus can be on it with
    // nothing in it — a project with no agents, or a body too short for the list
    // after the rule and the label. Naming `e: edit  d: delete` over an empty list
    // is the same invisible state the keys themselves refuse: the labels and the
    // behaviour have to agree, or the footer is advertising a lie.
    if matches!(state.view, CurrentView::Projects)
        && !state.active_in_project_list
        && state.projects_bounds.agent_rows > 0
    {
        return KeyTarget::ProjectAgents;
    }
    // The form's own keys are not one set: `Enter` and the arrows mean different
    // things on different fields, so the field index is part of the target rather
    // than a detail the handler keeps to itself.
    if matches!(state.view, CurrentView::AgentForm) {
        return KeyTarget::AgentForm(state.form_field_index);
    }
    KeyTarget::View(state.view.clone())
}

/// Whether a key aimed at the agent list may act, given what the last frame drew.
///
/// With a body too short for the list — three rows, where the rule and the label
/// take two — there is no row to act on, and acting on `agent_index` anyway would
/// mean editing an agent nobody can see. The four bindings consult this; it is a
/// free function over the bounds so a test can reach it, because a predicate that
/// only exists inside the event loop is a predicate with no test.
pub(super) fn agent_list_is_drawn(bounds: ListBounds) -> bool {
    bounds.agent_rows > 0
}

/// The agent `e` and `d` should act on, or `None` when there is nothing to act on.
///
/// Two ways to get `None`, and both mean the same thing to the user: the list was
/// not drawn, or `agent_index` names no agent. Either way the key would be
/// operating on a row that is not on screen.
pub(super) fn selected_agent(
    bounds: ListBounds,
    agents: &[crate::domain::agent::Agent],
    agent_index: usize,
) -> Option<&crate::domain::agent::Agent> {
    if !agent_list_is_drawn(bounds) {
        return None;
    }
    agents.get(agent_index)
}

/// Where `↑` or `↓` leaves the selection in the agent list.
///
/// Wraps, as it always has, and leaves it alone when the list was not drawn: there
/// is no row to move to, and a selection that moves while nothing moves on screen
/// is the invisible state this whole arrangement exists to prevent.
pub(super) fn move_agent_selection(
    bounds: ListBounds,
    current: usize,
    count: usize,
    delta: isize,
) -> usize {
    if !agent_list_is_drawn(bounds) || count == 0 {
        return current;
    }
    let next = (current as isize + delta).rem_euclid(count as isize);
    next as usize
}

/// Whether `←`/`→` may move focus, given which list the focus is on now.
///
/// The parameter is the focus's *current* state rather than the direction of the
/// move, so the call site passes `self.active_in_project_list` and reads as the
/// fact it is — no negation for the reader to get wrong.
///
/// **Direction matters, and the two directions are not symmetric.**
///
/// Moving *onto* the agent list needs a row in it — **not because of the footer.**
/// `key_target` re-checks `agent_rows > 0` when it resolves the target, so the
/// row would not relabel itself for a list that is not there even without this
/// gate. What the gate is for is the state: focus can only land on a list that
/// has a row, which is what makes "a row wears `active` wherever the focus is"
/// true. Move onto an empty list and `↑/↓` reaches [`move_agent_selection`],
/// which refuses for the same reason — so the footer would read `↑/↓: select`
/// over a list that selects nothing, and no row anywhere would wear `active`.
///
/// Moving *off* it never needs anything, because that is the only way back.
/// The agent list can still empty while the focus is on it: the focus flag
/// survives a view switch and a resize, and the rows are recomputed every frame,
/// so a body too short for the list after the rule and the label — or an agent
/// removed underneath the user — leaves the focus on a list with nothing in it.
/// An undirected `agent_rows > 0` would then refuse the exit, and since this key
/// is the only writer of the focus flag and no view switch resets it, project
/// selection, project delete and the way back would be gone for the life of the
/// process. Landing on the project list instead costs the user nothing: it is a
/// selection they can still see and act on with `a`, `n` and `d`.
pub(super) fn may_move_list_focus(bounds: ListBounds, focus_is_on_the_project_list: bool) -> bool {
    if focus_is_on_the_project_list {
        agent_list_is_drawn(bounds)
    } else {
        true
    }
}

/// Where the projects view's focus goes after its selected agent is deleted.
///
/// Back on the project list, always. The agent list has just lost a row and, at
/// one agent, has none — and the focus flag is what both the highlighted row and
/// the footer's labels follow. Left on the agent list, `key_target` falls through
/// to the projects row because `agent_rows == 0`, so the footer names
/// `←/→: agents` while the focus is on the projects, `↑/↓` drives a selection
/// nobody can see, `d` falls through to the project branch and is refused by
/// [`selected_agent`], and no row wears `active` at all: three of seven labels
/// wrong and nothing highlighted. Going back to the projects makes every label on
/// the row true at once.
///
/// A free function for the reason the three above it are: the event loop is not
/// reachable from a test, so a decision made inside it is a decision with no test.
pub(super) fn focus_after_agent_delete() -> bool {
    true
}

/// What a key press means in the chat input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatAction {
    /// Append this character to the buffer.
    Edit(char),
    /// Take the last character off the buffer.
    Backspace,
    /// Append a newline to the buffer.
    Newline,
    /// Send the buffer.
    Send,
    /// Nothing for the input to do: either the key belongs to navigation and
    /// scrolling, or the input is frozen because a reply is in flight.
    Other,
}

/// Routes a key press to what it means in the chat input.
///
/// `Enter` sends and `Shift+Enter` inserts a newline instead. That
/// distinction is the whole commitment behind the one-line input — without it
/// a multi-line prompt cannot be composed at all — so it lives here, in one
/// testable place, instead of buried in the event loop's match. The caller
/// applies the result and nothing else.
///
/// While `loading` the buffer cannot grow or be sent. `Shift+Enter` is refused
/// with them: a newline in a buffer that cannot otherwise change is not
/// composing anything. Backspace is the exception — it stays available so a
/// typo can still be taken back out of the draft.
pub fn chat_input_action(key: event::KeyEvent, loading: bool) -> ChatAction {
    match key.code {
        KeyCode::Backspace => ChatAction::Backspace,
        _ if loading => ChatAction::Other,
        KeyCode::Char(c) => ChatAction::Edit(c),
        KeyCode::Enter if matches!(key.modifiers, event::KeyModifiers::SHIFT) => {
            ChatAction::Newline
        }
        KeyCode::Enter => ChatAction::Send,
        _ => ChatAction::Other,
    }
}

/// The editor to open for `rules`, with `first` already typed into the focused rule.
///
/// `RuleEditor::new` starts empty and typing creates the first rule, so a character
/// pressed on the form's rules row survives the trip into the dialog rather than
/// being dropped on the way.
pub(super) fn rule_editor_opening(rules: &[String], first: Option<char>) -> RuleEditor {
    let mut editor = RuleEditor::new(rules.to_vec());
    if let Some(c) = first {
        editor.type_char(c);
    }
    editor
}

/// The key hints the shared footer shows.
///
/// This is the single place hint text lives. No `self`: the text depends
/// only on which keys the screen answers to, which keeps it reachable from a
/// test without a terminal or an `AppState`.
pub fn hints_for(target: KeyTarget) -> Vec<(&'static str, &'static str)> {
    match target {
        KeyTarget::View(CurrentView::Dashboard) => {
            vec![("Tab", "next view"), ("↑/↓", "scroll logs"), ("q", "quit")]
        }
        // The project list. `Tab` switches *views*, not lists — it is `←/→`
        // that moves between this list and the agent list below it — so the
        // row says so where the key is named. `n` is the only hint named by
        // one word, and only because adding "agent" to it overflows the row.
        KeyTarget::View(CurrentView::Projects) => vec![
            ("Tab", "view"),
            ("←/→", "agents"),
            ("↑/↓", "select"),
            ("a", "analyze"),
            ("n", "new"),
            ("d", "delete"),
            ("q", "quit"),
        ],
        // The agent list. `e` and `d` only target an agent while this one has
        // focus, so the hints move with the focus rather than being on the
        // screen either way. `n` is here and `a` is not: `n` creates an agent
        // in the project this row is showing, so it belongs to it, while `a`
        // analyses that project and the user has moved off the projects they
        // would be analysing.
        KeyTarget::ProjectAgents => vec![
            ("Tab", "view"),
            ("←/→", "projects"),
            ("↑/↓", "select"),
            ("e", "edit"),
            ("d", "delete"),
            ("n", "new"),
            ("q", "quit"),
        ],
        // The form. Two of its keys change meaning field by field, so the row
        // follows `form_field_index` rather than describing one field and
        // hoping: `Enter` is "next field" on the seven one-line fields and a
        // newline only in the prompt — the one field that can hold more than a
        // line, where a user who believed it always advanced would never write
        // a second one. The arrows only cycle the two fields that are not typed
        // into (the model and the project), which is the only way to change
        // them; anywhere else they are consumed by the form's own handler and
        // do nothing. Tab and Shift+Tab share a hint since they are one key's
        // two directions.
        KeyTarget::AgentForm(field) => {
            let prompt = field == crate::tui::views::agent_form::PROMPT_INDEX;
            let cycled = crate::tui::views::agent_form::CYCLES_WITH_ARROWS.contains(&field);
            vec![
                ("Tab/⇧Tab", "field"),
                ("←/→", "cycle"),
                (
                    "Enter",
                    if prompt {
                        "newline in prompt"
                    } else {
                        "next field"
                    },
                ),
                ("Ctrl+S", "save"),
                ("Esc", "back"),
            ]
            .into_iter()
            .filter(|(key, _)| cycled || *key != "←/→")
            // Only on the rules row, and only because the row is a one-line
            // join of a list: elsewhere in the form every key types into the
            // field, so a hint about writing them somewhere else would be a lie
            // about a row that has no such row to go to.
            .chain((field == RULES_FIELD).then_some(("Ctrl+R", "edit rules")))
            // Only on the tools row, for the same reason: the row is a one-line
            // join of a list, and elsewhere every key types into the field.
            .chain((field == OPTIMIZE_FIELD).then_some(("Ctrl+O", "edit tools")))
            .collect()
        }
        KeyTarget::View(CurrentView::Analysis) => {
            vec![("Esc", "back"), ("r", "re-analyze")]
        }
        KeyTarget::View(CurrentView::ContextView) => vec![("Esc", "back"), ("↑/↓", "scroll")],
        // One row, and a hint that does not fit is clipped from the right,
        // which takes `Esc: back` first. Sized to fit 80 columns: `PgUp/PgDn`
        // still scrolls, it is just not advertised — six hints fit, seven
        // do not.
        KeyTarget::View(CurrentView::Chat) => vec![
            ("←/→", "agent"),
            ("Enter", "send"),
            ("Shift+Enter", "newline"),
            ("↑↓", "scroll"),
            ("Tab", "view"),
            ("Esc", "back"),
        ],
        // The modal's own keys. The analysis view's handler is still the one
        // running underneath, so `↑/↓` and `r` answer too — but a proposal
        // list is a queue to clear, not a document to scroll, and `r`
        // would discard what is on screen without asking. Advertising them
        // here would be recommending them; they keep working for anyone who
        // knows them, as `PgUp` does in the chat.
        KeyTarget::Proposals => vec![
            ("Enter", "approve"),
            ("a", "approve all"),
            ("d", "discard"),
            ("Esc", "back"),
        ],
        // The confirmation's own answer set, which is the whole of what it
        // accepts. It is drawn over the projects view and its handler takes
        // *every* key while it is up, so the row underneath was naming seven
        // keys that did nothing — `q: quit` most of all, since the
        // interception is before the quit binding too. Naming what answers
        // the question is what makes the footer and the behaviour agree.
        KeyTarget::DeleteConfirm => vec![("y", "delete"), ("n", "cancel"), ("Esc", "cancel")],
        // Sized to fit 80 columns: a hint that does not fit is clipped from
        // the right, which takes `Esc: cancel` first.
        KeyTarget::SkillPicker => vec![
            ("↑/↓", "move"),
            ("space", "toggle"),
            ("type", "filter"),
            ("enter", "save"),
            ("esc", "cancel"),
        ],
        KeyTarget::RuleEditor => vec![
            ("⇥", "rule"),
            ("⏎", "new line"),
            ("^n", "add rule"),
            ("^d", "delete rule"),
            ("esc", "done"),
        ],
        KeyTarget::OptTools => vec![
            ("↑/↓", "move"),
            ("space", "toggle"),
            ("type", "filter"),
            ("enter", "save"),
            ("esc", "cancel"),
        ],
        // Unreachable through `key_target`, which resolves the form to
        // `AgentForm(field)` above. Kept honest rather than merged into one of
        // the other arms: this is the row that would name a false `Enter` on
        // seven of the eight fields.
        KeyTarget::View(CurrentView::AgentForm) => hints_for(KeyTarget::AgentForm(0)),
    }
}
