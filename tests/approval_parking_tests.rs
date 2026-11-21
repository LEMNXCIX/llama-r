//! Task 2: gate parking, real `WaitingApproval` status and the 202 acknowledgement.
//!
//! These tests drive the real HTTP chat handler, the real `ApprovalGate` and a
//! real SQLite execution store. The agent engine is a small fake on the sacred
//! `AgentEngine` port: it registers a pending in the gate (exactly what a tool
//! bridge does) and blocks on the oneshot until a human decides. That keeps the
//! test free of network and tool plumbing while exercising the whole seam this
//! task changes.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use llama_r::adapters::context::{CurrentModelSelector, NoopCompactor};
use llama_r::adapters::execution::SqliteExecutionStore;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::config::ProviderKind;
use llama_r::context::analyzer::ContextEnricher;
use llama_r::context::store::ContextStore;
use llama_r::domain::agent::{Agent, AgentConfig};
use llama_r::domain::execution::{AgentExecution, ExecutionStatus};
use llama_r::domain::models::{ChatAck, ChatRequest, ChatResponse, ModelInfo};
use llama_r::domain::policy::{ApprovalPolicy, ApprovalRule, PermissionPolicy};
use llama_r::domain::scope::{AgentScope, ToolsAllow};
use llama_r::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult};
use llama_r::ports::execution::ExecutionStore;
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::providers::LLMProvider;
use llama_r::runtime::build_app_state_with_gate;
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::agent_runtime::AgentRuntime;
use llama_r::services::approval::{args_hash, args_hash_hex, ApprovalDecision, ApprovalGate};
use llama_r::services::provider_stack::ProviderStack;
use llama_r::services::skill_manager::SkillManager;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::error::Error;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tower::ServiceExt;
use uuid::Uuid;

const ADMIN_TOKEN: &str = "parking-test-token";
const PARK_GRACE_MS: u64 = 300;

type BoxErr = Box<dyn Error + Send + Sync>;

struct NoopProvider;

#[async_trait]
impl LLMProvider for NoopProvider {
    fn get_base_url(&self) -> String {
        "http://noop".into()
    }
    async fn health_check(&self) -> Result<(), BoxErr> {
        Ok(())
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, BoxErr> {
        Ok(vec![])
    }
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, BoxErr> {
        Err("noop provider".into())
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, BoxErr>> + Send>>, BoxErr> {
        Err("noop provider".into())
    }
}

// ChatStreamEvent is imported through the same module as ChatResponse; keep the
// alias local so the provider impl above stays readable.
use llama_r::domain::models::ChatStreamEvent;

/// An engine that registers one pending approval and blocks on its oneshot, the
/// same shape a real tool bridge has. Deciding resumes this exact turn.
struct ParkingEngine {
    gate: std::sync::Arc<ApprovalGate>,
}

#[async_trait]
impl AgentEngine for ParkingEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        let args = json!({"table": "users"});
        let (_id, rx) = self
            .gate
            .request_for(Some(req.execution_id), "fake__tool", &args);
        match rx.await {
            Ok(ApprovalDecision::Approved) => Ok(AgentRunResult {
                text: "approved after parking".into(),
                tool_calls: 1,
                iterations: 1,
                model: req.model,
            }),
            Ok(ApprovalDecision::Denied { reason }) => Err(format!("denied: {reason}")),
            Err(_) => Err("approval sender dropped".into()),
        }
    }

    async fn run_stream(
        &self,
        _req: AgentRunRequest,
    ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
        Err("streaming not used by these tests".into())
    }
}

/// An engine that finishes immediately — used to prove non-approval turns are
/// still answered synchronously with a normal `ChatResponse`.
struct InstantEngine;

#[async_trait]
impl AgentEngine for InstantEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        Ok(AgentRunResult {
            text: "instant answer".into(),
            tool_calls: 0,
            iterations: 0,
            model: req.model,
        })
    }
    async fn run_stream(
        &self,
        _req: AgentRunRequest,
    ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
        Err("streaming not used by these tests".into())
    }
}

/// Serialise env mutations inside this test binary and restore them on drop.
static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
const MUTATED_ENV: &[&str] = &[
    "ADMIN_TOKEN",
    "PARK_GRACE_MS",
    "APPROVAL_PARK",
    "APPROVAL_TIMEOUT_SECS",
    "ROUTER_ENABLED",
];

