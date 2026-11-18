//! Boot-path validation of the single-`PROVIDER` stack builder and honest
//! health reporting. Each test builds a real `Config`, runs
//! `build_provider_stack`, and inspects the resulting stack and `/health`
//! response — no network is opened by any of these paths.

#![allow(clippy::await_holding_lock)] // the env lock is held for the whole test by design

use async_trait::async_trait;
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::IntoResponse,
    Json, Router,
};
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::handlers::AppState;
use llama_r::api::health::health;
use llama_r::config::{Config, ProviderKind};
use llama_r::context::store::ContextStore;
use llama_r::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
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

/// Credentials the new direct adapters read by *variable name* from their
/// profile. Cleared so a stack test cannot accidentally pick up the developer's
/// shell keys, and restored on drop.
const KEY_ENV_VARS: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "LLAMA_R_TEST_CUSTOM_KEY",
];

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
    prev_dir: Option<String>,
    dir: TempDir,
}

impl EnvGuard {
    fn fresh() -> Self {
        let saved = ROUTER_ENV_VARS
            .iter()
            .chain(KEY_ENV_VARS.iter())
            .map(|name| (*name, std::env::var(name).ok()))
            .collect();
        for name in ROUTER_ENV_VARS.iter().chain(KEY_ENV_VARS.iter()) {
            std::env::remove_var(name);
        }
        let prev_dir = std::env::var("LLAMA_R_DIR").ok();
        let dir = tempfile::tempdir().expect("temp dir");
        std::env::set_var("LLAMA_R_DIR", dir.path());
        Self {
            saved,
            prev_dir,
            dir,
        }
    }

    /// Write a `providers.toml` into the isolated base directory.
    fn write_providers(&self, contents: &str) {
        std::fs::write(self.dir.path().join("providers.toml"), contents)
            .expect("write providers.toml");
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

// --- Direct adapters + registry overlays ------------------------------------

fn direct_chat_request() -> ChatRequest {
    ChatRequest {
        model: String::new(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: "Hello".to_string(),
        }],
        stream: false,
    }
}

#[derive(Clone, Debug)]
struct SeenRequest {
    path: String,
    auth: String,
    body: serde_json::Value,
}

/// A real listener that answers every path with a reply carrying all three
/// chat-envelope shapes and records what each adapter actually sent. Serving a
/// union of shapes is deliberate: the assertions are on path/auth/body, so a
/// wrong endpoint still fails loudly on `path` rather than a parse error.
async fn recording_server() -> (String, Arc<Mutex<Vec<SeenRequest>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let app = Router::new().fallback(move |req: Request<Body>| {
        let sink = sink.clone();
        async move {
            let path = req.uri().path().to_string();
            let auth = req
                .headers()
                .get("authorization")
                .or_else(|| req.headers().get("x-api-key"))
                .or_else(|| req.headers().get("x-goog-api-key"))
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            let body = serde_json::from_slice::<serde_json::Value>(&bytes)
                .unwrap_or(serde_json::Value::Null);
            let is_embeddings = req_is_embeddings_path(&path);
            sink.lock().unwrap().push(SeenRequest {
                path,
                auth,
                body,
            });

            let reply = if is_embeddings {
                serde_json::json!({
                    "model": "text-embedding-3-small",
                    "data": [{ "object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3] }],
                    "embeddings": [{ "values": [0.1, 0.2, 0.3] }]
                })
            } else {
                serde_json::json!({
                    "model": "test-model",
                    "choices": [{ "message": { "role": "assistant", "content": "ok" } }],
                    "content": [{ "type": "text", "text": "ok" }],
                    "candidates": [{ "content": { "role": "model", "parts": [{ "text": "ok" }] }, "finishReason": "STOP" }]
                })
            };
            (StatusCode::OK, Json(reply)).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake upstream");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), seen)
}

fn req_is_embeddings_path(path: &str) -> bool {
    path.contains("embeddings") || path.contains("embedContent")
}

fn health_response(
    stack: llama_r::services::provider_stack::ProviderStack,
) -> llama_r::api::health::HealthResponse {
    let mut stack = stack;
    stack.provider = Arc::new(LocalHealthProvider);
    let state = state_for(stack);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async { health(State(state)).await.0 })
}

#[test]
fn all_six_provider_kinds_build_a_stack_without_a_network() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();

    for kind in [
        ProviderKind::Ollama,
        ProviderKind::OpenRouter,
        ProviderKind::OpenCode,
        ProviderKind::OpenAiCompatible,
        ProviderKind::Anthropic,
        ProviderKind::Gemini,
    ] {
        let stack = build_provider_stack(&config(kind))
            .unwrap_or_else(|err| panic!("{kind} must build a stack: {err}"));
        assert_eq!(stack.provider_name, kind.to_string());
        assert_eq!(stack.default_model, "test-model");
        assert!(stack.router.is_none(), "{kind}: routing is off by default");
    }
}

#[test]
fn direct_provider_capabilities_are_honest_and_keys_never_leak() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();

    // OpenAI-compatible and Gemini document an embeddings endpoint; their
    // stacks carry an adapter for it.
    for kind in [ProviderKind::OpenAiCompatible, ProviderKind::Gemini] {
        let stack = build_provider_stack(&config(kind)).expect("stack");
        assert!(stack.embeddings.is_some(), "{kind} serves embeddings");
        let response = health_response(stack);
        assert!(response.provider.embeddings, "{kind}");
        assert_eq!(response.provider.reason, None, "{kind}");
        let all = serde_json::to_string(&response).expect("serialize health");
        assert!(!all.contains("sk-secret-never-exposed"), "{kind}: {all}");
    }

    // Anthropic publishes no embeddings endpoint. With OLLAMA_URL empty there
    // is no fallback, so the gap is reported instead of faked.
    let stack = build_provider_stack(&config(ProviderKind::Anthropic)).expect("stack");
    assert!(stack.embeddings.is_none());
    let response = health_response(stack);
    assert!(!response.provider.embeddings);
    assert_eq!(response.provider.name, "anthropic");
    let reason = response
        .provider
        .reason
        .as_deref()
        .expect("a missing capability must be explained");
    assert!(reason.contains("embeddings"), "{reason}");
    assert!(
        reason.contains("OLLAMA_URL") && reason.contains("anthropic"),
        "{reason}"
    );
    let all = serde_json::to_string(&response).expect("serialize health");
    assert!(!all.contains("sk-secret-never-exposed"), "{all}");
}

#[tokio::test]
async fn a_custom_profile_overrides_the_direct_chat_and_embeddings_endpoints() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    let (url, seen) = recording_server().await;
    _env.write_providers(&format!(
        "[providers.openai-compatible]\nbase_url = \"{url}\"\napi_key_env = \"LLAMA_R_TEST_CUSTOM_KEY\"\n"
    ));
    std::env::set_var("LLAMA_R_TEST_CUSTOM_KEY", "custom-key-value");
    std::env::set_var("OPENAI_API_KEY", "builtin-key-must-not-be-used");

    let mut cfg = config(ProviderKind::OpenAiCompatible);
    cfg.embedding_dimensions = 3;
    let stack = build_provider_stack(&cfg).expect("stack");
    assert_eq!(
        stack.provider.get_base_url(),
        url,
        "the profile base_url reaches the direct adapter"
    );
    stack
        .provider
        .chat(direct_chat_request())
        .await
        .expect("chat");
    stack
        .embeddings
        .expect("embeddings")
        .embed(&["hello".to_string()])
        .await
        .expect("embed");

    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "one chat and one embeddings request");
    let chat = requests
        .iter()
        .find(|request| request.path.contains("chat/completions"))
        .expect("chat request");
    assert_eq!(chat.auth, "Bearer custom-key-value");
    assert_eq!(chat.body["model"], "test-model");
    assert_eq!(chat.body["messages"][0]["content"], "Hello");
    let embeddings = requests
        .iter()
        .find(|request| request.path.contains("embeddings"))
        .expect("embeddings request");
    assert_eq!(embeddings.auth, "Bearer custom-key-value");
    assert_eq!(embeddings.body["input"][0], "hello");
    assert_eq!(embeddings.body["model"], "nomic-embed-text");
    let all = format!("{requests:?}");
    assert!(!all.contains("builtin-key-must-not-be-used"), "{all}");
}

#[tokio::test]
async fn enabling_routing_installs_the_default_decision_router_for_direct_providers() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    std::env::set_var("ROUTER_ENABLED", "true");

