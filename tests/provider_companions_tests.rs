//! Task 9: provider-aware companions.
//!
//! RAG must use the selected stack's embedder (and degrade explicitly when the
//! provider has none), the Rig engine target must follow the selected provider,
//! and an agent-scoped chat on a provider with no engine must say so rather than
//! silently answering with a direct chat call. No network is opened here.

#![allow(clippy::await_holding_lock)] // the env lock is held for the whole test by design

use async_trait::async_trait;
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
};
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::adapters::rig_engine::{engine_target_for, EngineTarget, EngineTargetKind};
use llama_r::api::chat_core::execute_chat;
use llama_r::api::handlers::AppState;
use llama_r::api::health::{health, HealthResponse};
use llama_r::api::route_resolution::AgentSelection;
use llama_r::config::{Config, ProviderKind};
use llama_r::context::store::ContextStore;
use llama_r::domain::agent::{Agent, AgentConfig};
use llama_r::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::ports::rag::EmbeddingProvider;
use llama_r::providers::LLMProvider;
use llama_r::runtime::{
    build_app_state, build_companions_for_state, build_provider_stack, build_router,
    install_state_enricher,
};
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::provider_stack::ProviderStack;
use llama_r::services::skill_manager::SkillManager;
use std::collections::VecDeque;
use std::error::Error;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;
use tokio_stream::Stream;
use tower::ServiceExt;
use tracing_subscriber::prelude::*;

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn test_lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

/// Restore the working directory even if an assertion in the test panics — a
/// leftover cwd inside a removed TempDir would break the next test in this
/// (single-process) integration-test binary.
struct CwdGuard(std::path::PathBuf);

impl Drop for CwdGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).expect("restore cwd");
    }
}

fn config(provider: ProviderKind) -> Config {
    Config {
        port: 3000,
        provider,
        // Empty on purpose: OpenCode must degrade honestly with no fallback.
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
        ..Config::default()
    }
}

/// A provider that never touches the network. `health_check` succeeds so the
/// health response's capability fields are the only thing under test.
struct LocalProvider {
    answer: String,
}

#[async_trait]
impl LLMProvider for LocalProvider {
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
        Ok(ChatResponse {
            model: "test-model".to_string(),
            created_at: "now".to_string(),
            message: ChatMessage {
                role: "assistant".to_string(),
                content: self.answer.clone(),
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
        _request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        unreachable!("these tests do not stream")
    }
}

/// A no-network embedder that counts how many times it is asked to embed.
/// Installing one on a stack and then driving skill ranking tells us which
/// embedder the `AppState` enricher actually reached after an Apply.
#[derive(Clone)]
struct ProbeEmbedder {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl EmbeddingProvider for ProbeEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
    }
    fn dimensions(&self) -> usize {
        2
    }
}

fn admin_token_ready() {
    const TOKEN: &str = "test-admin-token";
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| std::env::set_var("ADMIN_TOKEN", TOKEN));
}

