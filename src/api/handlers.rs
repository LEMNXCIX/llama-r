use crate::api::chat_core::{
    execute_chat_resolved, execute_chat_stream_with_conversation_id,
    execute_chat_with_new_id_resolved, require_messages,
};
use crate::api::mcp_api;
use crate::api::observability::AppObservability;
use crate::api::route_resolution::{resolve_selection, AgentSelection, ResolvedSelection};
use crate::config::Config;
use crate::context::analyzer::ContextEnricher;
use crate::context::store::ContextStore;
use crate::domain::models::{
    ChatAck, ChatRequest, ChatResponse, JsonRpcRequest, JsonRpcResponse, ListModelsResponse,
};
use crate::error::AppError;
use crate::optimizer::metrics::TokenMetrics;
use crate::ports::auto::AutoDecider;
use crate::ports::channel::ChannelSignatureVerifier;
use crate::ports::delegation::Delegator;
use crate::ports::execution::ExecutionStore;
use crate::ports::mcp::McpServerRegistry;
use crate::ports::model_selection::ModelSelector;
use crate::ports::telemetry::TraceExporter;
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
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use tokio::sync::{oneshot, RwLock as TokioRwLock};
use tokio_stream::StreamExt as _;
use uuid::Uuid;

use crate::ports::compaction::ContextCompactor;
use crate::ports::history::ConversationStore;

pub struct AppState {
    /// Boot configuration — the single source of truth for the admin token,
    /// parking/grace/reaper timings and OTLP settings. Kept on the state handle
    /// so the middleware and chat handler do not re-read the environment per
    /// request, while the hot-swapped `ProviderStack` stays separate.
    pub config: Config,
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
    /// Optional automatic-approval decider, built at boot from the same
    /// `DecisionRouter` that serves agent routing when routing is enabled. Like
    /// `approval_gate`, it lives outside the hot-swapped `ProviderStack`, so a
    /// settings Apply cannot drop it. `None` means every `Auto` tool keeps
    /// asking a human (the pre-Fase-6 behaviour).
    pub auto_decider: Option<Arc<dyn AutoDecider>>,
    /// Context-compaction strategy used by the assembler on every agent turn.
    /// Boot installs `SummarizingCompactor` with the selected provider's
    /// summarizer, or `NoopCompactor` when no summarizer is usable (never the
    /// quiet summarizer with the real compactor — that would drop history).
    /// It lives outside the hot-swapped stack so a settings Apply rebuilds the
    /// engine with the same strategy.
    pub compactor: Arc<dyn ContextCompactor>,
    /// Best-effort model selector built at boot from the same decision router
    /// agent routing uses (or `CurrentModelSelector` when routing is off). Like
    /// the compactor, it lives outside the hot-swapped stack so a settings Apply
    /// rebuilds the engine with the same selector.
    pub selector: Arc<dyn ModelSelector>,
    /// Durable execution store. Built once at boot from the history DB path and
    /// deliberately NOT part of the hot-swapped `ProviderStack`: a TUI Apply
    /// rebuilds the engine but must not swap out the record of in-flight turns.
    /// A no-op store is installed when `history` is off or the SQLite store
    /// cannot be opened, so its presence never gates boot or a chat.
    pub execution_store: Arc<dyn ExecutionStore>,
    /// Bounded subagent delegator. Installed once at boot: the `RuntimeDelegator`
    /// holds a `Weak<AppState>` and a registry, so it must be built *after* this
    /// `Arc<AppState>` exists — hence the `OnceLock` instead of a plain field.
    /// It is not part of the hot-swapped `ProviderStack` (it holds no provider),
    /// so a settings Apply cannot lose it. `None` until boot installs it; a
    /// caller that finds it absent gets a named error, never a panic.
    pub delegator: OnceLock<Arc<dyn Delegator>>,
    /// Signature verifier for inbound webhook channels. Built once at boot from
    /// the HMAC adapter and deliberately kept outside the hot-swapped
    /// `ProviderStack`: a settings Apply rebuilds companions but must not drop
    /// the verifier a signed request depends on.
    pub channel_verifier: Arc<dyn ChannelSignatureVerifier>,
    /// Best-effort trace exporter built once at boot from the environment
    /// (`OTEL_ENABLED` and friends). `NoopExporter` when disabled, so the
    /// default build issues zero telemetry requests. Like the gate and the
    /// execution store it lives outside the hot-swapped `ProviderStack`: a
    /// settings Apply rebuilds companions but must not change telemetry wiring.
    pub exporter: Arc<dyn TraceExporter>,
}

impl AppState {
    /// Clone the current `ProviderStack` handle. The read guard is dropped
    /// before returning, so no lock is held across an await by the caller.
    pub async fn services_snapshot(&self) -> Arc<ProviderStack> {
        let guard = self.services.read().await;
        Arc::clone(&*guard)
    }

    /// Install the boot-built delegator. Called exactly once, right after the
    /// `Arc<AppState>` exists, so its `Weak` has something to upgrade. A second
    /// call is ignored rather than replacing a live handle.
    pub fn set_delegator(&self, delegator: Arc<dyn Delegator>) {
        let _ = self.delegator.set(delegator);
    }