    // None of the three direct providers exposes a d1 decisions endpoint, so
    // the stack itself carries no router — but enabling routing must still
    // install the separate llama.cpp decision model, exactly as on Ollama.
    let stack = build_provider_stack(&config(ProviderKind::Anthropic)).expect("stack");
    assert!(stack.router.is_none());
    let state = state_for(stack);
    assert!(state.router_config.enabled);
    let services = state.services_snapshot().await;
    assert!(
        services.router.is_some(),
        "build_app_state must install the llama.cpp router when routing is enabled"
    );
}

#[tokio::test]
async fn native_direct_adapters_use_their_built_in_key_variable_names() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    let (url, seen) = recording_server().await;
    _env.write_providers(&format!(
        "[providers.anthropic]\nbase_url = \"{url}\"\n\n[providers.gemini]\nbase_url = \"{url}\"\n"
    ));
    std::env::set_var("ANTHROPIC_API_KEY", "anthropic-builtin-key");
    std::env::set_var("GEMINI_API_KEY", "gemini-builtin-key");

    let anthropic = build_provider_stack(&config(ProviderKind::Anthropic)).expect("stack");
    anthropic
        .provider
        .chat(direct_chat_request())
        .await
        .expect("anthropic chat");
    let gemini = build_provider_stack(&config(ProviderKind::Gemini)).expect("stack");
    gemini
        .provider
        .chat(direct_chat_request())
        .await
        .expect("gemini chat");

    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let anthropic = requests
        .iter()
        .find(|request| request.path.contains("messages"))
        .expect("anthropic request");
    assert_eq!(anthropic.auth, "anthropic-builtin-key");
    assert_eq!(anthropic.body["max_tokens"], 4096);
    assert_eq!(anthropic.body["messages"][0]["content"], "Hello");
    let gemini = requests
        .iter()
        .find(|request| request.path.contains("generateContent"))
        .expect("gemini request");
    assert_eq!(gemini.auth, "gemini-builtin-key");
    assert_eq!(gemini.body["contents"][0]["parts"][0]["text"], "Hello");
}
