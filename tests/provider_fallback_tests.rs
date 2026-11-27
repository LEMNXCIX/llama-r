//! Real-behavior fallback and usage tests.
//!
//! Two real HTTP servers stand in for two configured profiles: profile A is
//! unreachable/500, profile B answers with OpenAI-shaped usage counts. A single
//! chat request must fail over from A to B (bounded to one retry), map the
//! upstream token counts into `ChatResponse.usage`, name the answering profile
//! without ever leaking a key, and never fabricate a cost from absent prices.

#![allow(clippy::await_holding_lock)] // the env lock is held for the whole test by design

use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::chat_core::{execute_chat, execute_chat_stream};
use llama_r::api::handlers::AppState;
use llama_r::api::route_resolution::AgentSelection;
use llama_r::config::{Config, ProviderKind};
use llama_r::context::store::ContextStore;
use llama_r::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent};
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::providers::openai_compatible::OpenAiCompatibleProvider;
use llama_r::providers::registry::ProviderRegistry;
use llama_r::runtime::{build_app_state, build_provider_stack};
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::provider_stack::{FallbackProvider, ProviderStack};
use llama_r::services::skill_manager::SkillManager;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const TEST_KEY: &str = "sk-this-secret-must-never-leak-12345";

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A tiny HTTP/1.1 server: `status = 500` makes the provider error, a 200 with a
/// canned JSON body makes it answer. One request per connection is all a single
/// JSON POST needs.
struct StubServer {
    addr: SocketAddr,
    hits: Arc<Mutex<usize>>,
    _handle: JoinHandle<()>,
}

impl StubServer {
    async fn start(status: u16, reply: serde_json::Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(0usize));
        let counter = hits.clone();
        let status_line = if status == 200 {
            "HTTP/1.1 200 OK"
        } else {
            "HTTP/1.1 500 Internal Server Error"
        };
        let body = serde_json::to_vec(&reply).unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                *counter.lock().unwrap() += 1;
                let status_line = status_line;
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    // Drain the request body before replying so the client does
                    // not see a broken pipe mid-write.
                    loop {
                        match socket.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let response = format!(
                        "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.flush().await;
                });
            }
        });
        Self {
            addr,
            hits,
            _handle: handle,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn hits(&self) -> usize {
        *self.hits.lock().unwrap()
    }
}

fn openai_reply(content: &str, input: u64, output: u64) -> serde_json::Value {
    serde_json::json!({
        "model": "openai/gpt-4o-mini",
        "choices": [{ "message": { "role": "assistant", "content": content } }],
        "usage": { "prompt_tokens": input, "completion_tokens": output, "total_tokens": input + output }
    })
}

fn state_for(stack: ProviderStack) -> Arc<AppState> {
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

fn chat(content: &str) -> ChatRequest {
    ChatRequest {
        model: "requested-model".into(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: content.into(),
        }],
        stream: false,
    }
}

fn selection() -> AgentSelection<'static> {
    AgentSelection {
        project_id: None,
        agent_id: None,
        conversation_id: None,
        debug: false,
    }
}

