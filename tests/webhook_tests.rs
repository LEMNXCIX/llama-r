//! Fase 8 Task 1: channel config, the signature-verifier port and its HMAC
//! adapter.
//!
//! The verifier is pure: it compares a signature over the RAW request body with
//! the value in a header, in constant time, and never trusts a string equality
//! on the hex form. Credentials live in `auth_env` as the NAME of an environment
//! variable — never the value — so a `ChannelConfig` has no secret field to
//! leak through `Debug`.

use llama_r::adapters::channel::{load_channels, HmacSha256Verifier};
use llama_r::domain::channel::{ChannelConfig, ChannelMode};
use llama_r::ports::channel::ChannelSignatureVerifier;
use llama_r::services::marker_template::{message_from_template, render_markers};
use serde_json::{json, Value};
use std::io::Write;
use std::sync::{Arc, Mutex};

// ── HMAC verifier ────────────────────────────────────────────────────────────

/// RFC 4231 test case 2: key = "Jefe", data = "what do ya want for nothing?".
/// Using a published vector proves the implementation, not a second copy of it.
const VECTOR_SECRET: &str = "Jefe";
const VECTOR_BODY: &[u8] = b"what do ya want for nothing?";
const VECTOR_HEX: &str = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";

#[test]
fn hmac_accepts_prefixed_and_bare_hex() {
    let verifier = HmacSha256Verifier;

    // `sha256=<hex>`, bare `<hex>` and uppercase hex all decode to the same
    // 32 bytes and must be accepted.
    assert!(verifier.verify(VECTOR_BODY, &format!("sha256={VECTOR_HEX}"), VECTOR_SECRET));
    assert!(verifier.verify(VECTOR_BODY, VECTOR_HEX, VECTOR_SECRET));
    assert!(verifier.verify(VECTOR_BODY, &VECTOR_HEX.to_ascii_uppercase(), VECTOR_SECRET));
}

#[test]
fn hmac_rejects_missing_malformed_and_wrong_signature() {
    let verifier = HmacSha256Verifier;

    // A one-byte body mutation with an otherwise valid header must fail: the
    // signature is over the RAW bytes, not a parsed/normalised representation.
    let mut mutated = VECTOR_BODY.to_vec();
    mutated[0] = b'X';
    assert!(
        !verifier.verify(&mutated, &format!("sha256={VECTOR_HEX}"), VECTOR_SECRET),
        "a one-byte body mutation must invalidate an otherwise valid header"
    );

    // Wrong digest, too-short hex, non-hex characters, empty header and empty
    // secret are all refusals, never a panic or an accidental match.
    let wrong = "0000000000000000000000000000000000000000000000000000000000000000";
    assert!(!verifier.verify(VECTOR_BODY, &format!("sha256={wrong}"), VECTOR_SECRET));
    assert!(!verifier.verify(VECTOR_BODY, "sha256=deadbeef", VECTOR_SECRET));
    assert!(!verifier.verify(
        VECTOR_BODY,
        "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        VECTOR_SECRET
    ));
    assert!(!verifier.verify(VECTOR_BODY, "", VECTOR_SECRET));
    assert!(!verifier.verify(VECTOR_BODY, VECTOR_HEX, ""));
}

// ── config loader ────────────────────────────────────────────────────────────

#[test]
fn load_channels_missing_file_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let channels = load_channels(tmp.path());
    assert!(
        channels.is_empty(),
        "a missing channels.toml is not an error"
    );
}

/// A shared buffer usable as a tracing writer, read back as text.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn malformed_channels_toml_is_warned_and_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("channels.toml");
    std::fs::write(&path, "this is = not [[[ valid toml").unwrap();

    let buf = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .without_time()
        .with_target(false)
        .finish();
    let channels = tracing::subscriber::with_default(subscriber, || load_channels(tmp.path()));

    assert!(
        channels.is_empty(),
        "malformed config must not produce channels"
    );
    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains(&path.display().to_string()),
        "the warning must name the full path: {log:?}"
    );
}

