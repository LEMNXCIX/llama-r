use axum::body::Body;
use axum::http::{Request, StatusCode};
use llama_r::runtime::{build_app_state, build_router};
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::provider_stack::ProviderStack;
use llama_r::services::skill_manager::SkillManager;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tower::ServiceExt;

use async_trait::async_trait;
use llama_r::adapters::mcp::StaticMcpRegistry;

use llama_r::context::store::ContextStore;
use llama_r::domain::models::{ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::providers::LLMProvider;
use std::error::Error;
use std::pin::Pin;
use tokio_stream::Stream;

struct InertProvider;

#[async_trait]
impl LLMProvider for InertProvider {
    fn get_base_url(&self) -> String {
        "http://inert".to_string()
    }
    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
    async fn chat(
        &self,
        _request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        Err("not reached".into())
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        Err("not reached".into())
    }
}

fn test_router_with_admin_token(token: &str) -> axum::Router {
    std::env::set_var("ADMIN_TOKEN", token);
    let dir = TempDir::new().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());
    std::mem::forget(dir);
    let registry = Arc::new(AgentRegistry::new());
    registry.reload_all(&[]).unwrap();
    let stack = ProviderStack::new(Arc::new(InertProvider), "fallback-model");
    let state = build_app_state(
        stack,
        registry,
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        Arc::new(Mutex::new(VecDeque::new())),
        Vec::new(),
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
    );
    // keep dir alive by leaking? TempDir dropped would delete; but state doesn't need dir after. Fine.
    build_router(state)
}

#[tokio::test]
async fn conversations_require_admin_token() {
    let app = test_router_with_admin_token("e2e");
    let no_token = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/conversations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(no_token.status(), StatusCode::UNAUTHORIZED);
    let with_token = app
        .oneshot(
            Request::builder()
                .uri("/api/conversations")
                .header("x-admin-token", "e2e")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(with_token.status(), StatusCode::OK);
}

#[tokio::test]
async fn oversized_chat_body_is_rejected() {
    let app = test_router_with_admin_token("e2e");
    let big = "x".repeat(3 * 1024 * 1024);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat")
                .header("content-type", "application/json")
                .body(Body::from(big))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn cors_rejects_unlisted_origin() {
    let app = test_router_with_admin_token("e2e");
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat")
                .header("origin", "http://evil.example")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"x","messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.headers().get("access-control-allow-origin").is_none());
}
