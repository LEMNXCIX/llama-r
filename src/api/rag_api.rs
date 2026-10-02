//! Admin/debug RAG endpoints (`POST /api/rag/ingest`, `POST /api/rag/query`).
//!
//! Gated by `X-Debug: true` header (same pattern as chat debug flag).

use crate::api::handlers::AppState;
use crate::domain::scope::AgentScope;
use crate::error::AppError;
use crate::services::rag_ingest::{IngestDocument, IngestRequest, RagIngestService};
use axum::{extract::State, http::HeaderMap, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

fn require_debug(headers: &HeaderMap) -> Result<(), AppError> {
    let ok = headers.get("x-debug").and_then(|value| value.to_str().ok()) == Some("true");
    if ok {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "RAG admin endpoints require header X-Debug: true".into(),
        ))
    }
}

fn resolve_scope(
    state: &AppState,
    project_id: Option<&str>,
    agent_id: Option<&str>,
) -> Result<AgentScope, AppError> {
    let registered = state
        .agent_registry
        .resolve(project_id, agent_id)
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "agent not found (project={:?}, agent={:?})",
                project_id, agent_id
            ))
        })?;
    Ok(registered.scope.clone())
}

fn default_top_k() -> usize {
    5
}

#[derive(Debug, Deserialize)]
pub struct RagIngestBody {
    pub project_id: Option<String>,
    pub agent_id: Option<String>,
    /// Target collection; must be allowed by agent scope write policy.
    pub source_id: String,
    /// Raw texts to ingest.
    #[serde(default)]
    pub texts: Vec<String>,
    /// Optional server-local file paths under the Llama-R base dir.
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RagIngestResponse {
    pub chunks_written: usize,
    pub files_read: usize,
    pub source_id: String,
    /// Requested files that were not indexed, with the reason. Lets a caller tell
    /// "nothing to index" from "indexed, but some inputs were skipped".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedFileDto>,
}

#[derive(Debug, Serialize)]
pub struct SkippedFileDto {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct RagQueryBody {
    pub project_id: Option<String>,
    pub agent_id: Option<String>,
    pub query: String,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

#[derive(Debug, Serialize)]
pub struct RagQueryResponse {
    pub hits: Vec<RagHitDto>,
}

#[derive(Debug, Serialize)]
pub struct RagHitDto {
    pub id: String,
    pub source_id: String,
    pub text: String,
    pub score: f32,
}

/// `POST /api/rag/ingest` — chunk + embed + upsert under agent scope.
pub async fn rag_ingest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<RagIngestBody>,
) -> Result<impl IntoResponse, AppError> {
    state.observability.record_http_request();
    require_debug(&headers)?;

    let store = state.rag_store.as_ref().ok_or_else(|| {
        AppError::Unavailable("RAG is disabled (RAG_ENABLED=false or unavailable)".into())
    })?;
    let ingest = state
        .rag_ingest
        .clone()
        .unwrap_or_else(|| Arc::new(RagIngestService::new(store.clone())));

    let scope = resolve_scope(&state, body.project_id.as_deref(), body.agent_id.as_deref())?;

    let mut chunks_written = 0usize;
    let mut files_read = 0usize;
    let mut skipped: Vec<SkippedFileDto> = Vec::new();

    if !body.texts.is_empty() {
        let documents = body
            .texts
            .into_iter()
            .enumerate()
            .map(|(i, text)| IngestDocument {
                id_hint: Some(format!("text-{i}")),
                text,
                metadata: serde_json::json!({ "kind": "api_text" }),
            })
            .collect();
        let result = ingest
            .ingest(IngestRequest {
                scope: scope.clone(),
                source_id: body.source_id.clone(),
                documents,
                chunk: Default::default(),
            })
            .await?;
        chunks_written += result.chunks_written;
    }

    if !body.files.is_empty() {
        let base = crate::core::paths::get_base_dir();
        let paths: Vec<_> = body
            .files
            .iter()
            .map(|p| {
                let path = std::path::PathBuf::from(p);
                if path.is_absolute() {
                    path
                } else {
                    base.join(path)
                }
            })
            .collect();
        let result = ingest
            .ingest_files(&scope, &body.source_id, &paths, Some(&base))
            .await?;
        chunks_written += result.chunks_written;
        files_read += result.files_read;
        skipped.extend(result.skipped.into_iter().map(|item| SkippedFileDto {
            path: item.path,
            reason: item.reason,
        }));
    }

    if chunks_written == 0 && files_read == 0 {
        return Err(AppError::Validation(
            "nothing to ingest: provide non-empty `texts` and/or readable `files`".into(),
        ));
    }

    Ok(Json(RagIngestResponse {
        chunks_written,
        files_read,
        source_id: body.source_id,
        skipped,
    }))
}

/// `POST /api/rag/query` — scoped similarity search for debugging.
pub async fn rag_query(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<RagQueryBody>,
) -> Result<impl IntoResponse, AppError> {
    state.observability.record_http_request();
    require_debug(&headers)?;

    let store = state.rag_store.as_ref().ok_or_else(|| {
        AppError::Unavailable("RAG is disabled (RAG_ENABLED=false or unavailable)".into())
    })?;

    if body.query.trim().is_empty() {
        return Err(AppError::Validation("query must not be empty".into()));
    }

    let scope = resolve_scope(&state, body.project_id.as_deref(), body.agent_id.as_deref())?;

    let hits = store
        .query_scoped(&scope, &body.query, body.top_k.max(1))
        .await
        .map_err(|err| {
            if err.starts_with(crate::services::rag_ingest::RAG_WRITE_DENIED) {
                AppError::Validation(err)
            } else {
                AppError::Runtime(err)
            }
        })?;

    Ok(Json(RagQueryResponse {
        hits: hits
            .into_iter()
            .map(|h| RagHitDto {
                id: h.id,
                source_id: h.source_id,
                text: h.text,
                score: h.score,
            })
            .collect(),
    }))
}
