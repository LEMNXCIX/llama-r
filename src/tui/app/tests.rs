// Test code only: the attribute below is the cut marker the source guards in
// `tests/tui_chrome.rs` look for in every `.rs` file under `src/tui` —
// `shipped()` keeps everything before the first one. Without it high on this
// file, the guards would count this module's own `render_footer` calls as
// shipped code.
#[cfg(test)]
use super::*;
use crate::tui::keys::{agent_list_is_drawn, KeyTarget};
use crossterm::event::{KeyEventKind, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::text::Line;

fn all_views() -> [CurrentView; 6] {
    [
        CurrentView::Dashboard,
        CurrentView::Projects,
        CurrentView::AgentForm,
        CurrentView::Analysis,
        CurrentView::ContextView,
        CurrentView::Chat,
    ]
}

/// A screen with no dialog over it and the form's caret at field 0.
///
/// Every screen below is one of those, and naming the two extra axes here
/// rather than at eight call sites is what keeps the argument list honest: a
/// test that wants the confirmation up, or a field, builds its own
/// [`ScreenState`] and mutates the one field it cares about — which cannot be
/// done by accident, where two positional `false`s could be swapped.
fn screen(view: &CurrentView, focus_on_projects: bool, bounds: ListBounds) -> ScreenState<'_> {
    ScreenState {
        view,
        active_in_project_list: focus_on_projects,
        analysis: &AnalysisState::Idle,
        projects_bounds: bounds,
        form_field_index: 0,
        confirm_delete: false,
        skill_picker: false,
        rule_editor: false,
        opt_tools: false,
    }
}

/// `key_target` for a screen with no dialog over it.
fn keys_for(
    view: &CurrentView,
    active_in_project_list: bool,
    analysis: &AnalysisState,
    projects_bounds: ListBounds,
) -> KeyTarget {
    key_target(&ScreenState {
        analysis,
        ..screen(view, active_in_project_list, projects_bounds)
    })
}

/// Every footer fits one row and loses nothing to the clip.
///
/// `render_footer` has no truncation logic: it hands the line to `Paragraph`,
/// which clips at the area width with no ellipsis, so a hint pushed past 80
/// columns simply stops being drawn — no marker, no second line. The
/// consequence is that a row which overflows loses its *tail*, which on every
/// one of these screens is `q: quit` or `Esc: back`: the keys nobody can do
/// without. So the pin is the rendered row compared against the hint text,
/// which fails the moment one hint is cut. A `width <= 80` check alone is
/// necessary but not sufficient, and measuring the row's own width proves
/// nothing at all — the buffer is 80 columns wide whatever was written into
/// it.
#[test]
fn every_footer_fits_one_row_at_eighty_columns() {
    // Every `KeyTarget` there is, and the form is listed at both of its field
    // shapes — the two rows are different lengths, and a list that only had
    // the longer one would leave the shorter untested.
    let mut targets = vec![
        KeyTarget::View(CurrentView::Dashboard),
        KeyTarget::View(CurrentView::Projects),
        KeyTarget::ProjectAgents,
        KeyTarget::View(CurrentView::Analysis),
        KeyTarget::Proposals,
        KeyTarget::View(CurrentView::ContextView),
        KeyTarget::View(CurrentView::Chat),
        KeyTarget::DeleteConfirm,
    ];
    for field in 0..=crate::tui::views::agent_form::PROMPT_INDEX {
        targets.push(KeyTarget::AgentForm(field));
    }
    for target in targets {
        let hints = hints_for(target.clone());
        let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|f| chrome::render_footer(f, f.area(), &hints))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect();

        // Measured from the hints, not from the row: the row is 80 columns
        // wide whatever was written into it, so measuring it there proves
        // nothing.
        let width = hints
            .iter()
            .map(|(key, hint)| Line::from(format!("{key}: {hint}")).width() + 2)
            .sum::<usize>()
            - 2;
        assert!(
            width <= 80,
            "the {target:?} hints are {width} columns wide and get clipped"
        );
        assert_eq!(
            row.trim_end(),
            hints
                .iter()
                .map(|(key, hint)| format!("{key}: {hint}"))
                .collect::<Vec<_>>()
                .join("  "),
            "every {target:?} hint must survive the 80-column row"
        );
        println!("{target:?} hints render {width} columns of the 80 available");
    }
}

/// Rendered `target`'s footer row at 80 columns.
fn footer_row(target: KeyTarget) -> String {
    let hints = hints_for(target);
    let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
    terminal
        .draw(|f| chrome::render_footer(f, f.area(), &hints))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.width)
        .map(|x| buffer[(x, 0)].symbol().to_string())
        .collect()
}

/// The scope the bar renders for these three selections.
fn bar_label(
    projects_selection: Option<&str>,
    agent: Option<&str>,
    agent_project: Option<&str>,
) -> String {
    chrome::context_label(&bar_context(
        projects_selection.map(str::to_string),
        agent.map(str::to_string),
        agent_project.map(str::to_string),
    ))
}

/// **The bar may not name a pair that does not exist.** An agent in scope
/// brings its own project with it, whatever the projects view happens to have
/// selected — the two are unrelated state, and reading one half from each
/// produced `clinica/nutricion`: an agent of one project under another's
/// name. Two projects can register an agent with the same id, which is why
/// the project half is there at all, so a bar that gets it wrong is worse
/// than the ambiguity it exists to remove.
#[test]
fn the_bar_pairs_an_agent_with_its_own_project() {
    // The repro: chat, `→` to `fudi`'s `nutricion`, then Tab to the projects
    // view and move its selection to `clinica`. Nothing about the agent
    // changes, so nothing about its project may change either.
    assert_eq!(
        bar_label(Some("clinica"), Some("nutricion"), Some("fudi")),
        "fudi/nutricion",
        "the agent's own project is the one that names it, not the projects view's selection"
    );
    // And the move in the other direction, which is the same pair.
    assert_eq!(
        bar_label(Some("fudi"), Some("nutricion"), Some("fudi")),
        "fudi/nutricion",
        "a selection that agrees changes nothing about the reading"
    );
    // Two projects, two agents with the same id: the pair is what tells them
    // apart, so the labels must differ.
    assert_ne!(
        bar_label(Some("clinica"), Some("soporte"), Some("fudi")),
        bar_label(Some("clinica"), Some("soporte"), Some("clinica")),
        "two projects with an agent of the same id must not render the same"
    );
}