/// Sets an env var for the duration of a test and restores its prior value on
/// drop, so a sentinel never leaks into another test in this binary.
struct EnvGuard {
    name: &'static str,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(name: &'static str, value: &str) -> Self {
        let previous = std::env::var(name).ok();
        std::env::set_var(name, value);
        Self { name, previous }
    }

    /// Explicitly remove a variable for a test, restoring it on drop.
    fn unset(name: &'static str) -> Self {
        let previous = std::env::var(name).ok();
        std::env::remove_var(name);
        Self { name, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

#[test]
fn auth_env_is_a_name_not_a_value() {
    const AUTH_ENV: &str = "WEBHOOK_SECRET_TASK1_AUTH_ENV";
    const SENTINEL: &str = "super-secret-value-TASK1-SENTINEL";

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("channels.toml"),
        format!(
            r#"
[[channels]]
id = "stripe"
agent_id = "billing"
mode = "async"
signature_header = "X-Signature"
auth_env = "{AUTH_ENV}"
enabled = true
"#
        ),
    )
    .unwrap();

    // Actually populate the variable named by `auth_env` with a distinctive
    // sentinel before formatting, so the no-leak assertion below is real: if a
    // secret field were ever added and captured this value, it would fail.
    let _guard = EnvGuard::set(AUTH_ENV, SENTINEL);

    let channels = load_channels(tmp.path());
    assert_eq!(channels.len(), 1);
    let cfg: &ChannelConfig = &channels[0];
    assert_eq!(cfg.auth_env, AUTH_ENV);
    assert_eq!(cfg.mode, ChannelMode::Async);
    assert!(cfg.enabled);

    // The struct holds the variable NAME and has no secret field: the sentinel
    // value cannot appear in Debug even though the env var now holds it.
    let debug = format!("{cfg:?}");
    assert!(debug.contains(AUTH_ENV));
    assert!(
        !debug.contains(SENTINEL),
        "Debug leaked the secret value from the env var: {debug:?}"
    );
}

// ── marker-template rendering (Task 2) ───────────────────────────────────────

#[test]
fn marker_template_extracts_message() {
    let payload = json!({
        "action": "opened",
        "sender": { "login": "octocat" },
        "issue": { "title": "Hello" }
    });

    // Several markers embedded in prose; whitespace around the path inside the
    // braces is trimmed before the dot-path lookup.
    let out = render_markers(
        "New issue from {{ sender.login }}: {{issue.title}}",
        &payload,
    )
    .unwrap();
    assert_eq!(out, "New issue from octocat: Hello");

    // No markers means the template is returned verbatim — including literal
    // single braces that are not a `{{...}}` marker.
    let verbatim = render_markers("use {action} but not a marker", &payload).unwrap();
    assert_eq!(verbatim, "use {action} but not a marker");
}

#[test]
fn marker_template_handles_numbers_and_bools() {
    let payload = json!({
        "count": 42,
        "ratio": 1.5,
        "negative": -7,
        "merged": true,
        "draft": false
    });
    let out = render_markers(
        "{{count}} / {{ratio}} / {{negative}} / {{merged}} / {{draft}}",
        &payload,
    )
    .unwrap();
    assert_eq!(out, "42 / 1.5 / -7 / true / false");
}

#[test]
fn absent_template_uses_raw_body() {
    let raw = "{\"sender\":{\"login\":\"octocat\"}}";
    let payload: Value = serde_json::from_str(raw).unwrap();

    // None, empty and blank templates all fall back to the raw body.
    assert_eq!(message_from_template(None, &payload, raw).unwrap(), raw);
    assert_eq!(message_from_template(Some(""), &payload, raw).unwrap(), raw);
    assert_eq!(
        message_from_template(Some("   "), &payload, raw).unwrap(),
        raw
    );

    // A real template still renders rather than falling back to the raw body.
    assert_eq!(
        message_from_template(Some("{{sender.login}}"), &payload, raw).unwrap(),
        "octocat"
    );
}

#[test]
fn unknown_marker_fails_without_echoing_body() {
    const SENTINEL: &str = "TOP-SECRET-PAYLOAD-VALUE-DO-NOT-ECHO";
    let payload = json!({
        "sender": { "login": "octocat", "password": SENTINEL },
        "token": SENTINEL,
    });

    // The missing path resolves through a parent object that itself holds a
    // secret; a naive implementation that dumps the body on failure would leak
    // it here.
    let err = render_markers("{{sender.nonexistent}}", &payload).unwrap_err();
    assert!(
        err.contains("sender.nonexistent"),
        "the error must name the missing path: {err:?}"
    );
    assert!(
        !err.contains(SENTINEL),
        "the error echoed body content: {err:?}"
    );

    let err2 = render_markers("{{no.such.path}}", &payload).unwrap_err();
    assert!(
        err2.contains("no.such.path"),
        "an unknown root path must be named: {err2:?}"
    );
    assert!(
        !err2.contains(SENTINEL),
        "the error echoed body content: {err2:?}"
    );
}

#[test]
fn object_marker_is_a_named_error() {
    let payload = json!({
        "sender": { "login": "octocat" },
        "items": [1, 2, 3],
    });

    let err = render_markers("{{sender}}", &payload).unwrap_err();
    assert!(
        err.contains("sender"),
        "the error must name the path: {err:?}"
    );
    assert!(
        err.contains("object"),
        "the error must say the value is an object: {err:?}"
    );

    let err2 = render_markers("{{items}}", &payload).unwrap_err();
    assert!(
        err2.contains("items"),
        "the error must name the path: {err2:?}"
    );
    assert!(
        err2.contains("array"),
        "the error must say the value is an array: {err2:?}"
    );
}

// ── webhook endpoint (Task 3) ────────────────────────────────────────────────

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::{get, post},
    Router,
};
use llama_r::adapters::context::{CurrentModelSelector, NoopCompactor};
use llama_r::adapters::execution::SqliteExecutionStore;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::execution_api::get_execution;
use llama_r::api::handlers::AppState;
use llama_r::api::observability::AppObservability;
use llama_r::api::webhook_api::webhook;
use llama_r::config::ProviderKind;
use llama_r::context::analyzer::ContextEnricher;
use llama_r::context::store::ContextStore;
use llama_r::domain::agent::{Agent, AgentConfig};
use llama_r::domain::models::{ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use llama_r::optimizer::metrics::TokenMetrics;
use llama_r::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult};
use llama_r::providers::registry::ProviderRegistry;
use llama_r::providers::LLMProvider;
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::agent_runtime::AgentRuntime;
use llama_r::services::approval::ApprovalGate;
use llama_r::services::provider_stack::ProviderStack;
use llama_r::services::router_config::RouterConfig;
use llama_r::services::scope_builder::ScopeBuilder;
use llama_r::services::skill_manager::SkillManager;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::error::Error as StdError;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::{OnceLock, RwLock};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::RwLock as TokioRwLock;
use tower::ServiceExt;

