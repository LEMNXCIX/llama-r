//! Wire-level validation of the router: a **real HTTP server** answering
//! `POST /v1/systemone`, reached by the **real `LlamaCppRouter`** over a real
//! socket.
//!
//! Every other test in this branch injects a `FakeRouter` behind the
//! `DecisionRouter` trait, so none of them sends a byte over a socket,
//! serialises a real request body, or parses a real response body. That gap is
//! the one the design doc calls out as its open risk: `llama-server`'s response
//! envelope was never observed live, so the adapter's structs were fitted to
//! the hosted API's documented shape (`src/adapters/router/fixtures/README.md`).
//!
//! These tests cannot close that risk — no `llama-server` is required or used —
//! but they pin everything around it. They answer questions a fake cannot:
//!
//! - does the adapter produce the documented request body on the wire?
//! - does it parse a documented-shape response?
//! - does a **non**-2xx status degrade instead of erroring the request?
//! - does a malformed body degrade rather than crash?
//! - does an unreachable server degrade rather than hang?
//! - does `ROUTER_URL`'s trailing slash not produce `//v1/systemone`?
//!
//! The fake here is the *model*, never the transport: the answer comes from
//! over the wire, and the assertions read what actually crossed the socket.

use async_trait::async_trait;
use llama_r::adapters::router::LlamaCppRouter;
use llama_r::api::chat_core::{execute_chat, execute_chat_stream, AgentSelection};
use llama_r::api::handlers::AppState;
use llama_r::context::store::ContextStore;
use llama_r::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::ports::router::DecisionRouter;
use llama_r::providers::LLMProvider;
use llama_r::runtime::build_app_state;
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::skill_manager::SkillManager;
use std::collections::VecDeque;
use std::error::Error;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tokio::net::TcpListener;
use tokio_stream::{Stream, StreamExt};

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

const ROUTER_ENV_VARS: &[&str] = &[
    "ROUTER_ENABLED",
    "ROUTER_URL",
    "ROUTER_MODEL",
    "ROUTER_TIMEOUT_MS",
    "MIN_CONFIDENCE",
    "MIN_SPECIALIZED",
];

struct RouterEnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl RouterEnvGuard {
    fn cleared() -> Self {
        let saved = ROUTER_ENV_VARS
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect();
        for name in ROUTER_ENV_VARS {
            std::env::remove_var(name);
        }
        Self { saved }
    }
}

impl Drop for RouterEnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// `_env` before `_lock`: Rust drops fields in declaration order, so the
/// environment is restored while the lock is still held.
struct WireEnv {
    _env: RouterEnvGuard,
    _lock: MutexGuard<'static, ()>,
}

impl WireEnv {
    fn new() -> Self {
        let lock = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self {
            _env: RouterEnvGuard::cleared(),
            _lock: lock,
        }
    }
}

/// A stand-in `llama-server`: a real TCP listener on `127.0.0.1:0` that
/// answers `POST /v1/systemone` with `reply`, and records the request bodies it
/// received so a test can assert on the **wire format** rather than on a
/// struct the adapter built in memory.
///
/// This is the only place in the branch that can observe the request body the
/// adapter actually sends.
struct FakeDecisionServer {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
    paths: Arc<Mutex<Vec<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl FakeDecisionServer {
    /// `reply` is the JSON body answered to every decision request.
    async fn start(reply: serde_json::Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let path_sink = paths.clone();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let sink = sink.clone();
                let path_sink = path_sink.clone();
                let reply = reply.clone();
                tokio::spawn(async move {
                    // Minimal HTTP/1.1: read headers, then the body announced by
                    // Content-Length. Enough for one request per connection,
                    // which is what reqwest does for a single JSON POST.
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    loop {
                        let read = match socket.read(&mut tmp).await {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => return,
                        };
                        buf.extend_from_slice(&tmp[..read]);
                        if let Some(header_end) = find_header_end(&buf) {
                            let headers = String::from_utf8_lossy(&buf[..header_end]);

                            // Only the documented path is served. A doubled
                            // slash (`//v1/systemone`) is what a broken
                            // trailing-slash trim would produce, and a server
                            // that answers *every* path would hide exactly the
                            // regression `a_trailing_slash_..._reaches_the_endpoint`
                            // exists to catch. Refusing here is what makes that
                            // test able to fail.
                            let path = headers
                                .lines()
                                .next()
                                .and_then(|line| line.split_whitespace().nth(1))
                                .unwrap_or_default()
                                .to_string();
                            path_sink.lock().unwrap().push(path.clone());
                            if path != "/v1/systemone" {
                                let body = b"wrong path";
                                let response = format!(
                                    "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\n\
                                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                                    body.len()
                                );
                                let _ = socket.write_all(response.as_bytes()).await;
                                let _ = socket.write_all(body).await;
                                let _ = socket.flush().await;
                                return;
                            }
                            let content_length = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.trim()
                                        .eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().ok())?
                                })
                                .unwrap_or(0);
                            if buf.len() >= header_end + 4 + content_length {
                                let body = &buf[header_end + 4..header_end + 4 + content_length];
                                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body)
                                {
                                    sink.lock().unwrap().push(value);
                                }
                                break;
                            }
                        }
                    }

