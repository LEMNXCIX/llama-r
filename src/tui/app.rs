use crate::api::handlers::AppState;
use crate::tui::views::dashboard::render_dashboard;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::sync::Arc;

#[derive(PartialEq)]
pub enum CurrentView {
    Dashboard,
    Projects,
    AgentForm,
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
    form_field_index: usize, // 0: ID, 1: Name, 2: Model, 3: Prompt
    editing_agent: Option<String>,
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
            form_field_index: 0,
            editing_agent: None,
        }
    }

    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        loop {
            terminal.draw(|f| match self.current_view {
                CurrentView::Dashboard => render_dashboard(f, &self.state),
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
                        &self.form_prompt,
                        self.form_field_index,
                    );
                }
            })?;

            if event::poll(std::time::Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != event::KeyEventKind::Press {
                        continue;
                    }
                    if self.current_view == CurrentView::AgentForm {
                        match key.code {
                            KeyCode::Esc => self.current_view = CurrentView::Projects,
                            KeyCode::Tab | KeyCode::Down => {
                                self.form_field_index = (self.form_field_index + 1) % 4
                            }
                            KeyCode::Up => {
                                self.form_field_index = if self.form_field_index == 0 {
                                    3
                                } else {
                                    self.form_field_index - 1
                                }
                            }
                            KeyCode::Enter => {
                                // Save agent
                                let projects = self.state.context_store.list_contexts();
                                if let Some(project) = projects.get(self.project_index) {
                                    let project_id = &project.project_id;
                                    let toml_content = format!(
                                        "name = \"{}\"\nmodel = \"{}\"\nsystem_prompt = \"\"\"\n{}\n\"\"\"\ncontext_project = \"{}\"\n",
                                        self.form_name, self.form_model, self.form_prompt, project_id
                                    );
                                    let path =
                                        crate::core::paths::get_project_agents_dir(project_id)
                                            .join(format!("{}.toml", self.form_id));
                                    let _ = std::fs::write(path, toml_content);
                                    let _ = self.state.agent_manager.load_agents();
                                }
                                self.current_view = CurrentView::Projects;
                            }
                            KeyCode::Char(c) => match self.form_field_index {
                                0 => self.form_id.push(c),
                                1 => self.form_name.push(c),
                                2 => self.form_model.push(c),
                                3 => self.form_prompt.push(c),
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
                                3 => {
                                    self.form_prompt.pop();
                                }
                                _ => {}
                            },
                            _ => {}
                        }
                        continue;
                    }

                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Tab => {
                            self.current_view = match self.current_view {
                                CurrentView::Dashboard => CurrentView::Projects,
                                CurrentView::Projects => CurrentView::Dashboard,
                                CurrentView::AgentForm => CurrentView::AgentForm,
                            };
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            if self.current_view == CurrentView::Projects {
                                if self.active_in_project_list {
                                    let count = self.state.context_store.list_contexts().len();
                                    if count > 0 {
                                        self.project_index = (self.project_index + 1) % count;
                                        self.agent_index = 0;
                                    }
                                } else {
                                    // Handle agent navigation logic later
                                    self.agent_index = self.agent_index.saturating_add(1);
                                }
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            if self.current_view == CurrentView::Projects {
                                if self.active_in_project_list {
                                    let count = self.state.context_store.list_contexts().len();
                                    if count > 0 {
                                        self.project_index = if self.project_index == 0 {
                                            count - 1
                                        } else {
                                            self.project_index - 1
                                        };
                                        self.agent_index = 0;
                                    }
                                } else {
                                    self.agent_index = self.agent_index.saturating_sub(1);
                                }
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
                                let projects = self.state.context_store.list_contexts();
                                if let Some(project) = projects.get(self.project_index) {
                                    let project_id = project.project_id.clone();
                                    let project_path = project.path.clone();
                                    let state = self.state.clone();

                                    tokio::spawn(async move {
                                        let provider = state.provider.clone();
                                        let responder = move |prompt: String| {
                                            let provider = provider.clone();
                                            Box::pin(async move {
                                                use crate::domain::models::ChatRequest;
                                                let chat_req = ChatRequest {
                                                    model: std::env::var("DEFAULT_MODEL")
                                                        .unwrap_or_else(|_| "llama3".to_string()),
                                                    messages: vec![
                                                        crate::domain::models::ChatMessage {
                                                            role: "user".to_string(),
                                                            content: prompt,
                                                        },
                                                    ],
                                                    stream: false,
                                                };
                                                provider
                                                    .chat(chat_req)
                                                    .await
                                                    .map(|r| r.message.content)
                                                    .map_err(|e| e.to_string())
                                            })
                                                as std::pin::Pin<
                                                    Box<
                                                        dyn std::future::Future<
                                                                Output = Result<String, String>,
                                                            > + Send,
                                                    >,
                                                >
                                        };

                                        let analyzer =
                                            crate::context::analyzer::ProjectAnalyzer::new(
                                                state.skill_manager.clone(),
                                            );
                                        log::info!("Starting TUI Analyze for {}...", project_id);
                                        match analyzer
                                            .analyze(&project_id, &project_path, responder)
                                            .await
                                        {
                                            Ok(ctx) => {
                                                let _ = state.context_store.save_context(ctx);
                                                log::info!(
                                                    "TUI Analysis complete for {}",
                                                    project_id
                                                );
                                            }
                                            Err(e) => log::error!("TUI Analysis failed: {}", e),
                                        }
                                    });
                                }
                            }
                        }
                        KeyCode::Char('d') => {
                            if self.current_view == CurrentView::Projects {
                                if self.active_in_project_list {
                                    let projects = self.state.context_store.list_contexts();
                                    if let Some(project) = projects.get(self.project_index) {
                                        let _ = self
                                            .state
                                            .context_store
                                            .delete_context(&project.project_id);
                                        self.project_index = 0;
                                    }
                                } else {
                                    // Delete agent
                                    let projects = self.state.context_store.list_contexts();
                                    if let Some(project) = projects.get(self.project_index) {
                                        let project_id = &project.project_id;
                                        let agents: Vec<_> = self
                                            .state
                                            .agent_manager
                                            .list_agents()
                                            .into_iter()
                                            .filter(|a| {
                                                a.config.context_project.as_ref()
                                                    == Some(project_id)
                                            })
                                            .collect();
                                        if let Some(agent) = agents.get(self.agent_index) {
                                            let path = crate::core::paths::get_project_agents_dir(
                                                project_id,
                                            )
                                            .join(format!("{}.toml", agent.id));
                                            let _ = std::fs::remove_file(path);
                                            let _ = self.state.agent_manager.load_agents();
                                            self.agent_index = 0;
                                        }
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
                                self.form_field_index = 0;
                                self.editing_agent = None;
                                self.current_view = CurrentView::AgentForm;
                            }
                        }
                        KeyCode::Char('e') => {
                            if self.current_view == CurrentView::Projects
                                && !self.active_in_project_list
                            {
                                let projects = self.state.context_store.list_contexts();
                                if let Some(project) = projects.get(self.project_index) {
                                    let project_id = &project.project_id;
                                    let agents: Vec<_> = self
                                        .state
                                        .agent_manager
                                        .list_agents()
                                        .into_iter()
                                        .filter(|a| {
                                            a.config.context_project.as_ref() == Some(project_id)
                                        })
                                        .collect();
                                    if let Some(agent) = agents.get(self.agent_index) {
                                        self.form_id = agent.id.clone();
                                        self.form_name = agent.config.name.clone();
                                        self.form_model = agent.config.model.clone();
                                        self.form_prompt = agent.config.system_prompt.clone();
                                        self.form_field_index = 0;
                                        self.editing_agent = Some(agent.id.clone());
                                        self.current_view = CurrentView::AgentForm;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
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
