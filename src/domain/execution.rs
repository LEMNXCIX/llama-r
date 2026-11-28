//! Durable execution domain model (Fase 7).
//!
//! One `AgentExecution` is one agent **turn**, not one tool-loop step. Rig still
//! owns the loop; the persisted events are an observation log plus the seed for
//! a future step-replay implementation. Recovery is turn-level: a turn left in
//! `Running`/`WaitingApproval` by a crash is marked `Interrupted` at boot and can
//! be retried from scratch, with tool effects protected by `operation_id`.
//!
//! `user_message` holds the original turn input with secret-bearing fields
//! redacted before it reaches the store (enforced by the SQLite adapter). It is
//! nullable so a row written before the column existed can be read, but a retry
//! of such a row must be refused rather than run with an empty prompt.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Lifecycle of one agent turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionStatus {
    /// Created, not yet dispatched.
    Pending,
    /// The engine owns the tool loop.
    Running,
    /// Blocked on a human decision; the pending approval is persisted.
    WaitingApproval,
    /// The turn finished successfully.
    Completed,
    /// The turn finished with a named error.
    Failed,
    /// A crash left the turn mid-flight; safe to retry from scratch.
    Interrupted,
    /// Cancelled by a caller.
    Cancelled,
}

/// One persisted agent turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentExecution {
    pub id: Uuid,
    /// Conversation this turn belongs to, when the caller supplied one.
    pub conversation_id: Option<String>,
    /// `project::agent` for a project agent, or the bare id for a global one.
    pub agent_qualified_id: String,
    pub provider: String,
    pub model: String,
    pub status: ExecutionStatus,
    /// How far the tool loop got. Observation only — there is no step replay.
    pub current_step: u32,
    /// The turn input, redacted before persistence. `None` only for legacy rows.
    pub user_message: Option<String>,
    /// Integrity check over the original (unredacted) input.
    pub user_message_hash: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

/// What happened during a turn. The event log is append-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    /// The turn was accepted by the engine.
    Started,
    /// A model completion was requested.
    ModelRequested,
    /// A tool call was about to be dispatched.
    ToolCallRequested,
    /// A tool call returned, or its stored result was replayed.
    ToolCallCompleted,
    /// A checkpoint emitted after a tool call. Carries step/tool/args_hash so a
    /// retry can steer the model past completed steps; tool effects replay via
    /// `operation_id`. The model itself re-decides its answer (Rig owns the loop).
    CheckpointCreated,
    /// The turn blocked on a human decision.
    WaitingApproval,
    /// A human approved the pending tool call.
    ApprovalGranted,
    /// A retry started on an interrupted turn.
    ExecutionResumed,
    /// The turn completed successfully.
    ExecutionCompleted,
    /// The turn failed.
    ExecutionFailed,
    /// Boot observed a mid-flight turn and marked it interrupted.
    Interrupted,
    /// A structured cost/latency trace for a finished turn (Fase 6c). The unit
    /// variant keeps `EventKind: Eq`; all trace data lives in the redacted
    /// event payload, never in the kind itself.
    TraceRecorded,
}

/// One append-only event within a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionEvent {
    pub id: Uuid,
    pub execution_id: Uuid,
    /// Monotonic per execution, assigned atomically by the store on append.
    pub seq: u32,
    pub kind: EventKind,
    /// Redacted before persistence. `{"replayed": true}` marks a cached result.
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

impl ExecutionEvent {
    /// Build an event for `execution_id`. `seq` and `created_at` are
    /// placeholders: the store assigns the next monotonic `seq` and stamps
    /// `created_at` on append, while the `id` generated here is preserved.
    pub fn new(execution_id: Uuid, kind: EventKind, payload: Value) -> Self {
        Self {
            id: Uuid::new_v4(),
            execution_id,
            seq: 0,
            kind,
            payload,
            created_at: Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_new_stamps_identity_but_leaves_seq_to_the_store() {
        let execution_id = Uuid::new_v4();
        let event = ExecutionEvent::new(execution_id, EventKind::Started, json!({}));

        assert_ne!(event.id, Uuid::nil());
        assert_eq!(event.execution_id, execution_id);
        assert_eq!(event.seq, 0);
        assert!(event.created_at <= Utc::now());
    }

    #[test]
    fn execution_and_event_round_trip_through_serde() {
        let now = Utc::now();
        let execution = AgentExecution {
            id: Uuid::new_v4(),
            conversation_id: None,
            agent_qualified_id: "agent".into(),
            provider: "ollama".into(),
            model: "llama3".into(),
            status: ExecutionStatus::Interrupted,
            current_step: 3,
            user_message: Some("hello".into()),
            user_message_hash: "cd".repeat(32),
            created_at: now,
            updated_at: now,
            finished_at: None,
            error: Some("crash".into()),
        };
        let encoded = serde_json::to_string(&execution).unwrap();
        assert_eq!(
            serde_json::from_str::<AgentExecution>(&encoded).unwrap(),
            execution
        );

        let event = ExecutionEvent::new(
            execution.id,
            EventKind::ExecutionFailed,
            json!({"error": "crash"}),
        );
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(
            serde_json::from_str::<ExecutionEvent>(&encoded).unwrap(),
            event
        );
    }
}
