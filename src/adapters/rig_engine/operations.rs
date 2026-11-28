//! Shared idempotency guard and checkpoint emitter for tool calls (Fase 7/10-C4).
//!
//! One `AgentExecution` is one turn. A retry of that turn reaches the same tool
//! with the same arguments; the store remembers the completed result under a
//! deterministic `operation_id`, so the retry replays it instead of performing
//! the tool's side effect a second time. Both bridge kinds use this one helper
//! so they cannot drift apart.
//!
//! Every guarded call also leaves an observation trail: `ToolCallRequested`
//! before dispatch and `CheckpointCreated` after it (on both the fresh and the
//! replay branch, and with `status: "failed"` when the tool returns an error).
//! The checkpoint payload carries `{step, next_step_index, tool, args_hash,
//! replayed, status}` — never the raw arguments or the tool result, which may
//! hold credentials. `current_step` advances via the additive
//! `ExecutionStore::set_current_step`.
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
/// cached result is returned (and a replay checkpoint appended) before
/// `execute` is ever polled; otherwise the result is recorded for a future
/// retry. Store errors never propagate — the caller's chat keeps working.
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
    let args_hash = args_hash_hex(args);
    let step = current_step(ctx).await;
    request_event(ctx, tool_name, &args_hash, step).await;

    match ctx.store.get_operation(&id).await {
        Ok(Some(stored)) => {
            append_event(ctx, tool_name, true).await;
            checkpoint(ctx, tool_name, &args_hash, step, true, "completed").await;
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

    match execute.await {
        Ok(result) => {
            if let Err(err) = ctx
                .store
                .record_operation(&id, &Value::String(result.clone()))
                .await
            {
                tracing::warn!(error = %err, operation_id = %id, "could not record a tool operation");
            }
            append_event(ctx, tool_name, false).await;
            checkpoint(ctx, tool_name, &args_hash, step, false, "completed").await;
            Ok(result)
        }
        Err(err) => {
            // A failed attempt is still a checkpoint: the step was reached and
            // consumed, but no result was cached, so a retry may execute it.
            checkpoint(ctx, tool_name, &args_hash, step, false, "failed").await;
            Err(err)
        }
    }
}

/// The step this call will checkpoint: the stored cursor, defaulting to 0.
/// A read failure is a warning and degrades to 0, never a failed tool call.
async fn current_step(ctx: &OperationContext) -> u32 {
    match ctx.store.get(ctx.execution_id).await {
        Ok(Some(execution)) => execution.current_step,
        Ok(None) => 0,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the execution step; using 0");
            0
        }
    }
}

/// Append the `ToolCallRequested` observation before dispatch. Its payload
/// carries only the step, tool and canonical hash — no arguments.
async fn request_event(ctx: &OperationContext, tool_name: &str, args_hash: &str, step: u32) {
    let event = ExecutionEvent::new(
        ctx.execution_id,
        EventKind::ToolCallRequested,
        serde_json::json!({
            "step": step,
            "next_step_index": step.saturating_add(1),
            "tool": tool_name,
            "args_hash": args_hash,
        }),
    );
    if let Err(err) = ctx.store.append_event(ctx.execution_id, &event).await {
        tracing::warn!(error = %err, "could not append a tool-requested execution event");
    }
}

/// Append the `CheckpointCreated` marker and advance the turn's cursor. Both
/// writes are best effort; the next call reads the cursor back, so a failed
/// write simply leaves the step where it was.
async fn checkpoint(
    ctx: &OperationContext,
    tool_name: &str,
    args_hash: &str,
    step: u32,
    replayed: bool,
    status: &str,
) {
    let next_step_index = step.saturating_add(1);
    let event = ExecutionEvent::new(
        ctx.execution_id,
        EventKind::CheckpointCreated,
        serde_json::json!({
            "step": step,
            "next_step_index": next_step_index,
            "tool": tool_name,
            "args_hash": args_hash,
            "replayed": replayed,
            "status": status,
        }),
    );
    if let Err(err) = ctx.store.append_event(ctx.execution_id, &event).await {
        tracing::warn!(error = %err, "could not append a checkpoint execution event");
    }
    if let Err(err) = ctx
        .store
        .set_current_step(ctx.execution_id, next_step_index)
        .await
    {
        tracing::warn!(error = %err, "could not advance the execution step");
    }
}

/// Append the observation event for a tool call's completion. Redaction and
/// `seq` assignment are the store's job; a failure is a warning, not a failed
/// tool call.
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