    /// Clone the installed delegator, or `None` before boot installs it.
    pub fn delegator(&self) -> Option<Arc<dyn Delegator>> {
        self.delegator.get().cloned()
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

/// Report whether the agent the request was resolved to carries approval
/// rules. This is the one check that decides whether a non-streaming chat needs
/// the park-and-acknowledge path; a turn without rules must stay synchronous and
/// byte-identical to the pre-parking behaviour.
///
/// The selection is resolved by the caller **once**, before this check, and the
/// same owned value is threaded into the executed turn. Resolving here too (as
/// this function used to) spent a second d1 decision on every routed chat — and
/// on a gated turn the spawned task could execute a *different* agent than the
/// one whose policy was checked.
fn turn_has_approval_rules(
    state: &AppState,
    resolved: &ResolvedSelection,
    model: &str,
) -> Result<bool, AppError> {
    let agent = if resolved.project_id.is_some() || resolved.agent_id.is_some() {
        state
            .agent_registry
            .resolve_agent(resolved.project_id.as_deref(), resolved.agent_id.as_deref())
    } else {
        // No project and no agent was resolved (a direct model request): keep
        // the model-id fallback a headerless caller has always had.
        state.agent_registry.get_agent(model)
    };
    Ok(agent.is_some_and(|agent| !agent.config.policy.approvals.is_empty()))
}

#[utoipa::path(
    post,
    path = "/chat",
    tag = "Chat",
    request_body = ChatRequest,
    responses(
        (status = 200, description = "Chat response (JSON for non-streaming, SSE stream for streaming). Also mounted at /api/chat", body = ChatResponse),
        (status = 202, description = "Turn parked on a human approval; observe it at /api/executions/:id", body = ChatAck),
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
        let result = execute_chat_stream_with_conversation_id(&state, payload, selection).await?;
        let mut headers = axum::http::HeaderMap::new();
        if let Some(cid) = &result.conversation_id {
            if let Ok(value) = axum::http::HeaderValue::from_str(cid) {
                headers.insert("x-conversation-id", value);
            }
        }
        let sse_stream = result.stream.map(|res| match res {
            Ok(event) => match serde_json::to_string(&event) {
                Ok(json_str) => Ok::<_, Infallible>(Event::default().data(json_str)),
                Err(error) => Ok::<_, Infallible>(
                    Event::default().data(format!("Serialization error: {}", error)),
                ),
            },
            Err(error) => Ok::<_, Infallible>(Event::default().data(format!("Error: {}", error))),
        });
        Ok((headers, Sse::new(sse_stream)).into_response())
    } else {
        // One resolution for the whole non-streaming path: the same
        // `ResolvedSelection` decides which agent's policy is checked below and
        // which agent the executed turn uses. Old code resolved once here (in
        // `turn_has_approval_rules`) and again inside dispatch, spending two d1
        // decisions and allowing the two to disagree on the target agent.
        // Empty messages are refused before a decision is spent, exactly as
        // the check inside dispatch did before resolution moved up here.
        require_messages(&payload)?;
        let resolved = resolve_selection(&state, selection, &payload.messages).await?;

        if turn_has_approval_rules(&state, &resolved, &payload.model)? {
            // A gated turn may block on a human for far longer than an HTTP request
            // should stay open. Mint the execution id up front, run the turn in a
            // task, and acknowledge with the ids a human needs if it is still
            // parked when the grace expires. Everything else stays synchronous.
            let execution_id = Uuid::new_v4();
            let (done_tx, done_rx) = oneshot::channel::<Result<ChatResponse, AppError>>();
            let spawned_state = state.clone();
            // `AgentSelection` borrows the headers; own the strings so the spawned
            // task is `'static` while rebuilding the same selection inside it.
            let project = selection.project_id.map(str::to_string);
            let agent = selection.agent_id.map(str::to_string);
            let conversation = selection.conversation_id.map(str::to_string);
            let debug = selection.debug;
            // The resolved selection is owned by this task and borrowed only
            // for the call below, so the agent that runs is the one whose
            // approval policy was just checked — with no second resolution.
            tokio::spawn(async move {
                let owned = AgentSelection {
                    project_id: project.as_deref(),
                    agent_id: agent.as_deref(),
                    conversation_id: conversation.as_deref(),
                    debug,
                };
                let result = execute_chat_with_new_id_resolved(
                    &spawned_state,
                    payload,
                    owned,
                    execution_id,
                    Some(&resolved),
                )
                .await;
                let _ = done_tx.send(result);
            });

            match tokio::time::timeout(
                std::time::Duration::from_millis(state.config.park_grace_ms),
                done_rx,
            )
            .await
            {
                Ok(Ok(Ok(response))) => {
                    let mut headers = axum::http::HeaderMap::new();
                    if let Some(cid) = &response.conversation_id {
                        if let Ok(value) = axum::http::HeaderValue::from_str(cid) {
                            headers.insert("x-conversation-id", value);
                        }
                    }
                    Ok((headers, Json(response)).into_response())
                }
                Ok(Ok(Err(err))) => Err(err),
                Ok(Err(_)) => Err(AppError::Runtime(
                    "approval turn ended without a response".into(),
                )),
                Err(_) => {
                    let ack = ChatAck {
                        execution_id: execution_id.to_string(),
                        approval_id: state
                            .approval_gate
                            .live_approval_for_execution(execution_id)
                            .map(|id| id.to_string()),
                    };
                    Ok((axum::http::StatusCode::ACCEPTED, Json(ack)).into_response())
                }
            }
        } else {
            let response =
                execute_chat_resolved(&state, payload, selection, Some(&resolved)).await?;
            let mut headers = axum::http::HeaderMap::new();
            if let Some(cid) = &response.conversation_id {
                if let Ok(value) = axum::http::HeaderValue::from_str(cid) {
                    headers.insert("x-conversation-id", value);
                }
            }
            Ok((headers, Json(response)).into_response())
        }
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
