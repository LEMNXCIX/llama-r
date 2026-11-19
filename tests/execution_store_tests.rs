//! Contract tests for the Fase 7 execution domain types and `ExecutionStore` port.
//!
//! Task 2 replaces this contract-only skeleton with adapter-backed CRUD, seq,
//! idempotency and pending-approval tests against `SqliteExecutionStore`. What
//! lives here already is the shape downstream tasks depend on: the types round
//! trip through serde, event construction needs no boilerplate, and the port is
//! usable as `Arc<dyn ExecutionStore>` (the form `AppState` will hold).

use chrono::Utc;
use llama_r::domain::execution::{AgentExecution, EventKind, ExecutionEvent, ExecutionStatus};
use llama_r::ports::execution::{ApprovalDecisionRecord, ExecutionStore, PendingApprovalRecord};
use serde_json::json;
use uuid::Uuid;

fn sample_execution() -> AgentExecution {
    let now = Utc::now();
    AgentExecution {
        id: Uuid::new_v4(),
        conversation_id: Some("conv-1".into()),
        agent_qualified_id: "project::agent".into(),
        provider: "ollama".into(),
        model: "llama3".into(),
        status: ExecutionStatus::Running,
        current_step: 2,
        user_message: Some("show me the {table}".into()),
        user_message_hash: "ab".repeat(32),
        created_at: now,
        updated_at: now,
        finished_at: None,
        error: None,
    }
}

#[test]
fn agent_execution_survives_a_serde_round_trip() {
    let execution = sample_execution();
    let encoded = serde_json::to_string(&execution).unwrap();
    let decoded: AgentExecution = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded, execution);
    assert!(encoded.contains("\"user_message\""));
    assert!(encoded.contains("\"user_message_hash\""));
}

#[test]
fn every_execution_status_survives_a_serde_round_trip() {
    let all = [
        ExecutionStatus::Pending,
        ExecutionStatus::Running,
        ExecutionStatus::WaitingApproval,
        ExecutionStatus::Completed,
        ExecutionStatus::Failed,
        ExecutionStatus::Interrupted,
        ExecutionStatus::Cancelled,
    ];
    for status in all {
        let encoded = serde_json::to_string(&status).unwrap();
        let decoded: ExecutionStatus = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, status, "status {encoded} did not round trip");
    }
}

#[test]
fn an_event_can_be_created_without_picking_an_id_or_timestamp() {
    let execution_id = Uuid::new_v4();
    let event = ExecutionEvent::new(
        execution_id,
        EventKind::ToolCallCompleted,
        json!({"tool": "llama_r__skill", "replayed": true}),
    );

    assert_ne!(event.id, Uuid::nil());
    assert_eq!(event.execution_id, execution_id);
    assert_eq!(event.seq, 0, "seq is assigned by the store");
    assert_eq!(event.kind, EventKind::ToolCallCompleted);
    assert_eq!(event.payload["replayed"], true);
    assert!(event.created_at <= Utc::now());
}

#[test]
fn an_execution_event_survives_a_serde_round_trip() {
    let execution_id = Uuid::new_v4();
    let event = ExecutionEvent::new(execution_id, EventKind::Started, json!({"step": 1}));
    let encoded = serde_json::to_string(&event).unwrap();
    let decoded: ExecutionEvent = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded, event);
}

#[test]
fn every_event_kind_survives_a_serde_round_trip() {
    let all = [
        EventKind::Started,
        EventKind::ModelRequested,
        EventKind::ToolCallRequested,
        EventKind::ToolCallCompleted,
        EventKind::CheckpointCreated,
        EventKind::WaitingApproval,
        EventKind::ApprovalGranted,
        EventKind::ExecutionResumed,
        EventKind::ExecutionCompleted,
        EventKind::ExecutionFailed,
        EventKind::Interrupted,
    ];
    for kind in all {
        let encoded = serde_json::to_string(&kind).unwrap();
        let decoded: EventKind = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, kind, "kind {encoded} did not round trip");
    }
}

#[test]
fn a_persisted_pending_approval_survives_a_serde_round_trip() {
    let record = PendingApprovalRecord {
        id: Uuid::new_v4(),
        execution_id: Some(Uuid::new_v4()),
        tool_name: "mail__send".into(),
        args: json!({"to": "a@example.com", "api_key": "[REDACTED]"}),
        args_hash: [0xab; 32],
        created_at: Utc::now(),
        decided: true,
        decision: Some(ApprovalDecisionRecord::Denied {
            reason: "denied by human".into(),
        }),
        decided_at: Some(Utc::now()),
    };

    let encoded = serde_json::to_string(&record).unwrap();
    let decoded: PendingApprovalRecord = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn the_store_port_is_usable_as_a_thread_safe_trait_object() {
    // `AppState` holds `Arc<dyn ExecutionStore>` across every server task; this
    // test is the compile-time proof that the async trait stays object safe.
    fn accepts(_store: &dyn ExecutionStore) {}
    fn accepts_shared(_store: std::sync::Arc<dyn ExecutionStore>) {}
    let _ = (accepts, accepts_shared);
}