/// Provider A fails (500). The stack's one fallback, profile "provider-b",
/// answers with token counts. The response must carry those counts, name B,
/// and never contain the configured key.
#[tokio::test]
async fn a_failing_provider_falls_back_to_the_next_profile_once() {
    let _lock = test_lock();

    let a = StubServer::start(500, serde_json::json!({"error": "upstream down"})).await;
    let b = StubServer::start(200, openai_reply("answered-by-B", 12, 7)).await;

    let primary = OpenAiCompatibleProvider::new(a.url(), TEST_KEY.into(), "m".into());
    let fallback = OpenAiCompatibleProvider::new(b.url(), TEST_KEY.into(), "m".into());
    let mut stack = ProviderStack::new(Arc::new(primary), "default-model");
    stack.provider_name = "provider-a".into();
    stack.fallback_provider = Some(FallbackProvider {
        name: "provider-b".into(),
        provider: Arc::new(fallback),
    });
    let state = state_for(stack);

    let response = execute_chat(&state, chat("hello"), selection())
        .await
        .expect("fallback should answer");

    assert_eq!(response.message.content, "answered-by-B");
    assert_eq!(response.served_by.as_deref(), Some("provider-b"));

    let usage = response.usage.as_ref().expect("upstream reported usage");
    assert_eq!(usage.input_tokens, 12);
    assert_eq!(usage.output_tokens, 7);
    assert_eq!(
        usage.cost_usd, None,
        "no cost may be fabricated from absent price figures"
    );

    // No key in any serialized field, debug output included.
    let dumped = format!("{response:?}");
    assert!(
        !dumped.contains(TEST_KEY),
        "key leaked through Debug: {dumped}"
    );
    let json = serde_json::to_string(&response).unwrap();
    assert!(!json.contains(TEST_KEY), "key leaked through JSON: {json}");

    // A is tried once for the requested model and once for DEFAULT_MODEL; B is
    // tried exactly once. The chain never grows past one fallback provider.
    assert_eq!(
        a.hits(),
        2,
        "primary: requested model + DEFAULT_MODEL retry"
    );
    assert_eq!(b.hits(), 1, "fallback is attempted exactly once");
}

/// When the primary answers, no fallback provider is contacted and `served_by`
/// stays absent.
#[tokio::test]
async fn a_healthy_primary_never_contacts_the_fallback() {
    let _lock = test_lock();

    let a = StubServer::start(200, openai_reply("answered-by-A", 3, 4)).await;
    let b = StubServer::start(200, openai_reply("should-not-be-used", 1, 1)).await;

    let primary = OpenAiCompatibleProvider::new(a.url(), TEST_KEY.into(), "m".into());
    let fallback = OpenAiCompatibleProvider::new(b.url(), TEST_KEY.into(), "m".into());
    let mut stack = ProviderStack::new(Arc::new(primary), "requested-model");
    stack.provider_name = "provider-a".into();
    stack.fallback_provider = Some(FallbackProvider {
        name: "provider-b".into(),
        provider: Arc::new(fallback),
    });
    let state = state_for(stack);

    let response = execute_chat(&state, chat("hello"), selection())
        .await
        .expect("primary should answer");

    assert_eq!(response.message.content, "answered-by-A");
    assert_eq!(response.served_by, None);
    assert_eq!(response.usage.as_ref().unwrap().input_tokens, 3);
    assert_eq!(a.hits(), 1);
    assert_eq!(b.hits(), 0, "fallback must not be contacted on success");
}

/// A provider that does not report token counts yields `usage: None` — optional
/// means never required and never fabricated.
#[tokio::test]
async fn missing_usage_is_none_not_zero() {
    let _lock = test_lock();

    let reply = serde_json::json!({
        "model": "openai/gpt-4o-mini",
        "choices": [{ "message": { "role": "assistant", "content": "no usage" } }]
    });
    let a = StubServer::start(200, reply).await;
    let primary = OpenAiCompatibleProvider::new(a.url(), TEST_KEY.into(), "m".into());
    let stack = ProviderStack::new(Arc::new(primary), "requested-model");
    let state = state_for(stack);

    let response = execute_chat(&state, chat("hello"), selection())
        .await
        .expect("primary should answer");

    assert_eq!(response.message.content, "no usage");
    assert!(
        response.usage.is_none(),
        "no upstream counts must produce no usage"
    );
}

/// Every env var a built-in profile names as its `api_key_env`. Cleared so a
/// startup test cannot accidentally pick up the developer's shell credentials,
/// and restored on drop.
const KEY_ENV_VARS: &[&str] = &[
    "OPENROUTER_API_KEY",
    "OPENCODE_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
];

/// Isolate `get_base_dir()` (so a developer's `providers.toml` cannot change the
/// built-in order) and every key var the fallback filter reads.
struct EnvGuard {
    saved_keys: Vec<(&'static str, Option<String>)>,
    prev_dir: Option<String>,
    _dir: TempDir,
}

impl EnvGuard {
    fn fresh() -> Self {
        let saved_keys = KEY_ENV_VARS
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect();
        let prev_dir = std::env::var("LLAMA_R_DIR").ok();
        let dir = tempfile::tempdir().expect("temp base dir");
        for name in KEY_ENV_VARS {
            std::env::remove_var(name);
        }
        std::env::set_var("LLAMA_R_DIR", dir.path());
        Self {
            saved_keys,
            prev_dir,
            _dir: dir,
        }
    }

