//! A per-agent `provider` in TOML must select that provider's wire, not the
//! upstream that booted the engine.
//!
//! This is the test the task-level reviews lacked: it drives `AgentRuntime::chat`
//! end to end (agent manifest → resolver → engine → Rig client) against a real
//! `TcpListener` and asserts the request that arrives on the socket is the
//! Anthropic/Gemini wire even though the engine booted with an Ollama target.
//! No live provider is contacted; the listener is bound to an ephemeral port.

#![allow(clippy::await_holding_lock)] // the env lock is held for the whole test by design

use llama_r::adapters::context::NoopCompactor;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::adapters::rig_engine::local_tools::LocalToolRegistry;
use llama_r::adapters::rig_engine::{engine_target_for, RigAgentEngine};
use llama_r::config::{Config, ProviderKind};
use llama_r::context::analyzer::ContextEnricher;
use llama_r::context::store::ContextStore;
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::providers::registry::ProviderRegistry;
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::agent_runtime::{AgentRuntime, RuntimeChatRequest};
use llama_r::services::skill_manager::SkillManager;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// `LLAMA_R_DIR` and the key env vars are process-wide; one lock for the whole
/// test keeps the two cases from stomping on each other.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

const KEY_ENV_VARS: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "OPENAI_BASE_URL",
];

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
    dir: TempDir,
    prev_dir: Option<String>,
}

impl EnvGuard {
    fn fresh(keys: &[(&str, &str)], base_url: &str) -> Self {
        let saved = KEY_ENV_VARS
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect();
        let prev_dir = std::env::var("LLAMA_R_DIR").ok();
        let dir = TempDir::new().expect("temp base dir");
        for name in KEY_ENV_VARS {
            std::env::remove_var(name);
        }
        for (name, value) in keys {
            std::env::set_var(name, value);
        }
        std::env::set_var("OPENAI_BASE_URL", base_url);
        std::env::set_var("LLAMA_R_DIR", dir.path());
        Self {
            saved,
            dir,
            prev_dir,
        }
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
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

/// Records the first HTTP request it sees and answers with a canned 401 whose
/// body echoes the key (the wrapper scrubs it — already covered elsewhere).
struct WireServer {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
    _handle: JoinHandle<()>,
}

impl WireServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let handle = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            let header_end = loop {
                let read = match socket.read(&mut tmp).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..read]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            sink.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..header_end]).to_string());
            let body = b"{\"error\":{\"message\":\"unauthorized\"}}";
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.write_all(body).await;
            let _ = socket.flush().await;
        });
        Self {
            addr,
            seen,
            _handle: handle,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn first_request(&self) -> String {
        self.seen
            .lock()
            .unwrap()
            .first()
            .cloned()
            .expect("a request must have reached the wire")
    }
}

/// A boot-Ollama engine carrying a `Config` whose OPENAI_BASE_URL points at the
/// local wire. The engine's boot target is Ollama; `run_inner` must replace it
/// with a target built from the request's resolved provider.
fn boot_ollama_runtime(config: &Config, base_dir: &std::path::Path, agent: &str) -> AgentRuntime {
    let agents = base_dir.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    let manifest = format!(
        "name = \"{}\"\nprovider = {:?}\nmodel = {:?}\nsystem_prompt = \"sys\"\n",
        agent,
        match agent {
            "reviewer-ant" => "anthropic",
            _ => "gemini",
        },
        match agent {
            "reviewer-ant" => "claude-3-5-sonnet",
            _ => "gemini-1.5-pro",
        }
    );
    std::fs::write(agents.join(format!("{agent}.toml")), manifest).unwrap();

    let registry = AgentRegistry::new();
    registry.reload_all(&[]).unwrap();

    let boot_target = engine_target_for(config).expect("Ollama gets a boot engine");
    assert_eq!(
        format!("{:?}", boot_target.kind()),
        "Ollama",
        "the engine boots against Ollama, not the agent's provider"
    );
    let engine = RigAgentEngine::from_target(
        boot_target,
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
        config.clone(),
        Arc::new(SkillManager::new()),
        Arc::new(LocalToolRegistry::builtins()),
        Arc::new(llama_r::services::approval::ApprovalGate::default()),
    );
    let enricher = ContextEnricher::new(
        Arc::new(ContextStore::new()),
        Arc::new(SkillManager::new()),
        config.default_model.clone(),
        Arc::new(ProviderRegistry::default()),
        config.provider,
    );
    AgentRuntime {
        registry: Arc::new(registry),
        engine: Arc::new(engine),
        history: None,
        rag: None,
        enricher: Arc::new(enricher),
        compactor: Arc::new(NoopCompactor),
    }
}