struct TestEnv {
    _lock: MutexGuard<'static, ()>,
    previous: Vec<(&'static str, Option<String>)>,
}

impl TestEnv {
    fn fresh() -> Self {
        let lock = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous: Vec<_> = MUTATED_ENV
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect();
        for name in MUTATED_ENV {
            std::env::remove_var(name);
        }
        std::env::set_var("ADMIN_TOKEN", ADMIN_TOKEN);
        std::env::set_var("PARK_GRACE_MS", PARK_GRACE_MS.to_string());
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        for (name, value) in &self.previous {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn scope(permission: PermissionPolicy) -> AgentScope {
    AgentScope {
        agent_id: "t".into(),
        project_id: None,
        mcp_sources: HashSet::new(),
        tools_allow: ToolsAllow::DenyAll,
        local_tools_allow: ToolsAllow::DenyAll,
        rag_sources: HashSet::new(),
        rag_write: llama_r::domain::agent::RagWritePolicy::None,
        max_tool_calls: 4,
        max_iterations: 3,
        timeout_secs: 30,
        permission,
    }
}

fn approval_policy() -> PermissionPolicy {
    PermissionPolicy {
        connections_allow: Vec::new(),
        approvals: vec![ApprovalRule {
            tool_pattern: "fake__tool".into(),
            policy: ApprovalPolicy::Always,
        }],
        auto_threshold: None,
    }
}

fn agent(id: &str, policy: PermissionPolicy) -> (Agent, AgentScope) {
    let config = AgentConfig {
        model: "m".into(),
        policy: policy.clone(),
        ..Default::default()
    };
    (
        Agent {
            id: id.into(),
            project_id: None,
            config,
        },
        scope(policy),
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "one fixture: gate, store, engine and the two registered agents"
)]
async fn test_state(
    gate: std::sync::Arc<ApprovalGate>,
    store: std::sync::Arc<dyn ExecutionStore>,
    engine: std::sync::Arc<dyn AgentEngine>,
) -> std::sync::Arc<llama_r::api::handlers::AppState> {
    let registry = std::sync::Arc::new(AgentRegistry::new());
    let (a, a_scope) = agent("approval-agent", approval_policy());
    registry.register(a, a_scope);
    let (p, p_scope) = agent("plain-agent", PermissionPolicy::default());
    registry.register(p, p_scope);

    let enricher = std::sync::Arc::new(ContextEnricher::new(
        std::sync::Arc::new(ContextStore::new()),
        std::sync::Arc::new(SkillManager::new()),
        "m".into(),
        std::sync::Arc::new(llama_r::providers::registry::ProviderRegistry::builtins()),
        ProviderKind::Ollama,
    ));
    let runtime = AgentRuntime {
        registry: registry.clone(),
        engine,
        history: None,
        rag: None,
        enricher,
        compactor: std::sync::Arc::new(NoopCompactor),
        selector: std::sync::Arc::new(CurrentModelSelector),
    };
    let mut stack = ProviderStack::new(std::sync::Arc::new(NoopProvider), "m");
    stack.agent_runtime = Some(std::sync::Arc::new(runtime));

    build_app_state_with_gate(
        gate,
        stack,
        registry,
        std::sync::Arc::new(SkillManager::new()),
        std::sync::Arc::new(ContextStore::new()),
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        Vec::new(),
        std::sync::Arc::new(StaticMcpRegistry::new()) as std::sync::Arc<dyn McpServerRegistry>,
        None,
        store,
    )
}

async fn call(
    state: &std::sync::Arc<llama_r::api::handlers::AppState>,
    uri: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(body.to_string())).unwrap();
    llama_r::runtime::build_router(state.clone())
        .oneshot(request)
        .await
        .unwrap()
}

fn chat_body(agent_id: &str) -> Value {
    json!({
        "model": agent_id,
        "messages": [{"role": "user", "content": "do it"}]
    })
}

fn execution(id: Uuid) -> AgentExecution {
    let now = chrono::Utc::now();
    AgentExecution {
        id,
        conversation_id: None,
        agent_qualified_id: "approval-agent".into(),
        provider: "ollama".into(),
        model: "m".into(),
        status: ExecutionStatus::Running,
        current_step: 0,
        user_message: Some("do it".into()),
        user_message_hash: args_hash_hex(&Value::String("do it".into())),
        created_at: now,
        updated_at: now,
        finished_at: None,
        error: None,
    }
}

#[tokio::test]
async fn approval_turn_returns_202_with_execution_id() {
    let _env = TestEnv::fresh();
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        SqliteExecutionStore::new(dir.path().join("history.db")).expect("open execution store"),
    );
    let gate = std::sync::Arc::new(ApprovalGate::parked().with_store(store.clone()));
    let engine = std::sync::Arc::new(ParkingEngine { gate: gate.clone() });
    let state = test_state(gate.clone(), store.clone(), engine).await;

    let response = call(
        &state,
        "/chat",
        &[("x-agent", "approval-agent")],
        chat_body("approval-agent"),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "an approval turn must be acknowledged, not held open"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let ack: ChatAck = serde_json::from_slice(&bytes).expect("a ChatAck body");
    let execution_id = Uuid::parse_str(&ack.execution_id).expect("a real execution id");
    let approval_id = Uuid::parse_str(ack.approval_id.as_ref().expect("an approval id"))
        .expect("a real approval id");

    // The turn is parked with a real status, not merely running with a timer.
    let parked = store
        .get(execution_id)
        .await
        .unwrap()
        .expect("execution row");
    assert_eq!(parked.status, ExecutionStatus::WaitingApproval);
    assert!(gate.is_parked_live(approval_id));

    // A human decision resumes the original turn (no reaper involved).
    let args = json!({"table": "users"});
    let decision = call(
        &state,
        &format!("/api/approvals/{approval_id}"),
        &[("x-admin-token", ADMIN_TOKEN)],
        json!({"approve": true, "args_hash": args_hash_hex(&args)}),
    )
    .await;
    assert_eq!(decision.status(), StatusCode::OK);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let current = store
            .get(execution_id)
            .await
            .unwrap()
            .expect("execution row");
        if current.status == ExecutionStatus::Completed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "status stayed {:?}",
            current.status
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !gate.is_parked_live(approval_id),
        "decision unparks the approval"
    );
}

#[tokio::test]
async fn gate_sets_waiting_approval() {
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        SqliteExecutionStore::new(dir.path().join("history.db")).expect("open execution store"),
    );
    let id = Uuid::new_v4();
    store.create(&execution(id)).await.unwrap();