                    let payload = serde_json::to_vec(&reply).unwrap();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&payload).await;
                    let _ = socket.flush().await;
                });
            }
        });

        Self {
            addr,
            seen,
            paths,
            handle,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<serde_json::Value> {
        self.seen.lock().unwrap().clone()
    }

    /// Every request line the server saw, so a test can assert on the path
    /// rather than infer it from whether an answer came back.
    fn paths(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }
}

impl Drop for FakeDecisionServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A documented-shape d1 answer: a `Choice` picking `winner`, and a `Noul`.
fn choice_reply(winner: &str, confidence: f64, specialised: f64) -> serde_json::Value {
    serde_json::json!({
        "model": "d1",
        "answers": {
            "agent": {
                "type": "choice",
                "choice": winner,
                "confidence": confidence,
                "probabilities": { winner: confidence }
            },
            "specialized": { "type": "noul", "noul": specialised }
        },
        "usage": { "input_tokens": 40, "output_tokens": 0 }
    })
}

/// Records the model it was asked for, so the *dispatch target* is observable.
struct RecordingProvider {
    seen_model: Mutex<Option<String>>,
    reply: String,
}

impl RecordingProvider {
    fn new(reply: &str) -> Self {
        Self {
            seen_model: Mutex::new(None),
            reply: reply.to_string(),
        }
    }

    fn seen_model(&self) -> Option<String> {
        self.seen_model.lock().unwrap().clone()
    }
}

#[async_trait]
impl LLMProvider for RecordingProvider {
    fn get_base_url(&self) -> String {
        "http://recording".to_string()
    }
    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
    async fn chat(
        &self,
        request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        let model = request.model;
        *self.seen_model.lock().unwrap() = Some(model.clone());
        Ok(ChatResponse {
            model,
            created_at: "now".to_string(),
            message: ChatMessage {
                role: "assistant".to_string(),
                content: self.reply.clone(),
            },
            done: true,
            debug_prompt: None,
            debug_route: None,
        })
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        let model = request.model;
        *self.seen_model.lock().unwrap() = Some(model.clone());
        let text = self.reply.clone();
        Ok(Box::pin(tokio_stream::iter(vec![Ok(ChatStreamEvent {
            model: model.clone(),
            created_at: "now".to_string(),
            message: ChatMessage {
                role: "assistant".to_string(),
                content: text,
            },
            done: true,
        })])))
    }
}

/// The fixture's state plus the provider handle, so a test can read back the
/// model the dispatch asked for.
struct Fixture {
    _dir: tempfile::TempDir,
    state: Arc<AppState>,
    provider: Arc<RecordingProvider>,
}

