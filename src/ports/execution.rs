//! Durable execution store port.
//!
//! The SQLite adapter lives in `adapters::execution`; the runtime depends only
//! on this trait so a different backend (or a degraded in-memory no-op) can be
//! substituted without changing chat or approval code. Every method returns
//! `Result<_, String>` to match the other ports in this crate.
//!
//! The store never fails a chat: an unavailable store is handled by the caller
//! (a no-op store at boot, a pass-through idempotency guard). The idempotency
//! methods are deliberately a *record of completed tool calls*, not a queue:
//! `record_operation` inserts once, and `get_operation` replays that result.

use crate::domain::execution::{AgentExecution, ExecutionEvent, ExecutionStatus};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// The human's answer as stored in SQLite. Deliberately a separate type from
/// the in-memory `services::approval::ApprovalDecision` (which carries a
/// oneshot): the port must not depend on a service adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalDecisionRecord {
    Approved,
    Denied { reason: String },
}

/// A durable snapshot of one pending (or already decided) human approval.
///
/// `args` is redacted before persistence; `args_hash` is computed over the
/// original arguments and is what binds a later decision to what the human
/// saw. A rehydrated entry has no oneshot — the HTTP decision is persisted and
/// consumed by the retry path, not by a live receiver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingApprovalRecord {
    pub id: Uuid,
    /// The turn that requested the approval, when one was in flight.
    pub execution_id: Option<Uuid>,
    pub tool_name: String,
    pub args: Value,
    pub args_hash: [u8; 32],
    pub created_at: DateTime<Utc>,
    pub decided: bool,
    pub decision: Option<ApprovalDecisionRecord>,
    pub decided_at: Option<DateTime<Utc>>,
}

#[async_trait]
pub trait ExecutionStore: Send + Sync {
    /// Insert a new execution. The caller supplies the id and timestamps.
    async fn create(&self, execution: &AgentExecution) -> Result<(), String>;

    /// Append one event to a turn. The store assigns the next monotonic `seq`
    /// for that execution (a `seq` on `event` is a placeholder and is ignored)
    /// and stamps `created_at`; `event.id` and `event.payload` are preserved.
    /// Secret-bearing fields in `payload` are redacted before persistence.
    async fn append_event(&self, execution_id: Uuid, event: &ExecutionEvent) -> Result<(), String>;

    /// Change a turn's status, updating `updated_at`; terminal statuses also
    /// set `finished_at`. `error` is stored verbatim for `Failed`.
    async fn set_status(
        &self,
        execution_id: Uuid,
        status: ExecutionStatus,
        error: Option<&str>,
    ) -> Result<(), String>;

    /// Advance the observation-only tool-loop cursor of a turn.
    ///
    /// Additive to the port: bridges write it next to each `CheckpointCreated`
    /// so a retry can see how far the turn got. A failure is a warning in the
    /// caller, never a failed tool call or chat.
    async fn set_current_step(&self, execution_id: Uuid, step: u32) -> Result<(), String>;

    /// Read one turn, or `None` for an unknown id.
    async fn get(&self, execution_id: Uuid) -> Result<Option<AgentExecution>, String>;

    /// List turns in `Running` or `WaitingApproval` — the ones a crash may have
    /// interrupted. Used at boot by the recovery marker.
    async fn list_interrupted(&self) -> Result<Vec<AgentExecution>, String>;

    /// List the most recent turns first, capped at `limit`. Backs the
    /// observability endpoint; `limit == 0` returns an empty list.
    async fn list(&self, limit: usize) -> Result<Vec<AgentExecution>, String>;

    /// Read a turn's full event log in ascending `seq` order.
    async fn get_events(&self, execution_id: Uuid) -> Result<Vec<ExecutionEvent>, String>;

    /// Record a completed tool call under an `operation_id`. Idempotent: a
    /// second call with the same id leaves the first result untouched.
    async fn record_operation(&self, operation_id: &str, result_json: &Value)
        -> Result<(), String>;

    /// The stored result of a completed tool call, if any. `Some` means a retry
    /// must replay it instead of re-executing the tool.
    async fn get_operation(&self, operation_id: &str) -> Result<Option<Value>, String>;

    /// Insert or update a pending approval record, including its decision once
    /// one has been made. Keyed by `id` so boot reload and HTTP decide agree.
    async fn save_pending_approval(&self, approval: &PendingApprovalRecord) -> Result<(), String>;

    /// Load every persisted approval, decided or not. Decided records are
    /// included on purpose: after a restart the retry path must be able to see
    /// a decision that arrived while no live oneshot existed.
    async fn load_pending_approvals(&self) -> Result<Vec<PendingApprovalRecord>, String>;

    /// Persist a human decision for one pending approval. Returns an error for
    /// an unknown id so a decision can never be silently lost. Hash binding is
    /// enforced by the gate in memory; the store only records the outcome.
    async fn decide_pending(
        &self,
        id: Uuid,
        decision: ApprovalDecisionRecord,
        decided_at: DateTime<Utc>,
    ) -> Result<(), String>;
}
