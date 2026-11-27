//! Boot wiring of `SummarizingCompactor` (Fase 10-C3, Task 3).
//!
//! The compactor used to be hard-coded to `NoopCompactor` at boot. These tests
//! pin that boot installs a real summarizing compactor whenever the selected
//! provider has a usable summarizer, that a quiet/no-op summarizer NEVER gets
//! paired with the summarizing compactor (D10 — that combination silently
//! drops the oldest turns), and that a settings Apply keeps the boot compactor
//! handle on the rebuilt engine instead of dropping it.
//!
//! Everything runs against local one-shot HTTP servers; no live provider is
//! needed. `build_summarizer` is `#[cfg(feature = "history")]`, so is this file.

#![cfg(feature = "history")]
#![allow(clippy::await_holding_lock)] // env guard held deliberately across .await to serialise tests

use axum::{body::to_bytes, extract::Request, http::StatusCode, routing::post, Json, Router};
use llama_r::adapters::execution::NoOpExecutionStore;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::handlers::AppState;
use llama_r::config::{Config, ProviderKind};
use llama_r::context::store::ContextStore;
use llama_r::domain::models::ChatMessage;
use llama_r::ports::compaction::CompactRequest;
use llama_r::ports::history::Summarizer;
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::runtime::{
    build_app_state_with_gate, build_companions_for_state, build_provider_stack, build_summarizer,
};
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::approval::ApprovalGate;
use llama_r::services::skill_manager::SkillManager;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

// ── env serialisation ────────────────────────────────────────────────────────

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

const ENV_VARS: &[&str] = &[
    "LLAMA_R_DIR",
    "OLLAMA_URL",
    "OPENAI_BASE_URL",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "OPENROUTER_API_KEY",
    "OPENCODE_URL",
    "OPENCODE_API_KEY",
    "DEFAULT_MODEL",
    "ROUTER_ENABLED",
];

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
    _dir: TempDir,
}

impl EnvGuard {
    fn fresh() -> Self {
        let saved = ENV_VARS
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect();
        let dir = TempDir::new().expect("temp base dir");
        for name in ENV_VARS {
            std::env::remove_var(name);
        }
        std::env::set_var("LLAMA_R_DIR", dir.path());
        Self { saved, _dir: dir }
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
    }
}

fn config_for(provider: ProviderKind) -> Config {
    Config {
        provider,
        default_model: "compaction-model".into(),
        ollama_url: String::new(),
        opencode_url: String::new(),
        openrouter_api_key: None,
        ..Config::default()
    }
}

// ── one-shot fake HTTP server, recording the path it was hit on ─────────────

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Option<String>>>);

impl Recorder {
    fn path(&self) -> String {
        self.0
            .lock()
            .expect("recorder mutex")
            .clone()
            .expect("the fake server captured a request")
    }
}

struct FakeServer {
    url: String,
    recorder: Recorder,
    _task: JoinHandle<()>,
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self._task.abort();
    }
}

async fn fake_server(path: &'static str, response: Value) -> FakeServer {
    let recorder = Recorder::default();
    let mut app = Router::new();
    let r = recorder.clone();
    app = app.route(
        path,
        post(move |req: Request| {
            let r = r.clone();
            let response = response.clone();
            async move {
                let path = req.uri().path().to_string();
                let _ = to_bytes(req.into_body(), 1_048_576).await;
                *r.0.lock().expect("recorder mutex") = Some(path);
                (StatusCode::OK, Json(response))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral listener");
    let url = format!("http://{}", listener.local_addr().expect("local address"));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("fake server");
    });
    FakeServer {
        url,
        recorder,
        _task: task,
    }
}

// ── state + request helpers ──────────────────────────────────────────────────

/// Builds boot state the same way boot does: the summarizer is supplied once
/// and the compactor is installed from it on `AppState`.
fn state_with_summarizer(config: &Config, summarizer: Arc<dyn Summarizer>) -> Arc<AppState> {
    let stack = build_provider_stack(config).expect("provider stack");
    build_app_state_with_gate(
        Arc::new(ApprovalGate::default()),
        stack,
        Arc::new(AgentRegistry::new()),
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        Arc::new(Mutex::new(VecDeque::new())),
        Vec::new(),
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
        Arc::new(NoOpExecutionStore),
        config.clone(),
        Some(summarizer),
    )
}

/// Five long turns with a budget that fits only the newest two: a real
/// compactor must summarize the first three, a no-op must keep all five.
fn pressured_request() -> CompactRequest {
    let history = (0..5)
        .map(|index| ChatMessage {
            role: if index % 2 == 0 { "user" } else { "assistant" }.into(),
            content: format!("turn {index}: {}", "x".repeat(400)),
        })
        .collect();
    CompactRequest {
        system_prompt: String::new(),
        history,
        budget_tokens: 300,
        reserved_tokens: 0,
    }
}

// ── boot installs the right compactor ───────────────────────────────────────

#[tokio::test]
async fn boot_with_a_configured_ollama_summarizer_installs_a_real_compactor() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    let server = fake_server(
        "/api/chat",
        json!({ "message": { "content": "resumen de ollama" } }),
    )
    .await;
    let mut config = config_for(ProviderKind::Ollama);
    config.ollama_url = server.url.clone();

    let summarizer = build_summarizer(&config, config.provider, &config.default_model);
    assert!(!summarizer.is_noop());
    let state = state_with_summarizer(&config, summarizer);

    let request = pressured_request();
    let original_len = request.history.len();
    let compacted = state
        .compactor
        .compact(request)
        .await
        .expect("compaction must not fail");
    assert_eq!(compacted.memory.as_deref(), Some("resumen de ollama"));
    assert!(
        compacted.history.len() < original_len,
        "a real compactor drops the summarized prefix"
    );
    assert_eq!(server.recorder.path(), "/api/chat");
}

#[tokio::test]
async fn boot_without_a_usable_summarizer_installs_noop_and_drops_nothing() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    // No OLLAMA_URL: build_summarizer degrades to the quiet strategy (D10).
    let config = config_for(ProviderKind::Ollama);
    let summarizer = build_summarizer(&config, config.provider, &config.default_model);
    assert!(summarizer.is_noop());
    let state = state_with_summarizer(&config, summarizer);

    let request = pressured_request();
    let original_contents: Vec<String> = request
        .history
        .iter()
        .map(|message| message.content.clone())
        .collect();
    let compacted = state
        .compactor
        .compact(request)
        .await
        .expect("noop compaction must not fail");
    assert_eq!(compacted.memory, None);
    let compacted_contents: Vec<String> = compacted
        .history
        .iter()
        .map(|message| message.content.clone())
        .collect();
    assert_eq!(
        compacted_contents, original_contents,
        "noop summarizer + summarizing compactor would silently drop the oldest turns"
    );
}

