//! Fase 7 Task 4: `execution_id` threading and idempotent tool operations.
//!
//! A retry of the same agent turn must not repeat a tool's side effects. The
//! guard lives in the shared `operations` helper and is used by BOTH bridge
//! kinds; these tests drive the MCP bridge (with a fake tool that counts calls)
//! and the helper directly, plus the no-op degradation path where the store
//! cannot record anything and the guard must be a transparent pass-through.
//!
//! The execution-lifecycle events (Started / ExecutionCompleted) are asserted
//! in `src/api/chat_core/tests.rs`, where the full `AppState` test rig lives.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use llama_r::adapters::execution::NoOpExecutionStore;
use llama_r::adapters::rig_engine::operations::{operation_id, run_idempotent, OperationContext};
use llama_r::domain::execution::{AgentExecution, EventKind, ExecutionEvent, ExecutionStatus};
use llama_r::ports::engine::AgentRunRequest;
use llama_r::ports::execution::{ApprovalDecisionRecord, ExecutionStore, PendingApprovalRecord};
use llama_r::services::approval::{args_hash, ApprovalDecision, ApprovalError, ApprovalGate};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// A tiny in-memory store so the idempotency guard can be tested under every
/// feature combination (the SQLite adapter needs the `history` feature). It
/// keeps the same "first result wins" and append-only semantics as the port,
/// including the durable pending-approval record (with a real decision).
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
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.execution_id == execution_id)
            .cloned()
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
        let mut all: Vec<_> = self.pendings.lock().unwrap().values().cloned().collect();
        all.sort_by_key(|record| record.created_at);
        Ok(all)
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

const EXEC_ID: &str = "11111111-1111-1111-1111-111111111111";

#[test]
fn operation_id_is_deterministic_and_scoped_to_execution_tool_and_args() {
    let execution_id = Uuid::parse_str(EXEC_ID).unwrap();
    let first = operation_id(execution_id, "mail__send", &json!({"b": 2, "a": 1}));
    // Canonical hashing: reordered object keys hash identically.
    assert_eq!(
        first,
        operation_id(execution_id, "mail__send", &json!({"a": 1, "b": 2}))
    );
    assert!(first.starts_with(&format!("{EXEC_ID}:mail__send:")));
    // Different args, tool, or execution each mint a different id.
    assert_ne!(
        first,
        operation_id(execution_id, "mail__send", &json!({"a": 2}))
    );
    assert_ne!(
        first,
        operation_id(execution_id, "mail__delete", &json!({"b": 2, "a": 1}))
    );
    assert_ne!(
        first,
        operation_id(Uuid::new_v4(), "mail__send", &json!({"b": 2, "a": 1}))
    );
}

#[tokio::test]
async fn a_retried_tool_call_reuses_the_stored_result() {
    let execution_id = Uuid::parse_str(EXEC_ID).unwrap();
    let store = std::sync::Arc::new(FakeStore::default());
    let ctx = OperationContext::new(execution_id, store.clone());
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    let args = json!({"table": "users"});
    let expected = operation_id(execution_id, "db__query", &args);

    let run = || {
        let calls = calls.clone();
        async move {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok::<String, String>(format!("result-{n}"))
        }
    };

    let first = run_idempotent(Some(&ctx), "db__query", &args, run())
        .await
        .unwrap();
    let second = run_idempotent(Some(&ctx), "db__query", &args, run())
        .await
        .unwrap();

    assert_eq!(first, "result-0");
    assert_eq!(second, "result-0", "the stored result is replayed verbatim");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the fake tool executed exactly once across two identical calls"
    );
    assert_eq!(
        store.get_operation(&expected).await.unwrap(),
        Some(json!("result-0"))
    );

    let events = store.get_events(execution_id).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::ToolCallCompleted && e.payload["replayed"] == true),
        "a replay emits a ToolCallCompleted event marked replayed"
    );
}

