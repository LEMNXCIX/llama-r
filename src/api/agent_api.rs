use crate::api::handlers::AppState;
use crate::domain::agent::{Agent, OptimizeConfig};
use crate::domain::scope::AgentScope;
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
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateAgentRequest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub model: String,
    pub system_prompt: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub context_project: Option<String>,
    #[serde(default)]
    pub context_files: Vec<String>,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub variables: HashMap<String, String>,
    #[serde(default)]
    pub optimize: OptimizeConfig,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AgentResponse {
    pub id: String,
    pub project_id: Option<String>,
    pub qualified_id: String,
    pub name: String,
    pub model: String,
    pub system_prompt: String,
    pub context_project: Option<String>,
    pub context_files: Vec<String>,
    pub rules: Vec<String>,
    pub skills: Vec<String>,
    pub auto_skills: Vec<String>,
}

impl From<Agent> for AgentResponse {
    fn from(agent: Agent) -> Self {
        let qualified_id = agent.qualified_id();
        let project_id = agent.project_id.clone();

        Self {
            id: agent.id,
            project_id,
            qualified_id,
            name: agent.config.name,
            model: agent.config.model,
            system_prompt: agent.config.system_prompt,
            context_project: agent.config.context_project,
            context_files: agent.config.context_files,
            rules: agent.config.rules,
            skills: agent.config.skills,
            auto_skills: agent.config.auto_skills,
        }
    }
}

fn project_header(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-project")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn resolve_project_id(req: &CreateAgentRequest, headers: &axum::http::HeaderMap) -> Option<String> {
    req.project_id
        .clone()
        .or_else(|| req.context_project.clone())
        .or_else(|| project_header(headers))
}

fn resolve_agent_path(project_id: Option<&str>, agent_id: &str) -> PathBuf {
    match project_id {
        Some(project_id) => crate::core::paths::get_project_agents_dir(project_id)
            .join(format!("{}.toml", agent_id)),
        None => crate::core::paths::get_agents_dir().join(format!("{}.toml", agent_id)),
    }
}

#[utoipa::path(
    get,
    path = "/api/agents",
    tag = "Agents",
    responses(
        (status = 200, description = "List all registered agents", body = Vec<AgentResponse>),
    )
)]
pub async fn list_agents_api(State(state): State<Arc<AppState>>) -> Json<Vec<AgentResponse>> {
    log::info!("Listando agentes:");
    let agents = state
        .agent_registry
        .list_agents()
        .into_iter()
        .map(AgentResponse::from)
        .collect();
    Json(agents)
}

