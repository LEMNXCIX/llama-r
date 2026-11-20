//! Fase 7 Task 6: execution observability endpoints and turn-level retry.
//!
//! The load-bearing proof is [the_load_bearing_retry_reuses_execution_id]:
//! a retry must re-run the turn under the ORIGINAL `execution_id`, so the shared
//! `operation_id` guard replays an already-recorded tool result instead of
//! repeating its side effect (the fake tool's counter stays at 1).

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use llama_r::adapters::context::NoopCompactor;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::adapters::rig_engine::operations::{operation_id, OperationContext};
use llama_r::api::chat_core::execute_chat_with_id;
use llama_r::api::handlers::AppState;
use llama_r::api::route_resolution::AgentSelection;
use llama_r::config::ProviderKind;
use llama_r::context::analyzer::ContextEnricher;
use llama_r::context::store::ContextStore;
use llama_r::domain::agent::AgentConfig;
use llama_r::domain::execution::{AgentExecution, EventKind, ExecutionEvent, ExecutionStatus};
use llama_r::domain::models::{ChatMessage, ChatRequest, ChatResponse, ModelInfo};
use llama_r::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult};
use llama_r::ports::execution::{ApprovalDecisionRecord, ExecutionStore, PendingApprovalRecord};
use llama_r::providers::registry::ProviderRegistry;
use llama_r::providers::LLMProvider;
use llama_r::runtime::{build_app_state_with_gate, build_router};
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::approval::{args_hash, ApprovalDecision, ApprovalGate};
use llama_r::services::provider_stack::ProviderStack;
use llama_r::services::skill_manager::SkillManager;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tower::ServiceExt;
use uuid::Uuid;

// ── in-memory store (works under every feature combination) ────────────────

#[derive(Default)]
struct FakeStore {
    operations: Mutex<HashMap<String, Value>>,
    events: Mutex<Vec<ExecutionEvent>>,
    executions: Mutex<HashMap<Uuid, AgentExecution>>,
    pendings: Mutex<HashMap<Uuid, PendingApprovalRecord>>,
}

#[async_trait]
impl ExecutionStore for FakeStore {
    async fn create(&self, execution: &AgentExecution) -> Result<(), String> {
        if self
            .executions
            .lock()
            .unwrap()
            .insert(execution.id, execution.clone())
            .is_some()
        {
            return Err(format!("execution {} already exists", execution.id));
        }
        Ok(())
    }
    async fn append_event(&self, _id: Uuid, event: &ExecutionEvent) -> Result<(), String> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
    async fn set_status(
        &self,
        id: Uuid,
        status: ExecutionStatus,
        error: Option<&str>,
    ) -> Result<(), String> {
        if let Some(e) = self.executions.lock().unwrap().get_mut(&id) {
            e.status = status;
            e.error = error.map(str::to_string);
        }
        Ok(())
    }
    async fn get(&self, id: Uuid) -> Result<Option<AgentExecution>, String> {
        Ok(self.executions.lock().unwrap().get(&id).cloned())
    }
    async fn list_interrupted(&self) -> Result<Vec<AgentExecution>, String> {
        Ok(Vec::new())
    }
    async fn list(&self, limit: usize) -> Result<Vec<AgentExecution>, String> {
        Ok(self
            .executions
            .lock()
            .unwrap()
            .values()
            .take(limit)
            .cloned()
            .collect())
    }
    async fn get_events(&self, id: Uuid) -> Result<Vec<ExecutionEvent>, String> {
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.execution_id == id)
            .cloned()
            .collect())
    }
    async fn record_operation(&self, id: &str, result: &Value) -> Result<(), String> {
        self.operations
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert_with(|| result.clone());
        Ok(())
    }
    async fn get_operation(&self, id: &str) -> Result<Option<Value>, String> {
        Ok(self.operations.lock().unwrap().get(id).cloned())
    }
    async fn save_pending_approval(&self, record: &PendingApprovalRecord) -> Result<(), String> {
        self.pendings
            .lock()
            .unwrap()
            .insert(record.id, record.clone());
        Ok(())
    }
    async fn load_pending_approvals(&self) -> Result<Vec<PendingApprovalRecord>, String> {
        Ok(self.pendings.lock().unwrap().values().cloned().collect())
    }
    async fn decide_pending(
        &self,
        id: Uuid,
        decision: ApprovalDecisionRecord,
        decided_at: DateTime<Utc>,
    ) -> Result<(), String> {
        let mut pendings = self.pendings.lock().unwrap();
        let record = pendings
            .get_mut(&id)
            .ok_or_else(|| format!("no pending approval {id}"))?;
        record.decided = true;
        record.decision = Some(decision);
        record.decided_at = Some(decided_at);
        Ok(())
    }
}

