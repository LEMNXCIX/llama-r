use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use llama_r::context::store::ContextStore;
use llama_r::domain::agent::AgentConfig;
use llama_r::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use llama_r::providers::LLMProvider;
use llama_r::runtime::{build_app_state, build_router};
use llama_r::services::agent_manager::AgentManager;
use llama_r::services::skill_manager::SkillManager;
use std::collections::VecDeque;
use std::error::Error;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;
use tokio_stream::Stream;
use tower::ServiceExt;

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn test_lock() -> &'static Mutex<()> {
    TEST_LOCK.get_or_init(|| Mutex::new(()))
}

struct FakeProvider {
    models: Vec<ModelInfo>,
    fail_primary_once: AtomicBool,
}

impl FakeProvider {
    fn new() -> Self {
        Self {
            models: vec![
                ModelInfo {
                    name: "fallback-model".to_string(),
                    modified_at: "2026-03-09T00:00:00Z".to_string(),
                    size: 1,
                },
                ModelInfo {
                    name: "agent-model".to_string(),
                    modified_at: "2026-03-09T00:00:00Z".to_string(),
                    size: 1,
                },
            ],
            fail_primary_once: AtomicBool::new(true),
        }
    }
}

#[async_trait]
impl LLMProvider for FakeProvider {
    fn get_base_url(&self) -> String {
        "http://fake-provider".to_string()
    }

    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(self.models.clone())
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        if request.model == "agent-model" && self.fail_primary_once.swap(false, Ordering::SeqCst) {
            return Err("synthetic provider failure".into());
        }

        let content = if request.messages[0].content.contains("Analyze this software project") {
            "## Project Overview\nTest project\n\n## Architecture\nRouter + provider\n\n## Tech Stack\nRust\n\n## Development Rules\nPrefer safe errors\n\n## Key Conventions\nUse tests".to_string()
        } else if request.messages[0].content.contains("Select ONLY the skills") {
            "[]".to_string()
        } else {
            format!(
                "model={} role={} content={}",
                request.model, request.messages[0].role, request.messages[0].content
            )
        };

        Ok(ChatResponse {
            model: request.model,
            created_at: "2026-03-09T00:00:00Z".to_string(),
            message: ChatMessage {
                role: "assistant".to_string(),
                content,
            },
            done: true,
        })
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        let event = ChatStreamEvent {
            model: request.model,
            created_at: "2026-03-09T00:00:00Z".to_string(),
            message: ChatMessage {
                role: "assistant".to_string(),
                content: "stream".to_string(),
            },
            done: true,
        };
        Ok(Box::pin(tokio_stream::iter(vec![Ok(event)])))
    }
}

struct TestApp {
    _guard: MutexGuard<'static, ()>,
    temp_dir: TempDir,
    router: axum::Router,
    agent_manager: Arc<AgentManager>,
}

fn setup_app() -> TestApp {
    let guard = test_lock().lock().unwrap();
    let temp_dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", temp_dir.path());
    std::env::set_var("DEFAULT_MODEL", "fallback-model");

    std::fs::create_dir_all(temp_dir.path().join("agents")).unwrap();
    std::fs::create_dir_all(temp_dir.path().join("contextos/projects")).unwrap();

    let agent_manager = Arc::new(AgentManager::new());
    agent_manager.load_agents().unwrap();
    let skill_manager = Arc::new(SkillManager::new());
    let context_store = Arc::new(ContextStore::new());
    let provider = Arc::new(FakeProvider::new());
    let logs = Arc::new(Mutex::new(VecDeque::new()));
    let state = build_app_state(
        provider,
        agent_manager.clone(),
        skill_manager,
        context_store,
        "fallback-model".to_string(),
        logs,
    );
    let router = build_router(state);

    TestApp {
        _guard: guard,
        temp_dir,
        router,
        agent_manager,
    }
}

#[tokio::test]
async fn health_should_report_runtime_status() {
    let app = setup_app();
    let response = app
        .router
        .oneshot(Request::builder().uri("/api/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn agent_crud_should_round_trip_over_http() {
    let app = setup_app();

    let create_body = serde_json::json!({
        "id": "writer",
        "name": "Writer",
        "model": "fallback-model",
        "system_prompt": "You are a writer"
    });
    let create_response = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/agents")
                .header("content-type", "application/json")
                .body(Body::from(create_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);

    let get_response = app
        .router
        .clone()
        .oneshot(Request::builder().uri("/api/agents/writer").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(get_response.status(), StatusCode::OK);

    let update_body = serde_json::json!({
        "id": "writer",
        "name": "Writer v2",
        "model": "fallback-model",
        "system_prompt": "Updated prompt"
    });
    let update_response = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/agents/writer")
                .header("content-type", "application/json")
                .body(Body::from(update_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_response.status(), StatusCode::OK);

    let delete_response = app
        .router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/agents/writer")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete_response.status(), StatusCode::OK);
}

#[tokio::test]
async fn context_create_should_conflict_when_project_exists() {
    let app = setup_app();
    let project_dir = app.temp_dir.path().join("demo-project");
    std::fs::create_dir_all(project_dir.join("src")).unwrap();
    std::fs::write(project_dir.join("Cargo.toml"), "[package]\nname='demo'\nversion='0.1.0'").unwrap();
    std::fs::write(project_dir.join("src/main.rs"), "fn main() {}\n").unwrap();

    let body = serde_json::json!({
        "project_id": "demo-project",
        "project_path": project_dir,
        "auto_analyze": true
    });

    let first = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/contexts")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let second = app
        .router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/contexts")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn openai_chat_should_use_x_agent_and_fallback_to_default_model() {
    let app = setup_app();
    let agent_path = app.temp_dir.path().join("agents/rusty.toml");
    let config = AgentConfig {
        name: "Rusty".to_string(),
        model: "agent-model".to_string(),
        system_prompt: "You are helpful".to_string(),
        context_project: None,
        context_files: vec![],
        variables: Default::default(),
        optimize: Default::default(),
    };
    std::fs::write(&agent_path, toml::to_string(&config).unwrap()).unwrap();
    app.agent_manager.load_agents().unwrap();

    let body = serde_json::json!({
        "model": "ignored",
        "messages": [{ "role": "user", "content": "hello" }],
        "stream": false
    });

    let response = app
        .router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("x-agent", "rusty")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn context_reanalyze_endpoint_should_refresh_existing_context() {
    let app = setup_app();
    let project_dir = app.temp_dir.path().join("refresh-project");
    std::fs::create_dir_all(project_dir.join("src")).unwrap();
    std::fs::write(project_dir.join("Cargo.toml"), "[package]\nname='demo'\nversion='0.1.0'").unwrap();
    std::fs::write(project_dir.join("src/lib.rs"), "pub fn demo() {}\n").unwrap();

    let body = serde_json::json!({
        "project_id": "refresh-project",
        "project_path": project_dir,
        "auto_analyze": true
    });
    let create_response = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/contexts")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);

    let response = app
        .router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/contexts/refresh-project/analyze")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}
