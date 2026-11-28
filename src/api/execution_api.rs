//! Execution observability and turn-level retry (Fase 7 Task 6).
//!
//! Three admin endpoints, all gated by `X-Debug: true` like RAG/Router:
//! - `GET /api/executions` — most recent turns first;
//! - `GET /api/executions/:id` — one turn plus its append-only event log;
//! - `POST /api/executions/:id/retry` — re-run an interrupted/failed turn,
//!   optionally from a given checkpoint step (`{"from_step": N}`).
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

use crate::api::chat_core::execute_chat_with_id_and_iterations;
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

/// Optional body of `POST /api/executions/:id/retry`.
///
/// `from_step` selects which recorded checkpoints feed the resume note: only
/// checkpoints whose `step >= from_step` are included. `0` (the default, also
/// what an absent body means) keeps the original retry behaviour of resuming
/// from the beginning of the turn with a note listing every completed step.
#[derive(Debug, Default, Deserialize)]
pub struct RetryBody {
    #[serde(default)]
    pub from_step: Option<u32>,
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

/// One checkpoint turned into a line of the resume note. Results are already
/// redacted and truncated by the time this struct is built, so the formatter is
/// a pure function that can be unit tested without a store or an async runtime.
#[derive(Debug)]
struct ResumeEntry {
    step: u32,
    tool: String,
    args_hash: String,
    result: String,
}

/// A stored tool result is rarely a bare scalar: bridges normally record a
/// `Value::String` holding the tool's output. If that output is JSON, parse it
/// before redacting so secret-bearing fields are masked by the shared helper;
/// plain prose is left alone (no regex guessing at a credential in free text).
fn redacted_result_text(value: &Value) -> String {
    let text = match value {
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(parsed @ (Value::Object(_) | Value::Array(_))) => {
                serde_json::to_string(&redact_secrets(&parsed)).unwrap_or_else(|_| text.clone())
            }
            _ => text.clone(),
        },
        other => serde_json::to_string(&redact_secrets(other))
            .unwrap_or_else(|_| "[unprintable result]".to_string()),
    };
    truncate_chars(&text, 200)
}

/// Unicode-safe truncation. An ellipsis is included in the 200-char budget, so
/// a note line can never grow past the cap because of the marker.
fn truncate_chars(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let mut out: String = chars[..max.saturating_sub(1)].iter().collect();
    out.push('…');
    out
}

/// Pure formatter for the resume note. An empty entry list yields an empty
/// string: a retry with no checkpoints is byte-for-byte the old retry.
fn format_resume_note(entries: &[ResumeEntry], from_step: u32) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let last_step = entries.iter().map(|entry| entry.step).max().unwrap_or(0);
    let mut note = format!(
        "[Resume] Este turno se reanuda desde el paso {from_step}.\n\
         Ya se completaron los siguientes pasos (sus efectos están guardados y NO deben ejecutarse de nuevo):"
    );
    for entry in entries {
        let hash: String = entry.args_hash.chars().take(12).collect();
        note.push_str(&format!(
            "\n- paso {}: herramienta '{}' (hash {}) — resultado: {}",
            entry.step, entry.tool, hash, entry.result
        ));
    }
    note.push_str(&format!(
        "\nContinúa desde el paso {}. No repitas los pasos anteriores.",
        last_step.saturating_add(1)
    ));
    note
}

