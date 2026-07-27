use crate::api::agent_api::{AgentResponse, AgentScopeResponse, CreateAgentRequest};
use crate::api::context_api::CreateContextRequest;
use crate::api::health::HealthResponse;
use crate::context::store::ProjectContext;
use crate::domain::models::{
    ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, JsonRpcRequest, JsonRpcResponse,
    ListModelsResponse, ModelInfo,
};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Llama-R API",
        description = "Personal AI gateway — chat, agents, contexts, MCP, and health endpoints",
        version = "0.1.0"
    ),
    paths(
        crate::api::health::health,
        crate::api::handlers::list_models,
        crate::api::handlers::chat,
        crate::api::handlers::openai_chat,
        crate::api::handlers::mcp_message,
        crate::api::agent_api::list_agents_api,
        crate::api::agent_api::get_agent,
        crate::api::agent_api::get_agent_scope,
        crate::api::agent_api::create_agent,
        crate::api::agent_api::update_agent,
        crate::api::agent_api::delete_agent,
        crate::api::context_api::list_contexts,
        crate::api::context_api::get_context,
        crate::api::context_api::create_context,
        crate::api::context_api::update_context,
        crate::api::context_api::delete_context,
        crate::api::context_api::analyze_project,
    ),
    components(
        schemas(
            ChatRequest,
            ChatResponse,
            ChatStreamEvent,
            ChatMessage,
            ModelInfo,
            ListModelsResponse,
            JsonRpcRequest,
            JsonRpcResponse,
            HealthResponse,
            CreateAgentRequest,
            AgentResponse,
            AgentScopeResponse,
            CreateContextRequest,
            ProjectContext,
        )
    ),
    tags(
        (name = "Health", description = "Health check endpoints"),
        (name = "Chat", description = "Chat completion endpoints"),
        (name = "Models", description = "Model listing endpoints"),
        (name = "Agents", description = "Agent management CRUD"),
        (name = "Contexts", description = "Project context management"),
        (name = "MCP", description = "Model Context Protocol endpoints"),
    )
)]
pub struct ApiDoc;

use axum::response::{Html, IntoResponse, Json};

pub async fn serve_openapi_json() -> impl IntoResponse {
    Json(ApiDoc::openapi())
}

pub async fn serve_scalar_ui() -> impl IntoResponse {
    Html(
        r#"<!DOCTYPE html>
<html>
<head>
    <title>Llama-R API Reference</title>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <style>
        body { margin: 0; padding: 0; }
    </style>
</head>
<body>
    <script id="api-reference" data-url="/openapi.json"></script>
    <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
</body>
</html>"#,
    )
}
