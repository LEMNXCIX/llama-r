//! Every view is rendered with the real chrome and asserted to keep it.
//!
//! `tests/tui_chrome.rs` scans source text; this test drives the real render
//! functions into a `TestBackend` and reads the resulting buffer. Between them,
//! a view cannot lose the bar by laying out `f.area()` nor by writing over the
//! chrome after layout — the two ways the bar has been lost in this repo.

use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::handlers::AppState;
use llama_r::tui::chrome;
use llama_r::tui::keys::AnalysisState;
use llama_r::tui::views::analysis::render_analysis;
use llama_r::tui::views::chat::render_chat;
use llama_r::tui::views::context::render_context;
use llama_r::tui::views::dashboard::render_dashboard;
use llama_r::tui::views::projects::render_projects;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::collections::VecDeque;
use std::sync::Arc;

struct FakeProvider;

#[async_trait::async_trait]
impl llama_r::providers::LLMProvider for FakeProvider {
    fn get_base_url(&self) -> String {
        "http://fake".into()
    }
    async fn health_check(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
    async fn list_models(
        &self,
    ) -> Result<Vec<llama_r::domain::models::ModelInfo>, Box<dyn std::error::Error + Send + Sync>>
    {
        Ok(vec![llama_r::domain::models::ModelInfo {
            name: "llama3".into(),
            modified_at: "now".into(),
            size: 1,
        }])
    }
    async fn chat(
        &self,
        req: llama_r::domain::models::ChatRequest,
    ) -> Result<llama_r::domain::models::ChatResponse, Box<dyn std::error::Error + Send + Sync>>
    {
        Ok(llama_r::domain::models::ChatResponse {
            model: req.model,
            created_at: "now".into(),
            message: llama_r::domain::models::ChatMessage {
                role: "assistant".into(),
                content: "ok".into(),
            },
            done: true,
            usage: None,
            served_by: None,
            debug_prompt: None,
            debug_route: None,
            conversation_id: None,
        })
    }
    async fn chat_stream(
        &self,
        _req: llama_r::domain::models::ChatRequest,
    ) -> Result<
        std::pin::Pin<
            Box<
                dyn tokio_stream::Stream<
                        Item = Result<
                            llama_r::domain::models::ChatStreamEvent,
                            Box<dyn std::error::Error + Send + Sync>,
                        >,
                    > + Send,
            >,
        >,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        Err("not used".into())
    }
}

fn state() -> Arc<AppState> {
    // Other integration binaries set LLAMA_R_DIR to their own temp dir; point
    // this one at a fresh one so a parallel test cannot change what we render.
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());
    std::mem::forget(dir); // keep the path valid for the whole test
    let stack =
        llama_r::services::provider_stack::ProviderStack::new(Arc::new(FakeProvider), "fallback");
    llama_r::runtime::build_app_state(
        stack,
        Arc::new(llama_r::services::agent_registry::AgentRegistry::new()),
        Arc::new(llama_r::services::skill_manager::SkillManager::new()),
        Arc::new(llama_r::context::store::ContextStore::new()),
        Arc::new(std::sync::Mutex::new(VecDeque::new())),
        Vec::new(),
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn llama_r::ports::mcp::McpServerRegistry>,
        None,
    )
}

/// Render one view into a real terminal buffer and return every row as text.
fn render(
    name: &str,
    view: &dyn Fn(&mut ratatui::Frame, ratatui::layout::Rect, &AppState),
) -> String {
    let state = state();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|f| {
            let c = chrome::layout(f.area());
            chrome::render_bar(
                f,
                c.bar,
                &[
                    "Dashboard",
                    "Projects",
                    "Agent",
                    "Analysis",
                    "Context",
                    "Chat",
                ],
                0,
                &llama_r::tui::chrome::Context::default(),
                true,
            );
            view(f, c.body, &state);
            chrome::render_footer(f, c.footer, &[("q", "quit"), ("Tab", "next view")]);
        })
        .unwrap_or_else(|e| panic!("{name} failed to render: {e}"));

    let buffer = terminal.backend().buffer();
    let text = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    // The chrome is fixed real estate: a view may not overwrite either row,
    // and the separator stays between the bar and the body.
    assert!(
        text.starts_with("Llama-R │ Dashboard │ Projects │ Agent │ Analysis │ Context │ Chat"),
        "{name}: the context bar was overwritten"
    );
    assert!(
        text.lines().last().unwrap().contains("q: quit"),
        "{name}: the footer was overwritten"
    );
    for corner in ['┌', '┐', '└', '┘'] {
        assert!(
            !text.contains(corner),
            "{name}: a box corner was drawn ({corner})"
        );
    }
    text
}