// ── stub provider and a spy engine ─────────────────────────────────────────

struct StubProvider;

#[async_trait]
impl LLMProvider for StubProvider {
    fn get_base_url(&self) -> String {
        "http://stub".into()
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
        Ok(ChatResponse {
            model: "model-a".into(),
            created_at: Utc::now().to_rfc3339(),
            message: ChatMessage {
                role: "assistant".into(),
                content: "direct".into(),
            },
            done: true,
            usage: None,
            served_by: None,
            debug_prompt: None,
            debug_route: None,
            conversation_id: None,
        })
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<
        std::pin::Pin<
            Box<
                dyn tokio_stream::Stream<
                        Item = Result<
                            llama_r::domain::models::ChatStreamEvent,
                            Box<dyn Error + Send + Sync>,
                        >,
                    > + Send,
            >,
        >,
        Box<dyn Error + Send + Sync>,
    > {
        Err("no stream".into())
    }
}

/// An engine that mirrors the real bridge seam: it guards one "tool" with the
/// shared `run_idempotent` helper, keyed by the request's `execution_id`. It also
/// records every execution id it was handed, so a test can prove a retry reused
/// the original id rather than minting a new one.
struct SpyEngine {
    store: Arc<FakeStore>,
    calls: Arc<AtomicU32>,
    seen_ids: Arc<Mutex<Vec<Uuid>>>,
}

#[async_trait]
impl AgentEngine for SpyEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        self.seen_ids.lock().unwrap().push(req.execution_id);
        let ctx = OperationContext::new(req.execution_id, self.store.clone());
        let calls = self.calls.clone();
        let result = llama_r::adapters::rig_engine::operations::run_idempotent(
            Some(&ctx),
            "db__write",
            &json!({"row": 1}),
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                Ok::<String, String>(format!("write-{n}"))
            },
        )
        .await?;
        Ok(AgentRunResult {
            text: result,
            tool_calls: 1,
            iterations: 2,
            model: req.model,
        })
    }

    async fn run_stream(
        &self,
        req: AgentRunRequest,
    ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
        let result = self.run(req).await?;
        let (tx, rx) = mpsc::channel(2);
        let _ = tx.send(AgentRunEvent::Completed { result }).await;
        Ok(rx)
    }
}

// ── rig ────────────────────────────────────────────────────────────────────

fn test_enricher() -> Arc<ContextEnricher> {
    Arc::new(ContextEnricher::new(
        Arc::new(ContextStore::new()),
        Arc::new(SkillManager::new()),
        "model-a".into(),
        Arc::new(ProviderRegistry::default()),
        ProviderKind::Ollama,
    ))
}

fn install_agent(registry: &AgentRegistry, project: &str, agent: &str) {
    let dir = llama_r::core::paths::get_project_agents_dir(project);
    std::fs::create_dir_all(&dir).unwrap();
    let config = AgentConfig {
        name: "Agent".into(),
        model: "model-a".into(),
        system_prompt: "you are helpful".into(),
        context_project: Some(project.into()),
        ..Default::default()
    };
    std::fs::write(
        dir.join(format!("{agent}.toml")),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    registry.reload_all(&[]).unwrap();
}

fn test_state(
    engine: Arc<dyn AgentEngine>,
    store: Arc<FakeStore>,
    gate: Arc<ApprovalGate>,
) -> (TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());
    let registry = Arc::new(AgentRegistry::new());
    install_agent(&registry, "proj", "agent");
    let runtime = llama_r::services::agent_runtime::AgentRuntime {
        registry: registry.clone(),
        engine,
        history: None,
        rag: None,
        enricher: test_enricher(),
        compactor: Arc::new(NoopCompactor),
        selector: Arc::new(llama_r::adapters::context::CurrentModelSelector),
    };
    let mut stack = ProviderStack::new(Arc::new(StubProvider), "model-a");
    stack.provider_name = "ollama".into();
    stack.agent_runtime = Some(Arc::new(runtime));
    let state = build_app_state_with_gate(
        gate,
        stack,
        registry,
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        Arc::new(Mutex::new(VecDeque::new())),
        vec![],
        Arc::new(StaticMcpRegistry::new()),
        None,
        store,
    );
    (dir, state)
}

fn selection() -> AgentSelection<'static> {
    AgentSelection {
        project_id: Some("proj"),
        agent_id: Some("agent"),
        conversation_id: None,
        debug: false,
    }
}