/// The other three readings, so the pairing rule above is not a rule that
/// happens to hold for one case: a global agent has no project to name, and
/// with no agent in scope the projects view's own selection is the scope.
#[test]
fn the_bar_names_a_global_agent_without_borrowing_a_project() {
    assert_eq!(
        bar_label(Some("clinica"), Some("notificador"), None),
        "notificador",
        "a global agent belongs to no project, so the project's half is absent"
    );
    assert_eq!(
        bar_label(Some("clinica"), None, None),
        "clinica",
        "with no agent in scope the selected project is the scope"
    );
    assert_eq!(
        bar_label(None, None, None),
        "direct",
        "with neither, the scope says so"
    );
    // The stale half is ignored rather than believed: a project left over from
    // an agent that no longer exists must not resurface under nothing.
    assert_eq!(
        bar_label(Some("clinica"), None, Some("fudi")),
        "clinica",
        "with no agent in scope there is no agent project to prefer"
    );
}

/// The rows that have to keep a specific hint, and what it costs to keep it.
///
/// The general fit test above proves each row is complete; this says which
/// hints are load-bearing enough that losing one to a future trim is a bug
/// rather than a shortened row. Each pair is (screen, the hint it needs).
#[test]
fn the_rows_keep_the_hints_they_owe_the_user() {
    for (target, key) in [
        // Without this the one-line input's multi-line path is undiscoverable,
        // which is what the design traded the input's rows for.
        (KeyTarget::View(CurrentView::Chat), "Shift+Enter: newline"),
        // `Esc` is the first thing a clip takes, and on these two rows it is
        // the only way out of a screen you entered by a key.
        (KeyTarget::View(CurrentView::Chat), "Esc: back"),
        (KeyTarget::View(CurrentView::Analysis), "Esc: back"),
        (KeyTarget::View(CurrentView::ContextView), "Esc: back"),
        (KeyTarget::AgentForm(0), "Esc: back"),
        // Without this the multi-line prompt is undiscoverable in the form
        // the same way it would be in the chat — and it is named on the
        // prompt field *only*, which is the field `Enter` inserts into.
        (
            KeyTarget::AgentForm(crate::tui::views::agent_form::PROMPT_INDEX),
            "Enter: newline in prompt",
        ),
        // The arrows are the only way to change the two fields that are not
        // typed into, so they have to be named on those.
        (KeyTarget::AgentForm(2), "←/→: cycle"),
        // The two screens whose keys belong to no view: the only place the
        // user is told approve and discard exist, and the only place the
        // agent list is reachable.
        (KeyTarget::Proposals, "a: approve all"),
        (KeyTarget::Proposals, "d: discard"),
        (KeyTarget::ProjectAgents, "e: edit"),
        (KeyTarget::ProjectAgents, "←/→: projects"),
    ] {
        let row = footer_row(target.clone());
        assert!(
            row.contains(key),
            "{target:?} must still show {key:?}: {row:?}"
        );
    }
}

/// `AnalysisState::Proposals` is the only screen whose keys belong to no
/// `CurrentView`, and its approve/discard keys were the ones the deleted
/// hard-coded footer used to be the only mention of. `key_target` is what
/// puts them back: the check goes through `key_target` rather than through
/// `hints_for` directly, because a footer that can name those keys but is
/// never asked to is still a feature the user cannot find.
#[test]
fn the_proposal_modals_keys_are_on_the_footer() {
    let pending = AnalysisState::Proposals {
        project_path: "/tmp/p".into(),
        proposals: vec![],
        selected: 0,
        results: Vec::new(),
    };
    assert_eq!(
        keys_for(
            &CurrentView::Analysis,
            true,
            &pending,
            ListBounds::default()
        ),
        KeyTarget::Proposals
    );

    let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
    let hints = hints_for(KeyTarget::Proposals);
    terminal
        .draw(|f| chrome::render_footer(f, f.area(), &hints))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let row: String = (0..buffer.area.width)
        .map(|x| buffer[(x, 0)].symbol().to_string())
        .collect();
    for key in [
        "Enter: approve",
        "a: approve all",
        "d: discard",
        "Esc: back",
    ] {
        assert!(
            row.contains(key),
            "the proposal modal's {key:?} must be on screen: {row:?}"
        );
    }
}

/// …and only while proposals are pending. The other analysis states answer
/// to the analysis view's own keys, so a footer that kept the modal's would
/// be advertising `a` and `d` for a screen where neither does anything.
#[test]
fn the_proposal_keys_leave_the_footer_when_the_decision_is_over() {
    for state in [
        AnalysisState::Idle,
        AnalysisState::Loading {
            started_at: Instant::now(),
        },
        AnalysisState::Loaded("done".into()),
        AnalysisState::Error("boom".into()),
    ] {
        assert_eq!(
            keys_for(&CurrentView::Analysis, true, &state, ListBounds::default()),
            KeyTarget::View(CurrentView::Analysis),
            "{state:?} is not the proposal modal"
        );
    }
}

