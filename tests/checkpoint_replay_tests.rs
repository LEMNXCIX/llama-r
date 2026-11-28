//! Fase 10-C4 Task 1: real checkpoints and the `current_step` cursor.
//!
//! A tool call observed by the shared idempotency guard must leave a pair of
//! events behind: `ToolCallRequested` before dispatch and `CheckpointCreated`
//! after it (including on the replay branch). Both carry the canonical
//! `args_hash` — never raw arguments or tool results — and each checkpoint
//! advances the turn's `current_step`. Store failures are warnings: a tool call
//! still runs and still returns its result.
//!
//! Task 3 adds the end-to-end proof: a real first turn produces the checkpoint,
//! a forced `Interrupted` turn is retried through `POST /api/executions/:id/retry`
//! with `from_step`, and the same engine sees a resume note while its tool is
//! replayed instead of executed a second time.

#![allow(clippy::await_holding_lock)] // the env lock is held for the whole test by design

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use llama_r::adapters::context::{CurrentModelSelector, NoopCompactor};
use llama_r::adapters::execution::NoOpExecutionStore;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::adapters::rig_engine::operations::{run_idempotent, OperationContext};
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
use llama_r::services::approval::ApprovalGate;
use llama_r::services::provider_stack::ProviderStack;
use llama_r::services::redaction::args_hash_hex;
use llama_r::services::skill_manager::SkillManager;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tower::ServiceExt;
use uuid::Uuid;

fn sample_execution() -> AgentExecution {
    let now = Utc::now();
    AgentExecution {
        id: Uuid::new_v4(),
        conversation_id: None,
        agent_qualified_id: "demo".into(),
        provider: "ollama".into(),
        model: "llama3".into(),
        status: ExecutionStatus::Running,
        current_step: 0,
        user_message: Some("query the database".into()),
        user_message_hash: "ab".repeat(32),
        created_at: now,
        updated_at: now,
        finished_at: None,
        error: None,
    }
}

/// In-memory store that keeps the same first-result-wins and append-only
/// semantics as the port, so the checkpoint payload can be inspected verbatim
/// (a real SQLite store would redact on write, hiding a bridge-level leak).
#[derive(Default)]
struct FakeStore {
    operations: Mutex<HashMap<String, Value>>,
    events: Mutex<Vec<ExecutionEvent>>,
    executions: Mutex<HashMap<Uuid, AgentExecution>>,
    pendings: Mutex<HashMap<Uuid, PendingApprovalRecord>>,
    fail_writes: AtomicBool,
    step_writes: AtomicU32,
}

impl FakeStore {
    fn events(&self) -> Vec<ExecutionEvent> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait]
impl ExecutionStore for FakeStore {
    async fn create(&self, execution: &AgentExecution) -> Result<(), String> {
        self.executions
            .lock()
            .unwrap()
            .insert(execution.id, execution.clone());
        Ok(())
    }

    async fn append_event(
        &self,
        _execution_id: Uuid,
        event: &ExecutionEvent,
    ) -> Result<(), String> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err("store unavailable".into());
        }
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }

    async fn set_status(
        &self,
        execution_id: Uuid,
        status: ExecutionStatus,
        error: Option<&str>,
    ) -> Result<(), String> {
        if let Some(execution) = self.executions.lock().unwrap().get_mut(&execution_id) {
            execution.status = status;
            execution.error = error.map(str::to_string);
        }
        Ok(())
    }

    async fn set_current_step(&self, execution_id: Uuid, step: u32) -> Result<(), String> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err("store unavailable".into());
        }
        self.step_writes.fetch_add(1, Ordering::SeqCst);
        if let Some(execution) = self.executions.lock().unwrap().get_mut(&execution_id) {
            execution.current_step = step;
        }
        Ok(())
    }

    async fn get(&self, execution_id: Uuid) -> Result<Option<AgentExecution>, String> {
        Ok(self.executions.lock().unwrap().get(&execution_id).cloned())
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

    async fn get_events(&self, execution_id: Uuid) -> Result<Vec<ExecutionEvent>, String> {
        Ok(self
            .events()
            .into_iter()
            .filter(|event| event.execution_id == execution_id)
            .collect())
    }

    async fn record_operation(&self, operation_id: &str, result: &Value) -> Result<(), String> {
        self.operations
            .lock()
            .unwrap()
            .entry(operation_id.to_string())
            .or_insert_with(|| result.clone());
        Ok(())
    }

    async fn get_operation(&self, operation_id: &str) -> Result<Option<Value>, String> {
        Ok(self.operations.lock().unwrap().get(operation_id).cloned())
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
            .ok_or_else(|| format!("no pending approval with id {id}"))?;
        record.decided = true;
        record.decision = Some(decision);
        record.decided_at = Some(decided_at);
        Ok(())
    }
}

