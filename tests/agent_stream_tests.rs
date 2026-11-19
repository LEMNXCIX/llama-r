//! Task 6: progressive agent tokens and the streaming capability gate.
//!
//! Two behaviours are pinned here with real sockets and the real engine:
//! - a streamed completion yields one `Token` event per text delta (never a
//!   single full-text prelude masquerading as a stream), followed by `Completed`;
//! - a resolved model whose catalog says `streaming: false` is rejected by the
//!   capability gate *before* the engine or the provider opens a connection.

use async_trait::async_trait;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::adapters::rig_engine::local_tools::LocalToolRegistry;
use llama_r::adapters::rig_engine::{EngineTarget, RigAgentEngine};
use llama_r::api::chat_core::execute_chat_stream;
use llama_r::api::handlers::AppState;
use llama_r::api::route_resolution::AgentSelection;
use llama_r::config::{Config, ProviderKind};
use llama_r::context::store::ContextStore;
use llama_r::domain::agent::{Agent, AgentConfig, RagWritePolicy};
use llama_r::domain::models::{ChatMessage, ChatRequest};
use llama_r::domain::scope::{AgentScope, ToolsAllow};
use llama_r::error::AppError;
use llama_r::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult};
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::providers::capabilities::capabilities_for;
use llama_r::providers::LLMProvider;
use llama_r::runtime::build_app_state;
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::agent_runtime::AgentRuntime;
use llama_r::services::provider_stack::ProviderStack;
use llama_r::services::skill_manager::SkillManager;
use std::collections::HashSet;
use std::error::Error;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::Stream;

const CHUNKS: [&str; 3] = ["Hel", "lo ", "world!"];
const FULL_TEXT: &str = "Hello world!";

/// Minimal Ollama-compatible server: one `POST /api/chat`, answered with an
/// NDJSON body carrying three text deltas and a final `done` frame. This is a
/// real byte stream, so rig's own NDJSON parser drives the agent stream.
struct OllamaStreamServer {
    addr: SocketAddr,
    handle: JoinHandle<()>,
}

impl OllamaStreamServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            let header_end = loop {
                let Ok(n) = socket.read(&mut tmp).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            let total = header_end + content_length;
            while buf.len() < total {
                let Ok(n) = socket.read(&mut tmp).await else {
                    break;
                };
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let request_body = String::from_utf8_lossy(&buf[header_end..total]).to_string();

            // A real Ollama streams only when asked to. Serving a single JSON
            // completion for the non-streaming request is what makes the old
            // "full completion dressed up as a stream" fail on token count,
            // rather than on a JSON parse error.
            let body = if request_body.contains("\"stream\":true") {
                let mut ndjson = String::new();
                for chunk in CHUNKS {
                    ndjson.push_str(&format!(
                        "{{\"model\":\"llama3.2\",\"created_at\":\"2024-01-01T00:00:00Z\",\"message\":{{\"role\":\"assistant\",\"content\":{}}},\"done\":false}}\n",
                        serde_json::to_string(chunk).unwrap()
                    ));
                }
                ndjson.push_str(
                    "{\"model\":\"llama3.2\",\"created_at\":\"2024-01-01T00:00:00Z\",\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"prompt_eval_count\":1,\"eval_count\":3}\n",
                );
                ndjson
            } else {
                format!(
                    "{{\"model\":\"llama3.2\",\"created_at\":\"2024-01-01T00:00:00Z\",\"message\":{{\"role\":\"assistant\",\"content\":{}}},\"done\":true}}\n",
                    serde_json::to_string(FULL_TEXT).unwrap()
                )
            };

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        });
        Self { addr, handle }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for OllamaStreamServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn run_request(model: &str, provider: ProviderKind) -> AgentRunRequest {
    AgentRunRequest {
        agent: Agent {
            id: "a".to_string(),
            project_id: None,
            config: AgentConfig::default(),
        },
        scope: AgentScope {
            agent_id: "a".to_string(),
            project_id: None,
            mcp_sources: HashSet::new(),
            tools_allow: ToolsAllow::DenyAll,
            local_tools_allow: ToolsAllow::DenyAll,
            rag_sources: HashSet::new(),
            rag_write: RagWritePolicy::None,
            max_tool_calls: 0,
            max_iterations: 1,
            timeout_secs: 5,
            permission: llama_r::domain::policy::PermissionPolicy::default(),
        },
        model: model.to_string(),
        provider,
        capabilities: capabilities_for(provider, model),
        system_prompt: "sys".to_string(),
        user_message: "hi".to_string(),
        history: Vec::new(),
        conversation_id: None,
        execution_id: uuid::Uuid::new_v4(),
        shared_budget: None,
    }
}