/// Builds an `AppState` over a temp base dir holding project `fudi`'s general
/// agent and an `ops` specialist, each with a **distinct `model`** so the
/// dispatch target is readable back through what the provider was asked for.
///
/// The router is the real `LlamaCppRouter` pointed at `url` — no fake behind
/// the trait anywhere in this file.
fn state_pointed_at(url: &str, min_confidence: &str, min_specialised: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());

    let agents_dir = dir
        .path()
        .join("contextos")
        .join("projects")
        .join("fudi")
        .join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("fudi.toml"),
        "name = \"Fudi\"\nsystem_prompt = \"base\"\nmodel = \"model-general\"\n\
         description = \"Asistente general de Fudi.\"\n",
    )
    .unwrap();
    std::fs::write(
        agents_dir.join("ops.toml"),
        "name = \"Fudi Ops\"\nsystem_prompt = \"base\"\nmodel = \"model-specialist\"\n\
         description = \"Cancela pedidos y gestiona el menu de Fudi.\"\n",
    )
    .unwrap();

    // Router configuration goes through the environment, read once inside
    // `build_app_state` — so it must be set before the state is built.
    std::env::set_var("ROUTER_ENABLED", "true");
    std::env::set_var("ROUTER_URL", url);
    std::env::set_var("MIN_CONFIDENCE", min_confidence);
    std::env::set_var("MIN_SPECIALIZED", min_specialised);

    let registry = AgentRegistry::new();
    registry.reload_all(&[]).unwrap();
    let provider = Arc::new(RecordingProvider::new("respuesta"));

    let state = build_app_state(
        provider.clone() as Arc<dyn LLMProvider + Send + Sync>,
        Arc::new(registry),
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        "fallback-model".to_string(),
        Arc::new(Mutex::new(VecDeque::new())),
        Vec::new(),
        Arc::new(llama_r::adapters::mcp::StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
        None,
        None,
        None,
        None,
    );

    // The provider handle is returned so a test can read back the model the
    // dispatch asked for — the only surface on which the resolved agent is
    // observable, since a routed response does not name its handler.
    Fixture {
        _dir: dir,
        state,
        provider,
    }
}

fn request(content: &str) -> ChatRequest {
    ChatRequest {
        model: "fallback-model".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: content.to_string(),
        }],
        stream: false,
    }
}

fn project_only() -> AgentSelection<'static> {
    AgentSelection {
        project_id: Some("fudi"),
        agent_id: None,
        conversation_id: None,
        debug: false,
    }
}

/// The same request with `X-Debug: true`, so the response carries
/// `debug_route` — the only place the routing *reason* is observable.
fn debug_project_only() -> AgentSelection<'static> {
    AgentSelection {
        project_id: Some("fudi"),
        agent_id: None,
        conversation_id: None,
        debug: true,
    }
}

/// The adapter sends the documented request shape: `state` plus a `questions`
/// map, a `choice` carrying `type`/`instructions`/`criteria`, and a `noul`
/// carrying no `criteria`.
#[tokio::test]
async fn the_adapter_sends_the_documented_request_body() {
    let _env = WireEnv::new();
    let server = FakeDecisionServer::start(choice_reply("fudi::ops", 0.9, 0.9)).await;
    let f = state_pointed_at(&server.url(), "0.60", "0.50");

    execute_chat(&f.state, request("cancela el pedido 42"), project_only())
        .await
        .unwrap();

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        1,
        "exactly one decision per request: {requests:?}"
    );

    let body = &requests[0];
    assert!(
        body["state"]
            .as_str()
            .unwrap()
            .contains("cancela el pedido 42"),
        "the user's message must reach the decision model: {body}"
    );

    let questions = body["questions"]
        .as_object()
        .expect("questions is an object");
    assert_eq!(
        questions.len(),
        2,
        "one forward pass, both questions: {body}"
    );

    let choice = &questions["agent"];
    assert_eq!(choice["type"], "choice");
    assert!(choice["instructions"].is_string());
    // A Choice's criteria are a map keyed by the registry's routing keys, and
    // the project's own general agent must be among them — every degrading
    // rung lands there.
    let criteria = choice["criteria"].as_object().expect("criteria is a map");
    assert!(
        criteria.contains_key("fudi::ops"),
        "the specialist is a candidate: {criteria:?}"
    );
    assert!(
        criteria.contains_key("fudi::fudi"),
        "the general agent is a candidate too: {criteria:?}"
    );
    assert!(
        criteria.get("fudi::ops").unwrap().is_string(),
        "each criterion is a description: {criteria:?}"
    );

    let noul = &questions["specialized"];
    assert_eq!(noul["type"], "noul");
    assert!(
        noul.get("criteria").is_none(),
        "a Noul carries no criteria: {noul}"
    );

    // The derived text must carry real capability signal, not just the id.
    let ops = criteria["fudi::ops"].as_str().unwrap();
    assert!(
        ops.contains("Cancela pedidos"),
        "the derived criteria text carries the description: {ops}"
    );
}