/// Read checkpoints and their stored tool results, best effort. A failed store
/// read logs a warning and degrades to no note — never a failed retry. Returns
/// the note and the number of checkpoints it covers (used to shrink the
/// retry's tool-loop budget).
async fn resume_note(state: &AppState, id: Uuid, from_step: u32) -> (String, usize) {
    let events = match state.execution_store.get_events(id).await {
        Ok(events) => events,
        Err(err) => {
            tracing::warn!(error = %err, "could not read checkpoints during retry; continuing without a resume note");
            return (String::new(), 0);
        }
    };
    let mut entries = Vec::new();
    for event in events
        .into_iter()
        .filter(|event| event.kind == EventKind::CheckpointCreated)
    {
        let Some(step) = event.payload.get("step").and_then(Value::as_u64) else {
            continue;
        };
        let Ok(step) = u32::try_from(step) else {
            continue;
        };
        if step < from_step {
            continue;
        }
        let tool = event
            .payload
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let args_hash = event
            .payload
            .get("args_hash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // `operation_id` is `{execution_id}:{tool}:{args_hash_hex}`; the
        // checkpoint already carries the hex hash, so no re-hash is needed
        // (and re-hashing the hash would miss every stored result).
        let result = if args_hash.is_empty() {
            "(sin resultado almacenado)".to_string()
        } else {
            let operation_id = format!("{id}:{tool}:{args_hash}");
            match state.execution_store.get_operation(&operation_id).await {
                Ok(Some(value)) => redacted_result_text(&value),
                Ok(None) => "(sin resultado almacenado)".to_string(),
                Err(err) => {
                    tracing::warn!(error = %err, operation_id = %operation_id, "could not read a checkpointed tool result; continuing");
                    "(resultado no disponible)".to_string()
                }
            }
        };
        entries.push(ResumeEntry {
            step,
            tool,
            args_hash,
            result,
        });
    }
    let count = entries.len();
    (format_resume_note(&entries, from_step), count)
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
    from_step: u32,
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

    // Checkpoint-informed steering: completed tool steps are replayed from the
    // idempotency cache, but the model re-decides its answer. The resume note
    // lives in the user message (not the system prompt) so it survives every
    // prompt assembler path, including the agent engine and its fallback. Both
    // the note and the reduced budget are best effort: a store/registry read
    // failure simply leaves the old retry behaviour in place.
    let (note, completed_steps) = resume_note(state, id, from_step).await;
    let iterations_override = state
        .agent_registry
        .resolve(project_id.as_deref(), agent_id.as_deref())
        .map(|registered| registered.scope.max_iterations as usize)
        .map(|max_iterations| max_iterations.saturating_sub(completed_steps).max(1));
    let content = if note.is_empty() {
        user_message
    } else {
        format!("{note}\n\n{user_message}")
    };
    let payload = crate::domain::models::ChatRequest {
        model: execution.model.clone(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content,
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
    execute_chat_with_id_and_iterations(state, payload, selection, id, iterations_override).await?;
    Ok(())
}

/// `POST /api/executions/:id/retry` — re-run an interrupted/failed turn from
/// scratch under the SAME id, so recorded tool effects are replayed. An optional
/// JSON body `{"from_step": N}` limits the resume note to checkpoints whose
/// `step >= N`; an absent body (or `0`) resumes from the beginning.
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
    // The body extractor must be LAST: axum allows only one extractor to read
    // the request body, and `Option<Json<_>>` keeps an empty/absent body valid
    // (the old no-body contract) while accepting `{"from_step": N}`.
    body: Option<Json<RetryBody>>,
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

    let from_step = body.and_then(|Json(body)| body.from_step).unwrap_or(0);
    retry_turn(&state, id, &execution, from_step).await?;

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

    #[test]
    fn resume_note_lists_each_completed_step_with_a_redacted_truncated_result() {
        let entries = vec![
            ResumeEntry {
                step: 0,
                tool: "server__tool".into(),
                args_hash: "abc123def456".into(),
                result: "{\"rows\":3}".into(),
            },
            ResumeEntry {
                step: 2,
                tool: "local_echo".into(),
                args_hash: "def456789012".into(),
                result: "[REDACTED]".into(),
            },
        ];
        let note = format_resume_note(&entries, 2);
        assert!(note.contains("[Resume]"), "{note}");
        assert!(note.contains("paso 0"), "{note}");
        assert!(note.contains("paso 2"), "{note}");
        assert!(note.contains("server__tool"), "{note}");
        assert!(note.contains("abc123def456"), "{note}");
        assert!(note.contains("Continúa desde el paso 3"), "{note}");
        assert!(format_resume_note(&[], 0).is_empty());
    }

    #[test]
    fn stored_results_are_redacted_and_capped_at_200_chars() {
        // Directly structured values and JSON encoded inside a tool's string
        // output both go through the shared redaction before truncation.
        let structured = redacted_result_text(&serde_json::json!({
            "api_key": "sk-NOTE-SECRET",
            "rows": 3,
        }));
        assert!(structured.contains("[REDACTED]"));
        assert!(!structured.contains("sk-NOTE-SECRET"));
        let encoded = redacted_result_text(&serde_json::json!(
            "{\"AccessToken\":\"sk-NOTE-SECRET\",\"ok\":true}"
        ));
        assert!(encoded.contains("[REDACTED]"));
        assert!(!encoded.contains("sk-NOTE-SECRET"));
        let long = redacted_result_text(&serde_json::json!("x".repeat(500)));
        assert_eq!(long.chars().count(), 200);
        assert!(long.ends_with('…'));
    }
}
