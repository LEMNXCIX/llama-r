//! `POST /api/route` — the dry-run: the routing decision without the chat.
//!
//! The debug gate is asserted the way `api_integration.rs` asserts the RAG
//! endpoints' gate — through a real `axum::extract::Request` handed to the
//! extractor, so a 403 comes from the header check running *before* the body
//! is parsed, not from a handler that was handed a body it should never have
//! seen. `src/api/rag_api.rs` is the pattern; its `GatedJson` is RAG-specific
//! in its message, so `route_api` carries its own copy of the structure.

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::extract::{FromRequest, State};
use axum::http::{Request, StatusCode};
use axum::Json;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::chat_core::{execute_chat, AgentSelection};
use llama_r::api::docs::ApiDoc;
use llama_r::api::handlers::AppState;
use llama_r::api::route_api::{
    route_preview, GatedJson, RoutePreviewRequest, RoutePreviewResponse,
};
use llama_r::context::store::ContextStore;
use llama_r::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use llama_r::error::AppError;
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::ports::router::{Answer, DecisionRequest, DecisionResponse, DecisionRouter};
use llama_r::providers::LLMProvider;
use llama_r::runtime::{build_app_state, build_router};
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::agent_router::{CHOICE_KEY, NOUL_KEY};
use llama_r::services::skill_manager::SkillManager;
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;
use tokio_stream::Stream;
use tower::ServiceExt;
use utoipa::OpenApi;

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Every variable `RouterConfig::load` reads. `LLAMA_R_DIR` is deliberately
/// absent: `state_with` points it at its own temp dir, exactly as
/// `router_dispatch_tests.rs` does.
const ROUTER_ENV_VARS: &[&str] = &[
    "ROUTER_ENABLED",
    "ROUTER_URL",
    "ROUTER_MODEL",
    "ROUTER_TIMEOUT_MS",
    "MIN_CONFIDENCE",
    "MIN_SPECIALIZED",
];

/// Clears the router's environment for one test and restores whatever was
/// there when it drops — the same discipline as `api_integration.rs`'s
/// `RouterEnvGuard`.
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

/// The environment one preview test runs under: router variables cleared on
/// entry, restored on exit, under a lock held for the whole test.
///
/// Field order is load-bearing: Rust drops struct fields in declaration
/// order, so `_env` restores the ambient `ROUTER_*` values while `_lock` is
/// still held — declared the other way round, the next test could build its
/// `AppState` against a half-restored environment.
struct PreviewEnv {
    _env: RouterEnvGuard,
    _lock: MutexGuard<'static, ()>,
}

impl PreviewEnv {
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

/// A provider that answers, so the chat-path tests can reach their
/// assertions without a real model.
struct EchoProvider;

#[async_trait]
impl LLMProvider for EchoProvider {
    fn get_base_url(&self) -> String {
        "http://echo".to_string()
    }
    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(vec![ModelInfo {
            name: "fallback-model".to_string(),
            modified_at: "now".to_string(),
            size: 1,
        }])
    }
    async fn chat(
        &self,
        request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        Ok(ChatResponse {
            model: request.model,
            created_at: "now".to_string(),
            message: ChatMessage {
                role: "assistant".to_string(),
                content: "echo".to_string(),
            },
            done: true,
            debug_prompt: None,
            debug_route: None,
        })
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        Err("not used by these tests".into())
    }
}

/// A provider whose `chat` panics: if the preview ever executes the chat,
/// the panic fails the test that reached it. `list_models` is never called.
struct ChatWouldPanicProvider;

#[async_trait]
impl LLMProvider for ChatWouldPanicProvider {
    fn get_base_url(&self) -> String {
        "http://never".to_string()
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
        panic!("the route preview must never execute the chat");
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        panic!("the route preview must never execute the chat");
    }
}

/// A `DecisionRouter` that answers from a fixed `DecisionResponse` and
/// records whether it was ever asked. The counter is shared through an
/// `Arc`, so a clone installed in `state.router` and the value the assertion
/// holds observe the same calls.
#[derive(Clone)]
struct FakeRouter {
    called: Arc<AtomicBool>,
    reply: DecisionResponse,
}

impl FakeRouter {
    fn returning(reply: DecisionResponse) -> Self {
        Self {
            called: Arc::new(AtomicBool::new(false)),
            reply,
        }
    }

