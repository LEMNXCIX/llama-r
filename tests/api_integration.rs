use async_trait::async_trait;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::grpc::pb::llama_gateway_server::LlamaGateway;
use llama_r::api::grpc::{pb, GrpcService};
use llama_r::context::store::ContextStore;
use llama_r::domain::agent::AgentConfig;
use llama_r::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::providers::LLMProvider;
use llama_r::runtime::{build_app_state, build_router};
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::skill_manager::SkillManager;
use std::collections::VecDeque;
use std::error::Error;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;
use tokio_stream::Stream;
use tonic::Request as GrpcRequest;
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

    async fn chat(
        &self,
        request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        if request.model == "agent-model" && self.fail_primary_once.swap(false, Ordering::SeqCst) {
            return Err("synthetic provider failure".into());
        }

        let content = if request.messages[0]
            .content
            .contains("Analyze this software project")
        {
            "## Project Overview\nTest project\n\n## Architecture\nRouter + provider\n\n## Tech Stack\nRust\n\n## Development Rules\nPrefer safe errors\n\n## Key Conventions\nUse tests".to_string()
        } else if request.messages[0]
            .content
            .contains("Select ONLY the skills")
        {
            "[]".to_string()
        } else {
            format!(
                "model={} role={} content={}",
                request.model,
                request
                    .messages
                    .last()
                    .map(|m| m.role.clone())
                    .unwrap_or_default(),
                request
                    .messages
                    .last()
                    .map(|m| m.content.clone())
                    .unwrap_or_default()
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
            debug_prompt: None,
        })
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        if request.model == "agent-model" && self.fail_primary_once.swap(false, Ordering::SeqCst) {
            return Err("synthetic provider failure".into());
        }
        let event = ChatStreamEvent {
            model: request.model,
            created_at: "2026-03-09T00:00:00Z".to_string(),
            message: ChatMessage {
                role: "assistant".to_string(),
                content: request
                    .messages
                    .last()
                    .map(|m| m.content.clone())
                    .unwrap_or_default(),
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
    grpc_service: GrpcService,
    agent_registry: Arc<AgentRegistry>,
}

fn setup_app() -> TestApp {
    let guard = test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp_dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", temp_dir.path());
    std::env::set_var("DEFAULT_MODEL", "fallback-model");

    std::fs::create_dir_all(temp_dir.path().join("agents")).unwrap();
    std::fs::create_dir_all(temp_dir.path().join("contextos/projects")).unwrap();

    let agent_registry = Arc::new(AgentRegistry::new());
    agent_registry.reload_all(&[]).unwrap();
    let skill_manager = Arc::new(SkillManager::new());
    let context_store = Arc::new(ContextStore::new());
    let provider = Arc::new(FakeProvider::new());
    let logs = Arc::new(Mutex::new(VecDeque::new()));
    let mcp_registry: Arc<dyn McpServerRegistry> = Arc::new(StaticMcpRegistry::new());
    let state = build_app_state(
        provider,
        agent_registry.clone(),
        skill_manager,
        context_store,
        "fallback-model".to_string(),
        logs,
        Vec::new(),
        mcp_registry,
        None,
        None,
        None,
        None,
        None,
    );
    let router = build_router(state.clone());
    let grpc_service = GrpcService::new(state);

    TestApp {
        _guard: guard,
        temp_dir,
        router,
        grpc_service,
        agent_registry,
    }
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn rag_admin_requires_debug_header() {
    let app = setup_app();

    for uri in [
        "/api/rag/ingest",
        "/api/rag/query",
        "/api/rag/delete-document",
    ] {
        let body = serde_json::json!({
            "source_id": "agent:writer/memory",
            "texts": ["some knowledge"],
            "query": "knowledge",
        })
        .to_string();

        // Missing X-Debug header must be rejected as forbidden.
        let response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{uri} without X-Debug must return 403"
        );

        // Wrong header value must also be rejected.
        let response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .header("x-debug", "false")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{uri} with X-Debug: false must return 403"
        );
    }
}

#[tokio::test]
async fn rag_admin_reports_not_implemented_when_store_absent() {
    // setup_app() passes rag_store = None (RAG off / feature compiled out).
    let app = setup_app();

    let response = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/rag/query")
                .header("content-type", "application/json")
                .header("x-debug", "true")
                .body(Body::from(
                    serde_json::json!({"query": "anything"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    // A disabled feature is not a server fault: 501, not 500.
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let json = body_json(response).await;
    assert_eq!(json["error"]["code"], "unavailable");
}

#[tokio::test]
async fn rag_delete_document_reports_rag_disabled() {
    // setup_app() passes rag_ingest = None, so the endpoint is mounted but RAG
    // is off: 501, not 500 and not a silent success.
    let app = setup_app();

    let response = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/rag/delete-document")
                .header("content-type", "application/json")
                .header("x-debug", "true")
                .body(Body::from(
                    serde_json::json!({
                        "source_id": "agent:writer/memory",
                        "doc_key": "policy",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let json = body_json(response).await;
    assert_eq!(json["error"]["code"], "unavailable");
}

#[tokio::test]
async fn rag_delete_document_gate_precedes_body_validation() {
    // An unauthorized caller must get 403 even with a body that would not
    // deserialize: the gate runs before parsing, so the request schema is not
    // leaked to callers without the header.
    let app = setup_app();

    let response = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/rag/delete-document")
                .header("content-type", "application/json")
                .body(Body::from("this is not json at all"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "the debug gate must be evaluated before the body is parsed"
    );
}

#[test]
fn startup_does_not_enter_interactive_setup_when_provider_is_down() {
    use llama_r::runtime::startup_plan::{plan, PlanInput, StartupPlan};

    // A configured gateway whose provider is temporarily down must boot and
    // report "degraded", not drop into interactive setup and refuse to start.
    assert_eq!(
        plan(PlanInput {
            health_ok: false,
            model_configured: true,
            is_terminal: true,
        }),
        StartupPlan::Degraded,
        "provider down with a configured model must not trigger setup"
    );

    // Even with no TTY at all (systemd, Docker, CI) it must not abort.
    assert_eq!(
        plan(PlanInput {
            health_ok: false,
            model_configured: true,
            is_terminal: false,
        }),
        StartupPlan::Degraded
    );

    // Healthy and configured: nothing to do.
    assert_eq!(
        plan(PlanInput {
            health_ok: true,
            model_configured: true,
            is_terminal: true,
        }),
        StartupPlan::Proceed
    );

    // Unconfigured: interactive setup only when a terminal can answer it.
    assert_eq!(
        plan(PlanInput {
            health_ok: true,
            model_configured: false,
            is_terminal: true,
        }),
        StartupPlan::InteractiveSetup
    );
    assert_eq!(
        plan(PlanInput {
            health_ok: false,
            model_configured: false,
            is_terminal: false,
        }),
        StartupPlan::Unconfigured,
        "no TTY and no model must boot degraded, not abort"
    );
}

#[tokio::test]
async fn health_should_report_runtime_status() {
    let app = setup_app();
    let response = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "ok");
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
        .oneshot(
            Request::builder()
                .uri("/api/agents/writer")
                .body(Body::empty())
                .unwrap(),
        )
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
    std::fs::write(
        project_dir.join("Cargo.toml"),
        "[package]\nname='demo'\nversion='0.1.0'",
    )
    .unwrap();
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
async fn transport_layers_should_resolve_same_agent_and_fallback() {
    let openai_app = setup_app();
    let openai_agent_path = openai_app.temp_dir.path().join("agents/rusty.toml");
    let config = AgentConfig {
        name: "Rusty".to_string(),
        model: "agent-model".to_string(),
        system_prompt: "You are helpful".to_string(),
        optimize: Default::default(),
        ..Default::default()
    };
    std::fs::write(&openai_agent_path, toml::to_string(&config).unwrap()).unwrap();
    openai_app.agent_registry.reload_all(&[]).unwrap();

    let body = serde_json::json!({
        "model": "ignored",
        "messages": [{ "role": "user", "content": "hello" }],
        "stream": false
    });

    let openai_response = openai_app
        .router
        .clone()
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
    assert_eq!(openai_response.status(), StatusCode::OK);
    let openai_json = body_json(openai_response).await;
    assert_eq!(openai_json["model"], "fallback-model");
    drop(openai_app);

    let grpc_app = setup_app();
    let grpc_agent_path = grpc_app.temp_dir.path().join("agents/rusty.toml");
    std::fs::write(&grpc_agent_path, toml::to_string(&config).unwrap()).unwrap();
    grpc_app.agent_registry.reload_all(&[]).unwrap();

    let grpc_response = grpc_app
        .grpc_service
        .chat(GrpcRequest::new(pb::ChatRequest {
            model: "rusty".to_string(),
            messages: vec![pb::ChatMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            stream: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(grpc_response.model, "fallback-model");
}

#[tokio::test]
async fn validation_errors_should_surface_for_http_and_grpc() {
    let app = setup_app();

    let http_response = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"fallback-model","messages":[],"stream":false}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(http_response.status(), StatusCode::BAD_REQUEST);

    let grpc_error = app
        .grpc_service
        .chat(GrpcRequest::new(pb::ChatRequest {
            model: "fallback-model".to_string(),
            messages: vec![],
            stream: false,
        }))
        .await
        .unwrap_err();
    assert_eq!(grpc_error.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn mcp_should_respond_to_initialize_tools_and_tool_call() {
    let app = setup_app();

    let initialize = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mcp")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "initialize",
                        "params": {}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(initialize.status(), StatusCode::OK);

    let tools_list = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mcp")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "tools/list",
                        "params": {}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(tools_list.status(), StatusCode::OK);

    let tool_call = app
        .router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mcp")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 3,
                        "method": "tools/call",
                        "params": {
                            "name": "get_api_spec",
                            "arguments": {}
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(tool_call.status(), StatusCode::OK);
}

#[tokio::test]
async fn health_should_report_non_empty_metrics_after_traffic() {
    let app = setup_app();

    let _ = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "model": "fallback-model",
                        "messages": [{ "role": "user", "content": "hello" }],
                        "stream": false
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let health = app
        .router
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let json = body_json(health).await;
    assert!(json["observability"]["http_requests"].as_u64().unwrap() >= 2);
    assert!(json["observability"]["chat_requests"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn context_reanalyze_endpoint_should_refresh_existing_context() {
    let app = setup_app();
    let project_dir = app.temp_dir.path().join("refresh-project");
    std::fs::create_dir_all(project_dir.join("src")).unwrap();
    std::fs::write(
        project_dir.join("Cargo.toml"),
        "[package]\nname='demo'\nversion='0.1.0'",
    )
    .unwrap();
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