/// **The form's two conditional keys follow the field.** `Enter` is "next
/// field" on the seven one-line fields and a newline only in the prompt, and
/// the arrows cycle only the two fields that are not typed into — anywhere
/// else the form's handler consumes them and nothing happens. One row for the
/// whole form described one field and hoped, and a test pinned the false label
/// as required.
#[test]
fn the_form_hints_follow_the_field_the_caret_is_in() {
    use crate::tui::views::agent_form::{CYCLES_WITH_ARROWS, PROMPT_INDEX};

    // `Enter`, field by field: every field gets exactly one of the two, and
    // the prompt is the only field that gets the newline.
    for field in 0..=PROMPT_INDEX {
        let row = footer_row(KeyTarget::AgentForm(field));
        let newline = field == PROMPT_INDEX;
        assert_eq!(
            row.contains("Enter: newline in prompt"),
            newline,
            "field {field}: only the prompt can hold a second line: {row:?}"
        );
        assert_eq!(
            row.contains("Enter: next field"),
            !newline,
            "field {field}: every other field advances: {row:?}"
        );
        // Never both, so a row cannot claim two meanings for one key.
        assert!(
            !(row.contains("Enter: newline in prompt") && row.contains("Enter: next field")),
            "field {field} names one meaning for Enter: {row:?}"
        );
        // And the arrows, on the two fields and nowhere else.
        assert_eq!(
            row.contains("←/→: cycle"),
            CYCLES_WITH_ARROWS.contains(&field),
            "field {field}: the arrows only cycle {CYCLES_WITH_ARROWS:?}: {row:?}"
        );
    }

    // The field index reaches the target, so this is a decision `key_target`
    // makes rather than one the test supplies directly.
    for field in 0..=PROMPT_INDEX {
        assert_eq!(
            key_target(&ScreenState {
                form_field_index: field,
                ..screen(&CurrentView::AgentForm, true, ListBounds::default())
            }),
            KeyTarget::AgentForm(field),
            "the form's target carries the field it is on"
        );
    }

    // The keys that do not vary are on every field, so the rows above are not
    // merely short.
    for field in 0..=PROMPT_INDEX {
        let row = footer_row(KeyTarget::AgentForm(field));
        for constant in ["Tab/⇧Tab: field", "Ctrl+S: save", "Esc: back"] {
            assert!(
                row.contains(constant),
                "field {field} must still name {constant:?}: {row:?}"
            );
        }
    }
}

/// **The view axis of the same rule.** `Esc` out of the modal sets
/// `current_view = Projects` and touches nothing else, so `analysis_state` is
/// still `Proposals` on the next frame. A check on the state alone would put
/// the modal's keys on the projects footer — where `a` re-triggers a full
/// analysis and `d` opens the delete confirmation, so the row would name two
/// keys for something they do not do.
///
/// This is the same defect item 3 was raised to fix, one layer up, and the
/// state-axis test above cannot see it: it never leaves the analysis view.
#[test]
fn the_proposal_keys_do_not_follow_the_user_out_of_the_modal() {
    let pending = AnalysisState::Proposals {
        project_path: "/tmp/p".into(),
        proposals: vec![],
        selected: 0,
        results: Vec::new(),
    };
    // Inside the modal: the modal's keys.
    assert_eq!(
        keys_for(
            &CurrentView::Analysis,
            true,
            &pending,
            ListBounds::default()
        ),
        KeyTarget::Proposals
    );

    // Left the modal. `analysis_state` is untouched — this is what `Esc`
    // does.
    for view in [
        CurrentView::Projects,
        CurrentView::Dashboard,
        CurrentView::Chat,
        CurrentView::ContextView,
        CurrentView::AgentForm,
    ] {
        let left = keys_for(&view, true, &pending, ListBounds::default());
        // The form is the one view whose target carries its field, so the
        // "back to the view's own row" claim is spelled out for it.
        assert_eq!(
            left,
            if view == CurrentView::AgentForm {
                KeyTarget::AgentForm(0)
            } else {
                KeyTarget::View(view.clone())
            },
            "{view:?} is not the proposal modal, whatever the analysis state says"
        );
        let row = footer_row(left);
        for modal_key in ["Enter: approve", "a: approve all", "d: discard"] {
            assert!(
                !row.contains(modal_key),
                "{view:?} must not show the modal's {modal_key:?}: {row:?}"
            );
        }
    }

    // …and the projects footer keeps its own keys, which is where `a` and `d`
    // are actually bound.
    let projects = footer_row(KeyTarget::View(CurrentView::Projects));
    assert!(
        projects.contains("a: analyze") && projects.contains("d: delete"),
        "the projects footer must name its own keys: {projects:?}"
    );
    assert!(
        !projects.contains("approve"),
        "and nothing from the modal: {projects:?}"
    );
}

/// The skill picker is a fifth screen whose keys belong to no `CurrentView`,
/// and the same defect the proposals modal and the delete confirmation each
/// had: it takes every key while it is up, so without its own target the form
/// underneath keeps naming `Enter: next field` under a list where `Enter`
/// confirms and `Space` toggles.
/// `Ctrl+R` names the rules row and only the rules row. Every other field's keys
/// type into that field, so a hint pointing elsewhere would advertise an action
/// the row cannot take.
#[test]
fn the_edit_rules_hint_is_on_the_rules_row_alone() {
    let row = footer_row(KeyTarget::AgentForm(RULES_FIELD));
    assert!(
        row.contains("Ctrl+R: edit rules"),
        "the rules row must say how to open the editor: {row:?}"
    );

    for field in 0..=crate::tui::views::agent_form::PROMPT_INDEX {
        if field == RULES_FIELD {
            continue;
        }
        let row = footer_row(KeyTarget::AgentForm(field));
        assert!(
            !row.contains("edit rules"),
            "field {field} has no rules to edit, so it must not say so: {row:?}"
        );
    }
}

/// While the editor is up it owns every key, so its footer must not keep naming
/// the form's: `Enter` writes a newline in there and moves to the next field out
/// here, and both cannot be on screen.
#[test]
fn the_rule_editor_outranks_the_form_it_sits_on() {
    let editor = key_target(&ScreenState {
        rule_editor: true,
        ..screen(&CurrentView::AgentForm, true, drawn_bounds())
    });
    assert_eq!(editor, KeyTarget::RuleEditor);

    let row = footer_row(KeyTarget::RuleEditor);
    assert!(row.contains("new line"), "the editor's own keys: {row:?}");
    assert!(
        !row.contains("next field"),
        "and none of the form's: {row:?}"
    );
}

