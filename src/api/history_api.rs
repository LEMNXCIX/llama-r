//! HTTP API endpoints for conversation history and management.

use crate::api::handlers::AppState;
use crate::error::AppError;
use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct ListConversationsParams {
    pub project_id: Option<String>,
    pub agent_id: Option<String>,
    pub page: Option<usize>,
    pub page_size: Option<usize>,
}

#[utoipa::path(
    get,
    path = "/api/conversations",
    tag = "History",
    params(
        ("project_id" = Option<String>, Query, description = "Filter by project ID"),
        ("agent_id" = Option<String>, Query, description = "Filter by agent ID"),
        ("page" = Option<usize>, Query, description = "Page index (0-based)"),
        ("page_size" = Option<usize>, Query, description = "Page size (default 20)")
    ),
    responses(
        (status = 200, description = "List of agent conversations"),
    )
)]
pub async fn list_conversations_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListConversationsParams>,
) -> Result<impl IntoResponse, AppError> {
    state.observability.record_http_request();
    let history = state
        .history_store
        .as_ref()
        .ok_or_else(|| AppError::Runtime("History feature is disabled or uninitialized".into()))?;

    let agent_qid = match (&params.project_id, &params.agent_id) {
        (Some(p), Some(a)) => format!("{p}/{a}"),
        (Some(p), None) => format!("{p}/{p}"),
        (None, Some(a)) => a.clone(),
        (None, None) => {
            return Err(AppError::Validation(
                "Must provide agent_id or project_id to list conversations".into(),
            ))
        }
    };

    let records = history
        .list_conversations(
            &agent_qid,
            params.page.unwrap_or(0),
            params.page_size.unwrap_or(20),
        )
        .await
        .map_err(AppError::Runtime)?;

    Ok(Json(records))
}

#[utoipa::path(
    get,
    path = "/api/conversations/{id}",
    tag = "History",
    responses(
        (status = 200, description = "Get conversation metadata by ID"),
        (status = 404, description = "Conversation not found"),
    )
)]
pub async fn get_conversation_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    state.observability.record_http_request();
    let history = state
        .history_store
        .as_ref()
        .ok_or_else(|| AppError::Runtime("History feature is disabled or uninitialized".into()))?;

    let record = history
        .get_conversation(&id)
        .await
        .map_err(AppError::Runtime)?
        .ok_or_else(|| AppError::NotFound(format!("Conversation '{id}' not found")))?;

    Ok(Json(record))
}

#[utoipa::path(
    get,
    path = "/api/conversations/{id}/messages",
    tag = "History",
    responses(
        (status = 200, description = "Get messages for a conversation"),
        (status = 404, description = "Conversation not found"),
    )
)]
pub async fn get_conversation_messages_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    state.observability.record_http_request();
    let history = state
        .history_store
        .as_ref()
        .ok_or_else(|| AppError::Runtime("History feature is disabled or uninitialized".into()))?;

    let _record = history
        .get_conversation(&id)
        .await
        .map_err(AppError::Runtime)?
        .ok_or_else(|| AppError::NotFound(format!("Conversation '{id}' not found")))?;

    let messages = history
        .get_history(&id, 0)
        .await
        .map_err(AppError::Runtime)?;

    Ok(Json(messages))
}

#[utoipa::path(
    delete,
    path = "/api/conversations/{id}",
    tag = "History",
    responses(
        (status = 200, description = "Delete conversation and messages"),
    )
)]
pub async fn delete_conversation_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    state.observability.record_http_request();
    let history = state
        .history_store
        .as_ref()
        .ok_or_else(|| AppError::Runtime("History feature is disabled or uninitialized".into()))?;

    history
        .delete_conversation(&id)
        .await
        .map_err(AppError::Runtime)?;

    Ok(Json(serde_json::json!({
        "status": "deleted",
        "id": id
    })))
}

#[utoipa::path(
    post,
    path = "/api/conversations/{id}/export",
    tag = "History",
    responses(
        (status = 200, description = "Export full conversation with messages and summaries"),
        (status = 404, description = "Conversation not found"),
    )
)]
pub async fn export_conversation_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    state.observability.record_http_request();
    let history = state
        .history_store
        .as_ref()
        .ok_or_else(|| AppError::Runtime("History feature is disabled or uninitialized".into()))?;

    let export_data = history
        .export_conversation(&id)
        .await
        .map_err(AppError::Runtime)?;

    Ok(Json(export_data))
}
