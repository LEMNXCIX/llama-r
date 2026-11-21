//! Task 1: shared-token auth on the admin endpoint group.
//!
//! These tests drive the REAL `build_router`, so a route accidentally left in
//! the public group (or a public route accidentally pulled into the admin
//! group) fails here rather than in a hand-built fixture. The gate is the only
//! thing that reads `ADMIN_TOKEN`; the token is never placed in `Config`, a log
//! line, a response body or a response header in this task.

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use llama_r::adapters::channel::hmac::constant_time_eq;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::handlers::AppState;
use llama_r::context::store::ContextStore;
use llama_r::domain::models::{ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::providers::LLMProvider;
use llama_r::runtime::{build_app_state, build_router};
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::provider_stack::ProviderStack;
use llama_r::services::skill_manager::SkillManager;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::error::Error;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;
use tokio_stream::Stream;
use tower::ServiceExt;

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

const TOKEN: &str = "s3cr3t-admin-token-value";
const ADMIN_HEADER: &str = "x-admin-token";

/// Every route-method pair the controller put in the protected set. A path's
/// methods are listed separately because the gate is method-agnostic: the
/// sub-router carries the middleware, so all of a route's methods inherit it.
const PROTECTED: &[(&str, &str)] = &[
    ("GET", "/api/approvals"),
    (
        "POST",
        "/api/approvals/11111111-1111-1111-1111-111111111111",
    ),
    ("GET", "/api/executions"),
    (
        "GET",
        "/api/executions/11111111-1111-1111-1111-111111111111",
    ),
    (
        "POST",
        "/api/executions/11111111-1111-1111-1111-111111111111/retry",
    ),
    ("POST", "/api/rag/ingest"),
    ("POST", "/api/rag/query"),
    ("POST", "/api/rag/delete-document"),
    ("POST", "/api/route"),
    ("GET", "/api/contexts"),
    ("POST", "/api/contexts"),
    ("GET", "/api/contexts/demo"),
    ("PUT", "/api/contexts/demo"),
    ("DELETE", "/api/contexts/demo"),
    ("POST", "/api/contexts/demo/analyze"),
    ("GET", "/api/agents"),
    ("POST", "/api/agents"),
    ("GET", "/api/agents/demo"),
    ("PUT", "/api/agents/demo"),
    ("DELETE", "/api/agents/demo"),
    ("GET", "/api/agents/demo/scope"),
];

/// The one fixed refusal body: no token, no reason, no echo. Pinning it as a
/// value also proves that a wrong token and a missing token are indistinguishable.
fn admin_refusal() -> Value {
    json!({
        "error": {
            "code": "unauthorized",
            "message": "Unauthorized: admin authentication required"
        }
    })
}

/// Serialise tests that move process-wide env. `ADMIN_TOKEN` is the one under
/// test; the router vars are cleared so an ambient `ROUTER_ENABLED=true` cannot
/// pull a decision model into a test that never spends a decision.
struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
    _lock: MutexGuard<'static, ()>,
}

const MUTATED_ENV: &[&str] = &[
    "ADMIN_TOKEN",
    "LLAMA_R_DIR",
    "ROUTER_ENABLED",
    "ROUTER_URL",
    "ROUTER_MODEL",
    "ROUTER_TIMEOUT_MS",
    "MIN_CONFIDENCE",
    "MIN_SPECIALIZED",
];

impl EnvGuard {
    fn new() -> Self {
        let lock = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = MUTATED_ENV
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect();
        for name in MUTATED_ENV {
            std::env::remove_var(name);
        }
        Self { saved, _lock: lock }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // Runs while `_lock` is still alive, so restoration cannot race the
        // next test's `EnvGuard::new()`.
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// None of these endpoints calls the provider; this only satisfies the trait.
struct InertProvider;

#[async_trait]
impl LLMProvider for InertProvider {
    fn get_base_url(&self) -> String {
        "http://inert".to_string()
    }
    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
    async fn chat(
        &self,
        _request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        Err("not reached".into())
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        Err("not reached".into())
    }
}

fn test_state() -> (TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());
    let registry = Arc::new(AgentRegistry::new());
    registry.reload_all(&[]).unwrap();
    let stack = ProviderStack::new(Arc::new(InertProvider), "fallback-model");
    let state = build_app_state(
        stack,
        registry,
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        Arc::new(Mutex::new(VecDeque::new())),
        Vec::new(),
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
    );
    (dir, state)
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    debug: bool,
    body: &str,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(ADMIN_HEADER, token);
    }
    if debug {
        builder = builder.header("x-debug", "true");
    }
    let request = builder
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn admin_token_required_on_all_admin_routes() {
    let _guard = EnvGuard::new();
    let (_dir, state) = test_state();
    let router = build_router(state);

    // Token configured but not presented. X-Debug: true is present on every
    // request: the token is additional to the debug gate, not a replacement.
    std::env::set_var("ADMIN_TOKEN", TOKEN);
    for (method, uri) in PROTECTED {
        let (status, body) = send(&router, method, uri, None, true, "{}").await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri}, no header"
        );
        assert_eq!(body, admin_refusal(), "{method} {uri}, no header");
    }

    // Fail closed when the token was never configured.
    std::env::remove_var("ADMIN_TOKEN");
    for (method, uri) in PROTECTED {
        let (status, body) = send(&router, method, uri, None, false, "{}").await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri}, unset token"
        );
        assert_eq!(body, admin_refusal(), "{method} {uri}, unset token");
    }

    // An empty token is the same as no token.
    std::env::set_var("ADMIN_TOKEN", "");
    for (method, uri) in PROTECTED {
        let (status, body) = send(&router, method, uri, None, false, "{}").await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri}, empty token"
        );
        assert_eq!(body, admin_refusal(), "{method} {uri}, empty token");
    }
}

