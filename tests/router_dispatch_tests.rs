//! Dispatch wiring for the agent router: `resolve_selection` fills the hole
//! `X-Agent` would have filled and produces the same `(project_id, agent_id)`
//! pair the header would have produced, so nothing downstream can tell a
//! routed selection from a header-derived one.
//!
//! The tests that carry the most weight here are the ones asserting the
//! router is **not** consulted: an explicit `X-Agent`, a request with no
//! `X-Project`, or a router nobody switched on. Routing is only allowed to
//! own one narrow path; `tests/api_integration.rs` — which predates this
//! feature — is the end-to-end check that it did not widen it.

use async_trait::async_trait;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::chat_core::{
    build_state_text, resolve_selection, resolve_selection_with, AgentSelection,
};
use llama_r::api::handlers::AppState;
use llama_r::context::store::ContextStore;
use llama_r::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use llama_r::error::AppError;
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::ports::router::{Answer, DecisionRequest, DecisionResponse, DecisionRouter};
use llama_r::providers::LLMProvider;
use llama_r::runtime::build_app_state;
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::agent_router::{CHOICE_KEY, NOUL_KEY};
use llama_r::services::skill_manager::SkillManager;
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tokio_stream::Stream;

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Every variable `RouterConfig::load` reads. `LLAMA_R_DIR` is deliberately
/// absent: `state_with_project_agent` sets it to its own temp dir, exactly as
/// `api_integration.rs`'s `setup_app` does.
const ROUTER_ENV_VARS: &[&str] = &[
    "ROUTER_ENABLED",
    "ROUTER_URL",
    "ROUTER_MODEL",
    "ROUTER_TIMEOUT_MS",
    "MIN_CONFIDENCE",
    "MIN_SPECIALIZED",
];

/// Clears the router's environment for the duration of one test and puts back
/// whatever was there when it drops — the same clear/restore discipline as
/// `RouterConfig`'s own `RouterEnvGuard` (`src/services/router_config.rs`),
/// which is `mod tests`-private and unreachable from here.
///
/// Without this, `build_app_state`'s `RouterConfig::load()` reads the
/// *ambient* shell: `ROUTER_ENABLED=true cargo test` would hand every test
/// here a state that routes whether the test asked for it or not. A suite's
/// verdict must never depend on the developer's environment.
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

/// The environment one dispatch test runs under: the router's variables
/// cleared on entry, restored on exit, under a lock held for the whole test.
///
/// Field order is load-bearing: Rust drops struct fields in declaration
/// order, so `_env` restores the ambient `ROUTER_*` values while `_lock` is
/// still held. Declared the other way round, the lock would release first and
/// the next test could build its `AppState` against a half-restored
/// environment — the race `api_integration.rs`'s `TestApp` documents.
struct DispatchEnv {
    _env: RouterEnvGuard,
    _lock: MutexGuard<'static, ()>,
}

impl DispatchEnv {
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

/// The endpoint never answers here, but the trait must be satisfied.
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

/// A `DecisionRouter` that answers from a fixed `DecisionResponse` (or fails
/// on demand) and records whether it was ever asked.
///
/// The call counter is shared through an `Arc`, so the clone installed in
/// `state.router` and the value the assertion holds observe the same calls —
/// which is what lets `an_explicit_agent_header_is_never_routed` move a fake
/// into the state and still ask `fake.was_called()` afterwards.
#[derive(Clone)]
struct FakeRouter {
    called: Arc<AtomicBool>,
    reply: Result<DecisionResponse, String>,
}

impl FakeRouter {
    fn returning(response: DecisionResponse) -> Self {
        Self {
            called: Arc::new(AtomicBool::new(false)),
            reply: Ok(response),
        }
    }

    fn failing() -> Self {
        Self {
            called: Arc::new(AtomicBool::new(false)),
            reply: Err("decision router is down".to_string()),
        }
    }

    /// The instance to install in the state: a clone sharing this fake's
    /// counter, so `was_called()` on `self` observes calls made through it.
    fn with_call_counter(&self) -> Self {
        self.clone()
    }

