//! Shared idempotency guard for tool calls (Fase 7).
//!
//! One `AgentExecution` is one turn. A retry of that turn reaches the same tool
//! with the same arguments; the store remembers the completed result under a
//! deterministic `operation_id`, so the retry replays it instead of performing
//! the tool's side effect a second time. Both bridge kinds use this one helper
//! so they cannot drift apart.
//!
//! Degradation is a pass-through, never a chat failure: a no-op store (no
//! `history` feature, or a SQLite store that could not be opened) returns
//! `Ok(None)`/`Ok(())`, and a store error is logged and ignored. In every such
//! case the tool executes normally.

use crate::domain::execution::{EventKind, ExecutionEvent};
use crate::ports::execution::ExecutionStore;
use crate::services::redaction::args_hash_hex;
use serde_json::Value;
use std::future::Future;
use std::sync::Arc;
use uuid::Uuid;

/// What a tool bridge needs to compute and guard its operations. `None` on a
/// bridge built outside the engine (e.g. a unit test) means no guard at all.
#[derive(Clone)]
pub struct OperationContext {
    pub execution_id: Uuid,
    pub store: Arc<dyn ExecutionStore>,
}

impl OperationContext {
    pub fn new(execution_id: Uuid, store: Arc<dyn ExecutionStore>) -> Self {
        Self {
            execution_id,
            store,
        }
    }

    /// A guard that records nothing — the no-op store used when `history` is
    /// off or a bridge is built outside the engine (e.g. by a unit test). It
    /// makes the caller's guard a transparent pass-through.
    pub fn disabled() -> Self {
        Self {
            execution_id: Uuid::nil(),
            store: Arc::new(crate::adapters::execution::NoOpExecutionStore),
        }
    }
}

/// Deterministic key for one completed tool call: turn, tool, canonical args.
///
/// The hash comes from the shared [`args_hash_hex`], so reordered object keys
/// hash identically and an identical retry mints the same id.
pub fn operation_id(execution_id: Uuid, tool_name: &str, args: &Value) -> String {
    format!("{execution_id}:{tool_name}:{}", args_hash_hex(args))
}

/// Guard a tool dispatch with the durable operation cache.
///
/// `ctx = None` means "no store available": execute directly. With a store, a
/// cached result is returned (and a replay event appended) before `execute` is
/// ever polled; otherwise the result is recorded for a future retry. Store
/// errors never propagate — the caller's chat keeps working.
pub async fn run_idempotent<Fut>(
    ctx: Option<&OperationContext>,
    tool_name: &str,
    args: &Value,
    execute: Fut,
) -> Result<String, String>
where
    Fut: Future<Output = Result<String, String>>,
{
    let Some(ctx) = ctx else {
        return execute.await;
    };

    let id = operation_id(ctx.execution_id, tool_name, args);
    match ctx.store.get_operation(&id).await {
        Ok(Some(stored)) => {
            append_event(ctx, tool_name, true).await;
            return Ok(match stored {
                Value::String(text) => text,
                other => other.to_string(),
            });
        }
        Ok(None) => {}
        Err(err) => tracing::warn!(
            error = %err,
            operation_id = %id,
            "could not read a stored tool operation; executing without a replay guard"
        ),
    }

    let result = execute.await?;
    if let Err(err) = ctx
        .store
        .record_operation(&id, &Value::String(result.clone()))
        .await
    {
        tracing::warn!(error = %err, operation_id = %id, "could not record a tool operation");
    }
    append_event(ctx, tool_name, false).await;
    Ok(result)
}

/// Append the observation event for a tool call. Redaction and `seq` assignment
/// are the store's job; a failure is a warning, not a failed tool call.
async fn append_event(ctx: &OperationContext, tool_name: &str, replayed: bool) {
    let event = ExecutionEvent::new(
        ctx.execution_id,
        EventKind::ToolCallCompleted,
        serde_json::json!({"tool": tool_name, "replayed": replayed}),
    );
    if let Err(err) = ctx.store.append_event(ctx.execution_id, &event).await {
        tracing::warn!(error = %err, "could not append a tool-completed execution event");
    }
}
