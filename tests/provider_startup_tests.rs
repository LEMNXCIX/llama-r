//! Boot-path validation of the single-`PROVIDER` stack builder and honest
//! health reporting. Each test builds a real `Config`, runs
//! `build_provider_stack`, and inspects the resulting stack and `/health`
//! response — no network is opened by any of these paths.

#![allow(clippy::await_holding_lock)] // the env lock is held for the whole test by design

use async_trait::async_trait;
use axum::{body::Body, extract::State, http::Request, http::StatusCode};
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::handlers::AppState;
use llama_r::api::health::health;
use llama_r::config::{Config, ProviderKind};
use llama_r::context::store::ContextStore;
use llama_r::domain::models::{ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::providers::LLMProvider;
use llama_r::runtime::{build_app_state, build_provider_stack, build_router};
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::skill_manager::SkillManager;
use std::collections::VecDeque;
use std::error::Error;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;
use tokio_stream::Stream;
use tower::ServiceExt;

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn test_lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Every variable `RouterConfig::load` reads. Cleared so the stack builder and
/// `build_app_state` both see routing off regardless of the developer's shell.
const ROUTER_ENV_VARS: &[&str] = &[
    "ROUTER_ENABLED",
    "ROUTER_URL",
    "ROUTER_MODEL",
    "ROUTER_TIMEOUT_MS",
    "MIN_CONFIDENCE",
    "MIN_SPECIALIZED",
];

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
    prev_dir: Option<String>,
    _dir: TempDir,
}

impl EnvGuard {
    fn fresh() -> Self {
        let saved = ROUTER_ENV_VARS
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect();
        for name in ROUTER_ENV_VARS {
            std::env::remove_var(name);
        }
        let prev_dir = std::env::var("LLAMA_R_DIR").ok();
        let dir = tempfile::tempdir().expect("temp dir");
        std::env::set_var("LLAMA_R_DIR", dir.path());
        Self {
            saved,
            prev_dir,
            _dir: dir,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        match &self.prev_dir {
            Some(value) => std::env::set_var("LLAMA_R_DIR", value),
            None => std::env::remove_var("LLAMA_R_DIR"),
        }
    }
}

fn config(provider: ProviderKind) -> Config {
    Config {
        port: 3000,
        provider,
        // Deliberately empty: OpenRouter supplies its own embeddings and
        // OpenCode must degrade honestly when no Ollama fallback exists.
        ollama_url: String::new(),
        default_model: "test-model".to_string(),
        embedding_model: "nomic-embed-text".to_string(),
        embedding_dimensions: 768,
        rag_enabled: true,
        history_retention_days: 90,
        openrouter_api_key: Some("sk-secret-never-exposed".to_string()),
        opencode_api_key: Some("opencode-secret-never-exposed".to_string()),
        opencode_url: "http://127.0.0.1:4096".to_string(),
        openai_base_url: None,
    }
}

/// `health()` probes the provider on every call. These tests assert on the
/// stack's `provider_name`/`embeddings` decisions, so the chat provider is
/// swapped for one that never touches the network after those decisions are
/// made. No container network needed.
struct LocalHealthProvider;

#[async_trait]
impl LLMProvider for LocalHealthProvider {
    fn get_base_url(&self) -> String {
        "http://unused".to_string()
    }
    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(Vec::new())
    }
    async fn chat(
        &self,
        _request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        unreachable!("not used by these tests")
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        unreachable!("not used by these tests")
    }
}

fn state_for(stack: llama_r::services::provider_stack::ProviderStack) -> Arc<AppState> {
    let mcp_registry: Arc<dyn McpServerRegistry> = Arc::new(StaticMcpRegistry::new());
    build_app_state(
        stack,
        Arc::new(AgentRegistry::new()),
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        Arc::new(Mutex::new(VecDeque::new())),
        Vec::new(),
        mcp_registry,
        None,
    )
}

#[test]
fn opencode_without_ollama_has_no_embeddings_and_health_says_why() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();

    let stack = build_provider_stack(&config(ProviderKind::OpenCode)).expect("build stack");
    assert!(
        stack.embeddings.is_none(),
        "OpenCode with no OLLAMA_URL has no embeddings provider"
    );
    assert_eq!(stack.provider_name, "opencode");
    assert_eq!(stack.default_model, "test-model");

    let mut stack = stack;
    stack.provider = Arc::new(LocalHealthProvider);
    let state = state_for(stack);
    let response = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async { health(State(state)).await.0 });

    assert!(!response.provider.embeddings);
    assert_eq!(response.provider.name, "opencode");
    let reason = response
        .provider
        .reason
        .as_deref()
        .expect("a missing capability must be explained");
    assert!(
        reason.contains("embeddings"),
        "the reason must name the missing capability: {reason}"
    );
    assert!(
        reason.contains("OLLAMA_URL") && reason.contains("opencode"),
        "the reason must say why they are disabled: {reason}"
    );
    // A key must never leak into any health field.
    let all = serde_json::to_string(&response).expect("serialize health");
    assert!(!all.contains("opencode-secret-never-exposed"), "{all}");
}

#[test]
fn openrouter_uses_its_embeddings_adapter() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();

    let stack = build_provider_stack(&config(ProviderKind::OpenRouter)).expect("build stack");
    // OLLAMA_URL is empty, so this could only be the OpenRouter embeddings
    // adapter — proving the provider selects all three services together.
    assert!(stack.embeddings.is_some());
    assert!(stack.router.is_none(), "routing is off by default");
    assert_eq!(stack.provider_name, "openrouter");

    let mut stack = stack;
    stack.provider = Arc::new(LocalHealthProvider);
    let state = state_for(stack);
    let response = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async { health(State(state)).await.0 });
    assert!(response.provider.embeddings);
    assert_eq!(response.provider.reason, None);
    let all = serde_json::to_string(&response).expect("serialize health");
    assert!(!all.contains("sk-secret-never-exposed"), "{all}");
}

/// A provider that answers health but rejects streaming with a boxed
/// `AppError::Unavailable`, exactly like the real non-Ollama adapters.
struct UnavailableStreamProvider;

#[async_trait]
impl LLMProvider for UnavailableStreamProvider {
    fn get_base_url(&self) -> String {
        "http://unused".to_string()
    }
    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(Vec::new())
    }
    async fn chat(
        &self,
        _request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        unreachable!("this test only streams")
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        Err(Box::new(llama_r::error::AppError::Unavailable(
            "streaming is not supported".to_string(),
        )))
    }
}

#[tokio::test]
async fn streaming_with_a_non_ollama_provider_returns_501_not_502() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();

    let stack = llama_r::services::provider_stack::ProviderStack::new(
        Arc::new(UnavailableStreamProvider),
        "test-model",
    );
    let state = state_for(stack);
    let router = build_router(state);

    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "model": "test-model",
                        "messages": [{"role": "user", "content": "hello"}],
                        "stream": true
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(
        response.status(),
        StatusCode::NOT_IMPLEMENTED,
        "an unsupported capability is 501, not a 502 provider failure"
    );
}
