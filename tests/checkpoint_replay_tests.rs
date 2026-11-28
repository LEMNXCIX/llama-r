//! Fase 10-C4 Task 1: real checkpoints and the `current_step` cursor.
//!
//! A tool call observed by the shared idempotency guard must leave a pair of
//! events behind: `ToolCallRequested` before dispatch and `CheckpointCreated`
//! after it (including on the replay branch). Both carry the canonical
//! `args_hash` — never raw arguments or tool results — and each checkpoint
//! advances the turn's `current_step`. Store failures are warnings: a tool call
//! still runs and still returns its result.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use llama_r::adapters::execution::NoOpExecutionStore;
use llama_r::adapters::rig_engine::operations::{run_idempotent, OperationContext};
use llama_r::domain::execution::{AgentExecution, EventKind, ExecutionEvent, ExecutionStatus};
use llama_r::ports::execution::{ApprovalDecisionRecord, ExecutionStore, PendingApprovalRecord};
use llama_r::services::redaction::args_hash_hex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
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

    async fn list(&self, _limit: usize) -> Result<Vec<AgentExecution>, String> {
        Ok(Vec::new())
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
