//! No-op [`ExecutionStore`] substitute.
//!
//! Compiled under **every** feature combination. It serves two roles:
//!
//! - Under `--no-default-features` there is no `rusqlite`, so this is the only
//!   store available and `AppState` must still be constructible.
//! - With `history` on, it is the degradation fallback when the SQLite store
//!   cannot be opened: boot warns and uses this instead of failing, and a chat
//!   never fails because the store is down.
//!
//! Every write succeeds and every read is empty. Callers do not branch on which
//! store they hold — durable behaviour is simply absent, never faked.

use crate::domain::execution::{AgentExecution, ExecutionEvent, ExecutionStatus};
use crate::ports::execution::{ApprovalDecisionRecord, ExecutionStore, PendingApprovalRecord};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

/// A store that remembers nothing. All methods are successful no-ops.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOpExecutionStore;

#[async_trait]
impl ExecutionStore for NoOpExecutionStore {
    async fn create(&self, _execution: &AgentExecution) -> Result<(), String> {
        Ok(())
    }

    async fn append_event(
        &self,
        _execution_id: Uuid,
        _event: &ExecutionEvent,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn set_status(
        &self,
        _execution_id: Uuid,
        _status: ExecutionStatus,
        _error: Option<&str>,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn set_current_step(&self, _execution_id: Uuid, _step: u32) -> Result<(), String> {
        Ok(())
    }

    async fn get(&self, _execution_id: Uuid) -> Result<Option<AgentExecution>, String> {
        Ok(None)
    }

    async fn list_interrupted(&self) -> Result<Vec<AgentExecution>, String> {
        Ok(Vec::new())
    }

    async fn list(&self, _limit: usize) -> Result<Vec<AgentExecution>, String> {
        Ok(Vec::new())
    }

    async fn get_events(&self, _execution_id: Uuid) -> Result<Vec<ExecutionEvent>, String> {
        Ok(Vec::new())
    }

    async fn record_operation(
        &self,
        _operation_id: &str,
        _result_json: &Value,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn get_operation(&self, _operation_id: &str) -> Result<Option<Value>, String> {
        Ok(None)
    }

    async fn save_pending_approval(&self, _approval: &PendingApprovalRecord) -> Result<(), String> {
        Ok(())
    }

    async fn load_pending_approvals(&self) -> Result<Vec<PendingApprovalRecord>, String> {
        Ok(Vec::new())
    }

    async fn decide_pending(
        &self,
        _id: Uuid,
        _decision: ApprovalDecisionRecord,
        _decided_at: DateTime<Utc>,
    ) -> Result<(), String> {
        Ok(())
    }
}
