use crate::api::handlers::AppState;
use crate::domain::models::ChatMessage;
use crate::services::agent_runtime::RuntimeChatRequest;
use crate::tui::chrome;
use crate::tui::views::chat::render_chat;
use crate::tui::views::dashboard::render_dashboard;
use crate::tui::views::projects::render_confirm_delete;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(PartialEq, Clone)]
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

#[derive(Clone)]
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

pub struct TuiApp {
    state: Arc<AppState>,
    current_view: CurrentView,
    project_index: usize,
    agent_index: usize,
    active_in_project_list: bool,
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

    /// The key hints the shared footer shows for `view`.
    ///
    /// This is the single place hint text lives: the per-view footers that
    /// used to repeat it are on their way out. No `self`: the text does not
    /// depend on app state, which keeps it reachable from a test.
    fn hints_for(view: CurrentView) -> Vec<(&'static str, &'static str)> {
        match view {
            CurrentView::Dashboard => {
                vec![("Tab", "next view"), ("↑/↓", "scroll logs"), ("q", "quit")]
            }
            // One row, and a hint that does not fit is clipped from the right,
            // which takes `q: quit` first — the one key nobody can do without.
            // Sized to fit 80 columns. Two hints are absent because what they
            // act on is not drawn here: `e` edits an agent and `←/→` toggles
            // focus to the agent list (`app.rs:1066`), neither of which this
            // view shows. `design.md:103` advertises `n`, `a` and `d` for this
            // screen. Both keys still work in the app; they are just not
            // advertised. `↑/↓` is advertised instead, because it moves the
            // selection in the one list that *is* drawn.
            CurrentView::Projects => vec![
                ("Tab", "switch list"),
                ("↑/↓", "select"),
                ("a", "analyze"),
                ("n", "new agent"),
                ("d", "delete"),
                ("q", "quit"),
            ],
            CurrentView::AgentForm => vec![
                ("Tab", "next field"),
                ("Shift+Tab", "prev field"),
                ("Enter", "newline in prompt"),
                ("←/→", "cycle project"),
                ("Ctrl+S", "save"),
                ("Esc", "cancel"),
            ],
            CurrentView::Analysis => vec![("Esc", "back"), ("r", "re-analyze")],
            CurrentView::ContextView => vec![("Esc", "back"), ("↑/↓", "scroll")],
            // One row, and a hint that does not fit is clipped from the right, which
            // takes `Esc: back` first. Sized to fit 80 columns: `PgUp/PgDn`
            // still scrolls, it is just not advertised — six hints fit, seven
            // do not.
            CurrentView::Chat => vec![
                ("←/→", "agent"),
                ("Enter", "send"),
                ("Shift+Enter", "newline"),
                ("↑↓", "scroll"),
                ("Tab", "view"),
                ("Esc", "back"),
            ],
        }
    }