#[utoipa::path(
    get,
    path = "/api/agents/{id}",
    tag = "Agents",
    params(
        ("id" = String, Path, description = "Agent ID"),
    ),
    responses(
        (status = 200, description = "Agent details", body = AgentResponse),
        (status = 404, description = "Agent not found"),
    )
)]
pub async fn get_agent(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<AgentResponse>, AppError> {
    let project_id = project_header(&headers);
    let agent = match project_id.as_deref() {
        Some(project_id) => state.agent_registry.get_project_agent(project_id, &id),
        None => state.agent_registry.get_agent(&id).or_else(|| {
            state
                .agent_registry
                .list_agents()
                .into_iter()
                .find(|a| a.id == id)
        }),
    };

    agent
        .map(|agent| Json(agent.into()))
        .ok_or_else(|| AppError::NotFound(format!("Agent '{}' not found", id)))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AgentScopeResponse {
    pub agent_id: String,
    pub project_id: Option<String>,
    pub mcp_sources: Vec<String>,
    pub tools_allow: String,
    pub rag_sources: Vec<String>,
    pub rag_write: String,
    pub max_tool_calls: u32,
    pub max_iterations: u32,
    pub timeout_secs: u64,
}

impl From<AgentScope> for AgentScopeResponse {
    fn from(scope: AgentScope) -> Self {
        let tools_allow = match &scope.tools_allow {
            crate::domain::scope::ToolsAllow::DenyAll => "deny_all".into(),
            crate::domain::scope::ToolsAllow::AllFromSources => "all_from_sources".into(),
            crate::domain::scope::ToolsAllow::Allowlist(list) => {
                let items: Vec<String> = list.iter().map(|t| t.qualified()).collect();
                format!("allowlist({})", items.join(", "))
            }
        };
        Self {
            agent_id: scope.agent_id,
            project_id: scope.project_id,
            mcp_sources: scope.mcp_sources.into_iter().collect(),
            tools_allow,
            rag_sources: scope.rag_sources.into_iter().collect(),
            rag_write: format!("{:?}", scope.rag_write),
            max_tool_calls: scope.max_tool_calls,
            max_iterations: scope.max_iterations,
            timeout_secs: scope.timeout_secs,
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/agents/{id}/scope",
    tag = "Agents",
    params(
        ("id" = String, Path, description = "Agent ID"),
    ),
    responses(
        (status = 200, description = "Agent scope (tools, MCP sources, RAG)", body = AgentScopeResponse),
        (status = 404, description = "Agent not found"),
    )
)]
pub async fn get_agent_scope(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<AgentScopeResponse>, AppError> {
    let project_id = headers
        .get("x-project")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let scope = state
        .agent_registry
        .resolve_scope(project_id, Some(&id))
        .or_else(|| {
            if project_id.is_some() {
                return None;
            }
            state
                .agent_registry
                .list_agents()
                .into_iter()
                .find(|a| a.id == id)
                .and_then(|a| {
                    state
                        .agent_registry
                        .resolve_scope(a.project_id.as_deref(), Some(&id))
                })
        })
        .ok_or_else(|| AppError::NotFound(format!("Agent '{}' not found", id)))?;
    Ok(Json(scope.into()))
}

#[utoipa::path(
    post,
    path = "/api/agents",
    tag = "Agents",
    request_body = CreateAgentRequest,
    responses(
        (status = 201, description = "Agent created", body = serde_json::Value),
        (status = 400, description = "Validation error"),
    )
)]
pub async fn create_agent(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CreateAgentRequest>,
) -> Result<impl IntoResponse, AppError> {
    let project_id = resolve_project_id(&req, &headers);
    validate_agent_request(&state, &req, project_id.as_deref())?;
    validate_model(&state, &req.model).await?;

    let toml_content = build_agent_toml(&req, project_id.as_deref());
    let path = resolve_agent_path(project_id.as_deref(), &req.id);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(&path, &toml_content)?;
    if let Err(err) = state.agent_registry.reload_all(&state.mcp_server_ids()) {
        tracing::warn!(error = %err, "Failed to reload agents after create");
    }

    tracing::info!(agent_id = %req.id, project_id = ?project_id, path = %path.display(), "Created agent");
    Ok((
        axum::http::StatusCode::CREATED,
        Json(serde_json::json!({
            "id": req.id,
            "project_id": project_id,
            "message": "Agent created successfully",
            "path": path
        })),
    ))
}

#[utoipa::path(
    put,
    path = "/api/agents/{id}",
    tag = "Agents",
    params(
        ("id" = String, Path, description = "Agent ID"),
    ),
    request_body = CreateAgentRequest,
    responses(
        (status = 200, description = "Agent updated", body = serde_json::Value),
        (status = 404, description = "Agent not found"),
    )
)]
pub async fn update_agent(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<CreateAgentRequest>,
) -> Result<impl IntoResponse, AppError> {
    let project_id = resolve_project_id(&req, &headers);
    validate_agent_request(&state, &req, project_id.as_deref())?;
    validate_model(&state, &req.model).await?;

    let path = resolve_agent_path(project_id.as_deref(), &id);
    if !path.exists() {
        return Err(AppError::NotFound(format!("Agent '{}' not found", id)));
    }

    fs::write(&path, build_agent_toml(&req, project_id.as_deref()))?;
    if let Err(err) = state.agent_registry.reload_all(&state.mcp_server_ids()) {
        tracing::warn!(error = %err, "Failed to reload agents after update");
    }

    Ok(Json(serde_json::json!({
        "id": id,
        "project_id": project_id,
        "message": "Agent updated successfully"
    })))
}