    let gate = ApprovalGate::parked().with_store(store.clone());
    let (approval_id, _rx) = gate.request_for(Some(id), "fake__tool", &json!({"a": 1}));

    let observed = store.get(id).await.unwrap().expect("execution row");
    assert_eq!(
        observed.status,
        ExecutionStatus::WaitingApproval,
        "request_for flips the real turn status"
    );
    assert!(gate.is_parked_live(approval_id));
}

#[tokio::test]
async fn turns_without_approval_rules_stay_synchronous() {
    let _env = TestEnv::fresh();
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        SqliteExecutionStore::new(dir.path().join("history.db")).expect("open execution store"),
    );
    let gate = std::sync::Arc::new(ApprovalGate::default());
    let state = test_state(gate, store, std::sync::Arc::new(InstantEngine)).await;

    let started = Instant::now();
    let response = call(
        &state,
        "/chat",
        &[("x-agent", "plain-agent")],
        chat_body("plain-agent"),
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        elapsed < Duration::from_millis(PARK_GRACE_MS),
        "a non-approval turn must not wait for the parking grace: {elapsed:?}"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: ChatResponse = serde_json::from_slice(&bytes).expect("a normal ChatResponse");
    assert_eq!(body.message.content, "instant answer");
}

#[tokio::test]
async fn decision_resumes_original_turn() {
    let gate = std::sync::Arc::new(ApprovalGate::parked());
    let args = json!({"x": 1});
    let (id, rx) = gate.request_for(None, "fake__tool", &args);
    let waiter = tokio::spawn(async move { rx.await.expect("oneshot delivered") });

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiter.is_finished(), "the bridge is still waiting");

    gate.decide(id, true, args_hash(&args)).unwrap();
    let decision = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("the decision arrives promptly")
        .expect("the task completed");
    assert_eq!(decision, ApprovalDecision::Approved);
    assert!(!gate.is_parked_live(id), "decide unparks before sending");
}

