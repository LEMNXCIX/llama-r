//! Task 5: wire-level validation of the three new Rig engine wrappers.
//!
//! A real `TcpListener` records the HTTP request (request line + headers) and
//! answers with a canned error whose body echoes the API key. That proves two
//! things across a socket rather than in memory:
//! - each wrapper builds the right native client, sending the key to that
//!   provider's documented place (and never in the `base_url` we construct);
//! - any upstream error carrying the key is scrubbed to `[redacted]` before it
//!   reaches a caller.
//!
//! No live provider is contacted; the listener is bound to an ephemeral port.

use llama_r::adapters::rig_engine::builder::{
    run_with_anthropic, run_with_gemini, run_with_openai_compatible,
};
use llama_r::adapters::rig_engine::local_tools::LocalToolDeps;
use llama_r::config::ProviderKind;
use llama_r::domain::agent::{Agent, AgentConfig, RagWritePolicy};
use llama_r::domain::scope::{AgentScope, ToolsAllow};
use llama_r::ports::engine::AgentRunRequest;
use llama_r::providers::capabilities::capabilities_for;
use llama_r::services::approval::ApprovalGate;
use llama_r::services::skill_manager::SkillManager;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

fn test_deps() -> LocalToolDeps {
    LocalToolDeps {
        skill_manager: Arc::new(SkillManager::with_paths(Vec::new())),
        rag: None,
        gate: Arc::new(ApprovalGate::default()),
        auto_decider: None,
        observability: None,
        delegator: None,
        delegate_bounds: None,
        delegation_context: None,
        event_tx: None,
        operations: llama_r::adapters::rig_engine::operations::OperationContext::disabled(),
    }
}

#[derive(Clone)]
struct RecordedRequest {
    request_line: String,
    headers: String,
}

/// Minimal HTTP/1.1 server: one canned reply for every path, recording the
/// first request it sees. A 401 with the key in the body is deliberate — it is
/// what exercises the wrapper's key scrub.
struct WireServer {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<RecordedRequest>>>,
    handle: JoinHandle<()>,
}

impl WireServer {
    async fn start(status: &'static str, body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let sink = sink.clone();
                let payload = body.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    let headers = loop {
                        let read = match socket.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..read]);
                        if let Some(end) = find_header_end(&buf) {
                            break String::from_utf8_lossy(&buf[..end]).to_string();
                        }
                    };
                    let request_line = headers.lines().next().unwrap_or_default().to_string();
                    sink.lock().unwrap().push(RecordedRequest {
                        request_line,
                        headers,
                    });

                    let response = format!(
                        "{status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(payload.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });

        Self { addr, seen, handle }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for WireServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (found, value) = line.split_once(':')?;
        found
            .trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim())
    })
}

fn request(provider: ProviderKind, model: &str) -> AgentRunRequest {
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
        delegation_context: None,
    }
}

/// An upstream error echoes the key; the wrapper must scrub it. The canned
/// 401 body deliberately contains the key so this assertion is meaningful.
fn key_echo_error(key: &str) -> String {
    format!("{{\"error\":{{\"message\":\"invalid api key {key}\"}}}}")
}

fn assert_scrubbed(err: &str, key: &str) {
    assert!(!err.contains(key), "key leaked in error: {err}");
    assert!(
        err.contains("[redacted]"),
        "no scrub marker in error: {err}"
    );
}

#[tokio::test]
async fn openai_compatible_sends_bearer_to_chat_completions_and_scrubs_the_key() {
    const KEY: &str = "sk-openai-wire-secret";
    let server = WireServer::start("HTTP/1.1 401 Unauthorized", key_echo_error(KEY)).await;
    let url = format!("{}/v1", server.url());

    let err = run_with_openai_compatible(
        KEY,
        Some(&url),
        request(ProviderKind::OpenAiCompatible, "gpt-4o-mini"),
        "sys".to_string(),
        Vec::new(),
        test_deps(),
    )
    .await
    .expect_err("the canned 401 must surface as an error");
    assert_scrubbed(&err, KEY);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "exactly one HTTP request expected");
    let request = &requests[0];
    assert!(
        request
            .request_line
            .starts_with("POST /v1/chat/completions "),
        "unexpected request line: {}",
        request.request_line
    );
    assert_eq!(
        header(&request.headers, "authorization"),
        Some(&format!("Bearer {KEY}")[..])
    );
    assert!(
        !request.request_line.contains(KEY),
        "the key must never be in the request line: {}",
        request.request_line
    );
}

#[tokio::test]
async fn anthropic_sends_x_api_key_to_messages_and_scrubs_the_key() {
    const KEY: &str = "sk-ant-wire-secret";
    let server = WireServer::start("HTTP/1.1 401 Unauthorized", key_echo_error(KEY)).await;

    let err = run_with_anthropic(
        KEY,
        Some(&server.url()),
        request(ProviderKind::Anthropic, "claude-3-5-sonnet"),
        "sys".to_string(),
        Vec::new(),
        test_deps(),
    )
    .await
    .expect_err("the canned 401 must surface as an error");
    assert_scrubbed(&err, KEY);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "exactly one HTTP request expected");
    let request = &requests[0];
    assert!(
        request.request_line.starts_with("POST /v1/messages "),
        "unexpected request line: {}",
        request.request_line
    );
    assert_eq!(header(&request.headers, "x-api-key"), Some(KEY));
    assert!(
        header(&request.headers, "authorization").is_none(),
        "Anthropic uses x-api-key, not a Bearer header"
    );
}

#[tokio::test]
async fn gemini_sends_x_goog_api_key_to_interactions_and_scrubs_the_key() {
    const KEY: &str = "sk-gemini-wire-secret";
    let server = WireServer::start("HTTP/1.1 401 Unauthorized", key_echo_error(KEY)).await;

    let err = run_with_gemini(
        KEY,
        Some(&server.url()),
        request(ProviderKind::Gemini, "gemini-1.5-pro"),
        "sys".to_string(),
        Vec::new(),
        test_deps(),
    )
    .await
    .expect_err("the canned 401 must surface as an error");
    assert_scrubbed(&err, KEY);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "exactly one HTTP request expected");
    let request = &requests[0];
    // The Interactions client posts here, not to :generateContent.
    assert!(
        request
            .request_line
            .starts_with("POST /v1beta/interactions "),
        "unexpected request line: {}",
        request.request_line
    );
    assert_eq!(header(&request.headers, "x-goog-api-key"), Some(KEY));
    // The key must never appear in the URL / request line, under `key=` or at
    // all. GenerateContent would have put it in a `?key=` query parameter.
    assert!(
        !request.request_line.contains("key="),
        "no key query parameter allowed: {}",
        request.request_line
    );
    assert!(
        !request.request_line.contains(KEY),
        "no key material in the request line: {}",
        request.request_line
    );
    assert!(!server.url().contains(KEY));
}