/// A documented-shape answer on the wire reaches the specialist: the provider
/// is asked for that agent's model, not the general agent's.
#[tokio::test]
async fn a_wire_decision_reaches_the_specialist_agent() {
    let _env = WireEnv::new();
    let server = FakeDecisionServer::start(choice_reply("fudi::ops", 0.9, 0.9)).await;
    let f = state_pointed_at(&server.url(), "0.60", "0.50");

    let response = execute_chat(&f.state, request("cancela el pedido 42"), project_only())
        .await
        .unwrap();
    assert_eq!(response.message.content, "respuesta");

    assert_eq!(
        f.provider.seen_model().as_deref(),
        Some("model-specialist"),
        "the routed specialist's model is what the provider was asked for"
    );
}

/// A low `noul` lands on the project's general agent, over the wire, with the
/// same `400`-free response the router produces in unit tests.
#[tokio::test]
async fn a_weak_specialisation_score_degrades_to_the_general_agent() {
    let _env = WireEnv::new();
    let server = FakeDecisionServer::start(choice_reply("fudi::ops", 0.95, 0.10)).await;
    let f = state_pointed_at(&server.url(), "0.60", "0.50");

    execute_chat(&f.state, request("hola"), project_only())
        .await
        .unwrap();

    assert_eq!(
        f.provider.seen_model().as_deref(),
        Some("model-general"),
        "a specialist the router itself judged unnecessary must not win"
    );
}

/// A low confidence lands on the general agent even with a strong `noul`.
#[tokio::test]
async fn a_weak_confidence_degrades_to_the_general_agent() {
    let _env = WireEnv::new();
    let server = FakeDecisionServer::start(choice_reply("fudi::ops", 0.20, 0.95)).await;
    let f = state_pointed_at(&server.url(), "0.60", "0.50");

    execute_chat(&f.state, request("hola"), project_only())
        .await
        .unwrap();

    assert_eq!(f.provider.seen_model().as_deref(), Some("model-general"));
}

/// The threshold wiring is observable over the wire: the same wire answer that
/// degrades at `0.60` reaches the specialist at `0.10`. Without this, swapping
/// `min_confidence` and `min_specialised` at the call site would be invisible
/// to the whole suite.
#[tokio::test]
async fn the_confidence_threshold_is_read_from_configuration_over_the_wire() {
    let _env = WireEnv::new();
    // Confidence 0.20, specialisation 0.95: passes MIN_SPECIALIZED, fails a
    // 0.60 MIN_CONFIDENCE.
    let server = FakeDecisionServer::start(choice_reply("fudi::ops", 0.20, 0.95)).await;

    {
        let f = state_pointed_at(&server.url(), "0.60", "0.50");
        execute_chat(&f.state, request("hola"), project_only())
            .await
            .unwrap();
        assert_eq!(
            f.provider.seen_model().as_deref(),
            Some("model-general"),
            "0.20 confidence is under a 0.60 threshold"
        );
    }

    {
        let f = state_pointed_at(&server.url(), "0.10", "0.50");
        execute_chat(&f.state, request("hola"), project_only())
            .await
            .unwrap();
        assert_eq!(
            f.provider.seen_model().as_deref(),
            Some("model-specialist"),
            "0.20 confidence clears a 0.10 threshold — so MIN_CONFIDENCE, not \
             MIN_SPECIALIZED, was the gate that refused it a moment ago"
        );
    }
}

/// A key the decision model invents, which was never a candidate, is refused —
/// and the request still succeeds on the general agent.
///
/// The `reason` is asserted as well as the destination, and that distinction is
/// the whole point. "Landed on the general agent" alone cannot tell *refusing a
/// hallucinated key* from *coercing it to some other agent* — and coercion is the
/// failure this guards. The general agent sorts first among the candidates, so a
/// mutation that substituted `candidates[0]` for an unknown key would dispatch
/// the general agent too, and a destination-only assertion would stay green.
#[tokio::test]
async fn a_hallucinated_key_is_refused_over_the_wire() {
    let _env = WireEnv::new();
    let server = FakeDecisionServer::start(choice_reply("fudi::inventado", 0.99, 0.99)).await;
    let f = state_pointed_at(&server.url(), "0.60", "0.50");

    let response = execute_chat(&f.state, request("hola"), debug_project_only())
        .await
        .expect("a hallucinated key must not fail the request");
    assert_eq!(response.message.content, "respuesta");

    let debug = response
        .debug_route
        .expect("X-Debug was sent, so the routing decision must be reported");
    assert_eq!(
        debug.reason.as_deref(),
        Some("unknown_key"),
        "the key was discarded as untrusted input, not coerced to another agent: \
         {debug:?}"
    );
    assert_eq!(debug.winner.as_deref(), Some("fudi::inventado"));
    assert!(
        !debug
            .candidates
            .iter()
            .any(|candidate| candidate == "fudi::inventado"),
        "and it was never a candidate to begin with: {debug:?}"
    );

    assert_eq!(
        f.provider.seen_model().as_deref(),
        Some("model-general"),
        "and the request still succeeded on the project's general agent"
    );
}