#[utoipa::path(
    delete,
    path = "/api/agents/{id}",
    tag = "Agents",
    params(
        ("id" = String, Path, description = "Agent ID"),
    ),
    responses(
        (status = 200, description = "Agent deleted", body = serde_json::Value),
        (status = 404, description = "Agent not found"),
    )
)]
pub async fn delete_agent(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let project_id = project_header(&headers);
    let resolved_project = project_id.clone().or_else(|| {
        state
            .agent_registry
            .list_agents()
            .into_iter()
            .find(|a| a.id == id)
            .and_then(|a| a.project_id)
    });
    let path = resolve_agent_path(resolved_project.as_deref(), &id);
    if !path.exists() {
        return Err(AppError::NotFound(format!("Agent '{}' not found", id)));
    }

    fs::remove_file(&path)?;
    if let Err(err) = state.agent_registry.reload_all(&state.mcp_server_ids()) {
        tracing::warn!(error = %err, "Failed to reload agents after delete");
    }

    Ok(Json(serde_json::json!({
        "project_id": project_id,
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

fn validate_agent_request(
    state: &AppState,
    req: &CreateAgentRequest,
    project_id: Option<&str>,
) -> Result<(), AppError> {
    validate_identifier(&req.id, "agent id")?;
    if req.name.trim().is_empty() {
        return Err(AppError::Validation(
            "agent name cannot be empty".to_string(),
        ));
    }
    if req.system_prompt.trim().is_empty() {
        return Err(AppError::Validation(
            "system_prompt cannot be empty".to_string(),
        ));
    }
    if let Some(project_id) = &req.project_id {
        validate_identifier(project_id, "project_id")?;
    }
    if let Some(project_id) = &req.context_project {
        validate_identifier(project_id, "context_project")?;
    }
    if req.rules.iter().any(|rule| rule.trim().is_empty()) {
        return Err(AppError::Validation(
            "agent rules cannot contain empty values".to_string(),
        ));
    }

    let project_path = project_id
        .and_then(|id| state.context_store.get_context(id))
        .map(|context| context.path);

    for skill_id in &req.skills {
        validate_identifier(skill_id, "skill_id")?;
        let exists = project_path
            .as_ref()
            .and_then(|path| {
                state
                    .skill_manager
                    .get_skill_for_project(skill_id, FsPath::new(path))
            })
            .or_else(|| state.skill_manager.get_skill(skill_id));
        if exists.is_none() {
            return Err(AppError::Validation(format!(
                "Skill '{}' is not installed or could not be loaded",
                skill_id
            )));
        }
    }
    Ok(())
}

fn build_agent_toml(req: &CreateAgentRequest, project_id: Option<&str>) -> String {
    let mut output = format!(
        "name = \"{}\"\nmodel = \"{}\"\nsystem_prompt = \"\"\"\n{}\n\"\"\"\n",
        req.name, req.model, req.system_prompt
    );

    let context_project = req.context_project.as_deref().or(project_id);
    if let Some(project_id) = context_project {
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

    if !req.rules.is_empty() {
        let rules: Vec<String> = req
            .rules
            .iter()
            .map(|rule| format!("\"{}\"", rule.replace('"', "\\\"")))
            .collect();
        output.push_str(&format!("rules = [{}]\n", rules.join(", ")));
    }

    if !req.skills.is_empty() {
        let skills: Vec<String> = req
            .skills
            .iter()
            .map(|skill| format!("\"{}\"", skill))
            .collect();
        output.push_str(&format!("skills = [{}]\n", skills.join(", ")));
    } else {
        output.push_str("skills = []\n");
    }
    output.push_str("auto_skills = []\n");

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