/// A character pressed on the rules row has to arrive inside the editor. That is
/// the whole reason typing opens the dialog rather than being swallowed, and a
/// lost character is a word silently missing from the user's own rules.
#[test]
fn a_character_typed_on_the_rules_row_lands_in_the_editor() {
    let opened = rule_editor_opening(&["primera".to_string()], Some('!'));
    assert_eq!(
        opened.rules,
        vec!["primera!".to_string()],
        "appended to the focused rule, which is the first one"
    );

    let empty = rule_editor_opening(&[], Some('a'));
    assert_eq!(
        empty.rules,
        vec!["a".to_string()],
        "and on an agent with no rules the character becomes the first one, \
             rather than being dropped for want of a row to land in"
    );

    assert_eq!(
        rule_editor_opening(&["primera".to_string()], None).rules,
        vec!["primera".to_string()],
        "`Ctrl+R` and `Backspace` open the editor and change nothing"
    );
}

/// The optimizer's `enabled` flag is the picker's, not the form's.
///
/// The format string said `enabled = true` literally while
/// `form_optimize_enabled` was assigned by the picker, loaded from the agent and
/// reset on a new form — and never read. Every saved agent claimed to be
/// optimising whether or not anything was selected, and nothing caught it: the
/// field *was* used, just not on the line that mattered.
///
/// Asserted against the source rather than a value, because the defect is a
/// literal in a `format!`. A behavioural test cannot see it: the written TOML is
/// exactly what the code says it is.
#[test]
fn the_saved_optimize_flag_comes_from_the_picker() {
    let body = include_str!("../app.rs")
        .split("#[cfg(test)]")
        .next()
        .expect("app.rs has a test module");

    assert!(
        !body.contains("enabled = true\\nrules"),
        "the saved TOML must take `enabled` from the picker, not hard-code it"
    );
    assert!(
        body.contains("enabled = {}\\nrules"),
        "and it must still write the key at all"
    );
    assert!(
        body.contains("self.form_optimize_enabled, opt_rules_str"),
        "the picker's value has to reach the format string"
    );
}

/// The form's rules row is a one-line join, and the join is the whole reason
/// there is an editor: a list drawn on one row cannot be read.
#[test]
fn the_rules_row_joins_the_list_and_keeps_it_readable() {
    assert_eq!(TuiApp::rules_line(&[]), "", "no rules, nothing to show");
    assert_eq!(
        TuiApp::rules_line(&["una".to_string()]),
        "una",
        "a single rule is itself"
    );
    assert_eq!(
        TuiApp::rules_line(&["una".to_string(), "dos".to_string()]),
        "una, dos",
        "two rules are separated, so the row is countable at a glance"
    );
}

/// The skill picker is a fifth screen whose keys belong to no `CurrentView`,
/// and the same defect the proposals modal and the delete confirmation each
/// had: it takes every key while it is up, so without its own target the form
/// underneath keeps naming `Enter: next field` under a list where `Enter`
/// confirms and `Space` toggles.
#[test]
fn the_picker_owns_the_footer_while_it_is_up() {
    let base = screen(&CurrentView::AgentForm, true, drawn_bounds());
    let on_skills_field = ScreenState {
        form_field_index: SKILLS_FIELD,
        ..base.clone()
    };

    // The one field that differs is named, so there is nothing to read twice
    // to check which of the two `false`s is which.
    let with_picker = key_target(&ScreenState {
        skill_picker: true,
        ..on_skills_field.clone()
    });
    assert_eq!(
        with_picker,
        KeyTarget::SkillPicker,
        "the picker is up, so it outranks the form"
    );

    let row = footer_row(KeyTarget::SkillPicker);
    for expected in [
        "space: toggle",
        "type: filter",
        "enter: save",
        "esc: cancel",
    ] {
        assert!(
            row.contains(expected),
            "{expected:?} must be on the row: {row:?}"
        );
    }
    assert!(
        !row.contains("next field"),
        "the form's own Enter must not leak through: {row:?}"
    );

    // Every other view keeps its own keys: the picker is up over the form, so
    // this is really about the form, but the axis is not view-scoped and could
    // have been written to swallow more.
    let without_picker = key_target(&on_skills_field);
    assert_eq!(without_picker, KeyTarget::AgentForm(SKILLS_FIELD));
    assert!(
        footer_row(without_picker).contains("next field"),
        "and the form gets its row back once the picker is down"
    );
}

/// The picker reads a project's *path* out of the context store, not the id
/// the form holds.
///
/// Worth a test because the wrong version compiles and looks right:
/// `list_skills_for_project` takes a `&Path`, and an id is a perfectly good
/// `&str` to hand it. It would then look for `fudi/skills` relative to the
/// working directory — a directory that does not exist — and the picker would
/// show only the shared skills, with the project's own quietly missing.
#[test]
fn the_picker_resolves_a_project_id_to_its_path() {
    use crate::context::store::ContextStore;
    use crate::services::skill_manager::SkillManager;

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());
    // Deliberately NOT named after the id, so a lookup by id cannot stumble
    // onto it by accident — the two are only distinguishable here.
    let project_path = dir.path().join("checkout");
    std::fs::create_dir_all(project_path.join("skills/project-only")).unwrap();
    std::fs::write(
        project_path.join("skills/project-only/SKILL.md"),
        "---\nname: Project Only\ndescription: d\n---\nbody\n",
    )
    .unwrap();

    let manager = SkillManager::with_paths(vec![dir.path().join("skills")]);
    manager.scan_and_load();

    let store = ContextStore::new();
    store
        .save_context(crate::context::store::ProjectContext {
            project_id: "fudi".to_string(),
            path: project_path.to_string_lossy().to_string(),
            context_md: String::new(),
            project_type: "rust".to_string(),
            skills_injected: Vec::new(),
            last_analyzed: chrono::Utc::now(),
            custom_rules: String::new(),
        })
        .unwrap();

    let ids: Vec<String> = available_skills(&store, &manager, "fudi")
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert!(
        ids.contains(&"project-only".to_string()),
        "the project's own skill must reach the picker: {ids:?}"
    );

    // The wrong version — passing the id where a path is wanted — finds
    // nothing here, which is exactly why the store lookup is not optional.
    let by_id: Vec<String> = manager
        .list_skills_for_project(Path::new("fudi"))
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert!(
        !by_id.contains(&"project-only".to_string()),
        "passing the id must NOT find them, or this test proves nothing: {by_id:?}"
    );
}