/// A non-2xx from the decision server degrades to the general agent instead of
/// failing the chat request — and does not act on the body, which deliberately
/// names the specialist with maximal scores.
#[tokio::test]
async fn a_non_2xx_from_the_decision_server_degrades_rather_than_failing() {
    let _env = WireEnv::new();
    let server = RejectingServer::start(500).await;
    let f = state_pointed_at(&server.url(), "0.60", "0.50");

    let response = execute_chat(&f.state, request("hola"), debug_project_only())
        .await
        .expect("an upstream refusal must not become the user's error");
    assert_eq!(response.message.content, "respuesta");
    assert_eq!(
        f.provider.seen_model().as_deref(),
        Some("model-general"),
        "a refused response must not be acted on, even when its body names a \
         specialist with maximal scores — only the status check stands between \
         the two"
    );
    assert_eq!(
        response
            .debug_route
            .expect("debug was requested")
            .reason
            .as_deref(),
        Some("router_unavailable")
    );
}

/// A malformed body degrades rather than panicking or erroring the request.
#[tokio::test]
async fn a_malformed_body_degrades_rather_than_failing() {
    let _env = WireEnv::new();
    for (label, reply) in [
        ("not json at all", serde_json::json!({"totally": "wrong"})),
        ("no answers object", serde_json::json!({"usage": {}})),
        (
            "answers is not an object",
            serde_json::json!({"answers": "nope"}),
        ),
        (
            "answer is not an object",
            serde_json::json!({"answers": {"agent": 7}}),
        ),
        (
            "unknown answer kind",
            serde_json::json!({"answers": {"agent": {"type": "oracle"}}}),
        ),
    ] {
        let server = FakeDecisionServer::start(reply).await;
        let f = state_pointed_at(&server.url(), "0.60", "0.50");

        let response = execute_chat(&f.state, request("hola"), project_only())
            .await
            .unwrap_or_else(|e| panic!("{label}: a malformed decision must degrade, got {e}"));
        assert_eq!(response.message.content, "respuesta", "{label}");
        assert_eq!(
            f.provider.seen_model().as_deref(),
            Some("model-general"),
            "{label}"
        );
    }
}

/// Nothing listening at the configured origin: connect fails, the request still
/// succeeds on the general agent, and it does so inside the timeout rather than
/// hanging.
#[tokio::test]
async fn an_unreachable_router_degrades_within_the_timeout() {
    let _env = WireEnv::new();
    // Bind and drop, so the port is almost certainly free.
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = dead.local_addr().unwrap();
    drop(dead);

    let started = std::time::Instant::now();
    // 1500 ms budget: comfortably under the test timeout, and a degrade that
    // waited on the OS's own connect timeout would blow past it.
    let f = state_pointed_at(&format!("http://{addr}"), "0.60", "0.50");

    let response = execute_chat(&f.state, request("hola"), project_only())
        .await
        .expect("an unreachable router must not fail the chat request");
    assert_eq!(response.message.content, "respuesta");
    assert_eq!(f.provider.seen_model().as_deref(), Some("model-general"));
    assert!(
        started.elapsed() < std::time::Duration::from_millis(1500),
        "the degrade must respect ROUTER_TIMEOUT_MS, took {:?}",
        started.elapsed()
    );
}

/// A configured URL with a trailing slash must not produce `//v1/systemone` —
/// which would 404 on a real server and degrade every request.
#[tokio::test]
async fn a_trailing_slash_in_the_configured_url_still_reaches_the_endpoint() {
    let _env = WireEnv::new();
    let server = FakeDecisionServer::start(choice_reply("fudi::ops", 0.9, 0.9)).await;
    let f = state_pointed_at(&format!("{}/", server.url()), "0.60", "0.50");

    execute_chat(&f.state, request("cancela el pedido 42"), project_only())
        .await
        .unwrap();

    assert_eq!(
        server.paths(),
        vec!["/v1/systemone".to_string()],
        "exactly one decision request, on the documented path — a trailing slash \
         in ROUTER_URL must not produce //v1/systemone"
    );
    assert_eq!(
        server.requests().len(),
        1,
        "and it carried a body, so the request completed rather than 404ing"
    );
    assert_eq!(
        f.provider.seen_model().as_deref(),
        Some("model-specialist"),
        "a doubled path would have 404ed, degraded to the general agent, and \
         failed this assertion"
    );
}

