use crate::api::agent_api::{AgentResponse, AgentScopeResponse, CreateAgentRequest};
use crate::api::chat_core::RoutedDebug;
use crate::api::context_api::CreateContextRequest;
use crate::api::health::{HealthResponse, RouterHealth};
use crate::api::observability::ObservabilitySnapshot;
use crate::api::route_api::{RouteCandidateView, RoutePreviewRequest, RoutePreviewResponse};
use crate::context::store::ProjectContext;
use crate::domain::agent::OptimizeConfig;
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
        crate::api::route_api::route_preview,
    ),
    components(
        schemas(
            // utoipa 5 collects transitively referenced schemas on its own:
            // every type below that is reachable from a registered path or
            // schema is emitted even when it is not listed here (verified by
            // deleting entries and re-generating — the document stayed
            // complete). The entries that merely document a dependency —
            // `RouterHealth`, `ObservabilitySnapshot`, `RoutedDebug`,
            // `OptimizeConfig` — are therefore explicit documentation of the
            // graph rather than a repair, and they keep the document correct
            // if utoipa's collection behaviour ever changes.
            ChatRequest,
            ChatResponse,
            ChatStreamEvent,
            ChatMessage,
            ModelInfo,
            ListModelsResponse,
            JsonRpcRequest,
            JsonRpcResponse,
            HealthResponse,
            RouterHealth,
            ObservabilitySnapshot,
            RoutedDebug,
            CreateAgentRequest,
            OptimizeConfig,
            AgentResponse,
            AgentScopeResponse,
            CreateContextRequest,
            ProjectContext,
            RoutePreviewRequest,
            RoutePreviewResponse,
            RouteCandidateView,
        )
    ),
    tags(
        (name = "Health", description = "Health check endpoints"),
        (name = "Chat", description = "Chat completion endpoints"),
        (name = "Models", description = "Model listing endpoints"),
        (name = "Agents", description = "Agent management CRUD"),
        (name = "Contexts", description = "Project context management"),
        (name = "MCP", description = "Model Context Protocol endpoints"),
        (name = "Router", description = "Routing decision dry-run, for tuning MIN_CONFIDENCE"),
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