/// Env vars are process-wide, so the webhook tests that point `LLAMA_R_DIR` at a
/// temp dir must not overlap. A tokio mutex (Send guard) serialises them.
static ENV_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

const SECRET_ENV: &str = "WEBHOOK_TASK3_SECRET";
const SECRET: &str = "task3-shared-secret-sentinel";
const PAYLOAD_SECRET: &str = "PAYLOAD-SECRET-SENTINEL-DO-NOT-ECHO";

/// RFC 2104 HMAC-SHA256, only so a test can produce a valid header. The
/// verifier itself has its own published-vector tests; this is a fixture.
fn sign_body(secret: &str, body: &[u8]) -> String {
    const B: usize = 64;
    let mut key_block = [0u8; B];
    if secret.len() > B {
        key_block[..32].copy_from_slice(&Sha256::digest(secret.as_bytes()));
    } else {
        key_block[..secret.len()].copy_from_slice(secret.as_bytes());
    }
    let mut inner_pad = [0x36u8; B];
    let mut outer_pad = [0x5cu8; B];
    for i in 0..B {
        inner_pad[i] ^= key_block[i];
        outer_pad[i] ^= key_block[i];
    }
    let mut inner = Vec::with_capacity(B + body.len());
    inner.extend_from_slice(&inner_pad);
    inner.extend_from_slice(body);
    let inner_hash = Sha256::digest(&inner);
    let mut outer = Vec::with_capacity(B + 32);
    outer.extend_from_slice(&outer_pad);
    outer.extend_from_slice(&inner_hash);
    Sha256::digest(&outer)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn signed(body: &str) -> String {
    format!("sha256={}", sign_body(SECRET, body.as_bytes()))
}

#[derive(Debug, Clone)]
struct RecordedCall {
    agent_id: String,
    project_id: Option<String>,
    user_message: String,
}

struct RecordingEngine {
    calls: Arc<Mutex<Vec<RecordedCall>>>,
    text: String,
}

#[async_trait]
impl AgentEngine for RecordingEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        self.calls.lock().unwrap().push(RecordedCall {
            agent_id: req.agent.id.clone(),
            project_id: req.agent.project_id.clone(),
            user_message: req.user_message.clone(),
        });
        Ok(AgentRunResult {
            text: self.text.clone(),
            tool_calls: 0,
            iterations: 1,
            model: req.model,
        })
    }

    async fn run_stream(
        &self,
        req: AgentRunRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<AgentRunEvent>, String> {
        let result = self.run(req).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let _ = tx.send(AgentRunEvent::Completed { result }).await;
        Ok(rx)
    }
}