    fn was_called(&self) -> bool {
        self.called.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl DecisionRouter for FakeRouter {
    async fn decide(&self, _request: DecisionRequest) -> Result<DecisionResponse, AppError> {
        self.called.store(true, Ordering::SeqCst);
        match &self.reply {
            Ok(response) => Ok(response.clone()),
            Err(message) => Err(AppError::Provider(message.clone())),
        }
    }
}

/// A response that picks `key` with enough confidence (0.95) and enough
/// specialisation (0.95) to clear both default thresholds — so any test using
/// it fails loudly if the router is never consulted at all.
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

fn user(content: &str) -> ChatMessage {
    ChatMessage {
        role: "user".to_string(),
        content: content.to_string(),
    }
}

/// An `AppState` whose registry holds project `fudi`'s own agents:
/// `contextos/projects/fudi/agents/fudi.toml` (the general agent — id equals
/// project id) and `contextos/projects/fudi/agents/ops.toml` (a specialist).
///
/// The router config comes from the ambient environment, which `DispatchEnv`
/// has cleared: `router_config.enabled == false` and `state.router is None`
/// unless the test switched `ROUTER_ENABLED` on before calling this. With
/// routing switched on, `build_app_state` also constructs the real
/// `LlamaCppRouter`; the tests replace it with a fake before any decision is
/// spent, which is what the `RwLock` around `AppState::router` is for.
///
/// The manifests only have to outlive `reload_all`: the registry copies them
/// into memory, and nothing on the resolution path reads them again, so the
/// temp dir may go when this function returns.
fn state_with_project_agent(project_id: &str) -> Arc<AppState> {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());

    let agents_dir = dir
        .path()
        .join("contextos")
        .join("projects")
        .join(project_id)
        .join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join(format!("{project_id}.toml")),
        format!(
            "name = \"{project_id}\"\nsystem_prompt = \"base\"\n\
             description = \"Asistente general de {project_id}.\"\n"
        ),
    )
    .unwrap();
    std::fs::write(
        agents_dir.join("ops.toml"),
        "name = \"Ops\"\nsystem_prompt = \"base\"\n\
         description = \"Cancela pedidos y gestiona operaciones.\"\n",
    )
    .unwrap();

    let registry = AgentRegistry::new();
    registry.reload_all(&[]).unwrap();