fn turn_request() -> ChatRequest {
    ChatRequest {
        model: "model-a".into(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: "do the write".into(),
        }],
        stream: false,
    }
}

async fn send(router: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, value)
}

fn debug_req(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("x-debug", "true")
        .body(Body::empty())
        .unwrap()
}

// ── tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn observability_endpoints_gate_on_debug_and_redact_secrets() {
    let store = Arc::new(FakeStore::default());
    // A structured message: the store redacts secret-bearing fields on write,
    // and the API must never echo them back.
    let execution = AgentExecution {
        id: Uuid::new_v4(),
        conversation_id: None,
        agent_qualified_id: "proj::agent".into(),
        provider: "ollama".into(),
        model: "model-a".into(),
        status: ExecutionStatus::Interrupted,
        current_step: 2,
        user_message: Some(json!({"task": "go", "api_key": "sk-top-secret"}).to_string()),
        user_message_hash: "ab".repeat(32),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        finished_at: None,
        error: Some("crashed".into()),
    };
    store.create(&execution).await.unwrap();
    store
        .append_event(
            execution.id,
            &ExecutionEvent::new(execution.id, EventKind::Started, json!({})),
        )
        .await
        .unwrap();
    let (_dir, state) = test_state(
        Arc::new(SpyEngine {
            store: store.clone(),
            calls: Arc::new(AtomicU32::new(0)),
            seen_ids: Arc::new(Mutex::new(vec![])),
        }),
        store.clone(),
        Arc::new(ApprovalGate::default()),
    );
    let router = build_router(state);

    // No X-Debug -> 403 before anything else.
    let (status, _) = send(
        &router,
        Request::builder()
            .uri("/api/executions")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, body) = send(&router, debug_req("GET", "/api/executions")).await;
    assert_eq!(status, StatusCode::OK);
    let list = body.as_array().expect("list is an array");
    assert_eq!(list.len(), 1);
    assert!(serde_json::to_string(&list).unwrap().contains("[REDACTED]"));
    assert!(!serde_json::to_string(&list)
        .unwrap()
        .contains("sk-top-secret"));

    let uri = format!("/api/executions/{}", execution.id);
    let (status, body) = send(&router, debug_req("GET", &uri)).await;
    assert_eq!(status, StatusCode::OK);
    let detail = body.to_string();
    assert!(detail.contains("\"execution\""));
    assert!(detail.contains("\"events\""));
    assert!(detail.contains("Started"));
    assert!(
        !detail.contains("sk-top-secret"),
        "no secret leaks in the detail"
    );

    let unknown = format!("/api/execuations/{}", Uuid::new_v4());
    let (status, _) = send(&router, debug_req("GET", &unknown)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a typo'd route is 404 too");
    let unknown = format!("/api/executions/{}", Uuid::new_v4());
    let (status, _) = send(&router, debug_req("GET", &unknown)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_load_bearing_retry_reuses_execution_id_and_does_not_reexecute_tools() {
    let store = Arc::new(FakeStore::default());
    let calls = Arc::new(AtomicU32::new(0));
    let seen_ids = Arc::new(Mutex::new(vec![]));
    let (_dir, state) = test_state(
        Arc::new(SpyEngine {
            store: store.clone(),
            calls: calls.clone(),
            seen_ids: seen_ids.clone(),
        }),
        store.clone(),
        Arc::new(ApprovalGate::default()),
    );

    // First turn: fresh execution, tool executed once and recorded.
    execute_chat_with_id(&state, turn_request(), selection(), None)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let before = store.list(10).await.unwrap();
    assert_eq!(before.len(), 1);
    let original = before[0].clone();
    assert_eq!(original.status, ExecutionStatus::Completed);

    // Simulate a crash mid-flight: the turn is interrupted and retryable.
    store
        .set_status(original.id, ExecutionStatus::Interrupted, None)
        .await
        .unwrap();

    // Retry through the real HTTP endpoint with the original id.
    let router = build_router(state);
    let uri = format!("/api/executions/{}/retry", original.id);
    let (status, body) = send(&router, debug_req("POST", &uri)).await;
    assert_eq!(status, StatusCode::OK, "retry body: {body}");

    // The tool was NOT re-executed: the operation was replayed under the same id.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the fake tool counter stayed at 1 across the retry"
    );
    assert_eq!(
        seen_ids.lock().unwrap().clone(),
        vec![original.id, original.id],
        "the retry ran under the original execution id, not a new one"
    );

    // Still exactly one execution — no orphan row with a fresh id.
    let after = store.list(10).await.unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, original.id);
    assert_eq!(after[0].status, ExecutionStatus::Completed);
    let events = store.get_events(original.id).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == EventKind::ExecutionResumed),
        "a retry records an ExecutionResumed event: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::ToolCallCompleted && e.payload["replayed"] == true),
        "the retried tool call was recorded as a replay"
    );

    // The operation id is keyed by the original execution id.
    assert!(store
        .get_operation(&operation_id(original.id, "db__write", &json!({"row": 1})))
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn retry_maps_unknown_wrong_status_and_legacy_null_message() {
    let store = Arc::new(FakeStore::default());
    let gate = Arc::new(ApprovalGate::default());
    let (_dir, state) = test_state(
        Arc::new(SpyEngine {
            store: store.clone(),
            calls: Arc::new(AtomicU32::new(0)),
            seen_ids: Arc::new(Mutex::new(vec![])),
        }),
        store.clone(),
        gate.clone(),
    );
    let router = build_router(state);

    // Unknown id -> 404.
    let uri = format!("/api/executions/{}/retry", Uuid::new_v4());
    let (status, _) = send(&router, debug_req("POST", &uri)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Wrong status (Completed is not retryable) -> 409.
    let completed = AgentExecution {
        id: Uuid::new_v4(),
        conversation_id: None,
        agent_qualified_id: "proj::agent".into(),
        provider: "ollama".into(),
        model: "model-a".into(),
        status: ExecutionStatus::Completed,
        current_step: 1,
        user_message: Some("go".into()),
        user_message_hash: "ab".repeat(32),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        finished_at: None,
        error: None,
    };
    store.create(&completed).await.unwrap();
    let (status, body) = send(
        &router,
        debug_req("POST", &format!("/api/executions/{}/retry", completed.id)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");

    // Legacy row with NULL user_message -> 403 with a named message.
    let legacy = AgentExecution {
        id: Uuid::new_v4(),
        conversation_id: None,
        agent_qualified_id: "proj::agent".into(),
        provider: "ollama".into(),
        model: "model-a".into(),
        status: ExecutionStatus::Interrupted,
        current_step: 1,
        user_message: None,
        user_message_hash: "ab".repeat(32),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        finished_at: None,
        error: None,
    };
    store.create(&legacy).await.unwrap();
    let (status, body) = send(
        &router,
        debug_req("POST", &format!("/api/executions/{}/retry", legacy.id)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert!(
        body.to_string().to_lowercase().contains("message"),
        "the refusal names the missing message: {body}"
    );
}

#[tokio::test]
async fn a_decided_approval_from_a_prior_run_is_consumed_by_the_retry() {
    // A pending registered under execution E is decided after a restart. A
    // fresh gate reloads the decision; requesting approval for the SAME tool and
    // args under E consumes it (no new pending, an immediately-approved result).
    let store = Arc::new(FakeStore::default());
    let execution_id = Uuid::new_v4();
    let args = json!({"table": "users"});

    let id = {
        let gate = ApprovalGate::default().with_store(store.clone());
        let (id, _rx) = gate.request_for(Some(execution_id), "db__drop", &args);
        assert_eq!(
            store.load_pending_approvals().await.unwrap()[0].execution_id,
            Some(execution_id)
        );
        id
    }; // owning gate dropped: the oneshot is gone (restart)

    let reloaded = ApprovalGate::default().with_store(store.clone());
    reloaded.load_from_store().await.unwrap();
    reloaded.decide(id, true, args_hash(&args)).unwrap();
    assert_eq!(reloaded.decision_for(id), Some(ApprovalDecision::Approved));

    // The retry's gate sees the persisted decision and consumes it.
    let retry_gate = ApprovalGate::default().with_store(store.clone());
    retry_gate.load_from_store().await.unwrap();
    let before = retry_gate.list_pending().len();
    let (_, rx) = retry_gate.request_for(Some(execution_id), "db__drop", &args);
    let decision = tokio::time::timeout(std::time::Duration::from_millis(200), rx)
        .await
        .expect("the decision must be immediately available, not a fresh pending")
        .expect("the receiver is resolved");
    assert_eq!(decision, ApprovalDecision::Approved);
    assert_eq!(
        retry_gate.list_pending().len(),
        before,
        "no new pending was registered on retry"
    );

    // Scoping: a different execution (or a different tool/args) must NOT reuse
    // it — it registers a fresh pending that blocks.
    let (_, other_rx) = retry_gate.request_for(Some(Uuid::new_v4()), "db__drop", &args);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), other_rx)
            .await
            .is_err(),
        "a different execution must not inherit the decision"
    );
}