#[tokio::test]
async fn park_wait_does_not_expire_after_timeout() {
    // A short timeout is enough to show expiry in the control gate; the parked
    // gate has no timeout at all, so the same passage of time cannot expire it.
    let expiring = ApprovalGate::new(Duration::from_millis(20));
    let args = json!({"table": "users"});
    let (expiring_id, _expiring_rx) = expiring.request("fake__tool", &args);

    let parked = ApprovalGate::parked();
    assert_eq!(parked.timeout(), None, "park means no bridge deadline");
    let (parked_id, mut parked_rx) = parked.request("fake__tool", &args);

    tokio::time::sleep(Duration::from_millis(80)).await;

    assert_eq!(
        expiring.decide(expiring_id, true, args_hash(&args)),
        Err(llama_r::services::approval::ApprovalError::Expired(
            expiring_id
        )),
        "the control gate still expires on its configured timeout"
    );
    parked
        .decide(parked_id, true, args_hash(&args))
        .expect("a parked approval never expires");
    assert!(matches!(
        parked_rx.try_recv(),
        Ok(ApprovalDecision::Approved)
    ));
}

// ── Task 3: background reaper for orphaned parked approvals ─────────────────

use chrono::Utc;
use llama_r::adapters::execution::NoOpExecutionStore;
use llama_r::ports::execution::{ApprovalDecisionRecord, PendingApprovalRecord};
use llama_r::services::approval_reaper::reap_once;
use std::sync::Arc;

fn waiting_execution(id: Uuid) -> AgentExecution {
    let mut execution = execution(id);
    execution.status = ExecutionStatus::WaitingApproval;
    execution
}

/// A durable pending row bound to one turn. `decision` is what a human recorded
/// while no live oneshot existed (a restart rehydrates it into the gate).
fn pending_record(
    id: Uuid,
    execution_id: Uuid,
    decision: Option<ApprovalDecisionRecord>,
    args: &Value,
) -> PendingApprovalRecord {
    let now = Utc::now();
    PendingApprovalRecord {
        id,
        execution_id: Some(execution_id),
        tool_name: "fake__tool".into(),
        args: args.clone(),
        args_hash: args_hash(args),
        created_at: now,
        decided: decision.is_some(),
        decision: decision.clone(),
        decided_at: decision.map(|_| now),
    }
}

/// A state whose engine is the same gate the reaper inspects, so a resumed turn
/// hits the gate's pre-resolved `request_for` seam rather than asking a human.
async fn reaper_state(
    gate: Arc<ApprovalGate>,
    store: Arc<dyn ExecutionStore>,
) -> Arc<llama_r::api::handlers::AppState> {
    let engine = Arc::new(ParkingEngine { gate: gate.clone() });
    test_state(gate, store, engine).await
}

#[tokio::test]
async fn restart_reaper_resumes_decided_orphan() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteExecutionStore::new(dir.path().join("history.db")).expect("open execution store"),
    ) as Arc<dyn ExecutionStore>;

    let execution_id = Uuid::new_v4();
    store
        .create(&waiting_execution(execution_id))
        .await
        .unwrap();

    let approval_id = Uuid::new_v4();
    let args = json!({"table": "users"});
    store
        .save_pending_approval(&pending_record(
            approval_id,
            execution_id,
            Some(ApprovalDecisionRecord::Approved),
            &args,
        ))
        .await
        .unwrap();

    // Simulate a restart: a fresh gate rehydrates the decision with no live
    // oneshot and an empty parked_live set — the orphan the reaper must resume.
    let gate = Arc::new(ApprovalGate::parked().with_store(store.clone()));
    gate.load_from_store().await.unwrap();
    assert_eq!(
        gate.decision_for(approval_id),
        Some(ApprovalDecision::Approved)
    );
    assert!(!gate.is_parked_live(approval_id));

    let state = reaper_state(gate.clone(), store.clone()).await;

    let reaped = reap_once(&state, Duration::from_secs(5))
        .await
        .expect("reaper pass must not fail");
    assert_eq!(reaped, 1, "the decided orphan is resumed exactly once");

    let resumed = store
        .get(execution_id)
        .await
        .unwrap()
        .expect("execution row");
    assert_eq!(resumed.status, ExecutionStatus::Completed);
    assert_eq!(
        resumed.id, execution_id,
        "the turn is resumed under the same execution id"
    );
    assert_eq!(
        store.list(10).await.unwrap().len(),
        1,
        "the retry did not mint a second execution"
    );
    assert!(
        !gate.is_parked_live(approval_id),
        "unpark is a no-op for an orphan"
    );
}