/// A provider that is never reached on the engine path; it exists only so a
/// real `ProviderStack` can be built without a live upstream.
struct StubProvider;

#[async_trait]
impl LLMProvider for StubProvider {
    fn get_base_url(&self) -> String {
        "http://stub".into()
    }
    async fn health_check(&self) -> Result<(), Box<dyn StdError + Send + Sync>> {
        Ok(())
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn StdError + Send + Sync>> {
        Ok(Vec::new())
    }
    async fn chat(
        &self,
        _req: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn StdError + Send + Sync>> {
        Err("stub provider is not used on the engine path".into())
    }
    async fn chat_stream(
        &self,
        _req: ChatRequest,
    ) -> Result<
        Pin<
            Box<
                dyn tokio_stream::Stream<
                        Item = Result<ChatStreamEvent, Box<dyn StdError + Send + Sync>>,
                    > + Send,
            >,
        >,
        Box<dyn StdError + Send + Sync>,
    > {
        Err("stub provider is not used on the engine path".into())
    }
}

fn register_agent(registry: &AgentRegistry, id: &str, project: Option<&str>) {
    let agent = Agent {
        id: id.to_string(),
        project_id: project.map(str::to_string),
        config: AgentConfig {
            name: id.to_string(),
            model: "model-a".into(),
            system_prompt: format!("you are {id}"),
            ..Default::default()
        },
    };
    let scope = ScopeBuilder::build(&agent).expect("manifest builds a scope");
    registry.register(agent, scope);
}

fn test_enricher() -> Arc<ContextEnricher> {
    Arc::new(ContextEnricher::new(
        Arc::new(ContextStore::new()),
        Arc::new(SkillManager::new()),
        "model-a".into(),
        Arc::new(ProviderRegistry::default()),
        ProviderKind::Ollama,
    ))
}

struct World {
    _temp: tempfile::TempDir,
    _dir_guard: EnvGuard,
    _secret_guard: EnvGuard,
    app: Router,
    calls: Arc<Mutex<Vec<RecordedCall>>>,
}

/// A deliberately naive verifier that accepts an empty key: it models an HMAC
/// implementation without the shipped adapter's empty-secret guard. The
/// endpoint must fail closed on a missing secret on its own, not rely on the
/// adapter happening to refuse an empty key.
struct EmptyKeyAcceptingVerifier;

impl ChannelSignatureVerifier for EmptyKeyAcceptingVerifier {
    fn verify(&self, body: &[u8], header: &str, secret: &str) -> bool {
        format!("sha256={}", sign_body(secret, body)) == header
    }
}

/// A production-shaped world: real Router, real AppState, real HMAC verifier
/// companion, real execution store, fake engine.
async fn world(channels_toml: &str) -> World {
    world_with_verifier(channels_toml, Arc::new(HmacSha256Verifier)).await
}

/// Same world, with a caller-supplied verifier installed on `AppState` before
/// the Router takes its clone. Used to prove the endpoint's own fail-closed
/// check independent of the adapter.
async fn world_with_verifier(
    channels_toml: &str,
    verifier: Arc<dyn ChannelSignatureVerifier>,
) -> World {
    let temp = tempfile::tempdir().unwrap();
    let dir_guard = EnvGuard::set("LLAMA_R_DIR", temp.path().to_str().unwrap());
    let secret_guard = EnvGuard::set(SECRET_ENV, SECRET);
    std::fs::write(temp.path().join("channels.toml"), channels_toml).unwrap();

    let calls = Arc::new(Mutex::new(Vec::new()));
    let engine = Arc::new(RecordingEngine {
        calls: calls.clone(),
        text: "engine answer".into(),
    });

    let registry = Arc::new(AgentRegistry::new());
    register_agent(&registry, "billing", Some("proj"));
    register_agent(&registry, "admin", Some("other"));

    let enricher = test_enricher();
    let mut stack = ProviderStack::new(Arc::new(StubProvider), "model-a");
    stack.provider_name = "ollama".into();
    stack.agent_runtime = Some(Arc::new(AgentRuntime {
        registry: registry.clone(),
        engine,
        history: None,
        rag: None,
        enricher: enricher.clone(),
        compactor: Arc::new(NoopCompactor),
        selector: Arc::new(CurrentModelSelector),
    }));

    // Built directly (rather than via the boot helper) because the boot helper
    // installs a Weak delegator, which makes Arc::get_mut unavailable for
    // swapping in the test verifier.
    let state = Arc::new(AppState {
        services: Arc::new(TokioRwLock::new(Arc::new(stack))),
        agent_registry: registry,
        skill_manager: Arc::new(SkillManager::new()),
        context_store: Arc::new(ContextStore::new()),
        context_enricher: Mutex::new(enricher),
        metrics: Arc::new(TokenMetrics::new()),
        observability: Arc::new(AppObservability::new()),
        api_running: AtomicBool::new(false),
        grpc_running: AtomicBool::new(false),
        logs: Arc::new(Mutex::new(VecDeque::new())),
        known_mcp_servers: RwLock::new(Vec::new()),
        mcp_registry: Arc::new(StaticMcpRegistry::new()),
        history_store: None,
        router_config: Arc::new(RouterConfig::default()),
        router_healthy: AtomicBool::new(false),
        approval_gate: Arc::new(ApprovalGate::default()),
        auto_decider: None,
        compactor: Arc::new(NoopCompactor),
        selector: Arc::new(CurrentModelSelector),
        execution_store: Arc::new(
            SqliteExecutionStore::new(temp.path().join("exec.db")).expect("execution store opens"),
        ),
        delegator: OnceLock::new(),
        channel_verifier: verifier,
    });
    let app = Router::new()
        .route("/api/webhooks/:channel", post(webhook))
        .route("/api/executions/:id", get(get_execution))
        .with_state(state.clone());

    World {
        _temp: temp,
        _dir_guard: dir_guard,
        _secret_guard: secret_guard,
        app,
        calls,
    }
}

async fn send(
    app: &Router,
    channel: &str,
    body: &[u8],
    signature: Option<&str>,
    extra: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/api/webhooks/{channel}"))
        .header("content-type", "application/json");
    if let Some(sig) = signature {
        builder = builder.header("x-signature", sig);
    }
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(body.to_vec())).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

const CHANNEL_SYNC: &str = r#"
[[channels]]
id = "github"
agent_id = "billing"
project_id = "proj"
mode = "sync"
signature_header = "X-Signature"
auth_env = "WEBHOOK_TASK3_SECRET"
template = "{{action}} by {{sender.login}}"
enabled = true
"#;

#[tokio::test]
async fn valid_signature_reaches_agent() {
    let _lock = ENV_LOCK.lock().await;
    let w = world(CHANNEL_SYNC).await;
    let body = r#"{"action":"opened","sender":{"login":"octocat"}}"#;

    let (status, value) = send(&w.app, "github", body.as_bytes(), Some(&signed(body)), &[]).await;
    assert_eq!(status, StatusCode::OK, "{value:?}");
    // The response carries the agent's answer; the rendered message is what
    // reached the agent, asserted on the recorded call below.
    assert_eq!(value["message"]["content"], "engine answer");

    let calls = w.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "exactly one agent turn");
    assert_eq!(calls[0].agent_id, "billing");
    assert_eq!(calls[0].project_id.as_deref(), Some("proj"));
    assert_eq!(calls[0].user_message, "opened by octocat");
}

#[tokio::test]
async fn invalid_or_missing_signature_is_401_before_parsing() {
    let _lock = ENV_LOCK.lock().await;
    let w = world(CHANNEL_SYNC).await;
    let broken = "this is not json {{{";

    // A bad signature over a malformed body is 401 — verification runs BEFORE
    // JSON parsing, so the malformed body is never looked at.
    let bad = format!("sha256={}", "0".repeat(64));
    let (status, _) = send(&w.app, "github", broken.as_bytes(), Some(&bad), &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // No signature at all is also 401, even for a well-formed body.
    let body = r#"{"action":"opened","sender":{"login":"octocat"}}"#;
    let (status, _) = send(&w.app, "github", body.as_bytes(), None, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A valid signature over the same malformed body DOES reach parsing (400),
    // which proves the 401 above was the gate, not a coincidence.
    let (status, _) = send(
        &w.app,
        "github",
        broken.as_bytes(),
        Some(&signed(broken)),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        w.calls.lock().unwrap().is_empty(),
        "no unsigned turn may reach an agent"
    );
}

#[tokio::test]
async fn sync_mode_returns_agent_answer() {
    let _lock = ENV_LOCK.lock().await;
    let w = world(
        r#"
[[channels]]
id = "github"
agent_id = "billing"
project_id = "proj"
mode = "sync"
signature_header = "X-Signature"
auth_env = "WEBHOOK_TASK3_SECRET"
enabled = true
"#,
    )
    .await;
    let body = r#"{"action":"opened"}"#;

    let (status, value) = send(&w.app, "github", body.as_bytes(), Some(&signed(body)), &[]).await;
    assert_eq!(status, StatusCode::OK, "{value:?}");
    assert_eq!(value["message"]["role"], "assistant");
    assert_eq!(value["message"]["content"], "engine answer");
    assert_eq!(value["done"], true);
    // No template means the raw body is the user message.
    assert_eq!(w.calls.lock().unwrap()[0].user_message, body);
}

#[tokio::test]
async fn async_mode_returns_202_with_execution_id() {
    let _lock = ENV_LOCK.lock().await;
    let w = world(
        r#"
[[channels]]
id = "github"
agent_id = "billing"
project_id = "proj"
signature_header = "X-Signature"
auth_env = "WEBHOOK_TASK3_SECRET"
enabled = true
"#,
    )
    .await;
    let body = r#"{"action":"opened"}"#;

    let (status, value) = send(&w.app, "github", body.as_bytes(), Some(&signed(body)), &[]).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{value:?}");
    let execution_id = value["execution_id"]
        .as_str()
        .expect("202 carries an execution_id");
    assert!(!execution_id.is_empty());

    // The id returned now MUST be the one observed in GET /api/executions/:id
    // once the spawned turn has run and finalised.
    let mut detail = Value::Null;
    for _ in 0..100 {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/executions/{execution_id}"))
            .header("x-debug", "true")
            .body(Body::empty())
            .unwrap();
        let response = w.app.clone().oneshot(request).await.unwrap();
        if response.status() == StatusCode::OK {
            let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            detail = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let status = detail["execution"]["status"].as_str().unwrap_or("");
            if status == "Completed" || status == "Failed" {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(detail["execution"]["id"].as_str(), Some(execution_id));
    let status = detail["execution"]["status"].as_str().unwrap_or("");
    assert!(
        status == "Completed" || status == "Failed",
        "turn never reached a terminal status: {detail:?}"
    );
    assert_eq!(w.calls.lock().unwrap().len(), 1, "engine eventually ran");
}

#[tokio::test]
async fn channel_cannot_force_debug_or_cross_project_agent() {
    let _lock = ENV_LOCK.lock().await;
    let w = world(
        r#"
[[channels]]
id = "github"
agent_id = "billing"
project_id = "proj"
mode = "sync"
signature_header = "X-Signature"
auth_env = "WEBHOOK_TASK3_SECRET"
enabled = true

[[channels]]
id = "bad"
agent_id = "admin"
project_id = "proj"
mode = "sync"
signature_header = "X-Signature"
auth_env = "WEBHOOK_TASK3_SECRET"
enabled = true

[[channels]]
id = "ghost-async"
agent_id = "ghost"
project_id = "proj"
signature_header = "X-Signature"
auth_env = "WEBHOOK_TASK3_SECRET"
enabled = true
"#,
    )
    .await;
    let body = r#"{"action":"opened"}"#;

    // The emitter tries to force a cross-project agent and X-Debug. Neither
    // header may change the selection built from the channel config.
    let (status, value) = send(
        &w.app,
        "github",
        body.as_bytes(),
        Some(&signed(body)),
        &[
            ("x-project", "other"),
            ("x-agent", "admin"),
            ("x-debug", "true"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value:?}");
    assert!(
        value.get("debug_prompt").is_none() && value.get("debug_route").is_none(),
        "emitter-supplied X-Debug must not reach dispatch: {value:?}"
    );
    {
        let calls = w.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].agent_id, "billing");
        assert_eq!(calls[0].project_id.as_deref(), Some("proj"));
    }

    // A channel configured with another project's agent cannot resolve inside
    // its own project: the config-only selection yields the same 400 as a
    // header-pinned request naming a missing agent.
    let (status, _) = send(&w.app, "bad", body.as_bytes(), Some(&signed(body)), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        w.calls.lock().unwrap().len(),
        1,
        "no cross-project turn ran"
    );

    // A missing agent is a 400 in async mode too — never a 202 for a turn
    // that cannot dispatch.
    let (status, _) = send(
        &w.app,
        "ghost-async",
        body.as_bytes(),
        Some(&signed(body)),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unknown_or_disabled_channel_is_404() {
    let _lock = ENV_LOCK.lock().await;
    let w = world(
        r#"
[[channels]]
id = "github"
agent_id = "billing"
project_id = "proj"
mode = "sync"
signature_header = "X-Signature"
auth_env = "WEBHOOK_TASK3_SECRET"
enabled = true

[[channels]]
id = "legacy"
agent_id = "billing"
project_id = "proj"
mode = "sync"
signature_header = "X-Signature"
auth_env = "WEBHOOK_TASK3_SECRET"
enabled = false
"#,
    )
    .await;
    let body = r#"{"action":"opened"}"#;

    let (status, _) = send(&w.app, "nope", body.as_bytes(), Some(&signed(body)), &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&w.app, "legacy", body.as_bytes(), Some(&signed(body)), &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(w.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn secret_never_appears_in_error_or_log() {
    let _lock = ENV_LOCK.lock().await;
    let w = world(
        r#"
[[channels]]
id = "github"
agent_id = "billing"
project_id = "proj"
mode = "sync"
signature_header = "X-Signature"
auth_env = "WEBHOOK_TASK3_SECRET"
template = "{{action}}"
enabled = true

[[channels]]
id = "templbad"
agent_id = "billing"
project_id = "proj"
mode = "sync"
signature_header = "X-Signature"
auth_env = "WEBHOOK_TASK3_SECRET"
template = "{{no.such.path}}"
enabled = true
"#,
    )
    .await;

    let body = format!(r#"{{"token":"{PAYLOAD_SECRET}","action":"opened"}}"#);
    let signature = signed(&body);
    let bad_signature = format!("sha256={}", "a".repeat(64));

    let buf = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .without_time()
        .with_target(false)
        .finish();
    // current_thread keeps the subscriber guard on the polling thread, so it
    // stays installed across every `.await` below.
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let (status, value) = send(&w.app, "github", body.as_bytes(), Some(&bad_signature), &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!value.to_string().contains(PAYLOAD_SECRET));
    assert!(!value.to_string().contains(SECRET));

    // A valid signature, but the template names an unknown path: the error
    // may name that path and nothing from the payload.
    let (status, value) = send(&w.app, "templbad", body.as_bytes(), Some(&signature), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value:?}");
    let text = value.to_string();
    assert!(
        text.contains("no.such.path"),
        "the error names the path: {text:?}"
    );
    assert!(
        !text.contains(PAYLOAD_SECRET),
        "the error echoed the body: {text:?}"
    );
    assert!(
        !text.contains(SECRET),
        "the error echoed the secret: {text:?}"
    );

    drop(_subscriber_guard);
    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        !log.contains(PAYLOAD_SECRET),
        "log echoed the body: {log:?}"
    );
    assert!(!log.contains(SECRET), "log echoed the secret: {log:?}");
    assert!(
        !log.contains(&bad_signature),
        "log echoed the signature: {log:?}"
    );
    assert!(
        !log.contains(&signature),
        "log echoed the signature: {log:?}"
    );
}

/// A channel whose shared secret is missing or empty must fail closed. The
/// forgery this blocks is an HMAC computed over an EMPTY key: before the fix,
/// `unwrap_or_default()` made that a valid signature for a misconfigured
/// channel, letting anyone through. Both the 401 and zero agent calls are
/// asserted, for unset and empty variables alike.
#[tokio::test]
async fn missing_or_empty_channel_secret_is_401_even_with_empty_key_signature() {
    let _lock = ENV_LOCK.lock().await;
    let w = world_with_verifier(
        r#"
[[channels]]
id = "unset-secret"
agent_id = "billing"
project_id = "proj"
mode = "sync"
signature_header = "X-Signature"
auth_env = "WEBHOOK_TASK3_UNSET_SECRET"
enabled = true

[[channels]]
id = "empty-secret"
agent_id = "billing"
project_id = "proj"
mode = "sync"
signature_header = "X-Signature"
auth_env = "WEBHOOK_TASK3_EMPTY_SECRET"
enabled = true
"#,
        // A verifier WITHOUT the empty-secret guard makes this test fail on
        // the pre-fix `unwrap_or_default()` code and pass only with the
        // endpoint's own explicit check.
        Arc::new(EmptyKeyAcceptingVerifier),
    )
    .await;
    let _unset = EnvGuard::unset("WEBHOOK_TASK3_UNSET_SECRET");
    let _empty = EnvGuard::set("WEBHOOK_TASK3_EMPTY_SECRET", "");

    let body = r#"{"action":"opened"}"#;
    // Exactly the signature an attacker would send when the key is empty.
    let empty_key_signature = format!("sha256={}", sign_body("", body.as_bytes()));

    for channel in ["unset-secret", "empty-secret"] {
        let (status, value) = send(
            &w.app,
            channel,
            body.as_bytes(),
            Some(&empty_key_signature),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{channel}: {value:?}");
        assert!(
            !value.to_string().contains("WEBHOOK_TASK3_UNSET_SECRET")
                && !value.to_string().contains("WEBHOOK_TASK3_EMPTY_SECRET"),
            "the refusal must not name the variable's value or config: {value:?}"
        );
    }
    assert!(
        w.calls.lock().unwrap().is_empty(),
        "no turn may reach an agent when the channel secret is missing"
    );
}
