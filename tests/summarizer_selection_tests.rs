//! Boot selection of the conversation `Summarizer` (`build_summarizer`).
//!
//! Task 1 added the three real strategies; this suite pins that boot picks the
//! right one for each of the six providers, that credentials reach the strategy
//! without ever being exposed by its label, and that a missing URL/key degrades
//! to the quiet `NoopSummarizer` (never a wrong URL).
//!
//! `build_summarizer` is `#[cfg(feature = "history")]`, so the whole file is
//! gated the same way.

#![cfg(feature = "history")]
#![allow(clippy::await_holding_lock)] // env guard held deliberately across .await to serialise tests

use axum::{
    body::to_bytes,
    extract::Request,
    http::{header::AUTHORIZATION, StatusCode},
    routing::post,
    Json, Router,
};
use llama_r::config::{Config, ProviderKind};
use llama_r::ports::history::ExportMessage;
use llama_r::runtime::build_summarizer;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

// ── env serialisation (integration tests cannot use cfg(test) helpers) ──────

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
];

/// Fresh temp base dir with every variable the registry/config read cleared.
/// Restores the previous environment on drop, so the suite can run alongside
/// the other integration binaries in the same process.
struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
    dir: TempDir,
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
        Self { saved, dir }
    }

    fn write_providers_toml(&self, contents: &str) {
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
    }
}

fn config_for(provider: ProviderKind) -> Config {
    Config {
        provider,
        default_model: "selection-model".into(),
        ..Config::default()
    }
}

fn sample_messages() -> Vec<ExportMessage> {
    vec![ExportMessage {
        id: "m1".into(),
        role: "user".into(),
        content: "remember this".into(),
        created_at: 1,
    }]
}

// ── one-shot fake HTTP server (records path + auth header + JSON body) ──────

#[derive(Clone, Debug, Default)]
struct Captured {
    path: String,
    authorization: Option<String>,
    body: Value,
}

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Option<Captured>>>);

impl Recorder {
    fn get(&self) -> Captured {
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

/// Serve one response on every supplied path, remembering the last request.
async fn fake_server(paths: &[&'static str], response: Value) -> FakeServer {
    let recorder = Recorder::default();
    let mut app = Router::new();
    for path in paths {
        let recorder = recorder.clone();
        let response = response.clone();
        app = app.route(
            path,
            post(move |req: Request| {
                let recorder = recorder.clone();
                let response = response.clone();
                async move {
                    let captured = Captured {
                        path: req.uri().path().to_string(),
                        authorization: req
                            .headers()
                            .get(AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string),
                        body: serde_json::from_slice(
                            &to_bytes(req.into_body(), 1_048_576)
                                .await
                                .expect("read body"),
                        )
                        .expect("JSON request body"),
                    };
                    *recorder.0.lock().expect("recorder mutex") = Some(captured);
                    (StatusCode::OK, Json(response))
                }
            }),
        );
    }
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

// ── strategy selection for all six providers ────────────────────────────────

#[test]
fn ollama_selects_the_ollama_wire_summarizer() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    let mut config = config_for(ProviderKind::Ollama);
    config.ollama_url = "http://ollama.test:11434".into();

    let summarizer = build_summarizer(&config, ProviderKind::Ollama, &config.default_model);

    assert_eq!(summarizer.strategy_name(), "ollama");
    assert!(
        !summarizer.is_noop(),
        "a configured Ollama boot is a real strategy"
    );
}

#[test]
fn openrouter_selects_chat_completions_with_the_config_key() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    let mut config = config_for(ProviderKind::OpenRouter);
    config.openrouter_api_key = Some("sk-or-selection-secret".into());

    let summarizer = build_summarizer(&config, ProviderKind::OpenRouter, &config.default_model);

    assert_eq!(summarizer.strategy_name(), "chat-completions");
    assert!(!summarizer.is_noop());
    assert!(
        !summarizer.strategy_name().contains("selection-secret"),
        "the strategy label is secret-free"
    );
}

#[test]
fn openai_compatible_selects_chat_completions_from_its_profile() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    std::env::set_var("OPENAI_BASE_URL", "http://compatible.test/v1");
    std::env::set_var("OPENAI_API_KEY", "sk-compatible-selection-secret");
    let config = config_for(ProviderKind::OpenAiCompatible);

    let summarizer = build_summarizer(
        &config,
        ProviderKind::OpenAiCompatible,
        &config.default_model,
    );

    assert_eq!(summarizer.strategy_name(), "chat-completions");
    assert!(!summarizer.is_noop());
}

#[test]
fn anthropic_selects_chat_completions_from_its_profile() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    _env.write_providers_toml(
        r#"
[providers.anthropic]
base_url = "http://anthropic.test/v1"
"#,
    );
    std::env::set_var("ANTHROPIC_API_KEY", "sk-anthropic-selection-secret");
    let config = config_for(ProviderKind::Anthropic);

    let summarizer = build_summarizer(&config, ProviderKind::Anthropic, &config.default_model);

    assert_eq!(summarizer.strategy_name(), "chat-completions");
    assert!(!summarizer.is_noop());
}

#[test]
fn gemini_selects_chat_completions_from_its_profile() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    _env.write_providers_toml(
        r#"
[providers.gemini]
base_url = "http://gemini.test/v1"
"#,
    );
    std::env::set_var("GEMINI_API_KEY", "gemini-selection-secret");
    let config = config_for(ProviderKind::Gemini);

    let summarizer = build_summarizer(&config, ProviderKind::Gemini, &config.default_model);

    assert_eq!(summarizer.strategy_name(), "chat-completions");
    assert!(!summarizer.is_noop());
}

