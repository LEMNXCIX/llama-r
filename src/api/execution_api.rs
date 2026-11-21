//! Execution observability and turn-level retry (Fase 7 Task 6).
//!
//! Three admin endpoints, all gated by `X-Debug: true` like RAG/Router:
//! - `GET /api/executions` — most recent turns first;
//! - `GET /api/executions/:id` — one turn plus its append-only event log;
//! - `POST /api/executions/:id/retry` — re-run an interrupted/failed turn.
//!
//! Secret handling: the store redacts structured fields before persistence;
//! the DTO layer applies the same shared redaction again as defence in depth,
//! so neither a legacy row nor a hand-seeded one can echo a credential back.
//!
//! Retry reuses the ORIGINAL execution id. That is the load-bearing invariant:
//! the tool operation cache is keyed on `(execution_id, tool, args_hash)`, so a
//! new id would re-execute already-completed tools and duplicate side effects.
//! The turn is also reconstructed from the stored (redacted) `user_message`; a
//! legacy row with no message is refused rather than retried with an empty one.

use crate::api::chat_core::execute_chat_with_id;
use crate::api::handlers::AppState;
use crate::api::route_resolution::AgentSelection;
use crate::domain::execution::{AgentExecution, EventKind, ExecutionEvent, ExecutionStatus};
use crate::domain::models::ChatMessage;
use crate::error::AppError;
use crate::services::redaction::redact_secrets;
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

/// `X-Debug: true` is checked before the id is parsed or the body touched, so an
/// unauthorised caller always gets 403 and never learns the request schema.
fn require_debug(headers: &HeaderMap) -> Result<(), AppError> {
    let ok = headers.get("x-debug").and_then(|v| v.to_str().ok()) == Some("true");
    if ok {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "execution endpoints require header X-Debug: true".into(),
        ))
    }
}

fn parse_id(raw: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(raw).map_err(|_| AppError::Validation(format!("invalid execution id '{raw}'")))
}

/// Redact a stored user message. Only structured JSON has fields for
/// `redact_secrets` to inspect; plain prose is left byte-for-byte intact.
fn redact_message(message: &str) -> String {
    match serde_json::from_str::<Value>(message) {
        Ok(value @ (Value::Object(_) | Value::Array(_))) => {
            serde_json::to_string(&redact_secrets(&value)).unwrap_or_else(|_| message.to_string())
        }
        _ => message.to_string(),
    }
}