    build_app_state(
        Arc::new(InertProvider),
        Arc::new(registry),
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
    )
}

// Review Focus #1: the most important test in the plan.
#[tokio::test]
async fn with_the_router_disabled_a_project_request_is_exactly_as_it_was() {
    let _env = DispatchEnv::new();
    let state = state_with_project_agent("fudi");
    // No router installed; router_config.enabled == false.
    let resolved = resolve_selection(
        &state,
        AgentSelection {
            project_id: Some("fudi"),
            agent_id: None,
            conversation_id: None,
            debug: false,
        },
        &[user("hola")],
    )
    .await
    .unwrap();
    assert_eq!(
        resolved.agent_id.as_deref(),
        Some("fudi"),
        "disabled router must leave X-Project alone pointing at the project's own agent"
    );
}

#[tokio::test]
async fn an_explicit_agent_header_is_never_routed() {
    let _env = DispatchEnv::new();
    // Everything but the X-Agent gate switched on, so the gate itself is what
    // keeps this request from spending a decision.
    std::env::set_var("ROUTER_ENABLED", "true");
    let state = state_with_project_agent("fudi");
    let fake = FakeRouter::returning(choice_for("fudi::ops"));
    *state.router.write().unwrap() = Some(Arc::new(fake.with_call_counter()));
    let resolved = resolve_selection(
        &state,
        AgentSelection {
            project_id: Some("fudi"),
            agent_id: Some("fudi"),
            conversation_id: None,
            debug: false,
        },
        &[user("hola")],
    )
    .await
    .unwrap();
    assert_eq!(resolved.agent_id.as_deref(), Some("fudi"));
    assert!(
        !fake.was_called(),
        "an explicit header must not spend a decision"
    );
}

#[tokio::test]
async fn a_project_request_without_an_agent_reaches_the_router() {
    let _env = DispatchEnv::new();
    std::env::set_var("ROUTER_ENABLED", "true");
    let state = state_with_project_agent("fudi");
    let fake = FakeRouter::returning(choice_for("fudi::ops"));
    *state.router.write().unwrap() = Some(Arc::new(fake.clone()));
    let resolved = resolve_selection(
        &state,
        AgentSelection {
            project_id: Some("fudi"),
            agent_id: None,
            conversation_id: None,
            debug: false,
        },
        &[user("cancela el pedido 42")],
    )
    .await
    .unwrap();
    assert_eq!(resolved.agent_id.as_deref(), Some("ops"));
    assert!(fake.was_called());
}

#[tokio::test]
async fn a_request_with_no_project_header_is_never_routed() {
    let _env = DispatchEnv::new();
    // Router on and installed; only the missing X-Project header stands
    // between the fake and a decision, so the fake staying silent is the
    // header gate's doing.
    std::env::set_var("ROUTER_ENABLED", "true");
    let state = state_with_project_agent("fudi");
    let fake = FakeRouter::returning(choice_for("fudi::ops"));
    *state.router.write().unwrap() = Some(Arc::new(fake.clone()));
    let resolved = resolve_selection(
        &state,
        AgentSelection {
            project_id: None,
            agent_id: None,
            conversation_id: None,
            debug: false,
        },
        &[user("hola")],
    )
    .await
    .unwrap();
    assert!(resolved.project_id.is_none() && resolved.agent_id.is_none());
    assert!(!fake.was_called());
}

#[tokio::test]
async fn a_down_router_leaves_the_project_general_agent_in_place() {
    let _env = DispatchEnv::new();
    // Consulted — the fake answers with an error, the way an unreachable
    // llama-server does — and every rung must land on the general agent.
    std::env::set_var("ROUTER_ENABLED", "true");
    let state = state_with_project_agent("fudi");
    *state.router.write().unwrap() = Some(Arc::new(FakeRouter::failing()));
    let resolved = resolve_selection(
        &state,
        AgentSelection {
            project_id: Some("fudi"),
            agent_id: None,
            conversation_id: None,
            debug: false,
        },
        &[user("hola")],
    )
    .await
    .unwrap();
    assert_eq!(resolved.agent_id.as_deref(), Some("fudi"));
}

// Review Focus #3: an oversized conversation truncates, never fails.
#[test]
fn state_text_keeps_the_most_recent_turns() {
    let messages: Vec<ChatMessage> = (0..500)
        .map(|i| ChatMessage {
            role: "user".into(),
            content: format!("turn {i} {}", "x".repeat(200)),
        })
        .collect();
    let text = build_state_text(&messages, 2_000);
    assert!(
        text.len() <= 2_000 + 200,
        "budget overshoot must stay within one message"
    );
    assert!(
        text.contains("turn 499"),
        "the newest turn must survive truncation"
    );
    assert!(
        !text.contains("turn 0"),
        "the oldest turn must be the one dropped"
    );
}

// Review Focus #4: hot reload can remove the chosen agent between the
// decision and the use. Resolution must re-check and degrade, not error.
#[tokio::test]
async fn an_agent_removed_by_a_reload_between_decision_and_use_degrades() {
    let _env = DispatchEnv::new();
    std::env::set_var("ROUTER_ENABLED", "true");
    // `state` has fudi::ops and fudi::fudi registered; the router picks ops.
    let state = state_with_project_agent("fudi");
    *state.router.write().unwrap() = Some(Arc::new(FakeRouter::returning(choice_for("fudi::ops"))));

    // Resolution against a registry that no longer holds the winner, as after
    // a hot reload. `AgentRegistry::new()` starts empty, which is enough.
    let reloaded = AgentRegistry::new();
    let resolved = resolve_selection_with(
        &state,
        &reloaded,
        AgentSelection {
            project_id: Some("fudi"),
            agent_id: None,
            conversation_id: None,
            debug: false,
        },
        &[user("cancela el pedido 42")],
    )
    .await
    .unwrap();
    assert_eq!(
        resolved.agent_id.as_deref(),
        Some("fudi"),
        "a vanished winner must fall back to the project's general agent"
    );
}
