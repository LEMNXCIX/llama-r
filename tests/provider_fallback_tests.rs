//! Real-behavior fallback and usage tests.
//!
//! Two real HTTP servers stand in for two configured profiles: profile A is
//! unreachable/500, profile B answers with OpenAI-shaped usage counts. A single
//! chat request must fail over from A to B (bounded to one retry), map the
//! upstream token counts into `ChatResponse.usage`, name the answering profile
//! without ever leaking a key, and never fabricate a cost from absent prices.

#![allow(clippy::await_holding_lock)] // the env lock is held for the whole test by design

use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::chat_core::execute_chat;
use llama_r::api::handlers::AppState;
use llama_r::api::route_resolution::AgentSelection;
use llama_r::config::{Config, ProviderKind};
use llama_r::context::store::ContextStore;
use llama_r::domain::models::{ChatMessage, ChatRequest};
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::providers::openai_compatible::OpenAiCompatibleProvider;
use llama_r::providers::registry::ProviderRegistry;
use llama_r::runtime::{build_app_state, build_provider_stack};
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::provider_stack::{FallbackProvider, ProviderStack};
use llama_r::services::skill_manager::SkillManager;
use std::collections::VecDeque;
use std::net::SocketAddr;
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

/// Isolate `get_base_dir()` so a developer's `providers.toml` cannot change the
/// built-in order this startup test relies on. Restored on drop.
struct BaseDirGuard {
    previous: Option<String>,
    _dir: TempDir,
}

impl BaseDirGuard {
    fn fresh() -> Self {
        let previous = std::env::var("LLAMA_R_DIR").ok();
        let dir = tempfile::tempdir().expect("temp base dir");
        std::env::set_var("LLAMA_R_DIR", dir.path());
        Self {
            previous,
            _dir: dir,
        }
    }
}

impl Drop for BaseDirGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("LLAMA_R_DIR", value),
            None => std::env::remove_var("LLAMA_R_DIR"),
        }
    }
}

/// Startup wires exactly one fallback, chosen as the next profile after the
/// primary in the deterministic order — never a key, never a chain.
#[test]
fn startup_builds_one_deterministic_fallback() {
    let _lock = test_lock();
    let _dir = BaseDirGuard::fresh();

    let config = Config {
        port: 3000,
        provider: ProviderKind::Ollama,
        ollama_url: "http://127.0.0.1:11434".into(),
        default_model: "model".into(),
        embedding_model: "nomic-embed-text".into(),
        embedding_dimensions: 768,
        rag_enabled: true,
        history_retention_days: 90,
        openrouter_api_key: None,
        opencode_api_key: None,
        opencode_url: "http://127.0.0.1:4096".into(),
        openai_base_url: None,
    };
    let stack = build_provider_stack(&config).expect("build stack");
    let fallback = stack.fallback_provider.expect("a next profile exists");

    // Built-ins sorted by name: anthropic, gemini, ollama, openai-compatible,
    // opencode, openrouter. Ollama's next is anthropic.
    assert_eq!(fallback.name, "anthropic");
    assert!(
        !fallback.name.contains("sk-"),
        "the recorded identity is a profile name, never a key"
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