/// A turn as exposed by the API. Ids are strings and timestamps RFC 3339 so the
/// DTO derives without a UUID/chrono feature and cannot leak internal types.
#[derive(Debug, Serialize)]
pub struct ExecutionDto {
    pub id: String,
    pub conversation_id: Option<String>,
    pub agent_qualified_id: String,
    pub provider: String,
    pub model: String,
    pub status: ExecutionStatus,
    pub current_step: u32,
    /// Redacted before serialisation — never a raw credential.
    pub user_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ExecutionEventDto {
    pub id: String,
    pub seq: u32,
    pub kind: EventKind,
    /// Redacted before serialisation.
    pub payload: Value,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct ExecutionDetailDto {
    pub execution: ExecutionDto,
    pub events: Vec<ExecutionEventDto>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Defaults to 50, capped at 200 so an unbounded scan cannot be requested.
    pub limit: Option<usize>,
}

fn to_dto(execution: &AgentExecution) -> ExecutionDto {
    ExecutionDto {
        id: execution.id.to_string(),
        conversation_id: execution.conversation_id.clone(),
        agent_qualified_id: execution.agent_qualified_id.clone(),
        provider: execution.provider.clone(),
        model: execution.model.clone(),
        status: execution.status.clone(),
        current_step: execution.current_step,
        user_message: execution.user_message.as_deref().map(redact_message),
        created_at: execution.created_at.to_rfc3339(),
        updated_at: execution.updated_at.to_rfc3339(),
        finished_at: execution.finished_at.map(|t| t.to_rfc3339()),
        error: execution.error.clone(),
    }
}

fn event_to_dto(event: &ExecutionEvent) -> ExecutionEventDto {
    ExecutionEventDto {
        id: event.id.to_string(),
        seq: event.seq,
        kind: event.kind.clone(),
        payload: redact_secrets(&event.payload),
        created_at: event.created_at.to_rfc3339(),
    }
}

/// `GET /api/executions` — the most recent turns first. `403` without `X-Debug`.
pub async fn list_executions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<ExecutionDto>>, AppError> {
    require_debug(&headers)?;
    state.observability.record_http_request();
    let limit = query.limit.unwrap_or(50).min(200);
    let executions = state
        .execution_store
        .list(limit)
        .await
        .map_err(AppError::Runtime)?;
    Ok(Json(executions.iter().map(to_dto).collect()))
}

/// `GET /api/executions/:id` — one turn and its event log. `403` without
/// `X-Debug`, `404` for an unknown id.
pub async fn get_execution(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(raw_id): Path<String>,
) -> Result<Json<ExecutionDetailDto>, AppError> {
    require_debug(&headers)?;
    state.observability.record_http_request();
    let id = parse_id(&raw_id)?;
    let execution = state
        .execution_store
        .get(id)
        .await
        .map_err(AppError::Runtime)?
        .ok_or_else(|| AppError::NotFound(format!("execution '{id}' not found")))?;
    let events = state
        .execution_store
        .get_events(id)
        .await
        .map_err(AppError::Runtime)?;
    Ok(Json(ExecutionDetailDto {
        execution: to_dto(&execution),
        events: events.iter().map(event_to_dto).collect(),
    }))
}

/// Split a stored qualified id (`project/agent` or a bare global id) back into
/// the pair `AgentSelection` and `engine_target` expect. Both separators are
/// accepted because older rows used `project::agent`.
fn split_qualified_id(qualified: &str) -> (Option<String>, Option<String>) {
    if let Some((project, agent)) = qualified.rsplit_once("::") {
        (Some(project.to_string()), Some(agent.to_string()))
    } else if let Some((project, agent)) = qualified.rsplit_once('/') {
        (Some(project.to_string()), Some(agent.to_string()))
    } else {
        (None, Some(qualified.to_string()))
    }
}

/// Surface a decision that was recorded after the original run stopped. The
/// gate's `request_for` is the behavioural seam that consumes it; this read is
/// the observability half — it logs which approvals are in play, using the very
/// accessor Task 5 added. A store/gate read failure is a warning, never a failed
/// retry (durability is best effort everywhere in this phase).
async fn note_decided_approvals(state: &AppState, execution_id: Uuid) {
    let records = match state.execution_store.load_pending_approvals().await {
        Ok(records) => records,
        Err(err) => {
            tracing::warn!(error = %err, "could not read approvals during retry; continuing");
            return;
        }
    };
    for record in records
        .into_iter()
        .filter(|r| r.execution_id == Some(execution_id) && r.decided)
    {
        match state.approval_gate.decision_for(record.id) {
            Some(decision) => tracing::info!(
                approval_id = %record.id,
                decision = ?decision,
                "retry consuming a human decision recorded after the original run"
            ),
            None => tracing::warn!(
                approval_id = %record.id,
                "approval was persisted as decided but is not in the gate; \
                 the retry may re-ask the human"
            ),
        }
    }
}

/// Re-run a stored turn under its ORIGINAL id. Shared by the HTTP retry endpoint
/// and the approval reaper so the replay-cache invariant lives in one place:
/// `execute_chat_with_id` resumes the turn and every tool effect already
/// recorded under this id is replayed rather than executed a second time.
///
/// The caller decides which statuses are retryable (HTTP: `Interrupted` /
/// `Failed`; the reaper: an orphaned `WaitingApproval`), so this helper does no
/// status gating of its own.
pub(crate) async fn retry_turn(
    state: &AppState,
    id: Uuid,
    execution: &AgentExecution,
) -> Result<(), AppError> {
    // A legacy row predating the `user_message` column has nothing to rebuild
    // the turn from. Refuse it by name rather than silently sending an empty
    // prompt through the engine.
    let user_message = execution
        .user_message
        .as_deref()
        .filter(|message| !message.trim().is_empty())
        .ok_or_else(|| {
            AppError::Forbidden(format!(
                "execution '{id}' has no stored user_message (legacy row); cannot retry without the original request"
            ))
        })?
        .to_string();

    // A decision made while no live oneshot existed is consumed by the gate's
    // `request_for` when the retry reaches that tool. Log which ones are in play.
    note_decided_approvals(state, id).await;

    let (project_id, agent_id) = split_qualified_id(&execution.agent_qualified_id);
    let payload = crate::domain::models::ChatRequest {
        model: execution.model.clone(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: user_message,
        }],
        stream: false,
    };
    let selection = AgentSelection {
        project_id: project_id.as_deref(),
        agent_id: agent_id.as_deref(),
        conversation_id: execution.conversation_id.as_deref(),
        debug: false,
    };

    // The one line that makes the retry idempotent: reuse the original id.
    execute_chat_with_id(state, payload, selection, Some(id)).await?;
    Ok(())
}

/// `POST /api/executions/:id/retry` — re-run an interrupted/failed turn from
/// scratch under the SAME id, so recorded tool effects are replayed.
///
/// - `200` the turn re-ran; the response is the updated execution;
/// - `400` malformed id;
/// - `403` no `X-Debug`, or a legacy row with no stored `user_message`;
/// - `404` unknown id;
/// - `409` the turn is not in a retryable status (`Interrupted`/`Failed`).
pub async fn retry_execution(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(raw_id): Path<String>,
) -> Result<Json<ExecutionDto>, AppError> {
    require_debug(&headers)?;
    state.observability.record_http_request();
    let id = parse_id(&raw_id)?;
    let execution = state
        .execution_store
        .get(id)
        .await
        .map_err(AppError::Runtime)?
        .ok_or_else(|| AppError::NotFound(format!("execution '{id}' not found")))?;

    // Only a turn a crash (or a named failure) left mid-flight is retryable. A
    // running turn has a live engine; a completed one already answered — replay
    // under the same id would duplicate its effects.
    if !matches!(
        execution.status,
        ExecutionStatus::Interrupted | ExecutionStatus::Failed
    ) {
        return Err(AppError::Conflict(format!(
            "execution '{id}' is {:?}; only interrupted or failed turns can be retried",
            execution.status
        )));
    }

    retry_turn(&state, id, &execution).await?;

    let updated = state
        .execution_store
        .get(id)
        .await
        .map_err(AppError::Runtime)?
        .ok_or_else(|| AppError::NotFound(format!("execution '{id}' not found after retry")))?;
    Ok(Json(to_dto(&updated)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_qualified_id_handles_project_global_and_legacy_forms() {
        assert_eq!(
            split_qualified_id("project/agent"),
            (Some("project".into()), Some("agent".into()))
        );
        assert_eq!(
            split_qualified_id("project::agent"),
            (Some("project".into()), Some("agent".into()))
        );
        assert_eq!(split_qualified_id("global"), (None, Some("global".into())));
    }

    #[test]
    fn redact_message_only_touches_structured_json() {
        assert_eq!(
            redact_message(r#"{"api_key":"sk-x","task":"go"}"#),
            r#"{"api_key":"[REDACTED]","task":"go"}"#
        );
        assert_eq!(
            redact_message("plain prose with key=sk-x"),
            "plain prose with key=sk-x"
        );
    }
}