/// The streaming surface routes exactly like the plain one. It carries no
/// `debug_route` by design, so the dispatch target is read back through the
/// model the provider was asked for.
#[tokio::test]
async fn the_streaming_path_routes_over_the_wire_too() {
    let _env = WireEnv::new();
    let server = FakeDecisionServer::start(choice_reply("fudi::ops", 0.9, 0.9)).await;
    let f = state_pointed_at(&server.url(), "0.60", "0.50");

    let mut payload = request("cancela el pedido 42");
    payload.stream = true;
    let stream = execute_chat_stream(&f.state, payload, project_only())
        .await
        .unwrap();

    let events: Vec<_> = stream.collect().await;
    assert!(
        events.iter().all(|e| e.is_ok()),
        "the stream must not carry an error: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.as_ref().unwrap().message.content == "respuesta"),
        "the streamed content must arrive: {events:?}"
    );

    assert_eq!(
        f.provider.seen_model().as_deref(),
        Some("model-specialist"),
        "SSE routes on the same wire decision as the plain path"
    );
}

/// With the router disabled, an installed decision server is never contacted and
/// the request keeps resolving to the project's own agent.
#[tokio::test]
async fn a_disabled_router_is_never_contacted_over_the_wire() {
    let _env = WireEnv::new();
    let server = FakeDecisionServer::start(choice_reply("fudi::ops", 0.99, 0.99)).await;

    let f = state_pointed_at(&server.url(), "0.60", "0.50");
    // Prove the fixture *would* have routed: a decision naming the specialist
    // with the strongest possible scores.
    execute_chat(&f.state, request("cancela el pedido 42"), project_only())
        .await
        .unwrap();
    assert_eq!(
        f.provider.seen_model().as_deref(),
        Some("model-specialist"),
        "the fixture is wired to route; the disabled case below is what this test is about"
    );

    // Now the hazard this test exists for: a state whose router **is installed**
    // but whose config says routing is off.
    //
    // It cannot be built by flipping the env var: `build_app_state` reads the
    // config once and only installs the router when `enabled`, so a state built
    // with `ROUTER_ENABLED=false` has `router: None` and is held inert by a
    // *second* gate. That is why the existing unit test
    // `an_installed_router_is_never_consulted_while_routing_is_disabled` passes
    // for the wrong reason, and why a mutation that deletes the `enabled` gate
    // leaves the suite green. So the router is installed by hand instead, which
    // is exactly the state a future refactor of `build_app_state` could produce.
    std::env::set_var("ROUTER_ENABLED", "false");
    let state = rebuild_with_config(&f);
    *state.router.write().unwrap() =
        Some(Arc::new(LlamaCppRouter::new(server.url(), "d1-3b", 3000)));
    assert!(
        !state.router_config.enabled,
        "the config must say routing is off for this test to mean anything"
    );
    let requests_before = server.requests().len();

    execute_chat(&state, request("cancela el pedido 42"), project_only())
        .await
        .unwrap();

    assert_eq!(
        server.requests().len(),
        requests_before,
        "a disabled router must not send a single byte: {:?}",
        server.requests()
    );
    assert_eq!(
        f.provider.seen_model().as_deref(),
        Some("model-general"),
        "and the request keeps today's resolution"
    );
}