#[tokio::test]
async fn different_args_produce_a_different_operation_id() {
    let execution_id = Uuid::parse_str(EXEC_ID).unwrap();
    let store = std::sync::Arc::new(FakeStore::default());
    let ctx = OperationContext::new(execution_id, store.clone());
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    let run = || {
        let calls = calls.clone();
        async move {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok::<String, String>(format!("result-{n}"))
        }
    };

    let first = run_idempotent(Some(&ctx), "db__query", &json!({"table": "users"}), run())
        .await
        .unwrap();
    let second = run_idempotent(Some(&ctx), "db__query", &json!({"table": "orders"}), run())
        .await
        .unwrap();

    assert_eq!(first, "result-0");
    assert_eq!(second, "result-1", "different args are not a replay");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_no_op_store_is_a_transparent_pass_through() {
    // Degradation: no history feature, or a store that cannot be opened. The
    // guard must never fail a chat — it executes the tool every time.
    let ctx = OperationContext::new(
        Uuid::parse_str(EXEC_ID).unwrap(),
        std::sync::Arc::new(NoOpExecutionStore),
    );
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let run = || {
        let calls = calls.clone();
        async move {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok::<String, String>(format!("result-{n}"))
        }
    };
    let args = json!({"table": "users"});
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
        "result-1"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[cfg(feature = "rig-engine")]
#[tokio::test]
async fn the_mcp_bridge_replays_a_stored_result_instead_of_calling_the_tool() {
    use llama_r::adapters::rig_engine::limits::ToolCallBudget;
    use llama_r::adapters::rig_engine::tools::McpToolBridge;
    use llama_r::domain::scope::{AgentScope, ToolsAllow};
    use llama_r::ports::mcp::{McpCallRequest, McpCallResult, McpClient, McpToolDef};
    use llama_r::services::approval::ApprovalGate;
    use tokio::sync::Mutex as TokioMutex;

    struct CountingClient {
        calls: std::sync::Arc<std::sync::atomic::AtomicU32>,
    }

    #[async_trait]
    impl McpClient for CountingClient {
        async fn list_tools(&self, _server_id: &str) -> Result<Vec<McpToolDef>, String> {
            Ok(vec![])
        }
        async fn call_tool(&self, _req: McpCallRequest) -> Result<McpCallResult, String> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(McpCallResult {
                content: json!(format!("call-{n}")),
                is_error: false,
            })
        }
        async fn health(&self, _server_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    let execution_id = Uuid::parse_str(EXEC_ID).unwrap();
    let store = std::sync::Arc::new(FakeStore::default());
    let ctx = OperationContext::new(execution_id, store.clone());
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let scope = AgentScope {
        agent_id: "a".into(),
        project_id: None,
        mcp_sources: ["fake".into()].into_iter().collect(),
        tools_allow: ToolsAllow::AllFromSources,
        local_tools_allow: ToolsAllow::DenyAll,
        rag_sources: std::collections::HashSet::new(),
        rag_write: llama_r::domain::agent::RagWritePolicy::None,
        max_tool_calls: 5,
        max_iterations: 3,
        timeout_secs: 5,
        permission: llama_r::domain::policy::PermissionPolicy::default(),
    };
    let tool = llama_r::adapters::rig_engine::ScopedMcpTool {
        def: McpToolDef {
            server_id: "fake".into(),
            name: "query".into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
        },
        client: std::sync::Arc::new(CountingClient {
            calls: calls.clone(),
        }),
        scope,
    };

    let bridge = McpToolBridge::new(
        tool,
        std::sync::Arc::new(TokioMutex::new(ToolCallBudget::new(5))),
        std::sync::Arc::new(ApprovalGate::default()),
        None,
    )
    .with_operations(ctx);

    let args = json!({"table": "users"});
    let first = bridge.invoke_json(args.clone()).await.unwrap();
    let second = bridge.invoke_json(args).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the bridge called the fake tool once, not twice"
    );
    assert!(store
        .get_events(execution_id)
        .await
        .unwrap()
        .iter()
        .any(|e| e.kind == EventKind::ToolCallCompleted && e.payload["replayed"] == true));
}

#[test]
fn an_agent_run_request_carries_an_execution_id() {
    // Compile-time + semantic proof that the cross-cutting field exists and is
    // carried per turn (the compiler rejects any literal that omits it).
    use llama_r::config::ProviderKind;
    use llama_r::domain::agent::{Agent, AgentConfig};
    use llama_r::domain::scope::{AgentScope, ToolsAllow};
    use llama_r::providers::capabilities::capabilities_for;

    let execution_id = Uuid::new_v4();
    let req = AgentRunRequest {
        execution_id,
        shared_budget: None,
        delegation_context: None,
        approval_event_tx: None,
        tool_event_tx: None,
        agent: Agent {
            id: "a".into(),
            project_id: None,
            config: AgentConfig::default(),
        },
        scope: AgentScope {
            agent_id: "a".into(),
            project_id: None,
            mcp_sources: std::collections::HashSet::new(),
            tools_allow: ToolsAllow::DenyAll,
            local_tools_allow: ToolsAllow::DenyAll,
            rag_sources: std::collections::HashSet::new(),
            rag_write: llama_r::domain::agent::RagWritePolicy::None,
            max_tool_calls: 0,
            max_iterations: 1,
            timeout_secs: 1,
            permission: llama_r::domain::policy::PermissionPolicy::default(),
        },
        model: "m".into(),
        provider: ProviderKind::Ollama,
        capabilities: capabilities_for(ProviderKind::Ollama, "m"),
        system_prompt: "sys".into(),
        user_message: "hi".into(),
        history: vec![],
        conversation_id: None,
    };
    assert_eq!(req.execution_id, execution_id);
    assert_ne!(req.execution_id, Uuid::nil());
}

// ── Task 5: approvals survive a restart ─────────────────────────────────────

#[tokio::test]
async fn a_pending_survives_gate_drop_and_reload() {
    let store = Arc::new(FakeStore::default());
    let args = json!({"table": "users"});

    let id = {
        let gate = ApprovalGate::default().with_store(store.clone());
        let (id, _rx) = gate.request("db__drop_table", &args);
        // The live request was persisted at request time.
        assert_eq!(store.load_pending_approvals().await.unwrap().len(), 1);
        id
    }; // the owning gate (and its oneshot) is dropped, simulating a restart

    let reloaded = ApprovalGate::default().with_store(store.clone());
    assert_eq!(reloaded.load_from_store().await.unwrap(), 1);

    // The rehydrated pending carries no oneshot but is fully observable: its
    // hash still binds a later decision to the exact arguments on file.
    assert_eq!(reloaded.stored_hash(id), Some(args_hash(&args)));
    let pending = reloaded.list_pending();
    assert_eq!(
        pending.len(),
        1,
        "the pending is discoverable after restart"
    );
    assert_eq!(pending[0].id, id);
    assert_eq!(pending[0].tool_name, "db__drop_table");
}

#[tokio::test]
async fn a_decision_after_restart_is_consumed_by_the_retry() {
    let store = Arc::new(FakeStore::default());
    let args = json!({"table": "users"});

    let id = {
        let gate = ApprovalGate::default().with_store(store.clone());
        let (id, _rx) = gate.request("db__drop_table", &args);
        id
    };

    // No live oneshot exists after the restart, yet the HTTP decision must
    // still be accepted, persisted and made observable to the retry path.
    let gate = ApprovalGate::default().with_store(store.clone());
    gate.load_from_store().await.unwrap();
    gate.decide(id, true, args_hash(&args))
        .expect("deciding a rehydrated pending must succeed");
    assert_eq!(gate.decision_for(id), Some(ApprovalDecision::Approved));

    // A fresh gate (the retry's view) loads the persisted decision from disk.
    let retry_gate = ApprovalGate::default().with_store(store.clone());
    retry_gate.load_from_store().await.unwrap();
    assert_eq!(
        retry_gate.decision_for(id),
        Some(ApprovalDecision::Approved),
        "the retry observes the decision made while no live request existed"
    );

    let records = store.load_pending_approvals().await.unwrap();
    assert_eq!(records.len(), 1);
    assert!(records[0].decided);
    assert_eq!(records[0].decision, Some(ApprovalDecisionRecord::Approved));
}

#[tokio::test]
async fn a_wrong_hash_after_reload_is_rejected() {
    let store = Arc::new(FakeStore::default());
    let args = json!({"amount": 100});

    let id = {
        let gate = ApprovalGate::default().with_store(store.clone());
        let (id, _rx) = gate.request("pay__send", &args);
        id
    };

    let gate = ApprovalGate::default().with_store(store.clone());
    gate.load_from_store().await.unwrap();

    let tampered = json!({"amount": 200});
    assert!(matches!(
        gate.decide(id, true, args_hash(&tampered)),
        Err(ApprovalError::ArgsChanged)
    ));
    assert_eq!(
        gate.decision_for(id),
        None,
        "a rejected hash yields no decision"
    );
    assert!(
        !store.load_pending_approvals().await.unwrap()[0].decided,
        "a rejected decision is never persisted"
    );

    // The correct hash still resolves the exact arguments on file, and a denial
    // is just as observable to the retry as an approval.
    gate.decide(id, false, args_hash(&args)).unwrap();
    assert_eq!(
        gate.decision_for(id),
        Some(ApprovalDecision::Denied {
            reason: "denied by human".into()
        })
    );
}

#[cfg(feature = "history")]
#[tokio::test]
async fn the_gate_persists_through_the_real_sqlite_store() {
    // Production path: the gate is driven from async handlers, so its store
    // writes run through `block_on`. SQLite uses a std mutex + synchronous
    // rusqlite (no tokio I/O), so this never parks. This test proves the real
    // adapter round-trips a pending and its decision through the gate, and
    // that the store — not the gate — redacts secret-bearing args (Task 2).
    use llama_r::adapters::execution::SqliteExecutionStore;
    use llama_r::services::redaction::REDACTED_MARKER;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let store = Arc::new(SqliteExecutionStore::new(dir.path().join("history.db")).unwrap());
    let args = json!({"table": "users", "api_key": "sk-super-secret"});

    let id = {
        let gate = ApprovalGate::default().with_store(store.clone());
        let (id, _rx) = gate.request("db__drop_table", &args);
        id
    };

    let reloaded = ApprovalGate::default().with_store(store.clone());
    assert_eq!(reloaded.load_from_store().await.unwrap(), 1);
    let pending = reloaded.list_pending();
    assert_eq!(pending.len(), 1);
    // The store redacts secret-bearing args on disk (one shared implementation,
    // no duplicate in the gate), so a rehydrated entry carries redacted args.
    assert_eq!(pending[0].args["api_key"], REDACTED_MARKER);
    assert_eq!(pending[0].args["table"], "users");
    let records = store.load_pending_approvals().await.unwrap();
    assert_eq!(records[0].args["api_key"], REDACTED_MARKER);
    assert!(!serde_json::to_string(&records[0])
        .unwrap()
        .contains("sk-super-secret"));

    reloaded.decide(id, true, args_hash(&args)).unwrap();
    let retry_gate = ApprovalGate::default().with_store(store.clone());
    retry_gate.load_from_store().await.unwrap();
    assert_eq!(
        retry_gate.decision_for(id),
        Some(ApprovalDecision::Approved)
    );
}
