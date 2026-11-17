use crate::api::handlers::AppState;
use crate::domain::models::ChatMessage;
use crate::services::agent_runtime::RuntimeChatRequest;
use crate::tui::chrome;
use crate::tui::keys::{
    chat_input_action, focus_after_agent_delete, hints_for, key_target, may_move_list_focus,
    move_agent_selection, rule_editor_opening, selected_agent, AnalysisState, ChatAction,
    CurrentView, ScreenState, OPTIMIZE_FIELD, RULES_FIELD, SKILLS_FIELD, VIEW_NAMES,
};
use crate::tui::views::chat::render_chat;
use crate::tui::views::dashboard::render_dashboard;
use crate::tui::views::modals::render_confirm_delete;
use crate::tui::views::modals::{render_opt_tools, render_rule_editor, render_skill_picker};
use crate::tui::views::opt_tools::{opt_tools_action, OptTools, OptToolsAction};
use crate::tui::views::projects::ListBounds;
use crate::tui::views::rule_editor::{rule_action, RuleAction};
use crate::tui::views::skill_picker::{picker_action, PickerAction};
use crate::tui::views::skill_picker::{InstalledSkill, SkillPicker};

/// The installed skills a picker over `project_id` should offer.
///
/// The manager wants a project's *path* and the form holds an id. Handing it the
/// id would make it look for `fudi/skills` relative to the working directory —
/// a directory that does not exist — so the project's own skills would vanish
/// from the picker and only the shared ones would show, with nothing saying so.
///
/// An id with no saved context has no path to resolve, so this falls back to the
/// unscoped listing rather than reporting none: the user is choosing from what is
/// installed, and an unsaved project still has shared skills to pick.
///
/// Takes the store and the manager rather than the whole `AppState`, so the
/// resolution is testable without standing up a runtime.
fn available_skills(
    store: &crate::context::store::ContextStore,
    manager: &crate::services::skill_manager::SkillManager,
    project_id: &str,
) -> Vec<crate::domain::models::Skill> {
    match store.get_context(project_id) {
        Some(context) => manager.list_skills_for_project(Path::new(&context.path)),
        None => manager.list_skills(),
    }
}
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
    form_rules: Vec<String>,
    /// The agent's `optimize.rules`, as a list. This used to be a `String` joined
    /// with commas, which made a name that matched no tool indistinguishable from
    /// one that did — and a name that matched nothing was a silent no-op.
    form_optimize_rules: Vec<String>,
    /// `optimize.enabled`. Set by the tool picker, and following its selection: the
    /// form used to write `true` whether or not anything was selected, producing a
    /// config that claimed to be optimising while running nothing.
    form_optimize_enabled: bool,
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
    /// The skill picker, while it is up. Dropping it is how `Esc` cancels, so
    /// there is no undo stack to keep in sync with the form behind it.
    skill_picker: Option<crate::tui::views::skill_picker::SkillPicker>,
    /// The rule editor, while it is up. `None` is the normal state; the form's own
    /// row for the rules is a one-line join of the list this holds.
    rule_editor: Option<crate::tui::views::rule_editor::RuleEditor>,
    /// The optimization-tool picker, while it is up.
    opt_tools: Option<crate::tui::views::opt_tools::OptTools>,
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

/// What a single key dispatch asks the event loop to do.
pub(super) enum DispatchOutcome {
    Continue,
    Quit,
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
            form_rules: Vec::new(),
            form_optimize_rules: Vec::new(),
            form_optimize_enabled: false,
            form_skills: String::new(),
            rule_editor: None,
            opt_tools: None,
            form_project_id: String::new(),
            form_field_index: 0,
            available_projects: Vec::new(),
            available_models: Vec::new(),
            editing_agent: None,
            analysis_state: Arc::new(Mutex::new(AnalysisState::Idle)),
            log_scroll: 0,
            context_scroll: 0,
            confirm_delete: None,
            skill_picker: None,
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