/// A confidence the adapter's `f64 -> f32` narrowing can actually produce from a
/// hostile body must not read as maximum trust.
///
/// `"confidence": 1e300` is not hypothetical here: the adapter narrows `f64` to
/// `f32`, and `f32::INFINITY < min_confidence` is **false**, so an unchecked
/// value would sail past the confidence gate on a request whose model is not
/// even loaded. The ladder rejects it as a malformed answer, which is what
/// makes this observable over the wire rather than only in unit tests.
#[tokio::test]
async fn a_confidence_the_adapter_narrows_to_infinity_is_not_trusted() {
    let _env = WireEnv::new();
    // Also an absurd `probabilities` entry, which survives narrowing the same
    // way, so both routes to "impossible number" are present.
    let server = FakeDecisionServer::start(serde_json::json!({
        "answers": {
            "agent": {
                "type": "choice",
                "choice": "fudi::ops",
                "confidence": 1e300,
                "probabilities": { "fudi::ops": 1e300 }
            },
            "specialized": { "type": "noul", "noul": 0.99 }
        }
    }))
    .await;
    let f = state_pointed_at(&server.url(), "0.60", "0.50");

    let response = execute_chat(&f.state, request("hola"), debug_project_only())
        .await
        .expect("an impossible number must degrade, not fail the request");
    assert_eq!(response.message.content, "respuesta");

    assert_eq!(
        f.provider.seen_model().as_deref(),
        Some("model-general"),
        "an infinite confidence must NOT be read as maximum trust and dispatch a \
         specialist on a value no decision model can emit"
    );
    let debug = response.debug_route.expect("debug was requested");
    assert_eq!(
        debug.reason.as_deref(),
        Some("router_unavailable"),
        "it is treated as a malformed answer, which is what the adapter's own \
         `Provider` error means upstream: {debug:?}"
    );
}

/// The adapter itself, exercised directly, so a regression in the wire format is
/// attributable to the adapter rather than to the dispatch around it.
#[tokio::test]
async fn the_adapter_alone_parses_a_documented_shape_response() {
    let server = FakeDecisionServer::start(choice_reply("fudi::ops", 0.77, 0.66)).await;
    let router = LlamaCppRouter::new(server.url(), "d1-3b", 3000);

    let request = llama_r::ports::router::DecisionRequest {
        state: "cancela el pedido 42".to_string(),
        questions: std::collections::BTreeMap::from([(
            "agent".to_string(),
            llama_r::ports::router::Question::Choice {
                instructions: "Which agent?".to_string(),
                criteria: std::collections::BTreeMap::from([
                    ("fudi::ops".to_string(), "Pedidos".to_string()),
                    ("fudi::fudi".to_string(), "General".to_string()),
                ]),
            },
        )]),
    };

    let response = router
        .decide(request)
        .await
        .expect("a documented shape parses");
    let answer = &response.answers["agent"];
    match answer {
        llama_r::ports::router::Answer::Choice {
            key,
            confidence,
            probabilities,
        } => {
            assert_eq!(key, "fudi::ops");
            assert!((confidence - 0.77).abs() < 1e-6);
            assert!(probabilities.contains_key("fudi::ops"));
        }
        other => panic!("expected a Choice, got {other:?}"),
    }
}

/// Rebuilds a state over the same manifests and provider with the router
/// config re-read, so a test can get a state whose router is installed but whose
/// `enabled` flag is whatever the environment says at rebuild time.
///
/// The registry is rebuilt too, because `build_app_state` takes it by value and
/// the original has already been moved into the first state.
fn rebuild_with_config(fixture: &Fixture) -> Arc<AppState> {
    let registry = AgentRegistry::new();
    registry.reload_all(&[]).unwrap();
    build_app_state(
        fixture.provider.clone() as Arc<dyn LLMProvider + Send + Sync>,
        Arc::new(registry),
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        "fallback-model".to_string(),
        Arc::new(Mutex::new(VecDeque::new())),
        Vec::new(),
        Arc::new(llama_r::adapters::mcp::StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
        None,
        None,
        None,
        None,
    )
}

/// A server that answers a fixed non-2xx status, so the adapter's
/// `!response.status().is_success()` arm is reachable over a real socket.
struct RejectingServer {
    addr: SocketAddr,
    handle: tokio::task::JoinHandle<()>,
}

impl RejectingServer {
    /// Answers `status` with a body that **parses as a valid decision naming the
    /// specialist** — deliberately. A refusal carrying junk would degrade anyway,
    /// because the JSON parse fails, so the test would pass with the status check
    /// removed and prove nothing. This shape means the *only* thing standing
    /// between a refused response and a dispatched specialist is the adapter's
    /// `!response.status().is_success()` arm.
    async fn start(status: u16) -> Self {
        let payload = serde_json::to_vec(&choice_reply("fudi::ops", 0.99, 0.99)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let response = format!(
                    "HTTP/1.1 {status} Refused\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(&payload).await;
                let _ = socket.flush().await;
            }
        });
        Self { addr, handle }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for RejectingServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