#[tokio::test]
async fn boot_with_an_opencode_summarizer_compacts_through_the_generate_endpoint() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    let server = fake_server(
        "/api/experimental/generate/text",
        json!({ "data": { "text": "resumen de opencode" } }),
    )
    .await;
    let mut config = config_for(ProviderKind::OpenCode);
    config.opencode_url = server.url.clone();

    let summarizer = build_summarizer(&config, config.provider, &config.default_model);
    assert_eq!(summarizer.strategy_name(), "opencode");
    let state = state_with_summarizer(&config, summarizer);

    let compacted = state
        .compactor
        .compact(pressured_request())
        .await
        .expect("compaction must not fail");
    assert_eq!(compacted.memory.as_deref(), Some("resumen de opencode"));
    assert_eq!(server.recorder.path(), "/api/experimental/generate/text");
}

// ── Apply keeps the boot compactor handle ───────────────────────────────────

#[cfg(feature = "rig-engine")]
#[tokio::test]
async fn apply_rebuilds_the_engine_with_the_boot_compactor_handle() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    let server = fake_server(
        "/api/chat",
        json!({ "message": { "content": "resumen compartido" } }),
    )
    .await;
    let mut boot_config = config_for(ProviderKind::Ollama);
    boot_config.ollama_url = server.url.clone();
    let summarizer = build_summarizer(
        &boot_config,
        boot_config.provider,
        &boot_config.default_model,
    );
    let state = state_with_summarizer(&boot_config, summarizer);

    // Boot installs companions once, as the real runtime does.
    let mut boot_stack = build_provider_stack(&boot_config).expect("boot stack");
    build_companions_for_state(&state, &boot_config, &mut boot_stack);
    state.replace_services(boot_stack).await;
    let boot_runtime = state
        .services
        .read()
        .await
        .agent_runtime
        .clone()
        .expect("Ollama has an engine");

    // A settings Apply rebuilds the stack+companions from the SAME AppState.
    // `finish_apply` (src/tui/app.rs) calls exactly this helper; the compactor
    // itself lives on AppState, outside the hot-swapped ProviderStack.
    let mut applied_config = config_for(ProviderKind::OpenRouter);
    applied_config.openai_base_url = Some("http://router.test/v1".into());
    applied_config.openrouter_api_key = Some("sk-never-used".into());
    let mut applied_stack = build_provider_stack(&applied_config).expect("applied stack");
    build_companions_for_state(&state, &applied_config, &mut applied_stack);
    let applied_runtime = applied_stack
        .agent_runtime
        .clone()
        .expect("OpenRouter has a Rig client, so Apply rebuilds an engine");

    assert!(
        Arc::ptr_eq(&applied_runtime.compactor, &state.compactor),
        "the rebuilt engine must use the compactor stored on AppState"
    );
    assert!(Arc::ptr_eq(
        &applied_runtime.compactor,
        &boot_runtime.compactor
    ));
    // And the strategy still reaches the boot provider's summarizer: Apply did
    // not silently replace real compaction with a no-op.
    let compacted = applied_runtime
        .compactor
        .compact(pressured_request())
        .await
        .expect("compaction must survive Apply");
    assert_eq!(compacted.memory.as_deref(), Some("resumen compartido"));
    assert_eq!(server.recorder.path(), "/api/chat");
}