    /// Load the model list from a synchronous key handler.
    ///
    /// `load_models` is async only because the provider call is. Key dispatch
    /// is synchronous so a plain test can inject a key, so the future is
    /// driven on the current runtime from a blocking thread — the models are
    /// in place before the key handler returns, exactly as before.
    fn load_models_blocking(&mut self) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(self.load_models()));
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

    /// The picker over what is installed for `project_id`, with the agent's
    /// current skills ticked and the analysis-chosen ones marked untouchable.
    ///
    /// Read from `state.skill_manager` in process rather than over HTTP: the TUI
    /// and the runtime share one `AppState`, so a request would be a round trip
    /// to this same object. `GET /api/skills` exists for callers that cannot do
    /// that.
    fn skill_picker_over(&self, project_id: &str) -> SkillPicker {
        let available: Vec<InstalledSkill> = available_skills(
            &self.state.context_store,
            &self.state.skill_manager,
            project_id,
        )
        .iter()
        .map(Into::into)
        .collect();

        let chosen: Vec<String> = self
            .form_skills
            .split(',')
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();

        SkillPicker::for_agent(available, &chosen, &self.editing_auto_skills())
    }

    /// The `auto_skills` of the agent being edited, and none when creating one —
    /// there is no agent yet, so there is nothing for the analysis to have chosen.
    fn editing_auto_skills(&self) -> Vec<String> {
        let Some(agent_id) = &self.editing_agent else {
            return Vec::new();
        };
        self.state
            .agent_registry
            .get_agent(agent_id)
            .map(|agent| agent.config.auto_skills.clone())
            .unwrap_or_default()
    }

    /// The one-line summary the form shows for the skills field.
    ///
    /// A count and the ids, rather than the bare comma list the field used to
    /// hold: that list was both the display and the data, so a long catalogue ran
    /// past the value column with nothing to say it had been cut.
    fn skills_summary(&self) -> String {
        let chosen: Vec<&str> = self
            .form_skills
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .collect();
        match chosen.len() {
            0 => "none — press s to choose".to_string(),
            n => format!("{n} skills: {}", chosen.join(", ")),
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

    /// What the footer row is being drawn for right now.
    ///
    /// One place reads the app's fields, so a new axis is added here and nowhere
    /// else. `analysis` is behind a lock that a poisoned mutex must not take the
    /// whole frame down with it, so a failed `try_lock` reads as `Idle` — which
    /// is what the draw already did before this existed.
    fn screen_state<'a>(&'a self, analysis: &'a AnalysisState) -> ScreenState<'a> {
        ScreenState {
            view: &self.current_view,
            active_in_project_list: self.active_in_project_list,
            analysis,
            projects_bounds: self.project_list_bounds,
            form_field_index: self.form_field_index,
            confirm_delete: self.confirm_delete.is_some(),
            skill_picker: self.skill_picker.is_some(),
            rule_editor: self.rule_editor.is_some(),
            opt_tools: self.opt_tools.is_some(),
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
                let hints = hints_for(key_target(&self.screen_state(&analysis)));
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
                            &Self::rules_line(&self.form_rules),
                            &Self::optimize_line(&self.form_optimize_rules),
                            &self.skills_summary(),
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
                if let Some(picker) = &self.opt_tools {
                    render_opt_tools(f, c.body, picker, &hints);
                }
                if let Some(editor) = &self.rule_editor {
                    render_rule_editor(f, c.body, editor, &hints);
                }
                if let Some(picker) = &self.skill_picker {
                    render_skill_picker(f, c.body, picker, &hints);
                }
                if let Some((ref confirm_type, ref confirm_id)) = self.confirm_delete {
                    render_confirm_delete(f, c.body, confirm_type, confirm_id);
                }
                // Silent while the picker is up: the panel already draws these
                // same hints, through the same renderer, next to the list they
                // apply to. Saying them twice in one screen is how a footer stops
                // being read.
                let hints: &[(&str, &str)] =
                    if self.skill_picker.is_some() || self.rule_editor.is_some() {
                        &[]
                    } else {
                        &hints
                    };
                chrome::render_footer(f, c.footer, hints);
            })?;

            if event::poll(std::time::Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) => {
                        if key.kind != event::KeyEventKind::Press {
                            continue;
                        }
                        if let DispatchOutcome::Quit = self.dispatch(key) {
                            break;
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

    /// One key press, handled against the current screen. `run` owns the
    /// terminal and the event loop; this only decides what the key means, so
    /// the whole key map can be exercised without a TTY.
    fn dispatch(&mut self, key: event::KeyEvent) -> DispatchOutcome {
        // Optimization tool picker interception
        //
        // Above the other dialogs for the same reason: the first guard to match
        // swallows the key, and this one opens from the agent form like
        // the rest of them.
        if self.opt_tools.is_some() {
            let Some(picker) = self.opt_tools.as_mut() else {
                unreachable!("guarded on is_some above");
            };
            match opt_tools_action(picker, key.code) {
                OptToolsAction::Applied => {}
                OptToolsAction::Save { tools, enabled } => {
                    self.opt_tools = None;
                    self.form_optimize_rules = tools;
                    self.form_optimize_enabled = enabled;
                }
                OptToolsAction::Cancel => self.opt_tools = None,
            }
            return DispatchOutcome::Continue;
        }
        // Rule editor interception
        //
        // Above the skill picker's guard, not below it: both open
        // from the agent form, and the order between two dialogs
        // matters for the same reason it matters between a dialog
        // and a view — whichever guard comes first swallows the key.
        if self.rule_editor.is_some() {
            let Some(editor) = self.rule_editor.as_mut() else {
                unreachable!("guarded on is_some above");
            };
            match rule_action(editor, key) {
                RuleAction::Applied => {}
                RuleAction::Done(rules) => {
                    self.rule_editor = None;
                    self.form_rules = rules;
                }
            }
            return DispatchOutcome::Continue;
        }
        // Delete confirmation interception
        if self.confirm_delete.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    let action = self.confirm_delete.take();
                    if let Some((ref confirm_type, ref confirm_id)) = action {
                        match confirm_type.as_str() {
                            "project" => {
                                let _ = self.state.context_store.delete_context(confirm_id);
                                self.project_index = 0;
                            }
                            "agent" => {
                                let agents = self.state.agent_registry.list_agents();
                                if let Some(agent) = agents.iter().find(|a| a.id == *confirm_id) {
                                    let path = if let Some(ref pid) = agent.project_id {
                                        crate::core::paths::get_project_agents_dir(pid)
                                            .join(format!("{}.toml", agent.id))
                                    } else {
                                        crate::core::paths::get_agents_dir()
                                            .join(format!("{}.toml", agent.id))
                                    };
                                    let _ = std::fs::remove_file(path);
                                    let _ = self.state.agent_registry.reload_all(&[]);
                                }
                                self.agent_index = 0;
                                // Not just the index: the focus has
                                // to leave a list that may now be
                                // empty, or the footer keeps
                                // naming the agent keys over rows
                                // that are gone.
                                self.active_in_project_list = focus_after_agent_delete();
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
            return DispatchOutcome::Continue;
        }
        // Skill picker interception
        if self.skill_picker.is_some() {
            let Some(picker) = self.skill_picker.as_mut() else {
                unreachable!("guarded on is_some above");
            };
            // The key map lives in `picker_action` so it can be
            // tested: this arm used to spell the rules out inline,
            // where no test could reach them — and where, written
            // below the form's own guard, none of them ran.
            match picker_action(picker, key.code) {
                PickerAction::Applied => {}
                PickerAction::Save(ids) => {
                    self.skill_picker = None;
                    self.form_skills = ids.join(", ");
                }
                PickerAction::Cancel => self.skill_picker = None,
            }
            return DispatchOutcome::Continue;
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
                    if self.form_field_index == 2 && !self.available_models.is_empty() {
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
                    } else if self.form_field_index == 3 && !self.available_projects.is_empty() {
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
                        self.form_project_id = self.available_projects[next_idx].clone();
                    }
                }
                KeyCode::Enter => {
                    if self.form_field_index == 7 {
                        self.form_prompt.push('\n');
                    } else {
                        self.form_field_index = (self.form_field_index + 1) % 8;
                    }
                }
                KeyCode::Char('s') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                    // Save agent
                    let project_id = &self.form_project_id;
                    // Already a list. It used to be joined with
                    // commas and split here, which is what made a
                    // rule containing a comma silently become two
                    // rules with nothing on screen saying so.
                    let rules_vec: Vec<String> = self.form_rules.clone();
                    // Already a list. It used to be joined with commas
                    // and split here, which is what made a tool name
                    // containing a comma indistinguishable from two
                    // tools — and a name matching no tool was already
                    // a silent no-op, so the split only added ways to
                    // get one by accident.
                    let opt_rules_vec: Vec<String> = self.form_optimize_rules.clone();
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
                    "name = \"{}\"\nmodel = \"{}\"\nsystem_prompt = \"\"\"\n{}\n\"\"\"\ncontext_project = \"{}\"\nrules = {}\nskills = {}\n\n[optimize]\nenabled = {}\nrules = {}\n",
                    self.form_name, self.form_model, self.form_prompt, project_id, rules_str, skills_str, self.form_optimize_enabled, opt_rules_str
                );
                    let path = crate::core::paths::get_project_agents_dir(project_id)
                        .join(format!("{}.toml", self.form_id));
                    let _ = std::fs::write(path, toml_content);
                    let _ = self.state.agent_registry.reload_all(&[]);
                    self.current_view = CurrentView::Projects;
                }
                // The skills field is not typed into. `s` opens
                // the picker instead, because `Enter` already
                // means "next field" here and cannot be freed.
                KeyCode::Char('s') if self.form_field_index == SKILLS_FIELD => {
                    self.skill_picker = Some(self.skill_picker_over(&self.form_project_id));
                }
                // Opens the tool picker. Modified, because `o` is a
                // letter the user would otherwise type into the field.
                KeyCode::Char('o')
                    if self.form_field_index == OPTIMIZE_FIELD
                        && key.modifiers.contains(event::KeyModifiers::CONTROL) =>
                {
                    self.open_opt_tools_with(None);
                }
                // Opens the rule editor over the list this field
                // joins into a line. Modified, because a bare letter
                // here would be a letter the user could not type into
                // their own rules.
                KeyCode::Char('r')
                    if self.form_field_index == RULES_FIELD
                        && key.modifiers.contains(event::KeyModifiers::CONTROL) =>
                {
                    self.open_rule_editor_with(None);
                }
                KeyCode::Char(c) => match self.form_field_index {
                    0 => self.form_id.push(c),
                    1 => self.form_name.push(c),
                    2 => self.form_model.push(c),
                    3 => {} // Project is handled by arrows
                    // The rules row is a one-line join of a list, so
                    // there is nothing here to append to. Swallowing
                    // the key would make the row look editable and
                    // do nothing — the field that looked broken when
                    // the picker shipped inert. Typing here opens the
                    // editor and lands the character in it.
                    4 => self.open_rule_editor_with(Some(c)),
                    5 => self.open_opt_tools_with(Some(c)),
                    6 => {} // Skills is chosen in the picker
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
                        // Opens the editor and deletes nothing.
                        // Backspacing the join is ambiguous — the
                        // last character may belong to the last
                        // rule, or it may be the separator — and
                        // the editor is where a deletion can be
                        // seen before it happens.
                        self.open_rule_editor_with(None);
                    }
                    5 => self.open_opt_tools_with(None),
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
            return DispatchOutcome::Continue;
        }

        // Confirm delete interception

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
                    self.chat_scroll = self.chat_scroll.saturating_sub(self.page_step());
                }
                KeyCode::PageDown => {
                    self.chat_auto_scroll = false;
                    self.chat_scroll = self.chat_scroll.saturating_add(self.page_step());
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
                return DispatchOutcome::Continue;
            }
        }

        match key.code {
            KeyCode::Char('q') => return DispatchOutcome::Quit,
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
                        let count = self.state.context_store.list_all_projects().len();
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
                        let count = self.state.context_store.list_all_projects().len();
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
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Char('h') | KeyCode::Left => {
                if self.current_view == CurrentView::Projects
                    && may_move_list_focus(self.project_list_bounds, self.active_in_project_list)
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
                            self.confirm_delete =
                                Some(("project".to_string(), project.project_id.clone()));
                        }
                    } else if let Some(agent) = selected_agent(
                        self.project_list_bounds,
                        &self.project_agents(),
                        self.agent_index,
                    ) {
                        self.confirm_delete = Some(("agent".to_string(), agent.id.clone()));
                    }
                }
            }
            KeyCode::Char('n') => {
                if self.current_view == CurrentView::Projects {
                    self.form_id = String::new();
                    self.form_name = String::new();
                    self.form_model = String::new();
                    self.form_prompt = String::new();
                    self.form_rules = Vec::new();
                    self.form_optimize_rules = Vec::new();
                    self.form_optimize_enabled = false;
                    self.form_skills = String::new();
                    self.available_projects = self
                        .state
                        .context_store
                        .list_all_projects()
                        .into_iter()
                        .map(|c| c.project_id)
                        .collect();
                    if let Some(p) = self.available_projects.get(self.project_index) {
                        self.form_project_id = p.clone();
                    } else {
                        self.form_project_id = String::new();
                    }
                    self.form_field_index = 0;
                    self.editing_agent = None;
                    self.current_view = CurrentView::AgentForm;
                    self.load_models_blocking();
                }
            }
            KeyCode::Char('e')
                if self.current_view == CurrentView::Projects && !self.active_in_project_list =>
            {
                let agents = self.project_agents();
                if let Some(agent) =
                    selected_agent(self.project_list_bounds, &agents, self.agent_index)
                {
                    self.form_id = agent.id.clone();
                    self.form_name = agent.config.name.clone();
                    self.form_model = agent.config.model.clone();
                    self.form_prompt = agent.config.system_prompt.clone();
                    self.form_rules = agent.config.rules.clone();
                    self.form_optimize_rules = agent.config.optimize.rules.clone();
                    self.form_optimize_enabled = agent.config.optimize.enabled;
                    self.form_skills = agent.config.skills.join(", ");
                    self.form_project_id = agent.project_id.clone().unwrap_or_default();
                    self.available_projects = if let Some(ref pid) = agent.project_id {
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
                    self.load_models_blocking();
                }
            }
            _ => {}
        }

        // Handle Analysis view keys
        if self.current_view == CurrentView::Analysis {
            match key.code {
                KeyCode::Esc | KeyCode::Tab => self.current_view = CurrentView::Projects,
                KeyCode::Up | KeyCode::Char('k') => self.move_proposal_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_proposal_selection(1),
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
            return DispatchOutcome::Continue;
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
            return DispatchOutcome::Continue;
        }
        DispatchOutcome::Continue
    }

    /// The editor to open for `rules`, with `first` already typed into the focused
    /// rule.
    ///
    /// Three things open it: `Ctrl+R` deliberately, and typing or backspacing on the
    /// rules row not so much. The row is a one-line join of a list, so there is no
    /// caret position in it to append at — and a row that looks editable, swallows
    /// every keypress and shows nothing is the same failure the skill picker shipped,
    /// in a different field. Landing the character in the editor it opens is what the
    /// user meant by pressing it.
    fn open_rule_editor_with(&mut self, first: Option<char>) {
        self.rule_editor = Some(rule_editor_opening(&self.form_rules, first));
    }

    /// Open the optimization-tool picker over the agent's tools, optionally with a
    /// character already in the filter.
    ///
    /// Three things open it, as with the rules editor: `Ctrl+O` deliberately, and
    /// typing or backspacing on the field not so much. The row is a one-line join of
    /// a list, so there is nothing in it to append at, and a field that looks
    /// editable, swallows every keypress and shows nothing is the picker-inert bug
    /// in another field.
    fn open_opt_tools_with(&mut self, first: Option<char>) {
        let mut picker = OptTools::for_agent(&self.form_optimize_rules);
        if let Some(c) = first {
            picker.push_filter_char(c);
        }
        self.opt_tools = Some(picker);
    }

    /// The one-line view of an agent's optimization tools, for the form's own row.
    ///
    /// A rendering, not the truth: `form_optimize_rules` is the list and
    /// `TokenOptimizer` applies it. `Ctrl+O` is how the whole thing is chosen.
    fn optimize_line(rules: &[String]) -> String {
        if rules.is_empty() {
            "none".to_string()
        } else {
            rules.join(", ")
        }
    }

    /// The one-line view of an agent's rules, for the form's own row.
    ///
    /// A rendering, not the truth: `form_rules` is the list and `analyzer.rs` renders
    /// each element as its own bullet. The join is what the form can afford to draw on
    /// one row, and `Ctrl+R` is how the whole thing is seen and written.
    fn rules_line(rules: &[String]) -> String {
        rules.join(", ")
    }
}

#[cfg(test)]
mod tests;
