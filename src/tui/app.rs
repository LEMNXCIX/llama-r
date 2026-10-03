use crate::api::handlers::AppState;
use crate::domain::models::ChatMessage;
use crate::services::agent_runtime::RuntimeChatRequest;
use crate::tui::chrome;
use crate::tui::views::chat::render_chat;
use crate::tui::views::dashboard::render_dashboard;
use crate::tui::views::modals::render_confirm_delete;
use crate::tui::views::projects::ListBounds;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
    fn index(&self) -> usize {
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
const VIEW_NAMES: [&str; 6] = [
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
enum KeyTarget {
    /// The view's own body.
    View(CurrentView),
    /// The selected project's agent list, inside the projects view.
    ProjectAgents,
    /// Skill proposals awaiting a decision, inside the analysis view.
    Proposals,
    /// The delete confirmation, drawn over the projects view.
    DeleteConfirm,
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
fn key_target(
    view: &CurrentView,
    active_in_project_list: bool,
    analysis: &AnalysisState,
    projects_bounds: ListBounds,
    form_field_index: usize,
    confirm_delete: bool,
) -> KeyTarget {
    // Checked first because it outranks every axis below it: the handler in the
    // event loop takes every key while the confirmation is up and `continue`s, so
    // no keypress reaches the screen any other target here describes.
    if confirm_delete {
        return KeyTarget::DeleteConfirm;
    }
    if matches!(view, CurrentView::Analysis) && matches!(analysis, AnalysisState::Proposals { .. })
    {
        return KeyTarget::Proposals;
    }
    // The agent list's keys, and only when the agent list has a row to aim at.
    // `ProjectAgents` is reached by focusing it, and focus can be on it with
    // nothing in it — a project with no agents, or a body too short for the list
    // after the rule and the label. Naming `e: edit  d: delete` over an empty list
    // is the same invisible state the keys themselves refuse: the labels and the
    // behaviour have to agree, or the footer is advertising a lie.
    if matches!(view, CurrentView::Projects)
        && !active_in_project_list
        && projects_bounds.agent_rows > 0
    {
        return KeyTarget::ProjectAgents;
    }
    // The form's own keys are not one set: `Enter` and the arrows mean different
    // things on different fields, so the field index is part of the target rather
    // than a detail the handler keeps to itself.
    if matches!(view, CurrentView::AgentForm) {
        return KeyTarget::AgentForm(form_field_index);
    }
    KeyTarget::View(view.clone())
}

/// Whether a key aimed at the agent list may act, given what the last frame drew.
///
/// With a body too short for the list — three rows, where the rule and the label
/// take two — there is no row to act on, and acting on `agent_index` anyway would
/// mean editing an agent nobody can see. The four bindings consult this; it is a
/// free function over the bounds so a test can reach it, because a predicate that
/// only exists inside the event loop is a predicate with no test.
fn agent_list_is_drawn(bounds: ListBounds) -> bool {
    bounds.agent_rows > 0
}

/// The agent `e` and `d` should act on, or `None` when there is nothing to act on.
///
/// Two ways to get `None`, and both mean the same thing to the user: the list was
/// not drawn, or `agent_index` names no agent. Either way the key would be
/// operating on a row that is not on screen.
fn selected_agent(
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
fn move_agent_selection(bounds: ListBounds, current: usize, count: usize, delta: isize) -> usize {
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
fn may_move_list_focus(bounds: ListBounds, focus_is_on_the_project_list: bool) -> bool {
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
fn focus_after_agent_delete() -> bool {
    true
}

/// The project and agent the context bar names.
///
/// **The agent's own project, not the projects view's selection.** They are
/// unrelated state — the chat's agent selector walks every agent in the
/// registry, the projects view walks its own projects — so reading one half from
/// each produced `clinica/nutricion`: an agent belonging to a different project,
/// which is the one pair [`crate::tui::chrome::context_label`] exists to rule out.
/// Two projects can register an agent with the same id, so showing the project is
/// what makes a selection unambiguous, and a bar that pairs halves from two
/// sources makes it ambiguous again.
///
/// The fallback is the projects view's own selection, because with no agent in
/// scope there is nothing to disambiguate. A *global* agent has no project at
/// all, so the bar names it alone rather than pairing it with whichever project
/// happened to be selected.
///
/// A free function for the reason the rest are: the pair is a decision about
/// state, and a decision reached inside the draw closure cannot be checked by
/// anything but a terminal.
fn bar_context(
    projects_selection: Option<String>,
    agent_selection: Option<String>,
    agent_project: Option<String>,
) -> chrome::Context {
    let project = match (agent_selection.is_some(), agent_project) {
        (true, Some(project)) => Some(project),
        // A global agent belongs to no project, so the project's half is absent
        // rather than borrowed from an unrelated selection.
        (true, None) => None,
        (false, _) => projects_selection,
    };
    chrome::Context {
        project,
        agent: agent_selection,
    }
}

pub struct TuiApp {
    state: Arc<AppState>,
    current_view: CurrentView,
    project_index: usize,
    agent_index: usize,
    active_in_project_list: bool,
    /// Rows the last frame actually drew for each of the projects view's two
    /// lists. The agent list is smaller than the body at some project counts, so
    /// the four keys that drive it read this rather than assuming it is there.
    project_list_bounds: ListBounds,
    // Form state
    form_id: String,
    form_name: String,
    form_model: String,
    form_prompt: String,
    form_rules: String,
    form_optimize_rules: String,
    form_skills: String,
    form_project_id: String,
    form_field_index: usize, // 0: ID, 1: Name, 2: Model, 3: Project, 4: Rules, 5: Optim. Rules, 6: Skills, 7: Prompt
    available_projects: Vec<String>,
    available_models: Vec<String>,
    editing_agent: Option<String>,
    // Analysis state
    analysis_state: Arc<Mutex<AnalysisState>>,
    // Dashboard scroll
    log_scroll: usize,
    // Context view scroll
    context_scroll: usize,
    // Delete confirmation
    confirm_delete: Option<(String, String)>,
    // Chat state
    chat_messages: Arc<Mutex<Vec<(String, String)>>>,
    chat_input: String,
    chat_loading: Arc<AtomicBool>,
    /// When the in-flight chat request started, used to drive the spinner.
    chat_loading_since: Arc<Mutex<Option<Instant>>>,
    chat_auto_scroll: bool,
    chat_agent_index: usize,
    chat_selected_agent: Option<String>,
    /// Project of the selected chat agent, so the header can disambiguate
    /// agents that share an id across projects.
    chat_selected_project: Option<String>,
    available_agents: Vec<String>,
    chat_scroll: usize,
    // Render-time feedback so key/mouse handlers can drive real bounds.
    chat_scroll_max: usize,
    chat_messages_rect: ratatui::layout::Rect,
}

impl TuiApp {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            current_view: CurrentView::Dashboard,
            project_index: 0,
            agent_index: 0,
            active_in_project_list: true,
            project_list_bounds: Default::default(),
            form_id: String::new(),
            form_name: String::new(),
            form_model: String::new(),
            form_prompt: String::new(),
            form_rules: String::new(),
            form_optimize_rules: String::new(),
            form_skills: String::new(),
            form_project_id: String::new(),
            form_field_index: 0,
            available_projects: Vec::new(),
            available_models: Vec::new(),
            editing_agent: None,
            analysis_state: Arc::new(Mutex::new(AnalysisState::Idle)),
            log_scroll: 0,
            context_scroll: 0,
            confirm_delete: None,
            chat_messages: Arc::new(Mutex::new(Vec::new())),
            chat_input: String::new(),
            chat_loading: Arc::new(AtomicBool::new(false)),
            chat_loading_since: Arc::new(Mutex::new(None)),
            chat_auto_scroll: true,
            chat_agent_index: 0,
            chat_selected_agent: None,
            chat_selected_project: None,
            available_agents: Vec::new(),
            chat_scroll: 0,
            chat_scroll_max: 0,
            chat_messages_rect: ratatui::layout::Rect::default(),
        }
    }

    /// Larger scroll step for PageUp/PageDown. Uses the messages rect
    /// height reported by the last render when available, falling back
    /// to a fixed jump otherwise.
    ///
    /// The messages rect has no borders, so its height is already the number
    /// of message rows on screen.
    fn page_step(&self) -> usize {
        let h = self.chat_messages_rect.height as usize;
        if h > 0 {
            h
        } else {
            10
        }
    }

    async fn load_models(&mut self) {
        match self.state.provider.list_models().await {
            Ok(models) => {
                self.available_models = models.into_iter().map(|m| m.name).collect();
                if !self.available_models.is_empty() && self.form_model.is_empty() {
                    self.form_model = self.available_models[0].clone();
                }
            }
            Err(_) => {
                self.available_models = Vec::new();
            }
        }
    }

    fn trigger_analysis(&self, project_id: String, project_path: String) {
        let state = self.state.clone();
        let analysis_state = self.analysis_state.clone();

        // Set loading state immediately
        let analysis_state_clone = analysis_state.clone();
        tokio::spawn(async move {
            *analysis_state_clone.lock().unwrap() = AnalysisState::Loading {
                started_at: Instant::now(),
            };
        });

        tokio::spawn(async move {
            let provider = state.provider.clone();
            let responder = move |prompt: String| {
                let provider = provider.clone();
                Box::pin(async move {
                    use crate::domain::models::ChatRequest;
                    let chat_req = ChatRequest {
                        model: std::env::var("DEFAULT_MODEL")
                            .unwrap_or_else(|_| "llama3".to_string()),
                        messages: vec![crate::domain::models::ChatMessage {
                            role: "user".to_string(),
                            content: prompt,
                        }],
                        stream: false,
                    };
                    provider
                        .chat(chat_req)
                        .await
                        .map(|r| r.message.content)
                        .map_err(|e| e.to_string())
                })
                    as std::pin::Pin<
                        Box<dyn std::future::Future<Output = Result<String, String>> + Send>,
                    >
            };

            let analyzer =
                crate::context::analyzer::ProjectAnalyzer::new(state.skill_manager.clone());
            log::info!("Starting TUI Analyze for {}...", project_id);

            let result = analyzer
                .analyze(&project_id, &project_path, responder)
                .await;

            let final_state = match result {
                Ok(ctx) => {
                    let project_type = ctx.project_type.clone();
                    let context_excerpt: String = ctx.context_md.chars().take(3000).collect();
                    let _ = state.context_store.save_context(ctx);
                    log::info!("TUI Analysis complete for {}", project_id);

                    // Propose skills; the user decides whether anything is written.
                    let proposals = crate::services::skill_generation::generate_proposals(
                        state.provider.clone(),
                        &state.default_model,
                        &project_id,
                        &project_type,
                        &context_excerpt,
                    )
                    .await;

                    if proposals.is_empty() {
                        AnalysisState::Loaded(
                            "Analysis complete! Context saved. No skill proposals.".to_string(),
                        )
                    } else {
                        AnalysisState::Proposals {
                            project_path: project_path.clone(),
                            proposals,
                            selected: 0,
                            results: Vec::new(),
                        }
                    }
                }
                Err(e) => {
                    log::error!("TUI Analysis failed: {}", e);
                    AnalysisState::Error(format!("Analysis failed: {}", e))
                }
            };

            *analysis_state.lock().unwrap() = final_state;
        });
    }

    fn move_proposal_selection(&self, delta: isize) {
        let mut lock = self.analysis_state.lock().unwrap();
        if let AnalysisState::Proposals {
            proposals,
            selected,
            ..
        } = &mut *lock
        {
            if proposals.is_empty() {
                return;
            }
            let len = proposals.len() as isize;
            let next = (*selected as isize + delta).rem_euclid(len);
            *selected = next as usize;
        }
    }

    /// Human-readable outcome of an approval, for the results list.
    fn describe_write(outcome: &crate::services::skill_generation::WriteOutcome) -> String {
        use crate::services::skill_generation::WriteOutcome;
        match outcome {
            WriteOutcome::Written(path) => format!("written: {}", path.display()),
            WriteOutcome::RejectedInvalidId(reason) => {
                format!("refused (unsafe id): {reason}")
            }
            WriteOutcome::RejectedHandwritten(id) => {
                format!("refused: '{id}' already exists and was written by hand")
            }
        }
    }

    fn approve_selected_proposal(&mut self) {
        use crate::services::skill_generation::write_proposal as write;
        let mut lock = self.analysis_state.lock().unwrap();
        let AnalysisState::Proposals {
            project_path,
            proposals,
            selected,
            results,
        } = &mut *lock
        else {
            return;
        };
        let Some(proposal) = proposals.get(*selected).cloned() else {
            return;
        };
        match write(std::path::Path::new(project_path), &proposal) {
            Ok(outcome) => {
                let line = Self::describe_write(&outcome);
                results.push(line);
                proposals.remove(*selected);
                if *selected >= proposals.len() {
                    *selected = proposals.len().saturating_sub(1);
                }
            }
            Err(err) => results.push(format!("failed: {err}")),
        }
        let done = proposals.is_empty();
        drop(lock);
        if done {
            // Re-scan so the freshly written skills are discoverable at once.
            self.state.skill_manager.scan_and_load();
        }
    }

    fn approve_all_proposals(&mut self) {
        use crate::services::skill_generation::write_proposal as write;
        let mut lock = self.analysis_state.lock().unwrap();
        let AnalysisState::Proposals {
            project_path,
            proposals,
            results,
            ..
        } = &mut *lock
        else {
            return;
        };
        let root = std::path::Path::new(project_path).to_path_buf();
        let pending = std::mem::take(proposals);
        for proposal in &pending {
            match write(&root, proposal) {
                Ok(outcome) => results.push(Self::describe_write(&outcome)),
                Err(err) => results.push(format!("failed: {err}")),
            }
        }
        drop(lock);
        self.state.skill_manager.scan_and_load();
    }

    fn discard_selected_proposal(&mut self) {
        let mut lock = self.analysis_state.lock().unwrap();
        let AnalysisState::Proposals {
            proposals,
            selected,
            results,
            ..
        } = &mut *lock
        else {
            return;
        };
        let Some(proposal) = proposals.get(*selected).cloned() else {
            return;
        };
        results.push(format!("discarded: {}", proposal.id));
        proposals.remove(*selected);
        if *selected >= proposals.len() {
            *selected = proposals.len().saturating_sub(1);
        }
    }

    fn refresh_available_agents(&mut self) {
        let mut agents: Vec<String> = self
            .state
            .agent_registry
            .list_agents()
            .into_iter()
            .map(|a| a.id)
            .collect();
        agents.sort();
        agents.insert(0, "Direct (no agent)".to_string());
        self.available_agents = agents;
    }

    /// The key hints the shared footer shows.
    ///
    /// This is the single place hint text lives. No `self`: the text depends
    /// only on which keys the screen answers to, which keeps it reachable from a
    /// test without a terminal or an `AppState`.
    fn hints_for(target: KeyTarget) -> Vec<(&'static str, &'static str)> {
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
            // Unreachable through `key_target`, which resolves the form to
            // `AgentForm(field)` above. Kept honest rather than merged into one of
            // the other arms: this is the row that would name a false `Enter` on
            // seven of the eight fields.
            KeyTarget::View(CurrentView::AgentForm) => Self::hints_for(KeyTarget::AgentForm(0)),
        }
    }

    /// Returns only agents belonging to the currently selected project, in the order
    /// the projects view lists them.
    ///
    /// Routed through [`crate::tui::views::projects::agents_of_project`] rather
    /// than filtered here: `agent_index` addresses this list *and* the view's, so
    /// two filters would have to agree by luck. They used to be two filters, and
    /// the registry they read is a `HashMap` walk, so nothing held them together.
    fn project_agents(&self) -> Vec<crate::domain::agent::Agent> {
        let projects = self.state.context_store.list_all_projects();
        let selected_project_id = projects
            .get(self.project_index)
            .map(|p| p.project_id.as_str());
        crate::tui::views::projects::agents_of_project(&self.state, selected_project_id)
    }

    fn update_chat_selected_agent(&mut self) {
        if self.available_agents.is_empty() || self.chat_agent_index == 0 {
            self.chat_selected_agent = None;
            self.chat_selected_project = None;
        } else {
            self.chat_selected_agent = Some(self.available_agents[self.chat_agent_index].clone());
            self.chat_selected_project = self.project_of(&self.chat_selected_agent.clone());
        }
    }

    /// The project an agent belongs to, if any (global agents have none).
    fn project_of(&self, agent_id: &Option<String>) -> Option<String> {
        let id = agent_id.as_ref()?;
        self.state
            .agent_registry
            .list_agents()
            .into_iter()
            .find(|a| &a.id == id)
            .and_then(|a| a.project_id)
    }

    fn send_chat_message(&mut self) {
        let input = std::mem::take(&mut self.chat_input);
        if input.trim().is_empty() {
            return;
        }

        self.chat_messages
            .lock()
            .unwrap()
            .push(("user".to_string(), input.clone()));
        self.chat_loading.store(true, Ordering::SeqCst);
        *self.chat_loading_since.lock().unwrap() = Some(Instant::now());
        self.chat_auto_scroll = true;

        let state = self.state.clone();
        let messages = self.chat_messages.clone();
        let loading = self.chat_loading.clone();
        let loading_since = self.chat_loading_since.clone();
        let agent_id = self.chat_selected_agent.clone();
        let agent_project_id = agent_id.as_ref().and_then(|aid| {
            self.state
                .agent_registry
                .list_agents()
                .into_iter()
                .find(|a| a.id == *aid)
                .and_then(|a| a.project_id)
        });

        tokio::spawn(async move {
            let result = if let Some(ref aid) = agent_id {
                if let Some(runtime) = &state.agent_runtime {
                    // Prior turns already in the UI buffer (excluding the just-appended user msg).
                    let history: Vec<ChatMessage> = {
                        let msgs = messages.lock().unwrap();
                        msgs.iter()
                            .rev()
                            .skip(1) // drop current user turn
                            .map(|(role, content)| ChatMessage {
                                role: role.clone(),
                                content: content.clone(),
                            })
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect()
                    };
                    runtime
                        .chat(RuntimeChatRequest {
                            project_id: agent_project_id,
                            agent_id: Some(aid.clone()),
                            conversation_id: None,
                            user_message: input,
                            history,
                            model_override: None,
                            system_prompt: None,
                            default_model: state.default_model.clone(),
                        })
                        .await
                        .map_err(|e| e.to_string())
                } else {
                    Err("Agent runtime not available (rig-engine feature disabled)".to_string())
                }
            } else {
                let req = crate::domain::models::ChatRequest {
                    model: state.default_model.clone(),
                    messages: vec![ChatMessage {
                        role: "user".to_string(),
                        content: input,
                    }],
                    stream: false,
                };
                match state.provider.chat(req).await {
                    Ok(resp) => Ok(resp.message.content),
                    Err(e) => Err(e.to_string()),
                }
            };

            match result {
                Ok(text) => {
                    messages
                        .lock()
                        .unwrap()
                        .push(("assistant".to_string(), text));
                }
                Err(e) => {
                    messages
                        .lock()
                        .unwrap()
                        .push(("assistant".to_string(), format!("Error: {e}")));
                }
            }
            loading.store(false, Ordering::SeqCst);
            *loading_since.lock().unwrap() = None;
        });
    }

    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        loop {
            if self.current_view == CurrentView::Chat && self.chat_auto_scroll {
                // Sentinel: stick to the bottom. The renderer reports the
                // real `scroll_max` back to us; the scroll handlers then
                // operate against that bound instead of the magic count.
                self.chat_scroll = usize::MAX;
            }

            terminal.draw(|f| {
                // The chrome owns the layout: the bar and footer come off the
                // top and bottom, and views only ever see what is left.
                let c = chrome::layout(f.area());
                // The two halves come from one decision, not from two screens'
                // state: see `bar_context`.
                let ctx = bar_context(
                    self.state
                        .context_store
                        .list_all_projects()
                        .get(self.project_index)
                        .map(|p| p.project_id.clone()),
                    self.chat_selected_agent.clone(),
                    self.chat_selected_project.clone(),
                );
                // Read once, for the view and for the footer: the footer has to
                // answer for the analysis state as well as the view, or the
                // proposal modal's keys are on no row at all.
                let analysis = self
                    .analysis_state
                    .try_lock()
                    .map(|g| g.clone())
                    .unwrap_or(AnalysisState::Idle);
                let hints = Self::hints_for(key_target(
                    &self.current_view,
                    self.active_in_project_list,
                    &analysis,
                    self.project_list_bounds,
                    self.form_field_index,
                    self.confirm_delete.is_some(),
                ));
                chrome::render_bar(
                    f,
                    c.bar,
                    &VIEW_NAMES,
                    self.current_view.index(),
                    &ctx,
                    self.state.api_running.load(Ordering::SeqCst),
                );
                match self.current_view {
                    CurrentView::Dashboard => {
                        render_dashboard(f, c.body, &self.state, self.log_scroll)
                    }
                    CurrentView::Projects => {
                        // Render-time feedback: the two lists share the body, so
                        // at some project counts one of them gets no rows and the
                        // keys that drive it have to know. Same reason
                        // `chat_scroll_max` exists.
                        let bounds = crate::tui::views::projects::render_projects(
                            f,
                            c.body,
                            &self.state,
                            self.project_index,
                            self.agent_index,
                            self.active_in_project_list,
                        );
                        self.project_list_bounds = bounds;
                    }
                    CurrentView::AgentForm => {
                        crate::tui::views::agent_form::render_agent_form(
                            f,
                            c.body,
                            &self.form_id,
                            &self.form_name,
                            &self.form_model,
                            &self.form_project_id,
                            &self.form_rules,
                            &self.form_optimize_rules,
                            &self.form_skills,
                            &self.form_prompt,
                            self.form_field_index,
                        );
                    }
                    CurrentView::Analysis => {
                        crate::tui::views::analysis::render_analysis(f, c.body, &analysis);
                    }
                    CurrentView::ContextView => {
                        crate::tui::views::context::render_context(
                            f,
                            c.body,
                            &self.state,
                            self.project_index,
                            self.context_scroll,
                        );
                    }
                    CurrentView::Chat => {
                        let messages = self
                            .chat_messages
                            .lock()
                            .map(|g| g.clone())
                            .unwrap_or_default();
                        let loading = self.chat_loading.load(Ordering::SeqCst);
                        let loading_since = if loading {
                            *self.chat_loading_since.lock().unwrap()
                        } else {
                            None
                        };
                        let (scroll_max, msg_rect) = render_chat(
                            f,
                            c.body,
                            &messages,
                            &self.chat_input,
                            loading,
                            loading_since,
                            &self.chat_selected_agent,
                            &self.chat_selected_project,
                            &self.available_agents,
                            self.chat_agent_index,
                            self.chat_scroll,
                        );
                        self.chat_scroll_max = scroll_max;
                        self.chat_messages_rect = msg_rect;
                    }
                }
                if let Some((ref confirm_type, ref confirm_id)) = self.confirm_delete {
                    render_confirm_delete(f, c.body, confirm_type, confirm_id);
                }
                chrome::render_footer(f, c.footer, &hints);
            })?;

            if event::poll(std::time::Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) => {
                        if key.kind != event::KeyEventKind::Press {
                            continue;
                        }
                        if self.current_view == CurrentView::AgentForm {
                            match key.code {
                                KeyCode::Esc => self.current_view = CurrentView::Projects,
                                KeyCode::Tab | KeyCode::Down => {
                                    self.form_field_index = (self.form_field_index + 1) % 8
                                }
                                KeyCode::BackTab | KeyCode::Up => {
                                    self.form_field_index = if self.form_field_index == 0 {
                                        7
                                    } else {
                                        self.form_field_index - 1
                                    }
                                }
                                KeyCode::Left | KeyCode::Right => {
                                    if self.form_field_index == 2
                                        && !self.available_models.is_empty()
                                    {
                                        let current_idx = self
                                            .available_models
                                            .iter()
                                            .position(|m| m == &self.form_model)
                                            .unwrap_or(0);
                                        let next_idx = if key.code == KeyCode::Right {
                                            (current_idx + 1) % self.available_models.len()
                                        } else {
                                            if current_idx == 0 {
                                                self.available_models.len() - 1
                                            } else {
                                                current_idx - 1
                                            }
                                        };
                                        self.form_model = self.available_models[next_idx].clone();
                                    } else if self.form_field_index == 3
                                        && !self.available_projects.is_empty()
                                    {
                                        let current_idx = self
                                            .available_projects
                                            .iter()
                                            .position(|p| p == &self.form_project_id)
                                            .unwrap_or(0);
                                        let next_idx = if key.code == KeyCode::Right {
                                            (current_idx + 1) % self.available_projects.len()
                                        } else {
                                            if current_idx == 0 {
                                                self.available_projects.len() - 1
                                            } else {
                                                current_idx - 1
                                            }
                                        };
                                        self.form_project_id =
                                            self.available_projects[next_idx].clone();
                                    }
                                }
                                KeyCode::Enter => {
                                    if self.form_field_index == 7 {
                                        self.form_prompt.push('\n');
                                    } else {
                                        self.form_field_index = (self.form_field_index + 1) % 8;
                                    }
                                }
                                KeyCode::Char('s')
                                    if key.modifiers.contains(event::KeyModifiers::CONTROL) =>
                                {
                                    // Save agent
                                    let project_id = &self.form_project_id;
                                    let rules_vec: Vec<String> = self
                                        .form_rules
                                        .split(',')
                                        .map(|s| s.trim().to_string())
                                        .filter(|s| !s.is_empty())
                                        .collect();
                                    let opt_rules_vec: Vec<String> = self
                                        .form_optimize_rules
                                        .split(',')
                                        .map(|s| s.trim().to_string())
                                        .filter(|s| !s.is_empty())
                                        .collect();
                                    let skills_vec: Vec<String> = self
                                        .form_skills
                                        .split(',')
                                        .map(|s| s.trim().to_string())
                                        .filter(|s| !s.is_empty())
                                        .collect();

                                    let rules_str = if rules_vec.is_empty() {
                                        "[]".to_string()
                                    } else {
                                        format!("{:?}", rules_vec)
                                    };
                                    let opt_rules_str = if opt_rules_vec.is_empty() {
                                        "[]".to_string()
                                    } else {
                                        format!("{:?}", opt_rules_vec)
                                    };
                                    let skills_str = if skills_vec.is_empty() {
                                        "[]".to_string()
                                    } else {
                                        format!("{:?}", skills_vec)
                                    };

                                    let toml_content = format!(
                                    "name = \"{}\"\nmodel = \"{}\"\nsystem_prompt = \"\"\"\n{}\n\"\"\"\ncontext_project = \"{}\"\nrules = {}\nskills = {}\n\n[optimize]\nenabled = true\nrules = {}\n",
                                    self.form_name, self.form_model, self.form_prompt, project_id, rules_str, skills_str, opt_rules_str
                                );
                                    let path =
                                        crate::core::paths::get_project_agents_dir(project_id)
                                            .join(format!("{}.toml", self.form_id));
                                    let _ = std::fs::write(path, toml_content);
                                    let _ = self.state.agent_registry.reload_all(&[]);
                                    self.current_view = CurrentView::Projects;
                                }
                                KeyCode::Char(c) => match self.form_field_index {
                                    0 => self.form_id.push(c),
                                    1 => self.form_name.push(c),
                                    2 => self.form_model.push(c),
                                    3 => {} // Project is handled by arrows
                                    4 => self.form_rules.push(c),
                                    5 => self.form_optimize_rules.push(c),
                                    6 => self.form_skills.push(c),
                                    7 => self.form_prompt.push(c),
                                    _ => {}
                                },
                                KeyCode::Backspace => match self.form_field_index {
                                    0 => {
                                        self.form_id.pop();
                                    }
                                    1 => {
                                        self.form_name.pop();
                                    }
                                    2 => {
                                        self.form_model.pop();
                                    }
                                    4 => {
                                        self.form_rules.pop();
                                    }
                                    5 => {
                                        self.form_optimize_rules.pop();
                                    }
                                    6 => {
                                        self.form_skills.pop();
                                    }
                                    7 => {
                                        self.form_prompt.pop();
                                    }
                                    _ => {}
                                },
                                _ => {}
                            }
                            continue;
                        }

                        // Confirm delete interception
                        if self.confirm_delete.is_some() {
                            match key.code {
                                KeyCode::Char('y') | KeyCode::Enter => {
                                    let action = self.confirm_delete.take();
                                    if let Some((ref confirm_type, ref confirm_id)) = action {
                                        match confirm_type.as_str() {
                                            "project" => {
                                                let _ = self
                                                    .state
                                                    .context_store
                                                    .delete_context(confirm_id);
                                                self.project_index = 0;
                                            }
                                            "agent" => {
                                                let agents =
                                                    self.state.agent_registry.list_agents();
                                                if let Some(agent) =
                                                    agents.iter().find(|a| a.id == *confirm_id)
                                                {
                                                    let path = if let Some(ref pid) =
                                                        agent.project_id
                                                    {
                                                        crate::core::paths::get_project_agents_dir(
                                                            pid,
                                                        )
                                                        .join(format!("{}.toml", agent.id))
                                                    } else {
                                                        crate::core::paths::get_agents_dir()
                                                            .join(format!("{}.toml", agent.id))
                                                    };
                                                    let _ = std::fs::remove_file(path);
                                                    let _ =
                                                        self.state.agent_registry.reload_all(&[]);
                                                }
                                                self.agent_index = 0;
                                                // Not just the index: the focus has
                                                // to leave a list that may now be
                                                // empty, or the footer keeps
                                                // naming the agent keys over rows
                                                // that are gone.
                                                self.active_in_project_list =
                                                    focus_after_agent_delete();
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                KeyCode::Char('n') | KeyCode::Esc => {
                                    self.confirm_delete = None;
                                }
                                _ => {}
                            }
                            continue;
                        }

                        // Chat view handling
                        if self.current_view == CurrentView::Chat {
                            // Composing the message comes first, and it is
                            // routed by `chat_input_action` so the Enter vs
                            // Shift+Enter rule is testable without a terminal.
                            match chat_input_action(key, self.chat_loading.load(Ordering::SeqCst)) {
                                ChatAction::Edit(c) => self.chat_input.push(c),
                                ChatAction::Backspace => {
                                    self.chat_input.pop();
                                }
                                ChatAction::Newline => self.chat_input.push('\n'),
                                ChatAction::Send => self.send_chat_message(),
                                ChatAction::Other => {}
                            }

                            // Everything below needs app state: navigation,
                            // agent cycling, and scrolling the messages.
                            match key.code {
                                KeyCode::Esc => {
                                    self.current_view = CurrentView::Projects;
                                }
                                KeyCode::Tab | KeyCode::BackTab => {
                                    // Let Tab fall through to main navigation
                                }
                                KeyCode::Left => {
                                    self.refresh_available_agents();
                                    let count = self.available_agents.len();
                                    if count > 0 {
                                        self.chat_agent_index = if self.chat_agent_index == 0 {
                                            count - 1
                                        } else {
                                            self.chat_agent_index - 1
                                        };
                                        self.update_chat_selected_agent();
                                    }
                                }
                                KeyCode::Right => {
                                    self.refresh_available_agents();
                                    let count = self.available_agents.len();
                                    if count > 0 {
                                        self.chat_agent_index = (self.chat_agent_index + 1) % count;
                                        self.update_chat_selected_agent();
                                    }
                                }
                                KeyCode::Up => {
                                    if self.chat_auto_scroll {
                                        // Jumping out of follow mode puts us at
                                        // the bottom, so one step up = last row.
                                        self.chat_scroll = self.chat_scroll_max;
                                        self.chat_auto_scroll = false;
                                    } else if self.chat_scroll > 0 {
                                        self.chat_scroll -= 1;
                                    }
                                }
                                KeyCode::Down => {
                                    self.chat_auto_scroll = false;
                                    if self.chat_scroll >= self.chat_scroll_max {
                                        // Reached the bottom: re-arm follow mode
                                        // so new messages keep flowing in.
                                        self.chat_auto_scroll = true;
                                    } else {
                                        self.chat_scroll = self.chat_scroll.saturating_add(1);
                                    }
                                }
                                KeyCode::PageUp => {
                                    if self.chat_auto_scroll {
                                        // Start paging up from the bottom.
                                        self.chat_scroll = self.chat_scroll_max;
                                    }
                                    self.chat_auto_scroll = false;
                                    self.chat_scroll =
                                        self.chat_scroll.saturating_sub(self.page_step());
                                }
                                KeyCode::PageDown => {
                                    self.chat_auto_scroll = false;
                                    self.chat_scroll =
                                        self.chat_scroll.saturating_add(self.page_step());
                                    if self.chat_scroll >= self.chat_scroll_max {
                                        self.chat_scroll = self.chat_scroll_max;
                                        self.chat_auto_scroll = true;
                                    }
                                }
                                KeyCode::Home => {
                                    self.chat_auto_scroll = false;
                                    self.chat_scroll = 0;
                                }
                                KeyCode::End => {
                                    self.chat_auto_scroll = true;
                                    self.chat_scroll = usize::MAX;
                                }
                                _ => {}
                            }
                            if key.code != KeyCode::Tab && key.code != KeyCode::BackTab {
                                continue;
                            }
                        }

                        match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Tab => {
                                self.current_view = match self.current_view {
                                    CurrentView::Dashboard => CurrentView::Projects,
                                    CurrentView::Projects => CurrentView::Chat,
                                    CurrentView::Chat => CurrentView::Dashboard,
                                    _ => self.current_view.clone(),
                                };
                                if self.current_view == CurrentView::Chat {
                                    self.refresh_available_agents();
                                    self.chat_auto_scroll = true;
                                }
                            }
                            KeyCode::Char('j') | KeyCode::Down => {
                                if self.current_view == CurrentView::Projects {
                                    if self.active_in_project_list {
                                        let count =
                                            self.state.context_store.list_all_projects().len();
                                        if count > 0 {
                                            self.project_index = (self.project_index + 1) % count;
                                            self.agent_index = 0;
                                        }
                                    } else {
                                        self.agent_index = move_agent_selection(
                                            self.project_list_bounds,
                                            self.agent_index,
                                            self.project_agents().len(),
                                            1,
                                        );
                                    }
                                } else if self.current_view == CurrentView::Dashboard {
                                    self.log_scroll = self.log_scroll.saturating_add(1);
                                } else if self.current_view == CurrentView::ContextView {
                                    self.context_scroll = self.context_scroll.saturating_add(1);
                                }
                            }
                            KeyCode::Char('k') | KeyCode::Up => {
                                if self.current_view == CurrentView::Projects {
                                    if self.active_in_project_list {
                                        let count =
                                            self.state.context_store.list_all_projects().len();
                                        if count > 0 {
                                            self.project_index = if self.project_index == 0 {
                                                count - 1
                                            } else {
                                                self.project_index - 1
                                            };
                                            self.agent_index = 0;
                                        }
                                    } else {
                                        self.agent_index = move_agent_selection(
                                            self.project_list_bounds,
                                            self.agent_index,
                                            self.project_agents().len(),
                                            -1,
                                        );
                                    }
                                } else if self.current_view == CurrentView::Dashboard {
                                    self.log_scroll = self.log_scroll.saturating_sub(1);
                                } else if self.current_view == CurrentView::ContextView {
                                    self.context_scroll = self.context_scroll.saturating_sub(1);
                                }
                            }
                            // Focus moves between the two lists the projects view draws: the project
                            // rows and the agent rows below them. Which way it may go depends on
                            // where the focus is now — see `may_move_list_focus`, which is the
                            // whole of the rule.
                            KeyCode::Char('l')
                            | KeyCode::Right
                            | KeyCode::Char('h')
                            | KeyCode::Left => {
                                if self.current_view == CurrentView::Projects
                                    && may_move_list_focus(
                                        self.project_list_bounds,
                                        self.active_in_project_list,
                                    )
                                {
                                    self.active_in_project_list = !self.active_in_project_list;
                                }
                            }
                            KeyCode::Char('a') => {
                                if self.current_view == CurrentView::Projects {
                                    let projects = self.state.context_store.list_all_projects();
                                    if let Some(project) = projects.get(self.project_index) {
                                        let project_id = project.project_id.clone();
                                        let project_path = project.path.clone();
                                        self.trigger_analysis(project_id, project_path);
                                        self.current_view = CurrentView::Analysis;
                                    }
                                }
                            }
                            KeyCode::Char('v') => {
                                if self.current_view == CurrentView::Projects {
                                    self.current_view = CurrentView::ContextView;
                                }
                            }
                            KeyCode::Char('d') => {
                                if self.current_view == CurrentView::Projects {
                                    if self.active_in_project_list {
                                        let projects = self.state.context_store.list_all_projects();
                                        if let Some(project) = projects.get(self.project_index) {
                                            self.confirm_delete = Some((
                                                "project".to_string(),
                                                project.project_id.clone(),
                                            ));
                                        }
                                    } else if let Some(agent) = selected_agent(
                                        self.project_list_bounds,
                                        &self.project_agents(),
                                        self.agent_index,
                                    ) {
                                        self.confirm_delete =
                                            Some(("agent".to_string(), agent.id.clone()));
                                    }
                                }
                            }
                            KeyCode::Char('n') => {
                                if self.current_view == CurrentView::Projects {
                                    self.form_id = String::new();
                                    self.form_name = String::new();
                                    self.form_model = String::new();
                                    self.form_prompt = String::new();
                                    self.form_rules = String::new();
                                    self.form_optimize_rules = String::new();
                                    self.form_skills = String::new();
                                    self.available_projects = self
                                        .state
                                        .context_store
                                        .list_all_projects()
                                        .into_iter()
                                        .map(|c| c.project_id)
                                        .collect();
                                    if let Some(p) = self.available_projects.get(self.project_index)
                                    {
                                        self.form_project_id = p.clone();
                                    } else {
                                        self.form_project_id = String::new();
                                    }
                                    self.form_field_index = 0;
                                    self.editing_agent = None;
                                    self.current_view = CurrentView::AgentForm;
                                    self.load_models().await;
                                }
                            }
                            KeyCode::Char('e') => {
                                if self.current_view == CurrentView::Projects
                                    && !self.active_in_project_list
                                {
                                    let agents = self.project_agents();
                                    if let Some(agent) = selected_agent(
                                        self.project_list_bounds,
                                        &agents,
                                        self.agent_index,
                                    ) {
                                        self.form_id = agent.id.clone();
                                        self.form_name = agent.config.name.clone();
                                        self.form_model = agent.config.model.clone();
                                        self.form_prompt = agent.config.system_prompt.clone();
                                        self.form_rules = agent.config.rules.join(", ");
                                        self.form_optimize_rules =
                                            agent.config.optimize.rules.join(", ");
                                        self.form_skills = agent.config.skills.join(", ");
                                        self.form_project_id =
                                            agent.project_id.clone().unwrap_or_default();
                                        self.available_projects =
                                            if let Some(ref pid) = agent.project_id {
                                                vec![pid.clone()]
                                            } else {
                                                self.state
                                                    .context_store
                                                    .list_all_projects()
                                                    .into_iter()
                                                    .map(|c| c.project_id)
                                                    .collect()
                                            };
                                        self.form_field_index = 0;
                                        self.editing_agent = Some(agent.id.clone());
                                        self.current_view = CurrentView::AgentForm;
                                        self.load_models().await;
                                    }
                                }
                            }
                            _ => {}
                        }

                        // Handle Analysis view keys
                        if self.current_view == CurrentView::Analysis {
                            match key.code {
                                KeyCode::Esc | KeyCode::Tab => {
                                    self.current_view = CurrentView::Projects
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    self.move_proposal_selection(-1)
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    self.move_proposal_selection(1)
                                }
                                KeyCode::Enter => self.approve_selected_proposal(),
                                KeyCode::Char('a') => self.approve_all_proposals(),
                                KeyCode::Char('d') => self.discard_selected_proposal(),
                                KeyCode::Char('r') => {
                                    // Re-analyze current project
                                    let projects = self.state.context_store.list_all_projects();
                                    if let Some(project) = projects.get(self.project_index) {
                                        let project_id = project.project_id.clone();
                                        let project_path = project.path.clone();
                                        self.trigger_analysis(project_id, project_path);
                                    }
                                }
                                _ => {}
                            }
                            continue;
                        }

                        // Handle ContextView keys
                        if self.current_view == CurrentView::ContextView {
                            match key.code {
                                KeyCode::Esc => self.current_view = CurrentView::Projects,
                                KeyCode::Tab => self.current_view = CurrentView::Projects,
                                KeyCode::Up | KeyCode::Char('k') => {
                                    self.context_scroll = self.context_scroll.saturating_sub(1);
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    self.context_scroll = self.context_scroll.saturating_add(1);
                                }
                                _ => {}
                            }
                            continue;
                        }
                    }
                    Event::Mouse(mouse) => {
                        // Mouse wheel scrolling over the chat messages region.
                        if self.current_view != CurrentView::Chat
                            || !self
                                .chat_messages_rect
                                .contains((mouse.column, mouse.row).into())
                        {
                            continue;
                        }
                        match mouse.kind {
                            event::MouseEventKind::ScrollUp => {
                                if self.chat_auto_scroll {
                                    self.chat_scroll = self.chat_scroll_max;
                                }
                                self.chat_auto_scroll = false;
                                self.chat_scroll = self.chat_scroll.saturating_sub(3);
                            }
                            event::MouseEventKind::ScrollDown => {
                                self.chat_auto_scroll = false;
                                self.chat_scroll = self.chat_scroll.saturating_add(3);
                                if self.chat_scroll >= self.chat_scroll_max {
                                    self.chat_scroll = self.chat_scroll_max;
                                    self.chat_auto_scroll = true;
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }

        // Restore terminal
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;

        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
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

    /// `key_target` with the delete confirmation down and the form's caret at
    /// field 0.
    ///
    /// Every screen below is one with no dialog over it, and naming the two extra
    /// axes here rather than at eight call sites is what keeps the argument list
    /// honest: a test that wants the confirmation up, or a field, calls
    /// `key_target`.
    fn keys_for(
        view: &CurrentView,
        active_in_project_list: bool,
        analysis: &AnalysisState,
        projects_bounds: ListBounds,
    ) -> KeyTarget {
        key_target(
            view,
            active_in_project_list,
            analysis,
            projects_bounds,
            0,
            false,
        )
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
            let hints = TuiApp::hints_for(target.clone());
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
        let hints = TuiApp::hints_for(target);
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
        let hints = TuiApp::hints_for(KeyTarget::Proposals);
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
                key_target(
                    &CurrentView::AgentForm,
                    true,
                    &AnalysisState::Idle,
                    ListBounds::default(),
                    field,
                    false
                ),
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

    /// **The delete confirmation is the same defect, on the sibling modal.** Its
    /// handler takes *every* key while it is up — it is consulted before the chat
    /// and before `q`, and every branch `continue`s — so the projects row
    /// underneath named seven keys that did nothing, `q: quit` most visibly of
    /// all. Nothing in the view says it is up: only the projects view opens it, and
    /// the projects view stays current underneath, so `key_target` has to be told.
    #[test]
    fn the_delete_confirmation_owns_the_footer_while_it_is_up() {
        let idle = AnalysisState::Idle;
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
                key_target(&view, focus, &idle, drawn, 0, true),
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
            !TuiApp::hints_for(KeyTarget::View(CurrentView::Projects))
                .iter()
                .any(|(key, _)| *key == "e"),
            "`e` edits an agent and must not be advertised while the agent list is not selected"
        );

        assert_eq!(
            keys_for(&CurrentView::Projects, false, &idle, drawn),
            KeyTarget::ProjectAgents
        );
        let agents = TuiApp::hints_for(KeyTarget::ProjectAgents);
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
}