/// A minimal config for building an engine in tests. Holds no keys. The URL is
/// the fake server's, because the per-request target now rebuilds Ollama from
/// `config.ollama_url` rather than reusing the engine's boot target.
fn test_config(ollama_url: &str) -> Config {
    Config {
        port: 3000,
        provider: ProviderKind::Ollama,
        ollama_url: ollama_url.to_string(),
        default_model: "llama3.2".to_string(),
        embedding_model: "nomic-embed-text".to_string(),
        embedding_dimensions: 768,
        rag_enabled: true,
        history_retention_days: 90,
        openrouter_api_key: None,
        opencode_api_key: None,
        opencode_url: "http://127.0.0.1:4096".to_string(),
        openai_base_url: None,
    }
}

/// Drive the real engine against the fake Ollama server and collect the events.
async fn collect_engine_stream() -> (Vec<String>, Option<AgentRunResult>) {
    let server = OllamaStreamServer::start().await;
    let engine = RigAgentEngine::from_target(
        EngineTarget::Ollama { url: server.url() },
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
        test_config(&server.url()),
        Arc::new(SkillManager::new()),
        Arc::new(LocalToolRegistry::builtins()),
        Arc::new(llama_r::services::approval::ApprovalGate::default()),
    );
    let mut rx = engine
        .run_stream(run_request("llama3.2", ProviderKind::Ollama))
        .await
        .expect("run_stream must open the event channel");

    let mut tokens = Vec::new();
    let mut completed = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentRunEvent::Token { text } => tokens.push(text),
            AgentRunEvent::Completed { result } => completed = Some(result),
            AgentRunEvent::Error { message } => panic!("unexpected stream error: {message}"),
            _ => {}
        }
    }
    (tokens, completed)
}

#[tokio::test]
async fn a_streaming_agent_yields_more_than_one_token_event() {
    let (tokens, completed) = collect_engine_stream().await;

    assert!(
        tokens.len() > 1,
        "a real stream must emit a token per delta, got {tokens:?}"
    );
    assert_eq!(tokens, CHUNKS, "each completed text delta is its own token");
    assert_eq!(tokens.concat(), FULL_TEXT);

    let completed = completed.expect("a streaming run must end with Completed");
    assert_eq!(completed.text, FULL_TEXT);
}

#[tokio::test]
async fn no_full_text_token_is_emitted_before_the_deltas() {
    let (tokens, _completed) = collect_engine_stream().await;

    assert!(!tokens.is_empty(), "expected at least one token");
    assert_eq!(
        tokens[0], CHUNKS[0],
        "the first token must be the first chunk, not a full-text prelude"
    );
    assert!(
        tokens.iter().all(|t| t != FULL_TEXT),
        "no token may be the whole answer: {tokens:?}"
    );
}

// ---- capability gate ----------------------------------------------------