#[test]
fn opencode_selects_the_opencode_generate_summarizer() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    let mut config = config_for(ProviderKind::OpenCode);
    config.opencode_url = "http://opencode.test:4096".into();
    config.opencode_api_key = Some("opencode-selection-secret".into());

    let summarizer = build_summarizer(&config, ProviderKind::OpenCode, &config.default_model);

    assert_eq!(summarizer.strategy_name(), "opencode");
    assert!(!summarizer.is_noop());
}

#[test]
fn opencode_without_a_password_is_still_a_real_summarizer() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    let mut config = config_for(ProviderKind::OpenCode);
    config.opencode_url = "http://opencode.test:4096".into();
    config.opencode_api_key = None; // the server has no password set

    let summarizer = build_summarizer(&config, ProviderKind::OpenCode, &config.default_model);

    assert_eq!(summarizer.strategy_name(), "opencode");
    assert!(!summarizer.is_noop());
}

// ── degradation: missing URL/key must never point at a wrong endpoint ───────

#[test]
fn missing_openrouter_key_degrades_to_noop() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    let mut config = config_for(ProviderKind::OpenRouter);
    config.openrouter_api_key = None;

    let summarizer = build_summarizer(&config, ProviderKind::OpenRouter, &config.default_model);

    assert!(summarizer.is_noop());
    assert_eq!(summarizer.strategy_name(), "noop");
}

#[test]
fn missing_anthropic_profile_url_degrades_to_noop() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    // Built-in Anthropic has no base URL; the key alone is not enough.
    std::env::set_var("ANTHROPIC_API_KEY", "sk-anthropic-selection-secret");
    let config = config_for(ProviderKind::Anthropic);

    let summarizer = build_summarizer(&config, ProviderKind::Anthropic, &config.default_model);

    assert!(summarizer.is_noop());
    assert_eq!(summarizer.strategy_name(), "noop");
}

#[test]
fn missing_opencode_url_degrades_to_noop() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    let mut config = config_for(ProviderKind::OpenCode);
    config.opencode_url = String::new();
    config.opencode_api_key = Some("opencode-selection-secret".into());

    let summarizer = build_summarizer(&config, ProviderKind::OpenCode, &config.default_model);

    assert!(summarizer.is_noop());
    assert_eq!(summarizer.strategy_name(), "noop");
}

// ── the selected strategy is wired to the right URL and credential ─────────

#[tokio::test]
async fn openrouter_selection_posts_bearer_key_to_the_configured_base() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    let server = fake_server(
        &["/v1/chat/completions"],
        json!({ "choices": [{ "message": { "content": "resumen" } }] }),
    )
    .await;
    let mut config = config_for(ProviderKind::OpenRouter);
    config.openai_base_url = Some(format!("{}/v1", server.url));
    config.openrouter_api_key = Some("sk-or-wired-secret".into());

    let summarizer = build_summarizer(&config, ProviderKind::OpenRouter, &config.default_model);
    let out = summarizer
        .summarize(&sample_messages())
        .await
        .expect("summarize against the fake OpenRouter endpoint");
    assert_eq!(out, "resumen");

    let captured = server.recorder.get();
    assert_eq!(captured.path, "/v1/chat/completions");
    assert_eq!(
        captured.authorization.as_deref(),
        Some("Bearer sk-or-wired-secret")
    );
    assert_eq!(captured.body["model"], "selection-model");
}

#[tokio::test]
async fn anthropic_selection_posts_profile_key_to_the_profile_base() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    let server = fake_server(
        &["/v1/chat/completions"],
        json!({ "choices": [{ "message": { "content": "resumen" } }] }),
    )
    .await;
    _env.write_providers_toml(&format!(
        "[providers.anthropic]\nbase_url = \"{}/v1\"\n",
        server.url
    ));
    std::env::set_var("ANTHROPIC_API_KEY", "sk-anthropic-wired-secret");
    let config = config_for(ProviderKind::Anthropic);

    let summarizer = build_summarizer(&config, ProviderKind::Anthropic, &config.default_model);
    let out = summarizer
        .summarize(&sample_messages())
        .await
        .expect("summarize against the fake Anthropic-compatible endpoint");
    assert_eq!(out, "resumen");

    let captured = server.recorder.get();
    assert_eq!(captured.path, "/v1/chat/completions");
    assert_eq!(
        captured.authorization.as_deref(),
        Some("Bearer sk-anthropic-wired-secret")
    );
}

#[tokio::test]
async fn opencode_selection_posts_basic_auth_to_the_generate_endpoint() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::fresh();
    let server = fake_server(
        &["/api/experimental/generate/text"],
        json!({ "data": { "text": "resumen" } }),
    )
    .await;
    let mut config = config_for(ProviderKind::OpenCode);
    config.opencode_url = server.url.clone();
    config.opencode_api_key = Some("opencode-wired-secret".into());

    let summarizer = build_summarizer(&config, ProviderKind::OpenCode, &config.default_model);
    let out = summarizer
        .summarize(&sample_messages())
        .await
        .expect("summarize against the fake OpenCode endpoint");
    assert_eq!(out, "resumen");

    let captured = server.recorder.get();
    assert_eq!(captured.path, "/api/experimental/generate/text");
    assert_eq!(
        captured.authorization.as_deref(),
        Some("Basic b3BlbmNvZGU6b3BlbmNvZGUtd2lyZWQtc2VjcmV0")
    );
    assert_eq!(captured.body["model"]["providerID"], "opencode");
}