fn bare_state(stack: ProviderStack) -> Arc<AppState> {
    // Config captures ADMIN_TOKEN when the state is built; seed it before that
    // point so the admin-route test in this suite sees a configured client.
    admin_token_ready();
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

/// Install the companions for `config` onto the state's current stack. This is
/// exactly what boot and a settings Apply do through `build_companions`.
async fn with_companions(state: &Arc<AppState>, config: &Config) {
    let mut stack = (*state.services_snapshot().await).clone();
    build_companions_for_state(state, config, &mut stack);
    state.replace_services(stack).await;
}

/// OpenCode with no OLLAMA_URL has no embeddings. RAG must be absent and the
/// debug query endpoint must answer the explicit 501 (naming embeddings), not
/// a 500 from a broken embedder pointed at an empty URL.
#[tokio::test]
async fn rag_store_is_absent_when_the_selected_provider_has_no_embeddings() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();

    let cfg = config(ProviderKind::OpenCode);
    let mut stack = build_provider_stack(&cfg).expect("build stack");
    // Keep the real provider stack for the capability decision; health's live
    // probe is replaced with a local provider below.
    stack.provider = Arc::new(LocalProvider {
        answer: String::new(),
    });
    let state = bare_state(stack);
    with_companions(&state, &cfg).await;

    let services = state.services_snapshot().await;
    assert!(
        services.rag_store.is_none(),
        "no embeddings means no RAG store — never a broken embedder"
    );
    assert!(
        services.rag_ingest.is_none(),
        "no RAG store means no ingest service either"
    );
    drop(services);

    // Admin-route tests run as a configured client (token seeded in
    // `bare_state`); the gate itself is in admin_auth_tests.
    const TEST_ADMIN_TOKEN: &str = "test-admin-token";

    let router = build_router(state);
    let response = router
        .oneshot(
            Request::builder()
                .header("x-admin-token", TEST_ADMIN_TOKEN)
                .method("POST")
                .uri("/api/rag/query")
                .header("content-type", "application/json")
                .header("x-debug", "true")
                .body(Body::from(
                    serde_json::json!({ "query": "refunds" }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(
        response.status(),
        StatusCode::NOT_IMPLEMENTED,
        "an unavailable capability is 501, not 500"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = String::from_utf8(body.to_vec()).expect("utf8");
    assert!(
        body.to_ascii_lowercase().contains("embedding"),
        "the 501 reason must name the missing capability: {body}"
    );
}

/// For OpenRouter the stack carries OpenRouter embeddings, so a real RAG store
/// (with the provider-selected embedder) is built.
#[tokio::test]
async fn rag_store_uses_the_selected_provider_embedder() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();

    let cfg = config(ProviderKind::OpenRouter);
    let mut stack = build_provider_stack(&cfg).expect("build stack");
    assert!(stack.embeddings.is_some(), "OpenRouter serves embeddings");
    stack.provider = Arc::new(LocalProvider {
        answer: String::new(),
    });
    let state = bare_state(stack);
    with_companions(&state, &cfg).await;

    let services = state.services_snapshot().await;
    assert!(services.rag_store.is_some(), "OpenRouter gets a RAG store");
    assert!(services.rag_ingest.is_some());
    assert!(services.embeddings.is_some());
    drop(services);

    let response = health(State(state)).await.0;
    assert!(response.provider.embeddings);
    assert!(
        response.provider.agent_engine,
        "OpenRouter has a Rig engine target, so agent_engine is true"
    );
}

/// The engine target (not a hardwired Ollama URL) follows the selected provider:
/// Ollama → Ollama, OpenRouter → OpenRouter, OpenCode → no engine at all.
#[test]
fn agent_engine_target_follows_the_provider() {
    let ollama = engine_target_for(&config(ProviderKind::Ollama));
    assert!(
        matches!(ollama, Some(EngineTarget::Ollama { .. })),
        "Ollama selects the Ollama engine target"
    );
    assert_eq!(
        ollama.as_ref().map(|t| t.kind()),
        Some(EngineTargetKind::Ollama)
    );

    let openrouter = engine_target_for(&config(ProviderKind::OpenRouter));
    match &openrouter {
        Some(EngineTarget::OpenRouter { api_key, base_url }) => {
            assert_eq!(api_key, "sk-secret-never-exposed");
            assert_eq!(base_url, "https://openrouter.ai/api/v1");
        }
        other => panic!(
            "OpenRouter must select the OpenRouter engine target: {:?}",
            other.as_ref().map(|t| t.kind())
        ),
    }
    assert_eq!(
        openrouter.as_ref().map(|t| t.kind()),
        Some(EngineTargetKind::OpenRouter)
    );

    assert!(
        engine_target_for(&config(ProviderKind::OpenCode)).is_none(),
        "OpenCode has no rig-core provider, so no engine is built"
    );
}

/// A shared text buffer that writes into an `Arc<Mutex<Vec<u8>>>`, used to prove
/// chat_core actually emits the degradation warning (not just that a message
/// builder exists).
#[derive(Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuffer {
    type Writer = SharedBuffer;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Register a global agent whose id is `agent-id` by writing its manifest.
fn register_global_agent(id: &str) {
    let dir = llama_r::core::paths::get_agents_dir();
    std::fs::create_dir_all(&dir).expect("create agents dir");
    let cfg = AgentConfig {
        name: "Helper".to_string(),
        model: String::new(),
        system_prompt: "You are helpful".to_string(),
        ..Default::default()
    };
    std::fs::write(
        dir.join(format!("{id}.toml")),
        toml::to_string(&cfg).unwrap(),
    )
    .unwrap();
}

/// OpenCode resolves to an agent but has no engine. The chat degrades to a
/// direct answer, and that degradation is observable: an explicit warning that
/// names the lost capabilities AND a health field/reason.
#[tokio::test]
async fn agent_chat_without_an_engine_is_not_silent() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    register_global_agent("helper-agent");

    let cfg = config(ProviderKind::OpenCode);
    let mut stack = build_provider_stack(&cfg).expect("build stack");
    assert!(stack.agent_runtime.is_none(), "OpenCode has no engine");
    stack.provider = Arc::new(LocalProvider {
        answer: "direct answer".to_string(),
    });
    let state = bare_state(stack);
    // Reload the freshly-written global agent manifest into the registry.
    state.agent_registry.reload_all(&[]).expect("reload agents");
    with_companions(&state, &cfg).await;

    let captured = SharedBuffer::default();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(captured.clone())
            .with_filter(tracing_subscriber::filter::LevelFilter::WARN),
    );

    // `set_default` is task-local and works across `.await` on the current
    // runtime (a nested `block_on` would panic here).
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);
    let response = execute_chat(
        &state,
        ChatRequest {
            model: "helper-agent".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            stream: false,
        },
        AgentSelection::default(),
    )
    .await
    .expect("chat succeeds via direct fallback");
    assert_eq!(response.message.content, "direct answer");

    let logs = String::from_utf8(captured.0.lock().expect("log buffer").clone()).expect("utf8");
    let lower = logs.to_ascii_lowercase();
    assert!(
        lower.contains("tool"),
        "the warning must name lost tools: {logs}"
    );
    assert!(
        lower.contains("rag"),
        "the warning must name lost RAG context: {logs}"
    );
    assert!(
        lower.contains("mcp"),
        "the warning must name lost MCP: {logs}"
    );
    assert!(
        lower.contains("opencode"),
        "the warning must name the provider: {logs}"
    );

    let health: HealthResponse = health(State(state)).await.0;
    assert!(!health.provider.agent_engine, "no engine on OpenCode");
    let reason = health
        .provider
        .agent_engine_reason
        .as_deref()
        .expect("the missing engine must be explained");
    let reason = reason.to_ascii_lowercase();
    assert!(reason.contains("tool") && reason.contains("rag") && reason.contains("mcp"));
    assert!(reason.contains("opencode"));
}

/// A settings Apply must rebuild the chat-path `ContextEnricher` from the NEW
/// stack's embedder. `chat_core` reads the enricher on `AppState` (not the one
/// inside `AgentRuntime`), so leaving the boot-time embedder in place would keep
/// ranking skills with the old provider after a switch. Embedder A is installed
/// at boot, embedder B on Apply; only B may be called when ranking runs.
#[tokio::test]
async fn apply_replaces_the_state_enricher_embedder() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    let root = _env._dir.path().to_path_buf();

    // A skill for the agent to consider. SkillManager scans `./skills`, so the
    // cwd must point at the temp dir while it loads.
    let skill_dir = root.join("skills").join("probe-skill");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: probe-skill\ndescription: A skill used to probe ranking\n---\nProbe skill body.",
    )
    .expect("SKILL.md");
    let _cwd = CwdGuard(std::env::current_dir().expect("cwd"));
    std::env::set_current_dir(&root).expect("set cwd");

    let boot_calls = Arc::new(AtomicUsize::new(0));
    let applied_calls = Arc::new(AtomicUsize::new(0));

    let cfg = config(ProviderKind::OpenRouter);
    // Boot stack: embedder A.
    let mut boot_stack = build_provider_stack(&cfg).expect("boot stack");
    boot_stack.embeddings = Some(Arc::new(ProbeEmbedder {
        calls: boot_calls.clone(),
    }));
    boot_stack.provider = Arc::new(LocalProvider {
        answer: String::new(),
    });
    let state = bare_state(boot_stack);
    state.skill_manager.scan_and_load();

    let agent = Agent {
        id: "probe-agent".to_string(),
        project_id: None,
        config: AgentConfig {
            name: "Probe".to_string(),
            system_prompt: "base".to_string(),
            skills: vec!["probe-skill".to_string()],
            ..Default::default()
        },
    };

    // Apply: a new stack with embedder B, installed through the same helper
    // `finish_apply` uses.
    let mut applied_stack = build_provider_stack(&cfg).expect("applied stack");
    applied_stack.embeddings = Some(Arc::new(ProbeEmbedder {
        calls: applied_calls.clone(),
    }));
    applied_stack.provider = Arc::new(LocalProvider {
        answer: String::new(),
    });
    build_companions_for_state(&state, &cfg, &mut applied_stack);
    install_state_enricher(&state, &cfg, &applied_stack);
    state.replace_services(applied_stack).await;

    // Drive skill ranking through the AppState enricher that chat_core reads.
    let prompt = state
        .context_enricher_snapshot()
        .build_system_prompt(&agent)
        .await;
    assert!(
        prompt.contains("probe-skill"),
        "the candidate skill must be injected: {prompt}"
    );

    assert_eq!(
        boot_calls.load(Ordering::SeqCst),
        0,
        "the boot embedder must not rank skills after an Apply"
    );
    assert!(
        applied_calls.load(Ordering::SeqCst) > 0,
        "the new stack's embedder must rank skills after an Apply"
    );
}