fn config(base_url: &str) -> Config {
    Config {
        port: 3000,
        provider: ProviderKind::Ollama,
        ollama_url: "http://127.0.0.1:11434".to_string(),
        default_model: "global-model".to_string(),
        embedding_model: "nomic-embed-text".to_string(),
        embedding_dimensions: 768,
        rag_enabled: true,
        history_retention_days: 90,
        openrouter_api_key: None,
        opencode_api_key: None,
        opencode_url: "http://127.0.0.1:4096".to_string(),
        openai_base_url: Some(base_url.to_string()),
    }
}

fn request(agent: &str) -> RuntimeChatRequest {
    RuntimeChatRequest {
        project_id: None,
        agent_id: Some(agent.to_string()),
        conversation_id: None,
        user_message: "hi".to_string(),
        history: Vec::new(),
        model_override: None,
        system_prompt: None,
        default_model: "global-model".to_string(),
        execution_id: uuid::Uuid::new_v4(),
        shared_budget: None,
        delegation_context: None,
        approval_event_tx: None,
        tool_event_tx: None,
    }
}

fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (found, value) = line.split_once(':')?;
        found
            .trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim())
    })
}

#[tokio::test]
async fn an_anthropic_agent_overrides_an_ollama_boot_on_the_wire() {
    let _guard = env_lock().lock().unwrap();
    const KEY: &str = "sk-anthropic-per-agent-wire";
    let server = WireServer::start().await;
    let _env = EnvGuard::fresh(&[("ANTHROPIC_API_KEY", KEY)], &server.url());
    let cfg = config(&server.url());
    let runtime = boot_ollama_runtime(&cfg, _env.path(), "reviewer-ant");

    // The canned 401 surfaces as an error; the assertion is about what arrived.
    let _ = runtime.chat(request("reviewer-ant")).await;

    let seen = server.first_request();
    assert!(
        seen.starts_with("POST /v1/messages "),
        "the Anthropic wire must be hit, not the boot Ollama wire: {seen}"
    );
    assert_eq!(header(&seen, "x-api-key"), Some(KEY));
    assert!(
        !seen.contains("/api/chat"),
        "an Ollama boot target must never be used for an Anthropic agent: {seen}"
    );
}

#[tokio::test]
async fn a_gemini_agent_overrides_an_ollama_boot_on_the_wire() {
    let _guard = env_lock().lock().unwrap();
    const KEY: &str = "sk-gemini-per-agent-wire";
    let server = WireServer::start().await;
    let _env = EnvGuard::fresh(&[("GEMINI_API_KEY", KEY)], &server.url());
    let cfg = config(&server.url());
    let runtime = boot_ollama_runtime(&cfg, _env.path(), "reviewer-gem");

    let _ = runtime.chat(request("reviewer-gem")).await;

    let seen = server.first_request();
    assert!(
        seen.starts_with("POST /v1beta/interactions "),
        "the Gemini wire must be hit, not the boot Ollama wire: {seen}"
    );
    assert_eq!(header(&seen, "x-goog-api-key"), Some(KEY));
    assert!(
        !seen.contains("/api/chat"),
        "an Ollama boot target must never be used for a Gemini agent: {seen}"
    );
}
