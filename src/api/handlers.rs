use crate::api::chat_core::{execute_chat, execute_chat_stream};
use crate::api::mcp_api;
use crate::api::observability::AppObservability;
use crate::api::route_resolution::AgentSelection;
use crate::context::analyzer::ContextEnricher;
use crate::context::store::ContextStore;
use crate::domain::models::{
    ChatRequest, ChatResponse, JsonRpcRequest, JsonRpcResponse, ListModelsResponse,
};
use crate::error::AppError;
use crate::optimizer::metrics::TokenMetrics;
use crate::ports::mcp::McpServerRegistry;
use crate::services::agent_registry::AgentRegistry;
use crate::services::approval::ApprovalGate;
use crate::services::provider_stack::ProviderStack;
use crate::services::router_config::RouterConfig;
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::RwLock as TokioRwLock;
use tokio_stream::StreamExt as _;

use crate::ports::history::ConversationStore;

pub struct AppState {
    /// The provider-dependent services, behind an `RwLock` so a new provider can
    /// be installed in the running process without rebuilding `AppState`. Every
    /// read clones the inner `Arc` before any `.await`; a snapshot taken before a
    /// swap keeps the old stack alive until the last in-flight request finishes.
    pub services: Arc<TokioRwLock<Arc<ProviderStack>>>,
    pub agent_registry: Arc<AgentRegistry>,
    pub skill_manager: Arc<SkillManager>,
    pub context_store: Arc<ContextStore>,
    /// The skill-ranking enricher used by `chat_core`. Behind a `Mutex` so a
    /// settings Apply can rebuild it from the new stack's embedder: a boot-time
    /// `Arc` would keep ranking skills with the old provider after a swap. Every
    /// read snapshots the inner `Arc` before any `.await`, so the guard is never
    /// held across one.
    pub context_enricher: Mutex<Arc<ContextEnricher>>,
    pub metrics: Arc<TokenMetrics>,
    pub observability: Arc<AppObservability>,
    pub api_running: AtomicBool,
    pub grpc_running: AtomicBool,
    pub logs: Arc<Mutex<VecDeque<String>>>,
    /// Known MCP server IDs loaded from mcp-servers/*.toml
    pub known_mcp_servers: RwLock<Vec<String>>,
    /// Runtime MCP server registry for tool discovery and execution.
    pub mcp_registry: Arc<dyn McpServerRegistry>,
    /// Conversation history store. None when feature is off or uninitialized.
    pub history_store: Option<Arc<dyn ConversationStore>>,
    /// Router configuration loaded at boot (env, then `router.toml`).
    pub router_config: Arc<RouterConfig>,
    /// Last known router reachability, set by the ladder. Deliberately not a live
    /// probe: `health()` already probes the provider on every call.
    pub router_healthy: AtomicBool,
    /// Human-approval gate. Built once at boot and deliberately NOT part of the
    /// hot-swapped `ProviderStack`: a TUI Apply rebuilds the engine and its
    /// companions but must not drop pending decisions. A dropped request leaves
    /// its pending in the gate until timeout (Fase 7 seeds a background reaper).
    pub approval_gate: Arc<ApprovalGate>,
}

impl AppState {
    /// Clone the current `ProviderStack` handle. The read guard is dropped
    /// before returning, so no lock is held across an await by the caller.
    pub async fn services_snapshot(&self) -> Arc<ProviderStack> {
        let guard = self.services.read().await;
        Arc::clone(&*guard)
    }

    /// Replace the whole provider stack. The write guard is released as soon as
    /// the value is installed; it is never held across an await.
    pub async fn replace_services(&self, new: ProviderStack) {
        let mut guard = self.services.write().await;
        *guard = Arc::new(new);
    }

    /// Clone the current skill-ranking enricher. The guard is dropped before
    /// returning, so a caller can `.await` on the returned `Arc` with no lock
    /// held — the same discipline as [`Self::services_snapshot`].
    pub fn context_enricher_snapshot(&self) -> Arc<ContextEnricher> {
        let guard = self
            .context_enricher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(&*guard)
    }

    /// Install a rebuilt enricher (with the new stack's embedder). The guard is
    /// released as soon as the value is installed; it is never held across an
    /// await.
    pub fn replace_context_enricher(&self, new: Arc<ContextEnricher>) {
        let mut guard = self
            .context_enricher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = new;
    }

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

    /// Record the router's last known reachability. The ladder calls this after
    /// every decision — `true` when d1 answered, `false` when the call errored —
    /// and `/health` reads the last value rather than probing again.
    pub fn record_router_health(&self, healthy: bool) {
        self.router_healthy.store(healthy, Ordering::SeqCst);
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
    let services = state.services_snapshot().await;
    let models = services.provider.list_models().await.map_err(|error| {
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
        conversation_id: headers
            .get("x-conversation-id")
            .and_then(|value| value.to_str().ok()),
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