#[tokio::test]
async fn valid_token_reaches_handler() {
    let _guard = EnvGuard::new();
    std::env::set_var("ADMIN_TOKEN", TOKEN);
    let (_dir, state) = test_state();
    let router = build_router(state);

    // An empty registry lists as `[]`: reaching the handler, not a 401.
    let (status, body) = send(&router, "GET", "/api/agents", Some(TOKEN), false, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));

    // X-Debug stays additive: the same token with the extra header still runs.
    let (status, body) = send(&router, "GET", "/api/agents", Some(TOKEN), true, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));

    // A wrong token is indistinguishable from a missing one.
    let (status, body) = send(&router, "GET", "/api/agents", Some("wrong-token"), true, "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, admin_refusal());
}

#[tokio::test]
async fn token_never_in_logs_or_response() {
    let _guard = EnvGuard::new();
    std::env::set_var("ADMIN_TOKEN", TOKEN);
    let (_dir, state) = test_state();
    let router = build_router(state.clone());

    let cases = [
        send(&router, "GET", "/api/agents", None, true, "").await,
        send(&router, "GET", "/api/agents", Some(TOKEN), true, "").await,
        send(&router, "POST", "/api/route", Some(TOKEN), true, "{}").await,
    ];
    for (status, body) in cases {
        let rendered = format!("{status} {}", serde_json::to_string(&body).unwrap());
        assert!(
            !rendered.contains(TOKEN),
            "token echoed in response: {rendered}"
        );
    }

    // The documented scheme names the header but never carries a token.
    let (_, openapi) = send(&router, "GET", "/openapi.json", None, false, "").await;
    let scheme = &openapi["components"]["securitySchemes"]["apiToken"];
    assert_eq!(scheme["type"], json!("apiKey"));
    assert_eq!(scheme["in"], json!("header"));
    assert_eq!(scheme["name"], json!("X-Admin-Token"));
    assert!(!scheme.to_string().contains(TOKEN));

    // Response headers too: the token is never reflected back as metadata.
    let request = Request::builder()
        .method("GET")
        .uri("/api/agents")
        .header(ADMIN_HEADER, TOKEN)
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    for (name, value) in response.headers() {
        let value = value.to_str().unwrap_or("");
        assert!(
            !value.contains(TOKEN),
            "token echoed in response header {name}: {value}"
        );
    }

    let logs = state.logs.lock().unwrap();
    for line in logs.iter() {
        assert!(
            !line.contains(TOKEN),
            "token echoed in application log: {line}"
        );
    }
}

#[tokio::test]
async fn chat_and_webhooks_unauthenticated() {
    let _guard = EnvGuard::new();
    std::env::set_var("ADMIN_TOKEN", TOKEN);
    let (_dir, state) = test_state();
    let router = build_router(state);

    let (admin_status, admin_body) = send(&router, "GET", "/api/agents", None, true, "").await;
    assert_eq!(admin_status, StatusCode::UNAUTHORIZED);

    // Unchanged read endpoints answer normally with no token at all.
    for uri in [
        "/",
        "/api",
        "/health",
        "/api/health",
        "/models",
        "/api/models",
        "/openapi.json",
        "/docs",
    ] {
        let (status, body) = send(&router, "GET", uri, None, false, "").await;
        assert_eq!(status, StatusCode::OK, "GET {uri} must stay public");
        assert_ne!(body, admin_body, "GET {uri} must not use the admin refusal");
    }

    // These two listing endpoints were never in the protected set, so they
    // keep their own status contract (200 or 501 without a store) — just not
    // the admin refusal.
    for uri in ["/api/skills", "/api/conversations"] {
        let (status, body) = send(&router, "GET", uri, None, false, "").await;
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "GET {uri} must stay public"
        );
        assert_ne!(body, admin_body, "GET {uri} must not use the admin refusal");
    }

    // Chat endpoints have their own validation/provider errors, never our gate.
    for uri in ["/chat", "/api/chat", "/v1/chat/completions"] {
        let (status, body) = send(&router, "POST", uri, None, false, "{}").await;
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "POST {uri} must stay public"
        );
        assert_ne!(
            body, admin_body,
            "POST {uri} must not use the admin refusal"
        );
    }

    // A webhook owns its signature gate (404 here for an unknown channel); it
    // must never answer with the admin refusal.
    let (_, body) = send(
        &router,
        "POST",
        "/api/webhooks/no-such-channel",
        None,
        false,
        "{}",
    )
    .await;
    assert_ne!(body, admin_body, "webhook must not use the admin refusal");
}

#[test]
fn constant_time_eq_vectors() {
    assert!(constant_time_eq(b"", b""));
    assert!(constant_time_eq(b"same-token", b"same-token"));

    // Same length, one byte different: false.
    assert!(!constant_time_eq(b"same-token", b"same-tokem"));
    assert!(!constant_time_eq(b"abc", b"abd"));

    // Different lengths: false, including the empty/non-empty edge.
    assert!(!constant_time_eq(b"abc", b"abcd"));
    assert!(!constant_time_eq(b"", b"x"));
}