/// The picker opens on the skills field and nowhere else. A `s` that opened it
/// from the prompt would swallow a character the user is typing.
#[test]
fn only_the_skills_field_opens_the_picker() {
    let drawn = drawn_bounds();

    for field in 0..SKILLS_FIELD {
        assert_eq!(
            key_target(&ScreenState {
                form_field_index: field,
                ..screen(&CurrentView::AgentForm, true, drawn)
            }),
            KeyTarget::AgentForm(field),
            "field {field} is a typing field and must not be the picker"
        );
    }
}

/// **The delete confirmation is the same defect, on the sibling modal.** Its
/// handler takes *every* key while it is up — it is consulted before the chat
/// and before `q`, and every branch `continue`s — so the projects row
/// underneath named seven keys that did nothing, `q: quit` most visibly of
/// all. Nothing in the view says it is up: only the projects view opens it, and
/// the projects view stays current underneath, so `key_target` has to be told.
#[test]
fn the_delete_confirmation_owns_the_footer_while_it_is_up() {
    let drawn = drawn_bounds();
    // Only the projects view opens it, but the check cannot know that: it has to
    // answer `DeleteConfirm` whichever view it is asked about, and over both
    // projects states, since the confirmation and the focus are separate inputs.
    for (view, focus) in [
        (CurrentView::Projects, true),
        (CurrentView::Projects, false),
        (CurrentView::Dashboard, true),
        (CurrentView::Chat, true),
        (CurrentView::Analysis, true),
        (CurrentView::ContextView, true),
        (CurrentView::AgentForm, true),
    ] {
        assert_eq!(
            key_target(&ScreenState {
                confirm_delete: true,
                ..screen(&view, focus, drawn)
            }),
            KeyTarget::DeleteConfirm,
            "{view:?} with the confirmation up is not {view:?}"
        );
    }

    let row = footer_row(KeyTarget::DeleteConfirm);
    for answer in ["y: delete", "n: cancel", "Esc: cancel"] {
        assert!(
            row.contains(answer),
            "the confirmation's {answer:?} must be on screen: {row:?}"
        );
    }
    // …and nothing of the view underneath, which is the whole point: every one
    // of those keys is swallowed before it reaches the view.
    for inert in [
        "a: analyze",
        "n: new",
        "d: delete",
        "e: edit",
        "←/→: agents",
        "←/→: projects",
        "q: quit",
        "Enter: approve",
    ] {
        assert!(
            !row.contains(inert),
            "nothing underneath answers while the confirmation is up, so {inert:?} \
                 must not be named: {row:?}"
        );
    }
}

/// **…and it leaves with it.** `y`/`n`/`Esc` all clear `confirm_delete`, so
/// the very next frame's footer must be the view's own again. A flag that
/// outlives its dialog is the same trap as the analysis state outliving the
/// proposal modal, one screen over.
#[test]
fn the_confirmation_keys_leave_the_footer_when_it_is_dismissed() {
    let idle = AnalysisState::Idle;
    let drawn = drawn_bounds();
    // Up, then dismissed: the second frame is the first frame's target again.
    assert_eq!(
        keys_for(&CurrentView::Projects, false, &idle, drawn),
        KeyTarget::ProjectAgents
    );
    let row = footer_row(KeyTarget::ProjectAgents);
    assert!(
        row.contains("e: edit"),
        "the agent list's own keys come back: {row:?}"
    );
    assert!(
        !row.contains("y: delete"),
        "and nothing of the confirmation stays: {row:?}"
    );
}

/// Deleting the selected agent puts the focus back on the projects, so every
/// label on the row that follows is true.    ///
/// The repro is a project with one agent: `→` `d` `y`. `agent_index` resets
/// and the agent list is empty, so `key_target` falls through to
/// `View(Projects)` — while the focus was still on the agent list. Three of
/// that row's seven labels were then wrong (`↑/↓` drove a selection nobody
/// could see, `d` fell through to the project branch and was refused,
/// `←/→: agents` was the wrong way round) and *no* row wore `active`, because
/// the project rows' marker follows this very flag.
#[test]
fn deleting_an_agent_puts_the_focus_back_where_the_labels_are_true() {
    // The value the delete handler writes into `active_in_project_list`.
    let focus_on_projects = focus_after_agent_delete();
    assert!(
        focus_on_projects,
        "the focus goes back to the project list, not to an agent list that may be empty"
    );

    // The state the next frame sees: project rows, no agent rows.
    let after = undrawn_bounds();
    let idle = AnalysisState::Idle;

    // The footer names the projects, which is where the focus now is.
    assert_eq!(
        keys_for(&CurrentView::Projects, focus_on_projects, &idle, after),
        KeyTarget::View(CurrentView::Projects),
        "the projects row is the one whose labels this focus makes true"
    );
    // The agent keys refuse, so the projects row's keys are the only honest set.
    assert!(
        selected_agent(after, &agents_named(&[]), 0).is_none(),
        "`e` and the agent arm of `d` have nothing to aim at"
    );
    assert_eq!(
        move_agent_selection(after, 0, 0, 1),
        0,
        "`↑/↓` must not move a selection with no row under it"
    );
    // Focus is already where the user can see it, so nothing needs `←/→` to
    // put it there — and the empty agent list must still never be entered.
    assert!(
        !may_move_list_focus(after, focus_on_projects),
        "an empty agent list must not be entered from the projects"
    );
    assert!(
        may_move_list_focus(after, !focus_on_projects),
        "and leaving one is never refused, or the project list is unreachable"
    );
    // The row the footer says the next keystroke acts on is a project row, so
    // it exists: this is the half that is a rendering claim, pinned by
    // `views/projects.rs::the_focused_list_is_the_only_one_with_an_active_row`.
    assert!(
        after.project_rows > 0,
        "the focus has to land somewhere: {after:?}"
    );
}