    /// Returns only agents belonging to the currently selected project.
    fn project_agents(&self) -> Vec<crate::domain::agent::Agent> {
        let projects = self.state.context_store.list_all_projects();
        let selected_project_id = projects
            .get(self.project_index)
            .map(|p| p.project_id.as_str());

        let all_agents = self.state.agent_registry.list_agents();
        all_agents
            .into_iter()
            .filter(|a| match (&a.project_id, selected_project_id) {
                (Some(pid), Some(sel)) => pid.as_str() == sel,
                _ => false,
            })
            .collect()
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
                let ctx = chrome::Context {
                    project: self
                        .state
                        .context_store
                        .list_all_projects()
                        .get(self.project_index)
                        .map(|p| p.project_id.clone()),
                    agent: self.chat_selected_agent.clone(),
                };
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
                        crate::tui::views::projects::render_projects(
                            f,
                            c.body,
                            &self.state,
                            self.project_index,
                            self.agent_index,
                            self.active_in_project_list,
                        );
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
                        let analysis_state = self
                            .analysis_state
                            .try_lock()
                            .map(|g| g.clone())
                            .unwrap_or(AnalysisState::Idle);
                        crate::tui::views::analysis::render_analysis(f, c.body, &analysis_state);
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
                chrome::render_footer(f, c.footer, &TuiApp::hints_for(self.current_view.clone()));
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
                                        let agent_count = self.project_agents().len();
                                        if agent_count > 0 {
                                            self.agent_index = (self.agent_index + 1) % agent_count;
                                        }
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
                                        let agent_count = self.project_agents().len();
                                        if agent_count > 0 {
                                            self.agent_index = if self.agent_index == 0 {
                                                agent_count - 1
                                            } else {
                                                self.agent_index - 1
                                            };
                                        }
                                    }
                                } else if self.current_view == CurrentView::Dashboard {
                                    self.log_scroll = self.log_scroll.saturating_sub(1);
                                } else if self.current_view == CurrentView::ContextView {
                                    self.context_scroll = self.context_scroll.saturating_sub(1);
                                }
                            }
                            KeyCode::Char('l')
                            | KeyCode::Right
                            | KeyCode::Char('h')
                            | KeyCode::Left => {
                                if self.current_view == CurrentView::Projects {
                                    self.active_in_project_list = !self.active_in_project_list;
                                }
                            }
                            // Placeholder for actions
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
                                    } else {
                                        let agents = self.project_agents();
                                        if let Some(agent) = agents.get(self.agent_index) {
                                            self.confirm_delete =
                                                Some(("agent".to_string(), agent.id.clone()));
                                        }
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
                                    if let Some(agent) = agents.get(self.agent_index) {
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

/// The old tab bar, superseded by [`chrome::render_bar`].
///
/// No longer called. Kept until the remaining per-view chrome is deleted, so
/// the old chrome goes away in one sweep rather than half a step at a time.
#[allow(dead_code)]
fn render_tab_bar(f: &mut ratatui::Frame, current: &CurrentView) {
    let area = f.area();
    let bar_rect = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let active_style = |active: bool| {
        if active {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    };
    let bar = Paragraph::new(Line::from(vec![
        Span::styled(
            "  [Dashboard]",
            active_style(*current == CurrentView::Dashboard),
        ),
        Span::styled(
            "  [Projects]",
            active_style(*current == CurrentView::Projects),
        ),
        Span::styled("  [Chat]", active_style(*current == CurrentView::Chat)),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(bar, bar_rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyModifiers};
    use ratatui::backend::TestBackend;

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

    /// The chat footer is one row, and a hint that does not fit is clipped away
    /// from the right — which takes `Esc: back` first. An escape key nobody can
    /// see is a real loss, so the row is pinned at the default terminal width.
    #[test]
    fn the_chat_hints_fit_one_row() {
        let hints = TuiApp::hints_for(CurrentView::Chat);
        let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|f| chrome::render_footer(f, f.area(), &hints))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            row.contains("Esc: back"),
            "the escape hint must survive the 80-column row: {row:?}"
        );
        assert!(
            row.contains("Shift+Enter: newline"),
            "the newline hint is the point of the one-line input: {row:?}"
        );
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
            "the chat hints are {width} columns wide and get clipped: {row:?}"
        );
        println!("chat hints render {width} columns of the 80 available");
    }

    /// The same pin for the projects row, which was 86 columns wide and lost
    /// `q: quit` to the clip.
    ///
    /// The pin is the rendered row, not the hint list: `render_footer` clips at
    /// the area width with no ellipsis, so a hint pushed past 80 columns simply
    /// stops being drawn. Comparing the row against the text the hints describe
    /// fails the moment one of them is cut, which a `width <= 80` check cannot
    /// do — the buffer is 80 columns wide whatever was written into it.
    #[test]
    fn the_projects_hints_fit_one_row() {
        let hints = TuiApp::hints_for(CurrentView::Projects);
        let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|f| chrome::render_footer(f, f.area(), &hints))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect();

        // Measured from the hints, not from the row: the row is 80 columns wide
        // whatever was written into it, so measuring it there proves nothing.
        let width = hints
            .iter()
            .map(|(key, hint)| Line::from(format!("{key}: {hint}")).width() + 2)
            .sum::<usize>()
            - 2;
        assert!(
            width <= 80,
            "the projects hints are {width} columns wide and get clipped: {row:?}"
        );
        assert_eq!(
            row.trim_end(),
            hints
                .iter()
                .map(|(key, hint)| format!("{key}: {hint}"))
                .collect::<Vec<_>>()
                .join("  "),
            "every hint must survive the 80-column row"
        );
        println!("projects hints render {width} columns of the 80 available");
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