    /// A dummy value used only to make a key-requiring profile usable. It is
    /// never sent anywhere: no network call is made in these startup tests.
    fn set_key(&self, name: &str, value: &str) {
        std::env::set_var(name, value);
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved_keys {
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

fn startup_config(ollama_url: &str, model: &str) -> Config {
    Config {
        port: 3000,
        provider: ProviderKind::Ollama,
        ollama_url: ollama_url.into(),
        default_model: model.into(),
        embedding_model: "nomic-embed-text".into(),
        embedding_dimensions: 768,
        rag_enabled: true,
        history_retention_days: 90,
        openrouter_api_key: None,
        opencode_api_key: None,
        opencode_url: "http://127.0.0.1:4096".into(),
        openai_base_url: None,
        ..Config::default()
    }
}

/// With no keys set, every non-primary built-in that precedes another profile
/// requires a credential it does not have. The fallback must be absent: falling
/// back to a provider we know cannot authenticate would replace the real
/// connection error with a spurious "Anthropic 401".
#[test]
fn startup_without_credentials_has_no_fallback() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();

    let stack = build_provider_stack(&startup_config("http://127.0.0.1:11434", "model"))
        .expect("build stack");
    assert!(
        stack.fallback_provider.is_none(),
        "no usable fallback exists without credentials"
    );
}

/// A key-requiring profile becomes the fallback once its env var is set. The
/// recorded identity stays a secret-free profile name.
#[test]
fn startup_picks_the_first_usable_profile_when_its_key_is_set() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    let dummy = "sk-dummy-key-never-sent";
    _env.set_key("ANTHROPIC_API_KEY", dummy);

    let stack = build_provider_stack(&startup_config("http://127.0.0.1:11434", "model"))
        .expect("build stack");
    let fallback = stack.fallback_provider.expect("anthropic is now usable");

    // Built-ins sorted by name: anthropic, gemini, ollama, openai-compatible,
    // opencode, openrouter. Anthropic is first and now has a key.
    assert_eq!(fallback.name, "anthropic");
    assert!(
        !fallback.name.contains(dummy),
        "the key value must never appear in the recorded identity"
    );
}

/// The contract in AGENTS.md is that an unreachable provider fails with a 502
/// naming the connection error. With no usable fallback that exact error must
/// survive dispatch — not be masked by a "Fallback also failed" wrapper.
#[tokio::test]
async fn with_no_usable_fallback_the_real_connection_error_survives() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();

    let a = StubServer::start(500, serde_json::json!({"error": "upstream down"})).await;
    let stack =
        build_provider_stack(&startup_config(&a.url(), "requested-model")).expect("build stack");
    let state = state_for(stack);

    let err = execute_chat(&state, chat("hello"), selection())
        .await
        .expect_err("the failing provider must surface its error");