/// The projects view draws two lists and four keys move between and within
/// them. The footer has to follow the focus, because `e` only edits an agent
/// while the agent list is selected: an `e: edit` hint on the project list
/// would be a key that does nothing.
#[test]
fn the_projects_footer_follows_the_list_that_has_focus() {
    let idle = AnalysisState::Idle;
    // The agent list has rows, which is the precondition for its keys existing
    // at all — see the test below for when it has none.
    let drawn = ListBounds {
        project_rows: 3,
        agent_rows: 2,
    };

    assert_eq!(
        keys_for(&CurrentView::Projects, true, &idle, drawn),
        KeyTarget::View(CurrentView::Projects)
    );
    assert!(
        !hints_for(KeyTarget::View(CurrentView::Projects))
            .iter()
            .any(|(key, _)| *key == "e"),
        "`e` edits an agent and must not be advertised while the agent list is not selected"
    );

    assert_eq!(
        keys_for(&CurrentView::Projects, false, &idle, drawn),
        KeyTarget::ProjectAgents
    );
    let agents = hints_for(KeyTarget::ProjectAgents);
    assert!(
        agents.iter().any(|(key, _)| *key == "e"),
        "the agent list is selected, so its edit key must be advertised: {agents:?}"
    );
    assert!(
        agents
            .iter()
            .any(|(key, hint)| *key == "←/→" && *hint == "projects"),
        "the agent list needs a way back to the projects: {agents:?}"
    );
    // Every hint on this row acts on an agent. `a` analyses the project, and
    // the user is looking at agents: naming it here would say the two rows
    // are the same selection.
    assert!(
        !agents.iter().any(|(key, _)| *key == "a"),
        "analysing a project is the project row's key, not the agent list's: {agents:?}"
    );
}
/// An agent list with rows, as `render_projects` reports it.
fn drawn_bounds() -> ListBounds {
    ListBounds {
        project_rows: 3,
        agent_rows: 2,
    }
}

/// An agent list with none: a body too short for it after the rule and the
/// label, or a project with no agents.
fn undrawn_bounds() -> ListBounds {
    ListBounds {
        project_rows: 3,
        agent_rows: 0,
    }
}

/// Two agents, by id, so a test can name one.
fn agents_named(ids: &[&str]) -> Vec<crate::domain::agent::Agent> {
    ids.iter()
        .map(|id| crate::domain::agent::Agent {
            id: (*id).to_string(),
            config: crate::domain::agent::AgentConfig::default(),
            project_id: Some("fudi".to_string()),
        })
        .collect()
}

/// **The deferral is guarded.** `e` and `d` must not act on an agent when the
/// list was not drawn: the row is on the model's screen and nowhere else, so
/// `e` would open a form for an agent the user cannot see highlighted.
///
/// This is the predicate `e` and `d` call, extracted so a test can reach it —
/// inside the event loop it was a guard with no test, which is the failure mode
/// this branch had already produced five times.
#[test]
fn a_key_aimed_at_the_agent_list_does_nothing_when_the_list_was_not_drawn() {
    let agents = agents_named(&["nutricion", "pediatra"]);

    // Drawn: the selected agent is the target.
    for (index, expected) in [(0, "nutricion"), (1, "pediatra")] {
        assert_eq!(
            selected_agent(drawn_bounds(), &agents, index).map(|a| a.id.as_str()),
            Some(expected),
            "index {index} must name a row that is on screen"
        );
    }

    // Not drawn: nothing, however good the index is.
    for index in 0..4 {
        assert!(
            selected_agent(undrawn_bounds(), &agents, index).is_none(),
            "index {index} names no visible row, so the key must do nothing"
        );
    }

    // Drawn but the index is past the end: also nothing.
    assert!(selected_agent(drawn_bounds(), &agents, 99).is_none());

    // No agents at all, whatever the bounds say.
    let empty: Vec<crate::domain::agent::Agent> = Vec::new();
    assert!(selected_agent(drawn_bounds(), &empty, 0).is_none());
}

/// The same for the two navigation keys, which move a selection that would
/// otherwise move with nothing on screen moving.
#[test]
fn the_agent_selection_does_not_move_when_the_list_was_not_drawn() {
    let count = 3;
    // Drawn: it wraps in both directions, as it always has.
    assert_eq!(move_agent_selection(drawn_bounds(), 0, count, 1), 1);
    assert_eq!(move_agent_selection(drawn_bounds(), 2, count, 1), 0);
    assert_eq!(move_agent_selection(drawn_bounds(), 0, count, -1), 2);

    // Not drawn: unmoved, for every index and both directions. A selection that
    // advances while the highlighted row stays where it is drawn is exactly the
    // invisible state the window exists to prevent.
    for index in 0..count {
        for delta in [-1, 1] {
            assert_eq!(
                move_agent_selection(undrawn_bounds(), index, count, delta),
                index,
                "index {index} must not move while the list is undrawn"
            );
        }
    }

    // Drawn but empty: nothing to move to.
    assert_eq!(move_agent_selection(drawn_bounds(), 0, 0, 1), 0);
    // Drawn with a stale index: the wrap puts it back in range, so one arrow
    // press recovers a selection that had drifted off the end rather than
    // leaving it there.
    assert_eq!(
        move_agent_selection(drawn_bounds(), 99, 2, -1),
        0,
        "a stale index self-heals on the next press"
    );
}

/// **Focus may not move *onto* a list with no rows** — which is what keeps `d`
/// meaning "delete the selected agent" once it gets there, rather than silently
/// doing nothing because the focus is on nothing.
#[test]
fn focus_does_not_move_onto_a_list_with_no_rows() {
    // Entering the agent list: gated on it having a row.
    assert!(
        may_move_list_focus(drawn_bounds(), true),
        "the agent list has rows, so focus may move onto it"
    );
    assert!(
        !may_move_list_focus(undrawn_bounds(), true),
        "the agent list has none, so focus must not move onto it"
    );
    assert!(
        !may_move_list_focus(ListBounds::default(), true),
        "nor when neither list has a row"
    );
    assert!(
        !agent_list_is_drawn(undrawn_bounds()),
        "the predicate the entering direction is built on says the same"
    );
}

