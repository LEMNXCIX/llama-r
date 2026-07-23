use crate::api::chat_core::{execute_chat, execute_chat_stream, AgentSelection};
use crate::api::mcp_api;
use crate::api::observability::AppObservability;
use crate::context::analyzer::ContextEnricher;
use crate::context::store::ContextStore;
use crate::domain::models::{ChatRequest, ChatResponse, JsonRpcRequest, JsonRpcResponse, ListModelsResponse};
use crate::error::AppError;
use crate::optimizer::metrics::TokenMetrics;
use crate::ports::mcp::McpServerRegistry;
use crate::providers::LLMProvider;
use crate::services::agent_registry::AgentRegistry;
use crate::services::skill_manager::SkillManager;
use axum::{
    extract::State,
    response::{
        sse::{Event, Sse},
        IntoResponse,
    },
    Json,
};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};
use tokio_stream::StreamExt as _;

pub struct AppState {
    pub provider: Arc<dyn LLMProvider + Send + Sync>,
    pub agent_registry: Arc<AgentRegistry>,
    pub skill_manager: Arc<SkillManager>,
    pub context_store: Arc<ContextStore>,
    pub context_enricher: Arc<ContextEnricher>,
    pub metrics: Arc<TokenMetrics>,
    pub observability: Arc<AppObservability>,
    pub default_model: String,
    pub api_running: AtomicBool,
    pub grpc_running: AtomicBool,
    pub logs: Arc<Mutex<VecDeque<String>>>,
    /// Known MCP server IDs loaded from mcp-servers/*.toml
    pub known_mcp_servers: RwLock<Vec<String>>,
    /// Runtime MCP server registry for tool discovery and execution.
    pub mcp_registry: Arc<dyn McpServerRegistry>,
}

impl AppState {
    pub fn mcp_server_ids(&self) -> Vec<String> {
        self.known_mcp_servers
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn set_mcp_server_ids(&self, ids: Vec<String>) {
        if let Ok(mut guard) = self.known_mcp_servers.write() {
            *guard = ids;
        }
    }
}

#[utoipa::path(
    get,
    path = "/models",
    tag = "Models",
    responses(
        (status = 200, description = "List available models from the LLM provider", body = ListModelsResponse),
    )
)]
pub async fn list_models(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    state.observability.record_http_request();
    let models = state.provider.list_models().await.map_err(|error| {
        state.observability.record_provider_error();
        AppError::Provider(error.to_string())
    })?;
    Ok(Json(ListModelsResponse { models }))
}

#[utoipa::path(
    post,
    path = "/chat",
    tag = "Chat",
    request_body = ChatRequest,
    responses(
        (status = 200, description = "Chat response (JSON for non-streaming, SSE stream for streaming). Also mounted at /api/chat", body = ChatResponse),
    )
)]
pub async fn chat(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(payload): Json<ChatRequest>,
) -> Result<impl IntoResponse, AppError> {
    state.observability.record_http_request();
    let selection = AgentSelection {
        project_id: headers
            .get("x-project")
            .and_then(|value| value.to_str().ok()),
        agent_id: headers.get("x-agent").and_then(|value| value.to_str().ok()),
        debug: headers.get("x-debug").and_then(|value| value.to_str().ok()) == Some("true"),
    };

    if payload.stream {
        let stream = execute_chat_stream(&state, payload, selection).await?;
        let sse_stream = stream.map(|res| match res {
            Ok(event) => match serde_json::to_string(&event) {
                Ok(json_str) => Ok::<_, Infallible>(Event::default().data(json_str)),
                Err(error) => Ok::<_, Infallible>(
                    Event::default().data(format!("Serialization error: {}", error)),
                ),
            },
            Err(error) => Ok::<_, Infallible>(Event::default().data(format!("Error: {}", error))),
        });
        Ok(Sse::new(sse_stream).into_response())
    } else {
        let response = execute_chat(&state, payload, selection).await?;
        Ok(Json(response).into_response())
    }
}

#[utoipa::path(
    post,
    path = "/v1/chat/completions",
    tag = "Chat",
    request_body = ChatRequest,
    responses(
        (status = 200, description = "OpenAI-compatible chat response (JSON or SSE stream)", body = ChatResponse),
    )
)]
pub async fn openai_chat(
    state_extractor: State<Arc<AppState>>,
    axum::extract::RawQuery(_raw_query): axum::extract::RawQuery,
    headers: axum::http::HeaderMap,
    payload: Json<ChatRequest>,
) -> Result<impl IntoResponse, AppError> {
    chat(state_extractor, headers, payload).await
}

#[utoipa::path(
    post,
    path = "/api/mcp",
    tag = "MCP",
    request_body = JsonRpcRequest,
    responses(
        (status = 200, description = "MCP JSON-RPC response", body = JsonRpcResponse),
    )
)]
pub async fn mcp_message(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    state.observability.record_http_request();
    mcp_api::handle_message(state, payload).await
}