fn events_of(store: &FakeStore, kind: EventKind) -> Vec<ExecutionEvent> {
    store
        .events()
        .into_iter()
        .filter(|event| event.kind == kind)
        .collect()
}

#[cfg(feature = "history")]
#[tokio::test]
async fn set_current_step_persists_and_reads_back() {
    use llama_r::adapters::execution::SqliteExecutionStore;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let store = SqliteExecutionStore::new(dir.path().join("history.db")).unwrap();
    let execution = sample_execution();
    store.create(&execution).await.unwrap();
    assert_eq!(
        store.get(execution.id).await.unwrap().unwrap().current_step,
        0
    );

    store.set_current_step(execution.id, 7).await.unwrap();
    assert_eq!(
        store.get(execution.id).await.unwrap().unwrap().current_step,
        7
    );

    // A later write replaces the cursor, it does not accumulate or insert a row.
    store.set_current_step(execution.id, 9).await.unwrap();
    assert_eq!(
        store.get(execution.id).await.unwrap().unwrap().current_step,
        9
    );
    assert_eq!(store.list(10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn noop_set_current_step_is_a_harmless_noop() {
    let store: Arc<dyn ExecutionStore> = Arc::new(NoOpExecutionStore);
    assert!(store.set_current_step(Uuid::new_v4(), 5).await.is_ok());
    assert!(store.get(Uuid::new_v4()).await.unwrap().is_none());
}

#[tokio::test]
async fn a_first_tool_call_emits_request_and_checkpoint_and_advances_the_step() {
    let store = Arc::new(FakeStore::default());
    let execution = sample_execution();
    store.create(&execution).await.unwrap();
    let ctx = OperationContext::new(execution.id, store.clone());
    let args = json!({"table": "users"});

    let result = run_idempotent(Some(&ctx), "db__query", &args, async {
        Ok::<_, String>("rows".into())
    })
    .await
    .unwrap();
    assert_eq!(result, "rows");

    let requested = events_of(&store, EventKind::ToolCallRequested);
    assert_eq!(requested.len(), 1, "one request event per tool call");
    assert_eq!(requested[0].payload["step"], 0);
    assert_eq!(requested[0].payload["next_step_index"], 1);
    assert_eq!(requested[0].payload["tool"], "db__query");
    assert_eq!(requested[0].payload["args_hash"], args_hash_hex(&args));
    assert!(
        requested[0].payload.get("arguments").is_none(),
        "a request event must never carry raw arguments"
    );

    let checkpoints = events_of(&store, EventKind::CheckpointCreated);
    assert_eq!(checkpoints.len(), 1, "one checkpoint per tool call");
    let payload = &checkpoints[0].payload;
    assert_eq!(payload["step"], 0);
    assert_eq!(payload["next_step_index"], 1);
    assert_eq!(payload["tool"], "db__query");
    assert_eq!(payload["args_hash"], args_hash_hex(&args));
    assert_eq!(payload["replayed"], false);
    assert_eq!(payload["status"], "completed");
    assert!(payload.get("arguments").is_none());
    assert!(payload.get("result").is_none());

    assert_eq!(
        store.get(execution.id).await.unwrap().unwrap().current_step,
        1,
        "the cursor points at the next step after a checkpoint"
    );
    assert_eq!(store.step_writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_replayed_tool_call_emits_a_checkpoint_marked_replayed() {
    let store = Arc::new(FakeStore::default());
    let execution = sample_execution();
    store.create(&execution).await.unwrap();
    let ctx = OperationContext::new(execution.id, store.clone());
    let args = json!({"table": "users"});
    let calls = Arc::new(AtomicU32::new(0));

    let run = || {
        let calls = calls.clone();
        async move {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, String>(format!("result-{n}"))
        }
    };

    assert_eq!(
        run_idempotent(Some(&ctx), "db__query", &args, run())
            .await
            .unwrap(),
        "result-0"
    );
    assert_eq!(
        run_idempotent(Some(&ctx), "db__query", &args, run())
            .await
            .unwrap(),
        "result-0",
        "the stored result is replayed, not recomputed"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let requested = events_of(&store, EventKind::ToolCallRequested);
    assert_eq!(requested.len(), 2, "both model requests are observed");
    let checkpoints = events_of(&store, EventKind::CheckpointCreated);
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(checkpoints[0].payload["replayed"], false);
    assert_eq!(checkpoints[0].payload["step"], 0);
    assert_eq!(checkpoints[1].payload["replayed"], true);
    assert_eq!(checkpoints[1].payload["step"], 1);
    assert_eq!(checkpoints[1].payload["next_step_index"], 2);
    assert_eq!(checkpoints[1].payload["status"], "completed");
    assert_eq!(
        store.get(execution.id).await.unwrap().unwrap().current_step,
        2,
        "a replay still advances the logical step cursor"
    );
}

#[tokio::test]
async fn a_checkpoint_never_leaks_an_api_key_argument() {
    let store = Arc::new(FakeStore::default());
    let execution = sample_execution();
    store.create(&execution).await.unwrap();
    let ctx = OperationContext::new(execution.id, store.clone());
    let args = json!({
        "table": "users",
        "api_key": "sk-CHECKPOINT-SECRET-9f8e7d",
        "nested": {"Authorization": "Bearer sk-CHECKPOINT-SECRET-9f8e7d"}
    });

    run_idempotent(Some(&ctx), "db__query", &args, async {
        Ok::<_, String>("ok".into())
    })
    .await
    .unwrap();

    let all = serde_json::to_string(&store.events()).unwrap();
    assert!(
        !all.contains("sk-CHECKPOINT-SECRET-9f8e7d"),
        "no bridge event may carry a credential, even in an in-memory store"
    );
    let checkpoint = &events_of(&store, EventKind::CheckpointCreated)[0].payload;
    assert_eq!(checkpoint["args_hash"], args_hash_hex(&args));
    assert!(checkpoint.get("api_key").is_none());
    assert!(checkpoint.get("arguments").is_none());
    assert!(checkpoint.get("nested").is_none());
}

#[tokio::test]
async fn a_failed_tool_call_emits_a_failed_checkpoint() {
    let store = Arc::new(FakeStore::default());
    let execution = sample_execution();
    store.create(&execution).await.unwrap();
    let ctx = OperationContext::new(execution.id, store.clone());
    let args = json!({"table": "users"});

    let err = run_idempotent(Some(&ctx), "db__query", &args, async {
        Err::<String, String>("tool exploded".into())
    })
    .await
    .unwrap_err();
    assert_eq!(err, "tool exploded");

    let checkpoints = events_of(&store, EventKind::CheckpointCreated);
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].payload["status"], "failed");
    assert_eq!(checkpoints[0].payload["replayed"], false);
    assert_eq!(checkpoints[0].payload["args_hash"], args_hash_hex(&args));
    assert!(
        checkpoints[0].payload.get("result").is_none(),
        "a failure checkpoint must not carry error text or results"
    );
    assert_eq!(
        store.get(execution.id).await.unwrap().unwrap().current_step,
        1
    );
}

#[tokio::test]
async fn store_errors_are_warnings_and_never_fail_a_tool_call() {
    let store = Arc::new(FakeStore::default());
    let execution = sample_execution();
    store.create(&execution).await.unwrap();
    store.fail_writes.store(true, Ordering::SeqCst);
    let ctx = OperationContext::new(execution.id, store.clone());

    let result = run_idempotent(Some(&ctx), "db__query", &json!({"table": "users"}), async {
        Ok::<_, String>("still works".into())
    })
    .await
    .unwrap();
    assert_eq!(result, "still works");
    assert!(
        store.events().is_empty(),
        "failed appends are simply skipped"
    );
    assert_eq!(store.step_writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        store.get(execution.id).await.unwrap().unwrap().current_step,
        0
    );
}

// ── Task 3: end-to-end checkpoint-aware retry ────────────────────────────────
//
// The unit tests above prove the bridge emits checkpoints; this section proves
// the whole loop: a fresh turn runs a guarded tool once and records a real
// checkpoint, a crash forces `Interrupted`, and the HTTP retry rebuilds the
// turn with a resume note while the stored tool result is replayed.

const TEST_ADMIN_TOKEN: &str = "test-c4-admin-token";

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

/// Engine that mirrors the real bridge seam: one guarded tool per turn, with a
/// counter that proves whether the tool body actually ran. It also records the
/// user message it received (the resume note is prefixed there by Task 2) and
/// every execution id it was handed.
struct RecordingEngine {
    store: Arc<FakeStore>,
    calls: Arc<AtomicU32>,
    messages: Arc<Mutex<Vec<String>>>,
    seen_ids: Arc<Mutex<Vec<Uuid>>>,
}

#[async_trait]
impl AgentEngine for RecordingEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        self.messages.lock().unwrap().push(req.user_message.clone());
        self.seen_ids.lock().unwrap().push(req.execution_id);
        let ctx = OperationContext::new(req.execution_id, self.store.clone());
        let calls = self.calls.clone();
        let result = run_idempotent(Some(&ctx), "db__write", &json!({"row": 1}), async move {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, String>(format!("write-{n}"))
        })
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

/// `LLAMA_R_DIR` is process-global: serialise every test in this binary that
/// changes it, so an agent install cannot race with another test's reload.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn test_state(engine: Arc<dyn AgentEngine>, store: Arc<FakeStore>) -> (TempDir, Arc<AppState>) {
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
        selector: Arc::new(CurrentModelSelector),
    };
    let mut stack = ProviderStack::new(Arc::new(StubProvider), "model-a");
    stack.provider_name = "ollama".into();
    stack.agent_runtime = Some(Arc::new(runtime));
    let state = build_app_state_with_gate(
        Arc::new(ApprovalGate::default()),
        stack,
        registry,
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        Arc::new(Mutex::new(VecDeque::new())),
        vec![],
        Arc::new(StaticMcpRegistry::new()),
        None,
        store,
        llama_r::config::Config {
            admin_token: Some(TEST_ADMIN_TOKEN.to_string()),
            ..Default::default()
        },
        None,
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

fn retry_request(uri: &str, from_step: u32) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("x-debug", "true")
        .header("x-admin-token", TEST_ADMIN_TOKEN)
        .header("content-type", "application/json")
        .body(Body::from(json!({"from_step": from_step}).to_string()))
        .unwrap()
}

#[tokio::test]
async fn e2e_retry_from_a_checkpoint_keeps_the_note_and_replays_the_tool() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let store = Arc::new(FakeStore::default());
    let calls = Arc::new(AtomicU32::new(0));
    let messages = Arc::new(Mutex::new(Vec::new()));
    let seen_ids = Arc::new(Mutex::new(Vec::new()));
    let (_dir, state) = test_state(
        Arc::new(RecordingEngine {
            store: store.clone(),
            calls: calls.clone(),
            messages: messages.clone(),
            seen_ids: seen_ids.clone(),
        }),
        store.clone(),
    );

    // First turn: the guarded tool really runs once and leaves a real
    // checkpoint plus a cached operation result behind.
    execute_chat_with_id(&state, turn_request(), selection(), None)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the tool ran once");
    let turns = store.list(10).await.unwrap();
    assert_eq!(turns.len(), 1, "one durable turn was created");
    let id = turns[0].id;
    assert_eq!(turns[0].status, ExecutionStatus::Completed);
    assert_eq!(
        turns[0].current_step, 1,
        "the first checkpoint advanced the cursor from 0 to 1"
    );
    assert_eq!(events_of(&store, EventKind::CheckpointCreated).len(), 1);

    // Simulate a crash after that tool completed, then retry through the real
    // admin endpoint asking to resume from step 0.
    store
        .set_status(id, ExecutionStatus::Interrupted, None)
        .await
        .unwrap();
    let router = build_router(state);
    let uri = format!("/api/executions/{id}/retry");
    let (status, body) = send(&router, retry_request(&uri, 0)).await;
    assert_eq!(status, StatusCode::OK, "retry body: {body}");

    // The note is built from the checkpoint the first turn itself emitted and
    // reaches the model as a prefix of the user message (Task 2's seam).
    {
        let received = messages.lock().unwrap();
        assert_eq!(received.len(), 2, "two engine turns in total");
        let note = &received[1];
        assert!(note.contains("[Resume]"), "note: {note}");
        assert!(note.contains("paso 0"), "note: {note}");
        assert!(note.contains("db__write"), "note: {note}");
        assert!(note.contains("write-0"), "note: {note}");
        assert!(note.contains("Continúa desde el paso 1"), "note: {note}");
        assert!(note.ends_with("do the write"), "note: {note}");
    }

    // Idempotency: the second turn saw a resume note but never ran the tool.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the tool was replayed, not re-executed"
    );
    assert_eq!(
        seen_ids.lock().unwrap().as_slice(),
        [id, id],
        "both turns used the original execution id"
    );
    assert!(
        events_of(&store, EventKind::ExecutionResumed)
            .iter()
            .any(|event| event.execution_id == id),
        "the retry reopened the original turn"
    );

    let completed = events_of(&store, EventKind::ToolCallCompleted);
    assert_eq!(completed.len(), 2, "one completion event per guarded call");
    assert_eq!(completed[0].payload["replayed"], false);
    assert_eq!(
        completed[1].payload["replayed"], true,
        "the retried call is recorded as a replay: {:?}",
        completed[1].payload
    );

    let checkpoints = events_of(&store, EventKind::CheckpointCreated);
    assert_eq!(checkpoints.len(), 2, "the replay also checkpoints");
    assert_eq!(checkpoints[1].payload["replayed"], true);
    assert_eq!(checkpoints[1].payload["step"], 1);

    let replayed = store.list(10).await.unwrap();
    assert_eq!(replayed.len(), 1, "no orphan row with a fresh id");
    assert_eq!(replayed[0].id, id);
    assert_eq!(replayed[0].status, ExecutionStatus::Completed);
    assert!(
        replayed[0].current_step > 0,
        "the cursor advanced past the replayed checkpoint"
    );
    assert_eq!(replayed[0].current_step, 2);

    // A legacy row without a stored message is still refused with 403 (the
    // canonical test lives in `execution_api_tests.rs`; this keeps the E2E rig
    // honest about the unchanged refusal).
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
        retry_request(&format!("/api/executions/{}/retry", legacy.id), 0),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "legacy body: {body}");
    assert!(
        body.to_string().to_lowercase().contains("message"),
        "the refusal names the missing message: {body}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a refused legacy retry spends no tool call"
    );
}