#[tokio::test]
async fn no_double_resume_when_live() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteExecutionStore::new(dir.path().join("history.db")).expect("open execution store"),
    ) as Arc<dyn ExecutionStore>;

    let execution_id = Uuid::new_v4();
    store
        .create(&waiting_execution(execution_id))
        .await
        .unwrap();
    let approval_id = Uuid::new_v4();
    let args = json!({"table": "users"});
    store
        .save_pending_approval(&pending_record(
            approval_id,
            execution_id,
            Some(ApprovalDecisionRecord::Approved),
            &args,
        ))
        .await
        .unwrap();

    let gate = Arc::new(ApprovalGate::parked().with_store(store.clone()));
    gate.load_from_store().await.unwrap();
    // A live turn is holding this approval open; decide unparks before firing,
    // but a still-parked id must never be claimed by the reaper.
    gate.park(approval_id);

    let state = reaper_state(gate, store.clone()).await;
    let reaped = reap_once(&state, Duration::from_secs(5)).await.unwrap();
    assert_eq!(reaped, 0, "a live parked approval is never reaped");
    assert_eq!(
        store.get(execution_id).await.unwrap().unwrap().status,
        ExecutionStatus::WaitingApproval,
        "the live turn is left untouched"
    );
}

#[tokio::test]
async fn deny_and_hash_mismatch_are_named_errors() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteExecutionStore::new(dir.path().join("history.db")).expect("open execution store"),
    ) as Arc<dyn ExecutionStore>;

    let denied_exec = Uuid::new_v4();
    let mismatch_exec = Uuid::new_v4();
    store.create(&waiting_execution(denied_exec)).await.unwrap();
    store
        .create(&waiting_execution(mismatch_exec))
        .await
        .unwrap();

    let args = json!({"table": "users"});
    store
        .save_pending_approval(&pending_record(
            Uuid::new_v4(),
            denied_exec,
            Some(ApprovalDecisionRecord::Denied {
                reason: "no".into(),
            }),
            &args,
        ))
        .await
        .unwrap();
    let mismatch_id = Uuid::new_v4();
    store
        .save_pending_approval(&pending_record(mismatch_id, mismatch_exec, None, &args))
        .await
        .unwrap();

    let gate = Arc::new(ApprovalGate::parked().with_store(store.clone()));
    gate.load_from_store().await.unwrap();

    // A tampered argument hash is a named error and records no decision at all,
    // so neither it nor a denial is a grant the reaper may act on.
    assert_eq!(
        gate.decide(mismatch_id, true, args_hash(&json!({"table": "orders"}))),
        Err(llama_r::services::approval::ApprovalError::ArgsChanged),
    );
    assert_eq!(gate.decision_for(mismatch_id), None);

    let state = reaper_state(gate, store.clone()).await;
    let reaped = reap_once(&state, Duration::from_secs(5)).await.unwrap();
    assert_eq!(reaped, 0, "neither a denial nor a hash mismatch is a grant");
    assert_eq!(
        store.get(denied_exec).await.unwrap().unwrap().status,
        ExecutionStatus::WaitingApproval
    );
    assert_eq!(
        store.get(mismatch_exec).await.unwrap().unwrap().status,
        ExecutionStatus::WaitingApproval
    );
}

#[tokio::test]
async fn unknown_or_failed_store_is_best_effort() {
    let gate = Arc::new(ApprovalGate::parked());
    let store: Arc<dyn ExecutionStore> = Arc::new(NoOpExecutionStore);
    let state = reaper_state(gate, store).await;

    // A degraded/no store lists nothing: one pass is a clean no-op, never a
    // panic and never a failed boot.
    let reaped = reap_once(&state, Duration::from_secs(5)).await.unwrap();
    assert_eq!(reaped, 0);
}