/// A provider whose `chat_stream` refuses immediately, exactly like the real
/// OpenRouter/OpenCode adapters. It records whether it was reached at all.
struct UnavailableProvider {
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl LLMProvider for UnavailableProvider {
    fn get_base_url(&self) -> String {
        "http://unavailable".to_string()
    }
    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }
    async fn list_models(
        &self,
    ) -> Result<Vec<llama_r::domain::models::ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
    async fn chat(
        &self,
        _request: ChatRequest,
    ) -> Result<llama_r::domain::models::ChatResponse, Box<dyn Error + Send + Sync>> {
        Err("not reached".into())
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<
        Pin<
            Box<
                dyn Stream<
                        Item = Result<
                            llama_r::domain::models::ChatStreamEvent,
                            Box<dyn Error + Send + Sync>,
                        >,
                    > + Send,
            >,
        >,
        Box<dyn Error + Send + Sync>,
    > {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Err(Box::new(AppError::Unavailable(
            "streaming unsupported".to_string(),
        )))
    }
}

/// An engine that connects to a listener when asked to stream. If a
/// non-streaming model ever reaches the engine branch, that connection is the
/// observable proof the capability gate failed.
struct ConnectingEngine {
    addr: SocketAddr,
}

#[async_trait]
impl AgentEngine for ConnectingEngine {
    async fn run(&self, _req: AgentRunRequest) -> Result<AgentRunResult, String> {
        Err("not reached".into())
    }
    async fn run_stream(
        &self,
        _req: AgentRunRequest,
    ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
        TcpStream::connect(self.addr)
            .await
            .map_err(|e| e.to_string())?;
        let (_tx, rx) = mpsc::channel(1);
        Ok(rx)
    }
}

#[tokio::test]
async fn a_non_streaming_provider_returns_unavailable_before_connecting() {
    // A listener that counts every inbound connection.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = connections.clone();
    let accept = tokio::spawn(async move {
        while listener.accept().await.is_ok() {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });

    // An agent whose resolved pair (openrouter / openai/gpt-4o) has
    // `streaming: false` in the capability catalog.
    let dir = tempfile::tempdir().unwrap();
    let previous_dir = std::env::var_os("LLAMA_R_DIR");
    std::env::set_var("LLAMA_R_DIR", dir.path());
    let agents_dir = dir
        .path()
        .join("contextos")
        .join("projects")
        .join("p")
        .join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("p.toml"),
        "name = \"p\"\nsystem_prompt = \"base\"\nprovider = \"openrouter\"\nmodel = \"openai/gpt-4o\"\n",
    )
    .unwrap();
    let registry = Arc::new(AgentRegistry::new());
    registry.reload_all(&[]).unwrap();

    let provider_hits = Arc::new(AtomicUsize::new(0));
    let stack = ProviderStack::new(
        Arc::new(UnavailableProvider {
            hits: provider_hits.clone(),
        }),
        "fallback-model",
    );
    let state: Arc<AppState> = build_app_state(
        stack,
        registry.clone(),
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        Vec::new(),
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
    );

    // Install an engine whose stream path would connect to the listener. Under
    // a provider-*name* gate this branch runs and the listener sees a hit; under
    // the capability gate it is skipped entirely.
    let runtime = AgentRuntime {
        registry: registry.clone(),
        engine: Arc::new(ConnectingEngine { addr }),
        history: None,
        rag: None,
        enricher: state.context_enricher_snapshot(),
    };
    let mut new_stack = (*state.services_snapshot().await).clone();
    new_stack.agent_runtime = Some(Arc::new(runtime));
    state.replace_services(new_stack).await;

    let payload = ChatRequest {
        model: String::new(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }],
        stream: true,
    };
    let err = match execute_chat_stream(
        &state,
        payload,
        AgentSelection {
            project_id: Some("p"),
            agent_id: None,
            conversation_id: None,
            debug: false,
        },
    )
    .await
    {
        Ok(_) => panic!("a non-streaming model must be refused"),
        Err(err) => err,
    };

    assert!(
        matches!(err, AppError::Unavailable(_)),
        "expected 501/Unavailable, got {err:?}"
    );
    assert_eq!(
        connections.load(Ordering::SeqCst),
        0,
        "the engine must not connect for a non-streaming model"
    );
    assert!(
        provider_hits.load(Ordering::SeqCst) >= 1,
        "the request must fall through to the provider's chat_stream"
    );

    accept.abort();
    match previous_dir {
        Some(value) => std::env::set_var("LLAMA_R_DIR", value),
        None => std::env::remove_var("LLAMA_R_DIR"),
    }
}