#[test]
fn every_view_renders_into_the_chrome() {
    let dashboard = render("dashboard", &|f, body, state| {
        render_dashboard(f, body, state, 0);
    });
    let _ = dashboard; // status row depends on live counters, asserted elsewhere

    let projects = render("projects", &|f, body, state| {
        render_projects(f, body, state, 0, 0, false);
    });
    // `render` already asserted the chrome survived; this view's content is
    // data-dependent (a project list may be empty) and asserted elsewhere.
    let _ = projects;

    let analysis = render("analysis", &|f, body, _| {
        render_analysis(f, body, &AnalysisState::Idle);
    });
    assert!(analysis.contains("No analysis running."));

    let context = render("context", &|f, body, state| {
        render_context(f, body, state, 0, 0);
    });
    let _ = context; // generated markdown depends on the project fixture

    let chat = render("chat", &|f, body, _| {
        let messages = vec![
            ("user".to_string(), "cancela el pedido 42".to_string()),
            (
                "assistant".to_string(),
                "He cancelado el pedido 42 correctamente.".to_string(),
            ),
        ];
        render_chat(
            f,
            body,
            &messages,
            "y dime el estado",
            true,
            Some(std::time::Instant::now()),
            &Some("ops".to_string()),
            &Some("fudi".to_string()),
            &["general".into(), "ops".into()],
            1,
            0,
        );
    });
    assert!(chat.contains("You:") && chat.contains("AI:") && chat.contains("Thinking"));
    assert!(chat.contains("> y dime el estado"));
}

/// The settings dialog is a modal over the dashboard, so it must keep the bar
/// it sits under and must never show the raw key. It keeps a border (it is a
/// dialog), so this cannot use `render`'s no-corners check — the check that
/// matters here is the chrome surviving and the secret staying masked.
#[test]
fn the_settings_dialog_keeps_the_chrome_and_masks_the_key() {
    let state = state();
    let mut settings = llama_r::tui::views::settings::Settings::new(
        llama_r::config::ProviderKind::OpenRouter,
        "sk-super-secret-999",
        "new-model",
    );
    settings.focus = llama_r::tui::views::settings::KEY_FIELD;
    let hints = llama_r::tui::keys::hints_for(llama_r::tui::keys::KeyTarget::Settings);

    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|f| {
            let c = chrome::layout(f.area());
            chrome::render_bar(
                f,
                c.bar,
                &[
                    "Dashboard",
                    "Projects",
                    "Agent",
                    "Analysis",
                    "Context",
                    "Chat",
                ],
                0,
                &llama_r::tui::chrome::Context::default(),
                true,
            );
            render_dashboard(f, c.body, &state, 0);
            llama_r::tui::views::modals::render_settings(f, c.body, &settings, &hints);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let text = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        text.starts_with("Llama-R │ Dashboard │ Projects │ Agent │ Analysis │ Context │ Chat"),
        "the bar must survive the settings modal: {text}"
    );
    assert!(
        !text.contains("sk-super-secret-999"),
        "the raw key must never reach the screen: {text}"
    );
    assert!(text.contains("(set)"), "the key is shown as set: {text}");
    assert!(text.contains("new-model"), "the model is editable: {text}");
    for (key, meaning) in &hints {
        assert!(
            text.contains(&format!("{key}: {meaning}")),
            "the dialog must show its own {key} hint: {text}"
        );
    }
}
