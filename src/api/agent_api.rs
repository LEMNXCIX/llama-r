use crate::api::handlers::AppState;
use crate::domain::agent::{Agent, OptimizeConfig};
use crate::error::AppError;
use crate::services::validation::validate_identifier;
use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateAgentRequest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub model: String,
    pub system_prompt: String,
    #[serde(default)]
    pub context_project: Option<String>,
    #[serde(default)]
    pub context_files: Vec<String>,
    #[serde(default)]
    pub variables: HashMap<String, String>,
    #[serde(default)]
    pub optimize: OptimizeConfig,
}

#[derive(Debug, Serialize)]
pub struct AgentResponse {
    pub id: String,
    pub name: String,
    pub model: String,
    pub system_prompt: String,
    pub context_project: Option<String>,
    pub context_files: Vec<String>,
}

impl From<Agent> for AgentResponse {
    fn from(agent: Agent) -> Self {
        Self {
            id: agent.id,
            name: agent.config.name,
            model: agent.config.model,
            system_prompt: agent.config.system_prompt,
            context_project: agent.config.context_project,
            context_files: agent.config.context_files,
        }
    }
}

pub async fn list_agents_api(State(state): State<Arc<AppState>>) -> Json<Vec<AgentResponse>> {
    let agents = state
        .agent_manager
        .list_agents()
        .into_iter()
        .map(AgentResponse::from)
        .collect();
    Json(agents)
}

pub async fn get_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<AgentResponse>, AppError> {
    state
        .agent_manager
        .get_agent(&id)
        .map(|agent| Json(agent.into()))
        .ok_or_else(|| AppError::NotFound(format!("Agent '{}' not found", id)))
}

pub async fn create_agent(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateAgentRequest>,
) -> Result<impl IntoResponse, AppError> {
    validate_agent_request(&req)?;
    validate_model(&state, &req.model).await?;

    let toml_content = build_agent_toml(&req);
    let agents_dir = crate::core::paths::get_agents_dir();
    fs::create_dir_all(&agents_dir)?;
    let path = agents_dir.join(format!("{}.toml", req.id));

    fs::write(&path, &toml_content)?;
    if let Err(err) = state.agent_manager.load_agents() {
        tracing::warn!(error = %err, "Failed to reload agents after create");
    }

    tracing::info!(agent_id = %req.id, path = %path.display(), "Created agent");
    Ok((
        axum::http::StatusCode::CREATED,
        Json(serde_json::json!({
            "id": req.id,
            "message": format!("Agent '{}' created successfully", req.id),
            "path": path
        })),
    ))
}

pub async fn update_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<CreateAgentRequest>,
) -> Result<impl IntoResponse, AppError> {
    validate_agent_request(&req)?;
    validate_model(&state, &req.model).await?;

    let agents_dir = crate::core::paths::get_agents_dir();
    let path = agents_dir.join(format!("{}.toml", id));
    if !path.exists() {
        return Err(AppError::NotFound(format!("Agent '{}' not found", id)));
    }

    fs::write(&path, build_agent_toml(&req))?;
    if let Err(err) = state.agent_manager.load_agents() {
        tracing::warn!(error = %err, "Failed to reload agents after update");
    }

    Ok(Json(serde_json::json!({
        "id": id,
        "message": format!("Agent '{}' updated successfully", id)
    })))
}

pub async fn delete_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let agents_dir = crate::core::paths::get_agents_dir();
    let path = agents_dir.join(format!("{}.toml", id));
    if !path.exists() {
        return Err(AppError::NotFound(format!("Agent '{}' not found", id)));
    }

    fs::remove_file(&path)?;
    if let Err(err) = state.agent_manager.load_agents() {
        tracing::warn!(error = %err, "Failed to reload agents after delete");
    }

    Ok(Json(serde_json::json!({
        "message": format!("Agent '{}' deleted", id)
    })))
}

async fn validate_model(state: &AppState, model: &str) -> Result<(), AppError> {
    if model.is_empty() {
        return Ok(());
    }

    let models = state
        .provider
        .list_models()
        .await
        .map_err(|err| AppError::Provider(err.to_string()))?;
    let exists = models.iter().any(|candidate| candidate.name == model);
    if exists {
        Ok(())
    } else {
        Err(AppError::Validation(format!(
            "Model '{}' is not available. Available: {}",
            model,
            models
                .into_iter()
                .map(|candidate| candidate.name)
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

fn validate_agent_request(req: &CreateAgentRequest) -> Result<(), AppError> {
    validate_identifier(&req.id, "agent id")?;
    if req.name.trim().is_empty() {
        return Err(AppError::Validation("agent name cannot be empty".to_string()));
    }
    if req.system_prompt.trim().is_empty() {
        return Err(AppError::Validation(
            "system_prompt cannot be empty".to_string(),
        ));
    }
    if let Some(project_id) = &req.context_project {
        validate_identifier(project_id, "context_project")?;
    }
    Ok(())
}

fn build_agent_toml(req: &CreateAgentRequest) -> String {
    let mut output = format!(
        "name = \"{}\"\nmodel = \"{}\"\nsystem_prompt = \"\"\"\n{}\n\"\"\"\n",
        req.name, req.model, req.system_prompt
    );

    if let Some(project_id) = &req.context_project {
        output.push_str(&format!("context_project = \"{}\"\n", project_id));
    }

    if !req.context_files.is_empty() {
        let files: Vec<String> = req
            .context_files
            .iter()
            .map(|file| format!("\"{}\"", file))
            .collect();
        output.push_str(&format!("context_files = [{}]\n", files.join(", ")));
    }

    if !req.variables.is_empty() {
        output.push_str("[variables]\n");
        for (key, value) in &req.variables {
            output.push_str(&format!("{} = \"{}\"\n", key, value));
        }
    }

    if req.optimize.enabled {
        output.push_str("[optimize]\nenabled = true\n");
        if !req.optimize.rules.is_empty() {
            let rules: Vec<String> = req
                .optimize
                .rules
                .iter()
                .map(|rule| format!("\"{}\"", rule))
                .collect();
            output.push_str(&format!("rules = [{}]\n", rules.join(", ")));
        }
    }

    output
}