    let text = err.to_string();
    assert!(
        text.contains("Ollama error"),
        "expected the real provider error: {text}"
    );
    assert!(
        !text.contains("Fallback also failed"),
        "no usable fallback must not replace the connection error: {text}"
    );
}

/// The registry's iteration order is deterministic and independent of the
/// hash map's random seed — the fallback "next profile" depends on this.
#[test]
fn registry_order_is_deterministic() {
    let a = ProviderRegistry::builtins();
    let b = ProviderRegistry::builtins();
    let names_a: Vec<_> = a.ordered().iter().map(|p| p.name.clone()).collect();
    let names_b: Vec<_> = b.ordered().iter().map(|p| p.name.clone()).collect();
    assert_eq!(names_a, names_b);
    let mut sorted = names_a.clone();
    sorted.sort();
    assert_eq!(names_a, sorted, "ordered() must be in name order");
}

/// A provider that records the model it was asked to run. `fail` makes both
/// `chat` and `chat_stream` fail with an ordinary error, so the fallback rung is
/// reached on both paths without needing a real upstream.
struct RecordingProvider {
    fail: bool,
    models: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl llama_r::providers::LLMProvider for RecordingProvider {
    fn get_base_url(&self) -> String {
        "http://recording.invalid".to_string()
    }

    async fn health_check(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn list_models(
        &self,
    ) -> Result<Vec<llama_r::domain::models::ModelInfo>, Box<dyn std::error::Error + Send + Sync>>
    {
        Ok(Vec::new())
    }

    async fn chat(
        &self,
        request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn std::error::Error + Send + Sync>> {
        self.models.lock().unwrap().push(request.model.clone());
        if self.fail {
            return Err("primary down".into());
        }
        Ok(ChatResponse {
            model: request.model,
            created_at: "now".into(),
            message: ChatMessage {
                role: "assistant".into(),
                content: "answered".into(),
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
        request: ChatRequest,
    ) -> Result<
        Pin<
            Box<
                dyn tokio_stream::Stream<
                        Item = Result<ChatStreamEvent, Box<dyn std::error::Error + Send + Sync>>,
                    > + Send,
            >,
        >,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        self.models.lock().unwrap().push(request.model.clone());
        if self.fail {
            return Err("primary stream down".into());
        }
        let event = ChatStreamEvent {
            model: request.model,
            created_at: "now".into(),
            message: ChatMessage {
                role: "assistant".into(),
                content: "answered".into(),
            },
            done: true,
            approval: None,
            tool: None,
        };
        Ok(Box::pin(tokio_stream::iter(vec![Ok(event)])))
    }
}

#[allow(clippy::type_complexity)]
fn recording_stack() -> (
    ProviderStack,
    Arc<Mutex<Vec<String>>>,
    Arc<Mutex<Vec<String>>>,
) {
    let primary_models = Arc::new(Mutex::new(Vec::new()));
    let fallback_models = Arc::new(Mutex::new(Vec::new()));
    let mut stack = ProviderStack::new(
        Arc::new(RecordingProvider {
            fail: true,
            models: Arc::clone(&primary_models),
        }),
        "default-model",
    );
    stack.fallback_provider = Some(FallbackProvider {
        name: "fallback".into(),
        provider: Arc::new(RecordingProvider {
            fail: false,
            models: Arc::clone(&fallback_models),
        }),
    });
    (stack, primary_models, fallback_models)
}

/// The fallback must be asked for DEFAULT_MODEL, never the primary's model:
/// adapters prefer a non-empty request model over their own default, so a
/// different catalog on the fallback would otherwise fail with model-not-found.
#[tokio::test]
async fn a_fallback_is_asked_for_the_default_model_not_the_primary_model() {
    let _lock = test_lock();
    let (stack, primary_models, fallback_models) = recording_stack();
    let state = state_for(stack);

    let response = execute_chat(&state, chat("hello"), selection())
        .await
        .expect("fallback should answer");
    assert_eq!(response.served_by.as_deref(), Some("fallback"));

    // Rung 1 retries the SAME provider with DEFAULT_MODEL; rung 2 uses the
    // fallback provider, also with DEFAULT_MODEL — never "requested-model".
    assert_eq!(
        primary_models.lock().unwrap().as_slice(),
        &["requested-model", "default-model"]
    );
    assert_eq!(
        fallback_models.lock().unwrap().as_slice(),
        &["default-model"]
    );
}

/// The streaming path applies the same rule when only opening a stream.
#[tokio::test]
async fn a_streaming_fallback_is_asked_for_the_default_model() {
    let _lock = test_lock();
    let (stack, primary_models, fallback_models) = recording_stack();
    let state = state_for(stack);

    let mut stream = execute_chat_stream(&state, chat("hello"), selection())
        .await
        .expect("fallback stream should open");
    use tokio_stream::StreamExt;
    let mut saw_done = false;
    while let Some(event) = stream.next().await {
        saw_done |= event.expect("stream event").done;
    }
    assert!(saw_done, "the fallback stream must complete");

    assert_eq!(
        primary_models.lock().unwrap().as_slice(),
        &["requested-model", "default-model"]
    );
    assert_eq!(
        fallback_models.lock().unwrap().as_slice(),
        &["default-model"]
    );
}
