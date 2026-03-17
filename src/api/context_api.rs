use crate::api::handlers::AppState;
use crate::context::store::ProjectContext;
use crate::domain::models::{ChatMessage, ChatRequest};
use crate::error::AppError;
use crate::services::agent_skill_sync::{summarize_sync_report, sync_project_agent_skills};
use crate::services::validation::{canonicalize_project_path, validate_identifier};
use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateContextRequest {
    pub project_id: String,
    pub project_path: String,
    #[serde(default = "default_true")]
    pub auto_analyze: bool,
    #[serde(default = "default_true")]
    pub inject_skills: bool,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub custom_rules: String,
}

fn default_true() -> bool {
    true
}

pub async fn list_contexts(State(state): State<Arc<AppState>>) -> Json<Vec<serde_json::Value>> {
    let summaries = state
        .context_store
        .list_contexts()
        .iter()
        .map(|context| {
            serde_json::json!({
                "project_id": context.project_id,
                "project_type": context.project_type,
                "path": context.path,
                "skills_injected": context.skills_injected,
                "last_analyzed": context.last_analyzed,
            })
        })
        .collect();
    Json(summaries)
}

pub async fn get_context(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ProjectContext>, AppError> {
    state
        .context_store
        .get_context(&id)
        .map(Json)
        .ok_or_else(|| AppError::NotFound(format!("Context '{}' not found", id)))
}

pub async fn create_context(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateContextRequest>,
) -> Result<impl IntoResponse, AppError> {
    validate_identifier(&req.project_id, "project_id")?;
    if let Some(agent_id) = &req.agent_id {
        validate_identifier(agent_id, "agent_id")?;
    }
    let canonical_path = canonicalize_project_path(&req.project_path)?;

    if state.context_store.get_context(&req.project_id).is_some() {
        return Err(AppError::Conflict(format!(
            "Context for '{}' already exists. Use re-analyze to refresh it.",
            req.project_id
        )));
    }

    if req.auto_analyze {
        let provider = state.provider.clone();
        let responder = move |prompt: String| {
            let provider = provider.clone();
            Box::pin(async move {
                let chat_req = ChatRequest {
                    model: std::env::var("DEFAULT_MODEL").unwrap_or_else(|_| "llama3".to_string()),
                    messages: vec![ChatMessage {
                        role: "user".to_string(),
                        content: prompt,
                    }],
                    stream: false,
                };
                provider
                    .chat(chat_req)
                    .await
                    .map(|response| response.message.content)
                    .map_err(|err| err.to_string())
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
        };

        let analyzer = crate::context::analyzer::ProjectAnalyzer::new(state.skill_manager.clone());
        let mut ctx = analyzer
            .analyze(
                &req.project_id,
                &canonical_path.to_string_lossy(),
                responder,
            )
            .await
            .map_err(AppError::Runtime)?;
        ctx.custom_rules = req.custom_rules.clone();
        state.context_store.save_context(ctx.clone())?;
        let sync_report = sync_project_agent_skills(&state, &ctx).await?;
        Ok((
            axum::http::StatusCode::CREATED,
            Json(serde_json::json!({
                "project_id": req.project_id,
                "message": format!("Context for '{}' generated and saved", req.project_id),
                "auto_analyzed": true,
                "agent_skill_sync": summarize_sync_report(&sync_report)
            })),
        ))
    } else {
        let ctx = ProjectContext {
            project_id: req.project_id.clone(),
            path: canonical_path.to_string_lossy().into_owned(),
            context_md: req.custom_rules.clone(),
            project_type: "unknown".to_string(),
            skills_injected: vec![],
            last_analyzed: chrono::Utc::now(),
            custom_rules: req.custom_rules.clone(),
        };
        state.context_store.save_context(ctx.clone())?;
        Ok((
            axum::http::StatusCode::CREATED,
            Json(serde_json::json!({
                "project_id": req.project_id,
                "message": "Context saved",
                "auto_analyzed": false
            })),
        ))
    }
}

pub async fn update_context(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut ctx = state
        .context_store
        .get_context(&id)
        .ok_or_else(|| AppError::NotFound(format!("Context '{}' not found", id)))?;

    if let Some(markdown) = body.get("context_md").and_then(|value| value.as_str()) {
        ctx.context_md = markdown.to_string();
    }
    if let Some(custom_rules) = body.get("custom_rules").and_then(|value| value.as_str()) {
        ctx.custom_rules = custom_rules.to_string();
    }

    state.context_store.save_context(ctx.clone())?;
    Ok(Json(serde_json::json!({
        "message": format!("Context '{}' updated", id)
    })))
}

pub async fn delete_context(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    if state.context_store.delete_context(&id) {
        Ok(Json(serde_json::json!({
            "message": format!("Context '{}' deleted", id)
        })))
    } else {
        Err(AppError::NotFound(format!("Context '{}' not found", id)))
    }
}

pub async fn analyze_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let existing = state
        .context_store
        .get_context(&id)
        .ok_or_else(|| AppError::NotFound(format!("Context '{}' not found", id)))?;

    let provider = state.provider.clone();
    let path = existing.path.clone();
    let responder = move |prompt: String| {
        let provider = provider.clone();
        Box::pin(async move {
            let chat_req = ChatRequest {
                model: std::env::var("DEFAULT_MODEL").unwrap_or_else(|_| "llama3".to_string()),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: prompt,
                }],
                stream: false,
            };
            provider
                .chat(chat_req)
                .await
                .map(|response| response.message.content)
                .map_err(|err| err.to_string())
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
    };

    let analyzer = crate::context::analyzer::ProjectAnalyzer::new(state.skill_manager.clone());
    let ctx = analyzer
        .analyze(&id, &path, responder)
        .await
        .map_err(AppError::Runtime)?;
    state.context_store.save_context(ctx.clone())?;
    let sync_report = sync_project_agent_skills(&state, &ctx).await?;
    Ok(Json(serde_json::json!({
        "project_id": id,
        "message": "Project re-analyzed successfully",
        "agent_skill_sync": summarize_sync_report(&sync_report)
    })))
}