    fn was_called(&self) -> bool {
        self.called.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl DecisionRouter for FakeRouter {
    async fn decide(&self, _request: DecisionRequest) -> Result<DecisionResponse, AppError> {
        self.called.store(true, Ordering::SeqCst);
        Ok(self.reply.clone())
    }
}

/// A response that picks `key` with enough confidence (0.95) and enough
/// specialisation (0.95) to clear both default thresholds — and a
/// `probabilities` map that contains its own winner, which is the shape a
/// consistent `Choice` has.
fn choice_for(key: &str) -> DecisionResponse {
    DecisionResponse {
        answers: BTreeMap::from([
            (
                CHOICE_KEY.to_string(),
                Answer::Choice {
                    key: key.to_string(),
                    confidence: 0.95,
                    probabilities: BTreeMap::from([(key.to_string(), 0.95)]),
                },
            ),
            (NOUL_KEY.to_string(), Answer::Noul { value: 0.95 }),
        ]),
    }
}

/// The adapter never checks that a `Choice`'s `key` appears among its own
/// `probabilities`, so this answer — a winner the distribution does not
/// contain — can arrive from the wire. Both debug surfaces must say so
/// rather than emit the distribution as an account of the winner.
fn choice_omitting_its_winner() -> DecisionResponse {
    DecisionResponse {
        answers: BTreeMap::from([
            (
                CHOICE_KEY.to_string(),
                Answer::Choice {
                    key: "fudi::ops".to_string(),
                    confidence: 0.95,
                    probabilities: BTreeMap::from([("fudi::fudi".to_string(), 0.95)]),
                },
            ),
            (NOUL_KEY.to_string(), Answer::Noul { value: 0.95 }),
        ]),
    }
}

const GENERAL: &str = "name = \"Fudi\"\nsystem_prompt = \"base\"\n\
                       description = \"Asistente general de Fudi.\"\n";

const OPS: &str = "name = \"Ops\"\nsystem_prompt = \"base\"\n\
                   description = \"Cancela pedidos y gestiona operaciones.\"\n";

/// An `AppState` over project `fudi`'s agents (`fudi` general, `ops`
/// specialist), with `ROUTER_ENABLED` deciding whether the config comes up
/// enabled — set before `build_app_state`, which reads the ambient `ROUTER_*`
/// this file's `PreviewEnv` cleared. With routing on, `build_app_state` also
/// constructs the real `LlamaCppRouter`; `fake` replaces it before any
/// decision is spent, which is what the `RwLock` around `AppState::router`
/// is for.
fn state_with(
    provider: Arc<dyn LLMProvider + Send + Sync>,
    router_enabled: bool,
    fake: Option<FakeRouter>,
) -> (TempDir, Arc<AppState>) {
    if router_enabled {
        std::env::set_var("ROUTER_ENABLED", "true");
    }
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());

    let agents_dir = dir
        .path()
        .join("contextos")
        .join("projects")
        .join("fudi")
        .join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(agents_dir.join("fudi.toml"), GENERAL).unwrap();
    std::fs::write(agents_dir.join("ops.toml"), OPS).unwrap();

    let registry = Arc::new(AgentRegistry::new());
    registry.reload_all(&[]).unwrap();

    let state = build_app_state(
        provider,
        registry,
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        "fallback-model".to_string(),
        Arc::new(Mutex::new(VecDeque::new())),
        Vec::new(),
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
        None,
        None,
        None,
        None,
    );
    if let Some(fake) = fake {
        *state.router.write().unwrap() = Some(Arc::new(fake));
    }
    (dir, state)
}

fn preview_json(project_id: &str, prompt: &str) -> String {
    serde_json::json!({ "project_id": project_id, "prompt": prompt }).to_string()
}

/// Run the real gate, then the handler: the path every request takes.
async fn call_preview(
    state: &Arc<AppState>,
    body: &str,
    debug: bool,
) -> Result<RoutePreviewResponse, AppError> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/route")
        .header("content-type", "application/json");
    if debug {
        builder = builder.header("x-debug", "true");
    }
    let request = builder.body(Body::from(body.to_string())).unwrap();
    let gated = GatedJson::<RoutePreviewRequest>::from_request(request, state).await?;
    let Json(response) = route_preview(State(state.clone()), gated).await?;
    Ok(response)
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn preview_requires_the_debug_header() {
    let _env = PreviewEnv::new();
    let (_dir, state) = state_with(Arc::new(EchoProvider), false, None);
    let err = call_preview(&state, &preview_json("fudi", "hola"), false)
        .await
        .unwrap_err();
    assert_eq!(err.status_code(), StatusCode::FORBIDDEN);
}

/// The gate runs before the body is parsed: without the header a body that
/// cannot even be JSON still reads 403 — never a 4xx that describes the
/// schema — and with the header the same body reaches the parser.
#[tokio::test]
async fn the_debug_gate_runs_before_the_body_is_parsed() {
    let _env = PreviewEnv::new();
    let (_dir, state) = state_with(Arc::new(EchoProvider), false, None);

    let err = call_preview(&state, "this is not json at all", false)
        .await
        .unwrap_err();
    assert_eq!(
        err.status_code(),
        StatusCode::FORBIDDEN,
        "an unauthorized caller must never learn the request schema"
    );

    let err = call_preview(&state, "this is not json at all", true)
        .await
        .unwrap_err();
    assert_eq!(
        err.status_code(),
        StatusCode::BAD_REQUEST,
        "with the header the body must reach the parser"
    );
}

#[tokio::test]
async fn preview_reports_501_when_the_router_is_off() {
    let _env = PreviewEnv::new();
    // Ambient `ROUTER_*` cleared: `build_app_state` yields
    // `RouterConfig::default()` — routing off, no router installed.
    let (_dir, state) = state_with(Arc::new(EchoProvider), false, None);
    let err = call_preview(&state, &preview_json("fudi", "hola"), true)
        .await
        .unwrap_err();
    assert_eq!(err.status_code(), StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn preview_returns_the_candidates_it_would_have_offered() {
    let _env = PreviewEnv::new();
    let fake = FakeRouter::returning(choice_for("fudi::ops"));
    let (_dir, state) = state_with(Arc::new(EchoProvider), true, Some(fake.clone()));
    let body = call_preview(&state, &preview_json("fudi", "cancela el pedido 42"), true)
        .await
        .unwrap();

    assert_eq!(body.decision, "agent");
    assert!(
        body.reason.is_none(),
        "a specialist winning reports no reason"
    );
    assert!(body.candidates.iter().any(|c| c.key == "fudi::ops"));
    assert!(
        body.candidates
            .iter()
            .find(|c| c.key == "fudi::fudi")
            .is_some_and(|c| c.is_project_general),
        "the preview must show which candidate is the ladder's fallback"
    );
    assert!(body.probabilities.contains_key("fudi::ops"));
    let confidence = body.confidence.expect("the model reported a confidence");
    assert!((confidence - 0.95).abs() < 1e-6);
    // The raw answer travels untouched beside the derived decision.
    assert_eq!(body.raw["answers"]["agent"]["key"], "fudi::ops");
    assert!(fake.was_called(), "the preview must consult the router");
}

/// Registration, end to end: the route exists on the `Router` and the gate
/// guards it there too, with the disabled router reported as 501 and code
/// `unavailable` — `AppError::Unavailable`'s standing mapping.
#[tokio::test]
async fn the_endpoint_is_mounted_behind_the_debug_gate() {
    let _env = PreviewEnv::new();
    let (_dir, state) = state_with(Arc::new(EchoProvider), false, None);
    let app = build_router(state);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/route")
                .header("content-type", "application/json")
                .body(Body::from(preview_json("fudi", "hola")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/route")
                .header("content-type", "application/json")
                .header("x-debug", "true")
                .body(Body::from(preview_json("fudi", "hola")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let json = body_json(response).await;
    assert_eq!(json["error"]["code"], "unavailable");
}

/// The brief's hard constraint: a preview runs the decision only. The
/// provider panics on `chat`, so reaching the assertion at all proves the
/// chat was never executed.
#[tokio::test]
async fn preview_never_executes_the_chat() {
    let _env = PreviewEnv::new();
    let (_dir, state) = state_with(
        Arc::new(ChatWouldPanicProvider),
        true,
        Some(FakeRouter::returning(choice_for("fudi::ops"))),
    );
    let body = call_preview(&state, &preview_json("fudi", "cancela el pedido 42"), true)
        .await
        .unwrap();
    assert_eq!(body.decision, "agent");
}

/// The deferred adapter finding, handled at the presentation layer: the
/// winner `fudi::ops` is absent from the returned `probabilities`, so the
/// response must say so instead of emitting a distribution that does not
/// account for its own winner. `raw` still carries exactly what the router
/// answered — nothing is lost, only the misleading presentation.
#[tokio::test]
async fn a_winner_absent_from_the_probabilities_is_said_rather_than_shown() {
    let _env = PreviewEnv::new();
    let (_dir, state) = state_with(
        Arc::new(EchoProvider),
        true,
        Some(FakeRouter::returning(choice_omitting_its_winner())),
    );
    let body = call_preview(&state, &preview_json("fudi", "cancela el pedido 42"), true)
        .await
        .unwrap();

    assert_eq!(
        body.decision, "agent",
        "the ladder decides from the winner, not from the distribution"
    );
    assert!(
        body.probabilities.is_empty(),
        "a distribution that does not account for the winner must not be emitted as if it did"
    );
    let note = body
        .probabilities_note
        .expect("the inconsistency must be said out loud");
    assert!(
        note.contains("fudi::ops"),
        "the note must name the winner: {note}"
    );

    let raw_winner = body.raw["answers"]["agent"]["probabilities"]["fudi::fudi"]
        .as_f64()
        .expect("raw keeps exactly what the router answered");
    assert!((raw_winner - 0.95).abs() < 1e-6);
}

/// Step 4: the non-streaming chat response carries the decision the router
/// made. Deliberately not gated on `X-Debug` (the brief says
/// `response.debug_route = resolved.routed`), unlike `debug_prompt`, which
/// stays `None` for this request.
#[tokio::test]
async fn debug_route_rides_on_the_non_streaming_chat_response() {
    let _env = PreviewEnv::new();
    let (_dir, state) = state_with(
        Arc::new(EchoProvider),
        true,
        Some(FakeRouter::returning(choice_for("fudi::ops"))),
    );
    let response = execute_chat(
        &state,
        ChatRequest {
            model: "fallback-model".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "cancela el pedido 42".to_string(),
            }],
            stream: false,
        },
        AgentSelection {
            project_id: Some("fudi"),
            agent_id: None,
            conversation_id: None,
            debug: false,
        },
    )
    .await
    .unwrap();

    assert!(
        response.debug_prompt.is_none(),
        "no X-Debug: no debug_prompt"
    );
    let routed = response
        .debug_route
        .expect("a routed non-streaming chat carries its decision");
    assert_eq!(routed.outcome, "agent");
    assert_eq!(routed.project_id, "fudi");
    assert_eq!(routed.winner.as_deref(), Some("fudi::ops"));
    assert!(routed.probabilities_note.is_none());
    assert!(routed.probabilities.contains_key("fudi::ops"));
}

/// The same inconsistency check on the chat surface: `debug_route` withholds
/// the distribution and says why, exactly as the preview does (one shared
/// extractor — `peeked_answer` — feeds both).
#[tokio::test]
async fn debug_route_withholds_a_distribution_that_does_not_account_for_its_winner() {
    let _env = PreviewEnv::new();
    let (_dir, state) = state_with(
        Arc::new(EchoProvider),
        true,
        Some(FakeRouter::returning(choice_omitting_its_winner())),
    );
    let response = execute_chat(
        &state,
        ChatRequest {
            model: "fallback-model".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "cancela el pedido 42".to_string(),
            }],
            stream: false,
        },
        AgentSelection {
            project_id: Some("fudi"),
            agent_id: None,
            conversation_id: None,
            debug: false,
        },
    )
    .await
    .unwrap();

    let routed = response
        .debug_route
        .expect("a routed non-streaming chat carries its decision");
    assert!(
        routed.probabilities.is_empty(),
        "a distribution that does not account for the winner must not be emitted as if it did"
    );
    let note = routed
        .probabilities_note
        .expect("the inconsistency must be said out loud");
    assert!(
        note.contains("fudi::ops"),
        "the note must name the winner: {note}"
    );
}

/// Open item 1: every `$ref` the document emits must resolve inside it.
/// `HealthResponse` (carrying `RouterHealth`), `ChatResponse` (carrying
/// `RoutedDebug`) and the new path's schemas are exactly the shapes whose
/// registration this pins — an unregistered `ToSchema` field type produces a
/// `$ref` pointing at nothing.
#[test]
fn the_openapi_document_has_no_unresolved_refs() {
    let doc = serde_json::to_value(ApiDoc::openapi()).unwrap();

    fn walk(value: &serde_json::Value, doc: &serde_json::Value, checked: &mut usize) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    if key == "$ref" {
                        let reference =
                            child.as_str().expect("a $ref must be a string").to_string();
                        let pointer = reference.strip_prefix('#').unwrap_or(&reference);
                        assert!(
                            doc.pointer(pointer).is_some(),
                            "unresolved $ref in /openapi.json: {reference}"
                        );
                        *checked += 1;
                    } else {
                        walk(child, doc, checked);
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, doc, checked);
                }
            }
            _ => {}
        }
    }

    let mut checked = 0usize;
    walk(&doc, &doc, &mut checked);
    assert!(
        checked >= 4,
        "the document must actually reference its schemas (checked {checked})"
    );
    // The task's endpoint must be *listed*, not only compilable: registering
    // the `utoipa::path` annotation in `docs.rs` is what puts it here.
    assert!(
        doc.pointer("/paths/~1api~1route").is_some(),
        "POST /api/route must appear in /openapi.json"
    );
}