/// **…but it may always move *off* one.** Deleting the selected project's last
/// agent drops `agent_rows` to zero while the focus stays where it was, and
/// this key is the only writer of the focus flag — no view switch resets it.
/// So refusing the exit would take project selection, project delete and the
/// way back away for the life of the process, in exchange for avoiding an
/// empty project row the user can still see.
#[test]
fn focus_can_leave_the_agent_list_after_its_last_agent_is_deleted() {
    // Before the delete: both directions open.
    let before = ListBounds {
        project_rows: 3,
        agent_rows: 1,
    };
    assert!(may_move_list_focus(before, true), "onto a list with a row");
    assert!(may_move_list_focus(before, false), "and off it");

    // `d` on the only agent, confirmed. `agent_index` resets to 0, the focus
    // stays on the agent list, and the next frame reports it empty.
    let after = ListBounds {
        project_rows: 3,
        agent_rows: 0,
    };
    assert!(
        !may_move_list_focus(after, true),
        "the now-empty agent list must not be entered"
    );
    assert!(
        may_move_list_focus(after, false),
        "but focus must be able to leave it, or the project list is unreachable"
    );

    // The worst form of the same trap: a body where the project list is empty
    // too. Refusing the exit here would strand the user on a screen with
    // nothing selectable and no way to leave it.
    let neither = ListBounds::default();
    assert!(
        may_move_list_focus(neither, false),
        "leaving is never refused, even with nothing to land on"
    );
}

/// **The labels follow the rows.** A project with no agents has an empty agent
/// list at any terminal size, so focusing it used to rename the footer to
/// `e: edit  d: delete` while all four keys did nothing and no row was
/// highlighted anywhere. The footer names the agent keys exactly when the
/// agent keys can act.
#[test]
fn the_footer_does_not_name_agent_keys_when_no_agent_row_is_drawn() {
    let idle = AnalysisState::Idle;
    // Focus is on the agent list and it is empty: this is the state `→` reaches
    // on a project with no agents.
    assert_eq!(
        keys_for(&CurrentView::Projects, false, &idle, undrawn_bounds()),
        KeyTarget::View(CurrentView::Projects),
        "an empty agent list must not get the agent row's labels"
    );
    let row = footer_row(KeyTarget::View(CurrentView::Projects));
    assert!(
        !row.contains("e: edit"),
        "nothing can be edited when no agent row is drawn: {row:?}"
    );
    for label in ["e: edit", "←/→: projects"] {
        assert!(
            !row.contains(label),
            "the projects row must not carry the agent row's {label:?}: {row:?}"
        );
    }
    // …and it keeps the keys that *do* work there.
    assert!(
        row.contains("a: analyze") && row.contains("d: delete"),
        "the projects footer must name its own keys: {row:?}"
    );

    // With rows drawn, the agent labels are back — so the gate is not simply
    // deleting them.
    assert_eq!(
        keys_for(&CurrentView::Projects, false, &idle, drawn_bounds()),
        KeyTarget::ProjectAgents
    );
    assert!(footer_row(KeyTarget::ProjectAgents).contains("e: edit"));
}
fn key(code: KeyCode, modifiers: KeyModifiers) -> event::KeyEvent {
    event::KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

/// The commitment behind the one-line input: `Enter` sends, `Shift+Enter`
/// inserts a newline. If this regresses a multi-line prompt becomes
/// impossible to compose and the shorter input stops being a fair trade.
#[test]
fn enter_sends_and_shift_enter_starts_a_new_line() {
    assert_eq!(
        chat_input_action(key(KeyCode::Enter, KeyModifiers::NONE), false),
        ChatAction::Send
    );
    assert_eq!(
        chat_input_action(key(KeyCode::Enter, KeyModifiers::SHIFT), false),
        ChatAction::Newline,
        "Shift+Enter must never send"
    );
    assert_ne!(
        chat_input_action(key(KeyCode::Enter, KeyModifiers::SHIFT), false),
        ChatAction::Send
    );
}

#[test]
fn an_ordinary_character_edits_the_buffer() {
    assert_eq!(
        chat_input_action(key(KeyCode::Char('a'), KeyModifiers::NONE), false),
        ChatAction::Edit('a')
    );
}

/// While a reply is in flight the buffer is frozen: nothing is typed and
/// nothing is sent, so a newline has nothing to compose into either.
/// Backspace stays available so a typo can still be taken back out.
#[test]
fn a_frozen_input_refuses_to_grow_or_send() {
    for code in [KeyCode::Enter, KeyCode::Char('a')] {
        for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
            assert_eq!(
                chat_input_action(key(code, modifiers), true),
                ChatAction::Other,
                "{code:?} with {modifiers:?} must do nothing while loading"
            );
        }
    }
    assert_eq!(
        chat_input_action(key(KeyCode::Backspace, KeyModifiers::NONE), true),
        ChatAction::Backspace,
        "a typo in the draft can still be removed while loading"
    );
}

#[test]
fn keys_that_are_not_input_composition_are_left_alone() {
    for code in [
        KeyCode::Esc,
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Home,
        KeyCode::End,
    ] {
        assert_eq!(
            chat_input_action(key(code, KeyModifiers::NONE), false),
            ChatAction::Other,
            "{code:?} is navigation, not the input's business"
        );
    }
}

/// Declaration order is the contract between `index()` and `VIEW_NAMES`:
/// the active view must land on a real name, and on the one the enum
/// declares it at.
#[test]
fn every_view_index_addresses_a_name_on_the_bar() {
    let indices: Vec<usize> = all_views().iter().map(|v| v.index()).collect();
    assert_eq!(indices, vec![0, 1, 2, 3, 4, 5]);
    for view in all_views() {
        assert!(
            view.index() < VIEW_NAMES.len(),
            "index {} is past the {} names on the bar",
            view.index(),
            VIEW_NAMES.len()
        );
    }
    assert_eq!(
        VIEW_NAMES.len(),
        all_views().len(),
        "every view needs a name of its own on the bar"
    );
}

