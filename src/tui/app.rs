use crate::api::handlers::AppState;
use crate::domain::models::ChatMessage;
use crate::services::agent_runtime::RuntimeChatRequest;
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

#[derive(Clone)]
pub enum AnalysisState {
    Idle,
    Loading { started_at: Instant },
    Loaded(String),
    Error(String),
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
            available_agents: Vec::new(),
            chat_scroll: 0,
            chat_scroll_max: 0,
            chat_messages_rect: ratatui::layout::Rect::default(),
        }
    }

    /// Larger scroll step for PageUp/PageDown. Uses the messages rect
    /// height reported by the last render when available, falling back
    /// to a fixed jump otherwise.
    fn page_step(&self) -> usize {
        let h = self.chat_messages_rect.height.saturating_sub(2) as usize;
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
                    let _ = state.context_store.save_context(ctx);
                    log::info!("TUI Analysis complete for {}", project_id);
                    AnalysisState::Loaded("Analysis complete! Context saved.".to_string())
                }
                Err(e) => {
                    log::error!("TUI Analysis failed: {}", e);
                    AnalysisState::Error(format!("Analysis failed: {}", e))
                }
            };

            *analysis_state.lock().unwrap() = final_state;
        });
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
        } else {
            self.chat_selected_agent = Some(self.available_agents[self.chat_agent_index].clone());
        }
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
                render_tab_bar(f, &self.current_view);
                match self.current_view {
                    CurrentView::Dashboard => render_dashboard(f, &self.state, self.log_scroll),
                    CurrentView::Projects => {
                        crate::tui::views::projects::render_projects(
                            f,
                            &self.state,
                            self.project_index,
                            self.agent_index,
                            self.active_in_project_list,
                        );
                    }
                    CurrentView::AgentForm => {
                        crate::tui::views::projects::render_agent_form(
                            f,
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
                        crate::tui::views::projects::render_analysis(f, &analysis_state);
                    }
                    CurrentView::ContextView => {
                        crate::tui::views::projects::render_context(
                            f,
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
                            &messages,
                            &self.chat_input,
                            loading,
                            loading_since,
                            &self.chat_selected_agent,
                            &self.available_agents,
                            self.chat_agent_index,
                            self.chat_scroll,
                        );
                        self.chat_scroll_max = scroll_max;
                        self.chat_messages_rect = msg_rect;
                    }
                }
                if let Some((ref confirm_type, ref confirm_id)) = self.confirm_delete {
                    render_confirm_delete(f, confirm_type, confirm_id);
                }
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
                                KeyCode::Enter => {
                                    if !self.chat_loading.load(Ordering::SeqCst) {
                                        self.send_chat_message();
                                    }
                                }
                                KeyCode::Backspace => {
                                    self.chat_input.pop();
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
                                KeyCode::Char(c) => {
                                    if !self.chat_loading.load(Ordering::SeqCst) {
                                        self.chat_input.push(c);
                                    }
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
                                KeyCode::Esc => self.current_view = CurrentView::Projects,
                                KeyCode::Tab => self.current_view = CurrentView::Projects,
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