/// Whichever view is active, its name must be on the bar *and* styled as
/// active. A view missing from `VIEW_NAMES` leaves a bar with no active
/// view — which is how the broken tab bar read.
#[test]
fn the_bar_marks_whichever_view_is_active() {
    let active = crate::tui::theme::active();
    for view in all_views() {
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        let name = VIEW_NAMES[view.index()];
        terminal
            .draw(|f| {
                let c = chrome::layout(f.area());
                chrome::render_bar(
                    f,
                    c.bar,
                    &VIEW_NAMES,
                    view.index(),
                    &chrome::Context::default(),
                    true,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect();
        // `find` gives a byte offset and the bar holds multi-byte separators,
        // so count characters to get the column.
        let start = row
            .find(name)
            .map(|byte| row[..byte].chars().count())
            .unwrap_or_else(|| panic!("the bar must show {name:?}: {row:?}"));
        // Compare the fields that mean "active" rather than the whole
        // style: the buffer's cells carry defaults the token does not.
        let style = buffer[(start as u16, 0)].style();
        assert_eq!(
            (style.fg, style.add_modifier),
            (active.fg, active.add_modifier),
            "{name:?} must be the styled-active view on the bar"
        );
    }
}

/// A `TuiApp` on the agent form with its caret at `field`, built without a
/// terminal.
///
/// `TuiApp::new` never touches the terminal, and none of the keys these tests
/// inject reach `load_models_blocking`, so a plain `#[test]` is enough. A real
/// `AppState` (over an inert provider) rather than a hand-built struct means the
/// picker's opener can resolve its project and the dialog constructors see the
/// same registry the shipped app uses.
fn app_on_form_field(field: usize) -> TuiApp {
    use crate::adapters::mcp::StaticMcpRegistry;
    use crate::context::store::ContextStore;
    use crate::ports::mcp::McpServerRegistry;
    use crate::providers::ollama::OllamaProvider;
    use crate::runtime::build_app_state;
    use crate::services::agent_registry::AgentRegistry;
    use crate::services::skill_manager::SkillManager;
    use std::collections::VecDeque;

    let state = build_app_state(
        Arc::new(OllamaProvider::new("http://localhost:11434".to_string())),
        Arc::new(AgentRegistry::new()),
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        "llama3".to_string(),
        Arc::new(Mutex::new(VecDeque::new())),
        Vec::new(),
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
        None,
        None,
        None,
        None,
    );
    let mut app = TuiApp::new(state);
    app.current_view = CurrentView::AgentForm;
    app.form_field_index = field;
    app
}

/// `s` on the skills field opens the picker rather than being swallowed by the
/// form's catch-all typing arm.
///
/// The two assertions distinguish the two outcomes: with the opener above the
/// typing arm the picker is up and no skill id was typed, while with the arms
/// swapped the catch-all claims the `s` (field 6 is a no-op there) and no picker
/// opens. This is the behaviour `a_dialog_opener_outranks_the_typing_arm` used
/// to check by source position.
#[test]
fn the_picker_opener_is_not_typed_into_the_form() {
    let mut app = app_on_form_field(SKILLS_FIELD);
    assert!(app.skill_picker.is_none(), "no picker up before the key");

    app.dispatch(key(KeyCode::Char('s'), KeyModifiers::NONE));

    assert_eq!(app.form_skills, "", "the skills field is not typed into");
    assert!(
        app.skill_picker.is_some(),
        "`s` on the skills field must open the picker, not fall through to the typing arm"
    );
}

/// `Ctrl+R` on the rules field opens the editor with nothing pre-typed, rather
/// than reaching the form's catch-all typing arm.
///
/// The editor's contents are asserted as well as its presence: if the typing
/// arm were moved above the opener it would still open an editor, but with an
/// `r` typed into it — a rule the user never wrote. That is the observable
/// difference the source-position guard could only infer from arm order.
#[test]
fn the_rules_opener_is_not_typed_into_the_form() {
    let mut app = app_on_form_field(RULES_FIELD);
    assert!(app.rule_editor.is_none(), "no editor up before the key");

    app.dispatch(key(KeyCode::Char('r'), KeyModifiers::CONTROL));

    assert!(app.form_rules.is_empty(), "the rules list is untouched");
    let editor = app
        .rule_editor
        .as_ref()
        .expect("Ctrl+R on the rules field must open the editor");
    assert!(
        editor.rules.is_empty(),
        "the opener passes no character through; a typing arm would have put `r` in: \
         {:?}",
        editor.rules
    );
}

/// Every dialog owns `Tab` while it is up, even over the agent form.
///
/// The form's `Tab` advances `form_field_index`; each dialog swallows it. One
/// case per dialog mirrors the four source checks `every_dialog_is_intercepted_
/// before_the_view_it_sits_over` used to make. If any dialog's guard ever sits
/// below the form's, the form advances the field and the case names that dialog.
#[test]
fn an_open_dialog_consumes_keys_the_form_would_handle() {
    let cases: Vec<(&str, Box<dyn Fn(&mut TuiApp)>)> = vec![
        (
            "the skill picker",
            Box::new(|app| {
                app.skill_picker = Some(SkillPicker::for_agent(vec![], &[], &[]));
            }),
        ),
        (
            "the rule editor",
            Box::new(|app| {
                app.rule_editor = Some(rule_editor_opening(&[], None));
            }),
        ),
        (
            "the optimization-tool picker",
            Box::new(|app| {
                app.opt_tools = Some(OptTools::for_agent(&[]));
            }),
        ),
        (
            "the delete confirmation",
            Box::new(|app| {
                app.confirm_delete = Some(("agent".to_string(), "ops".to_string()));
            }),
        ),
    ];

    for (label, open) in cases {
        let mut app = app_on_form_field(0);
        open(&mut app);
        let field = app.form_field_index;
        let view = app.current_view.clone();
        let skills = app.form_skills.clone();
        let rules = app.form_rules.clone();

        let outcome = app.dispatch(key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(
            matches!(outcome, DispatchOutcome::Continue),
            "{label} kept the key inside the dialog, so the loop continues"
        );
        assert_eq!(
            app.form_field_index, field,
            "{label} let Tab reach the form's field navigation"
        );
        assert_eq!(
            app.current_view, view,
            "{label} changed the view underneath"
        );
        assert_eq!(app.form_skills, skills, "{label} changed form_skills");
        assert_eq!(app.form_rules, rules, "{label} changed form_rules");
    }
}
