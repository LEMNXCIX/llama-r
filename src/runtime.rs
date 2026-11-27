use crate::adapters::auto::RouterAutoDecider;
use crate::adapters::context::{CurrentModelSelector, NoopCompactor, RouterModelSelector};
use crate::adapters::execution::NoOpExecutionStore;
use crate::adapters::mcp::{McpServerConfig, StaticMcpRegistry};
use crate::adapters::rag::{
    FileRagStore, GeminiEmbeddings, OllamaEmbeddings, OpenAiCompatibleEmbeddings,
    OpenRouterEmbeddings,
};
use crate::adapters::router::{LlamaCppRouter, OpenCodeRouter, OpenRouterRouter};
use crate::api::admin_auth::require_admin;
use crate::api::agent_api::{
    create_agent, delete_agent, get_agent, get_agent_scope, list_agents_api, update_agent,
};
use crate::api::approval_api::{decide_approval, list_approvals};
use crate::api::context_api::{
    analyze_project, create_context, delete_context, get_context, list_contexts, update_context,
};
use crate::api::docs::{serve_openapi_json, serve_scalar_ui};
use crate::api::execution_api::{get_execution, list_executions, retry_execution};
use crate::api::grpc::{pb::llama_gateway_server::LlamaGatewayServer, GrpcService};
use crate::api::handlers::{chat, list_models, mcp_message, openai_chat, AppState};
use crate::api::health::health;
use crate::api::history_api::{
    delete_conversation_handler, export_conversation_handler, get_conversation_handler,
    get_conversation_messages_handler, list_conversations_handler,
};
use crate::api::metrics::metrics;
use crate::api::observability::AppObservability;
use crate::api::rag_api::{rag_delete_document, rag_ingest, rag_query};
use crate::api::route_api::route_preview;
use crate::api::skill_api::list_skills_api;
use crate::api::webhook_api::webhook;
use crate::config::{Config, ProviderKind, OPENROUTER_BASE_URL};
use crate::context::analyzer::ContextEnricher;
use crate::context::store::ContextStore;
use crate::core::hot_reload::HotReloader;
use crate::domain::execution::ExecutionStatus;
use crate::error::AppError;
use crate::optimizer::metrics::TokenMetrics;
use crate::ports::auto::AutoDecider;
use crate::ports::compaction::ContextCompactor;
use crate::ports::delegation::Delegator;
#[cfg(feature = "rig-engine")]
use crate::ports::engine::AgentEngine;
use crate::ports::execution::ExecutionStore;
use crate::ports::history::ConversationStore;
use crate::ports::mcp::McpServerRegistry;
use crate::ports::model_selection::ModelSelector;
use crate::ports::rag::{EmbeddingProvider, RagStore};
use crate::ports::router::DecisionRouter;
use crate::ports::telemetry::TraceExporter;
use crate::providers::anthropic::AnthropicProvider;
use crate::providers::gemini::GeminiProvider;
use crate::providers::ollama::OllamaProvider;
use crate::providers::openai_compatible::OpenAiCompatibleProvider;
use crate::providers::opencode::OpenCodeProvider;
use crate::providers::registry::{ProviderProfile, ProviderRegistry, DEFAULT_PROVIDER_TIMEOUT_MS};
use crate::providers::LLMProvider;
use crate::services::agent_registry::AgentRegistry;
#[cfg(feature = "rig-engine")]
use crate::services::agent_runtime::AgentRuntime;
use crate::services::approval::ApprovalGate;
use crate::services::provider_stack::{FallbackProvider, ProviderStack};
use crate::services::rag_ingest::RagIngestService;
use crate::services::router_config::RouterConfig;
use crate::services::skill_manager::SkillManager;
use axum::{
    extract::DefaultBodyLimit,
    middleware::from_fn_with_state,
    routing::{get, post},
    Router,
};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use tokio::sync::watch;
use tokio::sync::RwLock as TokioRwLock;
use tonic::transport::Server as TonicServer;
// `CorsLayer` is built by `crate::net::cors_layer`; no direct import needed.

pub struct Runtime {
    pub config: Config,
    pub state: Arc<AppState>,
    pub router: Router,
    pub http_addr: SocketAddr,
    pub grpc_addr: SocketAddr,
}

/// Build the semantic skill-ranking enricher from a selected stack's embedder.
///
/// One constructor for all three sites that need one: the boot `AppState`, the
/// per-agent `AgentRuntime`, and the `AppState` enricher reinstalled on every
/// settings Apply. Sharing it is what keeps a hot swap from leaving chat_core
/// ranking skills with the boot-time provider's embedder. Without an embedder
/// every declared skill is used (best effort) rather than a broken Ollama
/// embedder being attached to a non-Ollama provider.
pub fn build_context_enricher(
    context_store: Arc<ContextStore>,
    skill_manager: Arc<SkillManager>,
    default_model: String,
    global_provider: ProviderKind,
    embeddings: Option<Arc<dyn EmbeddingProvider>>,
) -> Arc<ContextEnricher> {
    let provider_registry = Arc::new(match ProviderRegistry::load() {
        Ok(registry) => registry,
        Err(err) => {
            tracing::warn!(error = %err, "invalid provider registry; using built-in profiles");
            ProviderRegistry::default()
        }
    });
    let mut enricher = ContextEnricher::new(
        context_store,
        skill_manager,
        default_model,
        provider_registry,
        global_provider,
    );
    if let Some(embeddings) = embeddings {
        enricher = enricher.with_embeddings(embeddings);
    }
    Arc::new(enricher)
}

/// Build the `Auto` decider from the **same** `DecisionRouter` the routing
/// ladder uses, with `RouterConfig`'s `min_confidence` as its default threshold.
/// Kept as a pure helper so the wiring can be tested without boot, env or a
/// live decision model. No routing (disabled or no router) means no decider:
/// `Auto` then behaves exactly like `Always`.
fn build_auto_decider(
    router_config: &RouterConfig,
    router: Option<&Arc<dyn DecisionRouter>>,
) -> Option<Arc<dyn AutoDecider>> {
    if !router_config.enabled {
        return None;
    }
    router.map(|router| {
        Arc::new(RouterAutoDecider::new(
            router.clone(),
            router_config.min_confidence,
        )) as Arc<dyn AutoDecider>
    })
}

/// Build the complexity selector from the **same** `DecisionRouter` agent
/// routing (and `Auto`) use, so no second router instance is created. Routing
/// off, or no router available, installs `CurrentModelSelector`: an agent that
/// declares candidates but runs in a deployment without a decision model keeps
/// exactly the model it had before this phase. Pure helper, testable without
/// boot/env; boot and a settings Apply share the resulting handle on AppState.
fn build_model_selector(
    router_config: &RouterConfig,
    router: Option<&Arc<dyn DecisionRouter>>,
) -> Arc<dyn ModelSelector> {
    if router_config.enabled {
        if let Some(router) = router {
            return Arc::new(RouterModelSelector::new(
                router.clone(),
                router_config.min_confidence,
            ));
        }
    }
    Arc::new(CurrentModelSelector)
}

#[allow(
    clippy::too_many_arguments,
    reason = "stable interface: called from fourteen sites across the crate and tests"
)]
pub fn build_app_state(
    stack: ProviderStack,
    agent_registry: Arc<AgentRegistry>,
    skill_manager: Arc<SkillManager>,
    context_store: Arc<ContextStore>,
    logs: Arc<Mutex<VecDeque<String>>>,
    known_mcp_servers: Vec<String>,
    mcp_registry: Arc<dyn McpServerRegistry>,
    history_store: Option<Arc<dyn ConversationStore>>,
) -> Arc<AppState> {
    build_app_state_with_gate(
        Arc::new(ApprovalGate::default()),
        stack,
        agent_registry,
        skill_manager,
        context_store,
        logs,
        known_mcp_servers,
        mcp_registry,
        history_store,
        // The convenience constructor (used by tests and non-boot callers) has
        // no durable store to install. Boot calls `build_app_state_with_gate`
        // directly with the real store built from the history DB path.
        Arc::new(NoOpExecutionStore),
        // Non-boot callers get the same config the env describes; a malformed
        // value falls back to the documented defaults rather than failing a
        // test fixture. Boot passes its own already-parsed `Config` below.
        Config::from_env().unwrap_or_default(),
    )
}

/// Same as [`build_app_state`] but installs a caller-owned approval gate. Boot
/// uses this so the exact same `Arc` is given to the engine (via
/// [`CompanionHandles`]) and stored on `AppState`; a TUI Apply rebuilds the
/// engine from `CompanionHandles::from_state`, so pending decisions survive.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors build_app_state with the boot-owned approval gate added"
)]
pub fn build_app_state_with_gate(
    approval_gate: Arc<ApprovalGate>,
    mut stack: ProviderStack,
    agent_registry: Arc<AgentRegistry>,
    skill_manager: Arc<SkillManager>,
    context_store: Arc<ContextStore>,
    logs: Arc<Mutex<VecDeque<String>>>,
    known_mcp_servers: Vec<String>,
    mcp_registry: Arc<dyn McpServerRegistry>,
    history_store: Option<Arc<dyn ConversationStore>>,
    execution_store: Arc<dyn ExecutionStore>,
    config: Config,
) -> Arc<AppState> {
    let metrics = Arc::new(TokenMetrics::new());
    let default_model = stack.default_model.clone();
    let global_provider = stack
        .provider_name
        .parse::<ProviderKind>()
        .unwrap_or_default();
    // Semantic skill ranking uses the SELECTED stack's embedder. The same
    // constructor rebuilds it on every settings Apply, so boot and Apply cannot
    // disagree about which provider ranks skills.
    let context_enricher = build_context_enricher(
        context_store.clone(),
        skill_manager.clone(),
        default_model,
        global_provider,
        stack.embeddings.clone(),
    );

    // Router configuration. `RouterConfig` deliberately does not load `.env`
    // itself (that is `Config::from_env`'s job, which has already run on this
    // boot path), so this reads deployment values from the environment plus
    // the optional `router.toml` overlay. An unusable config must not stop a
    // server that otherwise works: warn — the message names the offending
    // variable — and fall back to the inert defaults, routing off.
    let router_config = match RouterConfig::load() {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "Invalid router configuration; falling back to defaults (routing disabled)"
            );
            RouterConfig::default()
        }
    };
    // The stack carries the decision router. Callers that already installed one
    // (a test fake, or a future settings-driven build) keep theirs; otherwise the
    // default Ollama-compatible router is built from the boot configuration.
    if stack.router.is_none() {
        stack.router = if router_config.enabled {
            tracing::info!(
                url = %router_config.url,
                model = %router_config.model,
                "Decision router initialized"
            );
            Some(Arc::new(LlamaCppRouter::new(
                router_config.url.clone(),
                router_config.model.clone(),
                router_config.timeout_ms,
            )))
        } else {
            tracing::info!("Router disabled via ROUTER_ENABLED=false");
            None
        };
    }

    // The automatic-approval decider wraps the very router the routing ladder
    // uses (built above, or injected by a caller). It is stored on AppState —
    // outside the hot-swapped ProviderStack — so a settings Apply cannot drop
    // it, exactly like the approval gate. With no router (routing disabled)
    // every `Auto` tool keeps asking a human.
    let auto_decider = build_auto_decider(&router_config, stack.router.as_ref());

    // The model selector wraps the very router above when routing is enabled;
    // otherwise it is the fail-closed `CurrentModelSelector`. Stored on AppState
    // so a settings Apply builds the engine from the same handle, exactly like
    // the gate, decider and compactor.
    let selector = build_model_selector(&router_config, stack.router.as_ref());

    // Compaction is transparent by default: without a usable summarizer there
    // is no summary to inject, and an unknown context window already means no
    // compaction. Stored on AppState so a settings Apply builds the engine from
    // the same handle, exactly like the gate and the auto decider.
    let compactor: Arc<dyn ContextCompactor> = Arc::new(NoopCompactor);

    // Telemetry is opt-in from `Config`. Built once here and stored on
    // AppState (outside the hot-swapped stack) so boot and a settings Apply
    // share the exact same exporter handle through `CompanionHandles`.
    let exporter = crate::adapters::telemetry::exporter_from_config(&config);

    let state = Arc::new(AppState {
        config,
        services: Arc::new(TokioRwLock::new(Arc::new(stack))),
        agent_registry: agent_registry.clone(),
        skill_manager,
        context_store,
        context_enricher: Mutex::new(context_enricher),
        metrics,
        observability: Arc::new(AppObservability::new()),
        api_running: AtomicBool::new(false),
        grpc_running: AtomicBool::new(false),
        logs,
        known_mcp_servers: RwLock::new(known_mcp_servers),
        mcp_registry,
        history_store,
        router_config: Arc::new(router_config),
        router_healthy: AtomicBool::new(false),
        approval_gate,
        auto_decider,
        compactor,
        selector,
        execution_store,
        delegator: OnceLock::new(),
        channel_verifier: Arc::new(crate::adapters::channel::HmacSha256Verifier),
        exporter,
    });
    // The delegator needs the *finished* Arc<AppState> so its Weak can upgrade
    // at chat time. Building it here means every state factory (boot and tests)
    // hands out a state whose delegator is already installed; the engine reads
    // it from `CompanionHandles::from_state` when companions are built.
    let delegator = Arc::new(crate::services::delegation::RuntimeDelegator::new(
        agent_registry,
        Arc::downgrade(&state),
    ));
    state.set_delegator(delegator);
    state
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let cors =
        crate::net::cors_layer(&state.config.cors_origins).expect("validated at config load");

    // Public surface: chat, webhooks, health, models, MCP, docs and the
    // skill listing endpoint. None of these is admin-gated.
    let public = Router::new()
        .route("/", get(|| async { "Llama-R API is running" }))
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/chat", post(chat))
        .route("/models", get(list_models))
        .route("/v1/chat/completions", post(openai_chat))
        .route("/api", get(|| async { "Llama-R API is running" }))
        .route("/api/health", get(health))
        .route(
            "/api/mcp",
            get(|| async {
                let stream = async_stream::stream! {
                    yield Ok::<_, std::convert::Infallible>(axum::response::sse::Event::default().event("endpoint").data("/api/mcp"));
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                        yield Ok::<_, std::convert::Infallible>(axum::response::sse::Event::default().comment("keep-alive"));
                    }
                };
                axum::response::sse::Sse::new(stream)
            })
            .post(mcp_message),
        )
        .route("/api/models", get(list_models))
        .route("/api/chat", post(chat))
        .route("/api/skills", get(list_skills_api))
        .route("/api/webhooks/:channel", post(webhook))
        .route("/openapi.json", get(serve_openapi_json))
        .route("/docs", get(serve_scalar_ui));

    // Admin surface: one gate for every route below. `X-Debug` is still
    // checked by the individual handlers that had it before — the token is
    // additional, never a replacement. `from_fn` is axum's `from_fn_with_state`
    // with `()` state; the `State<Arc<AppState>>` extractor here needs the real
    // state, so this is the stateful form of the same from-function middleware.
    let admin = Router::new()
        .route("/api/agents", get(list_agents_api).post(create_agent))
        .route("/api/approvals", get(list_approvals))
        .route("/api/approvals/:id", post(decide_approval))
        .route(
            "/api/agents/:id",
            get(get_agent).put(update_agent).delete(delete_agent),
        )
        .route("/api/agents/:id/scope", get(get_agent_scope))
        .route("/api/contexts", get(list_contexts).post(create_context))
        .route(
            "/api/contexts/:id",
            get(get_context).put(update_context).delete(delete_context),
        )
        .route("/api/contexts/:id/analyze", post(analyze_project))
        .route("/api/executions", get(list_executions))
        .route("/api/executions/:id", get(get_execution))
        .route("/api/executions/:id/retry", post(retry_execution))
        .route("/api/rag/ingest", post(rag_ingest))
        .route("/api/rag/query", post(rag_query))
        .route("/api/rag/delete-document", post(rag_delete_document))
        .route("/api/route", post(route_preview))
        .route("/api/conversations", get(list_conversations_handler))
        .route(
            "/api/conversations/:id",
            get(get_conversation_handler).delete(delete_conversation_handler),
        )
        .route(
            "/api/conversations/:id/messages",
            get(get_conversation_messages_handler),
        )
        .route(
            "/api/conversations/:id/export",
            post(export_conversation_handler),
        )
        .layer(from_fn_with_state(state.clone(), require_admin));

    public
        .merge(admin)
        .layer(cors)
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(state)
}

pub async fn build_runtime(logs: Arc<Mutex<VecDeque<String>>>) -> Result<Runtime, AppError> {
    let mut config = Config::from_env()?;

    if let Err(err) = crate::core::paths::ensure_dirs() {
        tracing::error!(error = %err, "Failed to create data directories");
    }

    // Load MCP server configs and build the registry
    let (known_mcp_servers, mcp_server_configs) = load_mcp_server_configs();
    let mcp_registry_inner = Arc::new(match StaticMcpRegistry::from_configs(&mcp_server_configs) {
        Ok(registry) => {
            tracing::info!(
                count = known_mcp_servers.len(),
                "MCP server registry initialized"
            );
            registry
        }
        Err(err) => {
            tracing::error!(error = %err, "Failed to initialize MCP server registry; continuing without MCP clients");
            StaticMcpRegistry::new()
        }
    });
    let mcp_registry: Arc<dyn McpServerRegistry> = mcp_registry_inner.clone();

    let agent_registry = Arc::new(AgentRegistry::new());
    if let Err(err) = agent_registry.reload_all(&known_mcp_servers) {
        tracing::error!(error = %err, "Failed to load agents on startup; continuing with partial state");
    }

    let base_dir = crate::core::paths::get_base_dir();
    let mcp_reload: Arc<dyn Fn() + Send + Sync> = {
        let reload_registry = mcp_registry_inner.clone();
        let mcp_servers_dir = base_dir.join("mcp-servers");
        Arc::new(move || {
            if !mcp_servers_dir.exists() {
                return;
            }
            let mut configs = Vec::new();
            if let Ok(entries) = fs::read_dir(&mcp_servers_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                        continue;
                    }
                    if let Ok(content) = fs::read_to_string(&path) {
                        if let Ok(cfg) = toml::from_str::<McpServerConfig>(&content) {
                            if cfg.enabled {
                                configs.push(cfg);
                            }
                        }
                    }
                }
            }
            if let Err(err) = reload_registry.reload_from_configs(&configs) {
                tracing::error!(error = %err, "Failed to reload MCP server configs");
            } else {
                tracing::info!(count = configs.len(), "MCP server configs reloaded");
            }
        })
    };
    let reloader = HotReloader::new(agent_registry.clone(), known_mcp_servers.clone())
        .with_mcp_reload(mcp_reload);
    if let Err(err) = reloader.watch(&base_dir) {
        tracing::error!(path = %base_dir.display(), error = %err, "Failed to watch base folder; hot reload disabled");
    }

    let skill_manager = Arc::new(SkillManager::new());
    skill_manager.scan_and_load();

    // Probe with the provider the operator actually selected. The stack builder
    // encapsulates the `PROVIDER` choice (chat, router and embeddings together),
    // so reusing it here keeps the startup probe, the agent-model validation and
    // the interactive-setup seed on the same provider the app will run on. The
    // stack itself is rebuilt below with RAG/agent-runtime attached; only its
    // chat provider is needed at this point.
    let mut provider_impl: Arc<dyn LLMProvider + Send + Sync> =
        build_provider_stack(&config)?.provider.clone();

    tracing::info!(provider_url = %provider_impl.get_base_url(), "Verifying LLM provider health");
    let health_ok = provider_impl.health_check().await.is_ok();
    let is_terminal = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let plan = startup_plan::plan(startup_plan::PlanInput {
        health_ok,
        model_configured: config.is_configured(),
        is_terminal,
    });

    // A completed interactive setup returns the fully built stack for the
    // chosen provider. Hold it until the companion services (RAG, history,
    // agent runtime) are attached below rather than rebuilding a second stack
    // from config and risking the two disagreeing about the entered key.
    let mut setup_stack: Option<ProviderStack> = None;

    match plan {
        startup_plan::StartupPlan::Proceed => {}
        startup_plan::StartupPlan::Degraded => {
            // Never fatal: serve the API and let /health report "degraded".
            tracing::warn!(
                provider_url = %provider_impl.get_base_url(),
                "LLM provider is not reachable; starting degraded. \
                 /api/health will report degraded until it recovers."
            );
        }
        startup_plan::StartupPlan::Unconfigured => {
            tracing::warn!(
                "No DEFAULT_MODEL configured and no terminal available for interactive setup; \
                 starting degraded. Set DEFAULT_MODEL in .env and restart."
            );
        }
        startup_plan::StartupPlan::InteractiveSetup => {
            println!("Welcome to Llama-R. Let's configure your default provider.");
            // Setup mutates `config` with the chosen provider, credentials and
            // model, then hands back the stack built from exactly those values.
            match crate::cli::interactive::run_interactive_setup(&mut config).await {
                Ok((stack, selected_model)) => {
                    config.default_model = selected_model;
                    if let Err(err) = config.save_to_env() {
                        tracing::error!(error = %err, "Failed to persist configuration to .env");
                    }
                    provider_impl = stack.provider.clone();
                    setup_stack = Some(stack);
                }
                Err(err) => {
                    // Setup is a convenience, not a precondition. Keep serving.
                    tracing::error!(
                        error = %err,
                        "Interactive setup did not complete; starting with the existing configuration"
                    );
                }
            }
        }
    }

    if matches!(plan, startup_plan::StartupPlan::Proceed) {
        tracing::info!(default_model = %config.default_model, "Provider healthy; validating configured agent models");
        if let Ok(models) = provider_impl.list_models().await {
            let model_names: Vec<String> = models.iter().map(|model| model.name.clone()).collect();
            for agent in agent_registry.list_agents() {
                let model_to_check = if agent.config.model.is_empty() {
                    &config.default_model
                } else {
                    &agent.config.model
                };
                if !model_names
                    .iter()
                    .any(|candidate| candidate == model_to_check)
                {
                    tracing::warn!(agent_id = %agent.qualified_id(), model = %model_to_check, available_models = ?model_names, "Agent references unavailable model");
                }
            }
        }
    }

    let context_store = Arc::new(ContextStore::new());
    // The summarizer speaks the Ollama `/api/chat` wire format, so it is only
    // installed for providers that can serve it. See `build_history_store`.
    let history_store = build_history_store(&config, config.provider);

    if let Some(history_arc) = history_store.clone() {
        let retention = config.history_retention_days;
        // The per-agent overrides are derived from the registry, not from a
        // static global: an agent's `[memory] retention_days` wins over the
        // global value for its own conversations. The map is rebuilt from the
        // registry at boot and on every daily tick so a hot reload that changes
        // an agent's retention is honoured without a restart.
        let h1 = history_arc.clone();
        let startup_registry = agent_registry.clone();
        tokio::spawn(async move {
            let per_agent = build_agent_retention_map(&startup_registry);
            match h1
                .purge_old_conversations(retention, per_agent.as_ref())
                .await
            {
                Ok(n) => tracing::info!(deleted = n, "History purge on startup completed"),
                Err(e) => tracing::warn!(error = %e, "History purge on startup failed"),
            }
        });

        let h2 = history_arc.clone();
        let daily_registry = agent_registry.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(86_400));
            interval.tick().await;
            loop {
                interval.tick().await;
                let per_agent = build_agent_retention_map(&daily_registry);
                match h2
                    .purge_old_conversations(retention, per_agent.as_ref())
                    .await
                {
                    Ok(n) => tracing::info!(deleted = n, "History purge completed"),
                    Err(e) => tracing::warn!(error = %e, "History purge failed"),
                }
            }
        });
    }

    // Durable execution store, built from the same `history.db` as above and
    // owned by `AppState` (outside `ProviderStack`, so a hot Apply never drops
    // it). An unopenable store degrades to the no-op one instead of failing boot.
    // Boot recovery and the approval reaper run once the state exists, below.
    let execution_store = build_execution_store();

    // One `PROVIDER` selects chat, the decision router and embeddings. A
    // completed interactive setup already produced such a stack from the values
    // the user entered; otherwise build one from the boot configuration.
    let stack = match setup_stack {
        Some(stack) => stack,
        None => build_provider_stack(&config)?,
    };
    // Build the companions FROM THE SELECTED STACK. Doing it from `config` (the
    // old behaviour) meant a non-Ollama provider got a broken Ollama embedder
    // and an engine hardwired to Ollama. One helper serves boot and hot Apply,
    // so the two cannot disagree about which upstream the engine targets.
    // One gate for the process lifetime, created before either the engine or
    // `AppState` needs it. Sharing the same `Arc` is what makes a settings Apply
    // safe: it rebuilds the engine but leaves pending decisions intact. The
    // durable store is attached so pendings/decisions survive a restart; a boot
    // reload rehydrates them without a oneshot (there is no live request yet).
    // `Config::approval_timeout` decides whether a pending waits forever or on
    // the bounded deadline — one source of truth shared with docs and tests.
    let approval_gate = Arc::new(
        match config.approval_timeout() {
            Some(timeout) => ApprovalGate::new(timeout),
            None => ApprovalGate::parked(),
        }
        .with_store(execution_store.clone()),
    );
    if let Err(err) = approval_gate.load_from_store().await {
        tracing::warn!(error = %err, "Boot could not reload pending approvals; continuing");
    }
    let state = build_app_state_with_gate(
        approval_gate,
        stack,
        agent_registry.clone(),
        skill_manager,
        context_store,
        logs,
        known_mcp_servers,
        mcp_registry,
        history_store,
        execution_store.clone(),
        config.clone(),
    );
    // Build the companions only now, from the state that owns the boot gate
    // *and* the boot `auto_decider`. `CompanionHandles::from_state` reads both,
    // so boot and a settings Apply share one engine-building path and the
    // engine can never be left without the decider the routing router built.
    let mut stack = {
        let guard = state.services.read().await;
        (**guard).clone()
    };
    build_companions_for_state(&state, &config, &mut stack);
    state.replace_services(stack).await;

    // Approval reaper (Task 3): resume turns parked on a decision a human made
    // while this process was down. The first pass runs BEFORE boot recovery
    // marks mid-flight turns `Interrupted`, so a decided orphan is consumed
    // rather than buried as a generic interrupted turn. Both steps are best
    // effort: a store error is a warning and never stops boot.
    let reaper_interval = std::time::Duration::from_secs(config.reaper_interval_secs);
    if let Err(err) = crate::services::approval_reaper::reap_once(&state).await {
        tracing::warn!(error = %err, "Approval reaper first pass failed; boot continues");
    }
    if let Err(err) = mark_interrupted(execution_store.as_ref()).await {
        tracing::warn!(error = %err, "Boot recovery could not list interrupted executions; continuing");
    }
    // The periodic pass catches any orphan that appears while the service runs.
    crate::services::approval_reaper::spawn_approval_reaper(state.clone(), reaper_interval);

    // One probe at startup, then last-known-value: the ladder updates the flag
    // after every decision and `/api/health` never probes — the point of the
    // design is exactly that no second round trip is added per health check.
    // "Not yet probed" is a different fact from "unreachable", and an operator
    // who switched the router on deserves "ok" from a working deployment on
    // the first request, not after the first routed chat. Non-fatal either
    // way: a router down at boot degrades the service, it does not stop it —
    // the same posture `startup_plan` takes for the provider.
    if state.router_config.enabled {
        let url = state.router_config.url.clone();
        let timeout_ms = state.router_config.timeout_ms;
        let healthy = tokio::task::spawn_blocking(move || probe_router_origin(&url, timeout_ms))
            .await
            .unwrap_or(false);
        state.record_router_health(healthy);
        if healthy {
            tracing::info!(url = %state.router_config.url, "Decision router reachable at startup");
        } else {
            tracing::warn!(
                url = %state.router_config.url,
                "Decision router unreachable at startup; /api/health reports degraded until it answers"
            );
        }
    }

    let router = build_router(state.clone());

    Ok(Runtime {
        http_addr: SocketAddr::from(([127, 0, 0, 1], config.port)),
        grpc_addr: SocketAddr::from(([127, 0, 0, 1], 50051)),
        config,
        state,
        router,
    })
}

/// One-shot reachability probe of the decision model's origin.
///
/// A TCP connect, not an HTTP request: the probe answers "is something
/// listening at the configured origin", and knowing *which* path says
/// "healthy" on a llama-server is the adapter's URL knowledge
/// (`src/adapters/router/llamacpp.rs`), not the runtime's. A semantic probe —
/// one real decision — is the ladder's job on the first routed message.
///
/// Never fatal: an unreachable router at boot is a degraded service, not a
/// failed start. A URL that cannot even be parsed counts as unreachable and
/// warns, so a typo in `ROUTER_URL` is visible in the boot log either way.
fn probe_router_origin(url: &str, timeout_ms: u64) -> bool {
    let Some(origin) = router_origin(url) else {
        tracing::warn!(
            url,
            "Router URL has no recognisable host; treating the router as unreachable"
        );
        return false;
    };
    use std::net::ToSocketAddrs;
    let mut addrs = match origin.to_socket_addrs() {
        Ok(addrs) => addrs,
        Err(err) => {
            tracing::warn!(url, origin, error = %err, "Router origin does not resolve; treating the router as unreachable");
            return false;
        }
    };
    let Some(addr) = addrs.next() else {
        tracing::warn!(
            url,
            origin,
            "Router origin resolved to no address; treating the router as unreachable"
        );
        return false;
    };
    let timeout = std::time::Duration::from_millis(timeout_ms.max(1));
    match std::net::TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(url, origin, error = %err, "Router origin did not accept a connection");
            false
        }
    }
}

/// `http://host:port/…` → `host:port`, defaulting the port by scheme.
///
/// Deliberately not a URL parser: no dependency is pulled in for two fields,
/// and every surprise (no authority, a non-numeric port) yields `None` so the
/// caller reports "unreachable" rather than guessing. Userinfo before an `@`
/// is stripped, and an IPv6 literal keeps its brackets, which is the spelling
/// `ToSocketAddrs` accepts.
fn router_origin(url: &str) -> Option<String> {
    let (default_port, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (443, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (80, rest)
    } else {
        // A bare `host:port` still probes; a bare scheme-less path does not,
        // because `ToSocketAddrs` will refuse it and the caller warns.
        (80, url)
    };
    let authority = rest.split('/').next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        return None;
    }
    if let Some(end) = authority.find(']') {
        // IPv6 literal, e.g. `[::1]:8080`.
        let host = &authority[..=end];
        let port = authority[end + 1..]
            .strip_prefix(':')
            .and_then(|raw| raw.parse::<u16>().ok())
            .unwrap_or(default_port);
        return Some(format!("{host}:{port}"));
    }
    match authority.rsplit_once(':') {
        Some((host, raw_port)) => raw_port
            .parse::<u16>()
            .ok()
            .map(|port| format!("{host}:{port}")),
        None => Some(format!("{authority}:{default_port}")),
    }
}

/// Process-wide shutdown trigger. A `watch` channel retains the last value, so
/// a `request_shutdown()` that fires before (or during) registration of a
/// waiter is never lost — unlike `Notify`, whose `notify_waiters()` only wakes
/// waiters already registered on first poll. Every `shutdown_signal()` future
/// subscribes first, then re-checks the flag, closing the lost-wakeup window.
static SHUTDOWN_WATCH: OnceLock<watch::Sender<bool>> = OnceLock::new();
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

fn shutdown_sender() -> &'static watch::Sender<bool> {
    SHUTDOWN_WATCH.get_or_init(|| {
        let (tx, _rx) = watch::channel(false);
        tx
    })
}

/// Trigger shutdown from within the process. Safe to call from any task; wakes
/// every future currently awaiting [`shutdown_signal`]. Idempotent.
pub fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    let _ = shutdown_sender().send(true);
}

/// Whether shutdown has already been requested (in-process or via OS signal).
pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst) || *shutdown_sender().subscribe().borrow()
}

pub async fn shutdown_signal() {
    // Capture the waiter FIRST so there is no window in which
    // `request_shutdown()` can fire between the flag check and registration.
    let mut rx = shutdown_sender().subscribe();

    // Already triggered: resolve immediately instead of waiting.
    if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) || *rx.borrow() {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
        let _ = shutdown_sender().send(true);
        tracing::info!("shutdown signal received");
        return;
    }

    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %err, "Failed to listen for Ctrl-C");
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(err) => tracing::error!(error = %err, "Failed to listen for SIGTERM"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
        res = rx.changed() => { let _ = res; },
    }
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    let _ = shutdown_sender().send(true);
    tracing::info!("shutdown signal received");
}

pub async fn start_http_server(runtime: &Runtime) -> Result<tokio::task::JoinHandle<()>, AppError> {
    let listener = tokio::net::TcpListener::bind(runtime.http_addr)
        .await
        .map_err(|err| {
            AppError::Runtime(format!(
                "Failed to bind HTTP listener on {}: {}",
                runtime.http_addr, err
            ))
        })?;
    let app = runtime.router.clone();
    let state = runtime.state.clone();
    Ok(tokio::spawn(async move {
        state.api_running.store(true, Ordering::SeqCst);
        let serve_result = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await;
        state.api_running.store(false, Ordering::SeqCst);
        if let Err(err) = serve_result {
            tracing::error!(error = %err, "HTTP server stopped unexpectedly");
        }
    }))
}

/// Startup decision, kept pure so the rules are testable without a provider.
pub mod startup_plan {
    /// What the runtime should do about provider setup on boot.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum StartupPlan {
        /// Provider reachable and a model configured.
        Proceed,
        /// A model is configured but the provider is unreachable. Boot anyway:
        /// `/health` reports `degraded` and chat requests fail with a clear
        /// provider error. A gateway that refuses to start is worse than one
        /// that starts and says it is unhealthy.
        Degraded,
        /// No model configured and a terminal is available to ask for one.
        InteractiveSetup,
        /// No model configured and no terminal (systemd, Docker, CI). Boot
        /// anyway with a warning instead of aborting on an interactive prompt.
        Unconfigured,
    }

    /// Inputs to the startup decision.
    #[derive(Debug, Clone, Copy)]
    pub struct PlanInput {
        pub health_ok: bool,
        pub model_configured: bool,
        pub is_terminal: bool,
    }

    /// Decide how to handle provider configuration at boot.
    ///
    /// The provider being unreachable must not by itself trigger interactive
    /// setup: that path needs a TTY, so on a headless host it turned a
    /// recoverable "provider is down" into a hard startup failure.
    pub fn plan(input: PlanInput) -> StartupPlan {
        if !input.model_configured {
            return if input.is_terminal {
                StartupPlan::InteractiveSetup
            } else {
                StartupPlan::Unconfigured
            };
        }
        if input.health_ok {
            StartupPlan::Proceed
        } else {
            StartupPlan::Degraded
        }
    }
}

/// Load the optional `providers.toml` once per stack build. A malformed file
/// is non-fatal: warn and use the built-ins, exactly like `build_app_state`.
fn load_provider_registry() -> ProviderRegistry {
    match ProviderRegistry::load() {
        Ok(registry) => registry,
        Err(err) => {
            tracing::warn!(error = %err, "invalid provider registry; using built-in profiles");
            ProviderRegistry::builtins()
        }
    }
}

/// The profile for the selected `PROVIDER`. The registry always contains the
/// six canonical names, so this fallback is only a safety net for a hand-built
/// registry.
fn profile_for(registry: &ProviderRegistry, config: &Config) -> ProviderProfile {
    registry
        .get(&config.provider.to_string())
        .cloned()
        .unwrap_or_else(|| ProviderProfile {
            name: config.provider.to_string(),
            kind: config.provider,
            base_url: None,
            api_key_env: None,
            timeout_ms: DEFAULT_PROVIDER_TIMEOUT_MS,
        })
}

/// Read a credential by the *variable name* a profile carries. `api_key_env`
/// is never a key value; the value stays inside the adapter and is never put
/// in a URL, log line, health field or error string.
fn profile_api_key(profile: &ProviderProfile) -> String {
    profile
        .api_key_env
        .as_deref()
        .map(|name| std::env::var(name).unwrap_or_default())
        .unwrap_or_default()
}

/// Build a chat provider from a profile's public fields, for the one fallback
/// rung. The key is read by *variable name* from `api_key_env` and handed straight
/// to the adapter: it never enters a URL, a log line or an error string. Only the
/// fields on `ProviderProfile` are used — never a key value.
fn chat_provider_from_profile(
    profile: &ProviderProfile,
    model: &str,
) -> Arc<dyn LLMProvider + Send + Sync> {
    let key = profile_api_key(profile);
    let base_url = profile.base_url.clone();
    let timeout = std::time::Duration::from_millis(profile.timeout_ms);
    match profile.kind {
        ProviderKind::Ollama => Arc::new(OllamaProvider::with_timeout(
            base_url.unwrap_or_else(|| "http://localhost:11434".to_string()),
            model.to_string(),
            timeout,
        )),
        ProviderKind::OpenRouter => Arc::new(OpenAiCompatibleProvider::with_timeout(
            base_url.unwrap_or_else(|| OPENROUTER_BASE_URL.to_string()),
            key,
            model.to_string(),
            timeout,
        )),
        ProviderKind::OpenCode => Arc::new(OpenCodeProvider::with_timeout(
            base_url.unwrap_or_else(|| "http://127.0.0.1:4096".to_string()),
            if key.is_empty() { None } else { Some(key) },
            model.to_string(),
            timeout,
        )),
        ProviderKind::OpenAiCompatible => {
            Arc::new(OpenAiCompatibleProvider::with_provider_name_and_timeout(
                base_url.unwrap_or_else(|| "https://api.openai.com".to_string()),
                key,
                model.to_string(),
                profile.name.clone(),
                timeout,
            ))
        }
        ProviderKind::Anthropic => {
            Arc::new(AnthropicProvider::new(base_url, key, model.to_string()))
        }
        ProviderKind::Gemini => Arc::new(GeminiProvider::new(base_url, key, model.to_string())),
    }
}

/// A profile is a usable fallback only when it can authenticate: local providers
/// carry no `api_key_env`, while remote ones need their named variable to resolve
/// to a non-empty value. The value is read only to test presence via the existing
/// `profile_api_key` helper — it is never logged, named in an error or stored on
/// the stack. Falling back to a profile we know cannot authenticate would mask
/// the primary's real connection error with a spurious 401.
fn profile_is_usable(profile: &ProviderProfile) -> bool {
    profile.api_key_env.is_none() || !profile_api_key(profile).trim().is_empty()
}

/// The single next *usable* profile after the primary, from the registry's
/// deterministic (`ProviderRegistry::ordered`) order. Returning one `Option` is
/// the bound: a request can fail over to at most one other upstream, never walk
/// a chain. When no profile can authenticate, there is no fallback and the
/// original degraded behavior — the primary's own connection error — stands.
fn build_fallback_provider(
    registry: &ProviderRegistry,
    config: &Config,
) -> Option<FallbackProvider> {
    let primary = config.provider.to_string();
    registry
        .ordered()
        .into_iter()
        .find(|profile| profile.name != primary && profile_is_usable(profile))
        .map(|profile| FallbackProvider {
            name: profile.name.clone(),
            provider: chat_provider_from_profile(profile, &config.default_model),
        })
}

/// Ollama can supply embeddings to a provider that has no endpoint of its own.
/// An empty `OLLAMA_URL` skips this rung — never a default localhost that is not
/// running — and the caller may try the OpenAI-compatible fallback next.
fn ollama_embeddings_fallback(
    config: &Config,
    provider: &str,
) -> Option<Arc<dyn EmbeddingProvider>> {
    if config.ollama_url.trim().is_empty() {
        tracing::warn!(
            provider,
            "embeddings are disabled: this provider does not serve them and no OLLAMA_URL or OPENAI_BASE_URL/OPENROUTER_API_KEY fallback is configured"
        );
        None
    } else {
        Some(Arc::new(OllamaEmbeddings::new(
            config.ollama_url.clone(),
            config.embedding_model.clone(),
            config.embedding_dimensions,
        )))
    }
}

/// An OpenAI-compatible origin can also serve embeddings to a provider with no
/// endpoint of its own (notably OpenCode and Anthropic). It reuses the two
/// settings that already exist for chat — `OPENAI_BASE_URL` and
/// `OPENROUTER_API_KEY` — so no new configuration is introduced. Both must be
/// present: an empty URL means there is no upstream, and an empty key would
/// only turn a working primary connection error into a spurious 401.
///
/// OpenRouter's hosted origin (or any `/api/v1` origin) uses that adapter's
/// path; every other origin is treated as a plain OpenAI-compatible endpoint.
/// The key is never placed in a URL, an error or a log line — the warning below
/// names only the provider and the two variable names.
pub fn openai_compatible_embeddings_fallback(
    config: &Config,
    provider: &str,
) -> Option<Arc<dyn EmbeddingProvider>> {
    let base = config
        .openai_base_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())?;
    let Some(key) = config
        .openrouter_api_key
        .as_deref()
        .filter(|key| !key.is_empty())
    else {
        tracing::warn!(
            provider,
            "embeddings fallback: OPENAI_BASE_URL is set but OPENROUTER_API_KEY is empty"
        );
        return None;
    };
    let model = config.embedding_model.clone();
    let dims = config.embedding_dimensions;
    let embedded: Arc<dyn EmbeddingProvider> =
        if base.contains("openrouter.ai") || base.trim_end_matches('/').ends_with("/api/v1") {
            // `with_endpoint` expects an *origin*: it appends `/api/v1/embeddings`
            // itself. `OPENAI_BASE_URL` is conventionally given with `/api/v1`
            // (the documented OpenRouter value), so strip that suffix here —
            // mirroring `OpenAiCompatibleEmbeddings::new`'s `/v1` strip — or the
            // request lands on `/api/v1/api/v1/embeddings` and 404s.
            let base = base.trim_end_matches('/');
            let base = base.strip_suffix("/api/v1").unwrap_or(base);
            Arc::new(OpenRouterEmbeddings::with_endpoint(
                base.to_string(),
                key.to_string(),
                model,
                dims,
            ))
        } else {
            Arc::new(OpenAiCompatibleEmbeddings::new(
                base.to_string(),
                key.to_string(),
                model,
                dims,
            ))
        };
    Some(embedded)
}

/// Build the whole provider-dependent stack from the single `PROVIDER` choice.
///
/// One setting selects chat, the decision router and — where the upstream can
/// serve them — embeddings; there is no per-service mix. Credentials are passed
/// to each adapter as a separate argument and never placed in a URL, an error
/// string or a log line. A capability an upstream cannot offer is reported as
/// missing rather than faked, so health can degrade honestly.
pub fn build_provider_stack(config: &Config) -> Result<ProviderStack, AppError> {
    // Routing is inert by default, mirroring the Ollama path in
    // `build_app_state`. Its tuning lives in `RouterConfig`, not `Config`;
    // loading it here is what gives the non-Ollama decision adapters the
    // operator's `ROUTER_MODEL`/`ROUTER_TIMEOUT_MS` instead of hard-coded values.
    // An unusable config is non-fatal, exactly as on the Ollama boot path.
    let router_config = match RouterConfig::load() {
        Ok(loaded) => loaded,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "Invalid router configuration; falling back to defaults (routing disabled)"
            );
            RouterConfig::default()
        }
    };

    // Profile overlays (`providers.toml`) are read once and passed into the
    // direct adapters below, so a custom `api_key_env` or `base_url` reaches
    // the selected provider and its embeddings instead of a built-in guess.
    let registry = load_provider_registry();
    // One bounded fallback upstream, chosen deterministically from the same
    // registry the primary was built from. Built before the match so every arm
    // shares it; embeddings and router stay on the primary (no per-service mix).
    let fallback_provider = build_fallback_provider(&registry, config);

    let provider: Arc<dyn LLMProvider + Send + Sync>;
    let embeddings: Option<Arc<dyn EmbeddingProvider>>;
    let router: Option<Arc<dyn DecisionRouter>>;

    match config.provider {
        ProviderKind::Ollama => {
            provider = Arc::new(OllamaProvider::with_timeout(
                config.ollama_url.clone(),
                config.default_model.clone(),
                std::time::Duration::from_millis(config.http_timeout_ms),
            ));
            embeddings = Some(Arc::new(OllamaEmbeddings::new(
                config.ollama_url.clone(),
                config.embedding_model.clone(),
                config.embedding_dimensions,
            )));
            // Left `None` so `build_app_state` builds `LlamaCppRouter` from the
            // same `RouterConfig` as before — the default boot path is unchanged.
            router = None;
        }
        ProviderKind::OpenRouter => {
            let base_url = config
                .openai_base_url
                .clone()
                .unwrap_or_else(|| OPENROUTER_BASE_URL.to_string());
            provider = Arc::new(OpenAiCompatibleProvider::with_timeout(
                base_url,
                config.openrouter_api_key.clone().unwrap_or_default(),
                config.default_model.clone(),
                std::time::Duration::from_millis(config.http_timeout_ms),
            ));
            embeddings = Some(Arc::new(OpenRouterEmbeddings::new(
                config.openrouter_api_key.clone().unwrap_or_default(),
                config.embedding_model.clone(),
                config.embedding_dimensions,
            )));
            router = if router_config.enabled {
                Some(Arc::new(OpenRouterRouter::new(
                    config.openrouter_api_key.clone().unwrap_or_default(),
                    router_config.model.clone(),
                    router_config.timeout_ms,
                )))
            } else {
                None
            };
        }
        ProviderKind::OpenCode => {
            provider = Arc::new(OpenCodeProvider::with_timeout(
                config.opencode_url.clone(),
                config.opencode_api_key.clone(),
                config.default_model.clone(),
                std::time::Duration::from_millis(config.http_timeout_ms),
            ));
            // OpenCode serves no embeddings endpoint. Ollama can supply them
            // when a URL was configured; otherwise an OpenAI-compatible URL +
            // key can. With neither, the capability stays missing and health
            // says why. No key value is ever logged.
            embeddings = ollama_embeddings_fallback(config, "opencode")
                .or_else(|| openai_compatible_embeddings_fallback(config, "opencode"));
            router = if router_config.enabled {
                Some(Arc::new(OpenCodeRouter::new(
                    config.opencode_url.clone(),
                    config.opencode_api_key.clone(),
                    router_config.model.clone(),
                    router_config.timeout_ms,
                )))
            } else {
                None
            };
        }
        // Groq/Mistral/DeepSeek/xAI and any self-hosted compatible gateway
        // speak OpenAI's chat and embeddings wires, so they reuse the same
        // adapters with the profile's base URL — no duplicated client.
        ProviderKind::OpenAiCompatible => {
            let profile = profile_for(&registry, config);
            let key = profile_api_key(&profile);
            let base_url = profile
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com".to_string());
            provider = Arc::new(OpenAiCompatibleProvider::with_provider_name_and_timeout(
                base_url.clone(),
                key.clone(),
                config.default_model.clone(),
                config.provider.to_string(),
                std::time::Duration::from_millis(config.http_timeout_ms),
            ));
            embeddings = Some(Arc::new(OpenAiCompatibleEmbeddings::new(
                base_url,
                key,
                config.embedding_model.clone(),
                config.embedding_dimensions,
            )));
            // No d1 decisions endpoint exists on the OpenAI wire. `None` lets
            // `build_app_state` install the separate llama.cpp d1 router when
            // routing is enabled, exactly as on the Ollama path.
            router = None;
        }
        // Anthropic serves no embeddings endpoint of its own; an Ollama URL or
        // an OpenAI-compatible URL + key can supply them. With neither, the
        // capability stays missing and health says why.
        ProviderKind::Anthropic => {
            let profile = profile_for(&registry, config);
            provider = Arc::new(AnthropicProvider::new(
                profile.base_url.clone(),
                profile_api_key(&profile),
                config.default_model.clone(),
            ));
            embeddings = ollama_embeddings_fallback(config, "anthropic")
                .or_else(|| openai_compatible_embeddings_fallback(config, "anthropic"));
            router = None;
        }
        ProviderKind::Gemini => {
            let profile = profile_for(&registry, config);
            let key = profile_api_key(&profile);
            let base_url = profile.base_url.clone();
            provider = Arc::new(GeminiProvider::new(
                base_url.clone(),
                key.clone(),
                config.default_model.clone(),
            ));
            embeddings = Some(Arc::new(GeminiEmbeddings::new(
                base_url,
                key,
                config.embedding_model.clone(),
                config.embedding_dimensions,
            )));
            router = None;
        }
    }

    tracing::info!(provider = %config.provider, "Provider stack initialized");

    Ok(ProviderStack {
        provider,
        agent_runtime: None,
        rag_store: None,
        rag_ingest: None,
        embeddings,
        router,
        fallback_provider,
        default_model: config.default_model.clone(),
        provider_name: config.provider.to_string(),
        engine_target: None,
    })
}

/// Build embeddings + persistent RAG store when enabled.
///
/// Always compiled; `RAG_ENABLED` decides at runtime whether a store is built.
/// `embeddings` is the SELECTED stack's provider. Without one there is no
/// embedder to build a store around: RAG stays off and the debug endpoints give
/// their explicit `Unavailable` reason instead of a 500 from a broken embedder.
fn build_rag(
    config: &Config,
    embeddings: Option<Arc<dyn EmbeddingProvider>>,
) -> (Option<Arc<dyn RagStore>>, Option<Arc<RagIngestService>>) {
    if !config.rag_enabled {
        tracing::info!("RAG disabled via RAG_ENABLED=false");
        return (None, None);
    }

    let Some(embeddings) = embeddings else {
        tracing::warn!(
            "RAG unavailable: the selected provider exposes no embeddings and no OLLAMA_URL or OPENAI_BASE_URL/OPENROUTER_API_KEY fallback is configured"
        );
        return (None, None);
    };

    let dir = crate::core::paths::get_lancedb_dir();
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(path = %dir.display(), error = %err, "failed to create RAG data dir");
    }
    tracing::info!(
        path = %dir.display(),
        embedding_model = %config.embedding_model,
        dimensions = config.embedding_dimensions,
        "RAG store initialized (FileRagStore)"
    );
    let store: Arc<dyn RagStore> = Arc::new(FileRagStore::new(dir, embeddings));
    let ingest = Arc::new(RagIngestService::new(store.clone()));
    (Some(store), Some(ingest))
}

/// Select the injected summarizer strategy at boot.
///
/// The summarizer POSTs an Ollama-wire `/api/chat`, so it is only wired for the
/// Ollama provider. Other providers still persist conversations; only the
/// best-effort summaries are disabled (with a warning), never pointed at a
/// wrong/empty URL. This is a boot-time decision: history lives on `AppState`,
/// not on the hot-swapped stack. Exposed for the integration tests that pin
/// this degradation rule.
#[cfg(feature = "history")]
#[doc(hidden)]
pub fn build_summarizer(
    provider: ProviderKind,
    ollama_url: &str,
    default_model: &str,
) -> Arc<dyn crate::ports::history::Summarizer> {
    use crate::adapters::history::summarizer::{NoopSummarizer, OllamaSummarizer};

    if provider == ProviderKind::Ollama {
        Arc::new(OllamaSummarizer::new(ollama_url, default_model))
    } else {
        tracing::warn!(
            provider = %provider,
            "history summaries disabled: the summarizer speaks the Ollama wire format; conversations still persist"
        );
        Arc::new(NoopSummarizer)
    }
}

/// Build the per-agent retention overrides the history purge applies.
///
/// Keyed by `agent_qualified_id` — the exact column the SQLite purge reads — so
/// an agent's `[memory] retention_days` wins for its own conversations without
/// affecting other agents. A value of `0` is invalid (it is reserved for the
/// global kill-switch): warn and omit that entry so the global value applies,
/// never clamp it to a positive number. `None` when no agent has a usable
/// override, preserving the legacy "global only" path. Rebuilt from the live
/// registry on every purge so hot reloads are honoured.
pub fn build_agent_retention_map(registry: &AgentRegistry) -> Option<HashMap<String, u32>> {
    let mut per_agent = HashMap::new();
    for registered in registry.list() {
        let days = registered.agent.config.memory.retention_days;
        if days == 0 {
            tracing::warn!(
                agent_id = %registered.agent.qualified_id(),
                "agent [memory] retention_days is 0; ignoring the override and using the global retention"
            );
            continue;
        }
        per_agent.insert(registered.agent.qualified_id(), days);
    }
    if per_agent.is_empty() {
        None
    } else {
        Some(per_agent)
    }
}

/// Build history store (SQLite) when feature is enabled.
///
/// The summarizer is injected from [`build_summarizer`] so the boot provider
/// decides the strategy: Ollama gets the Ollama-wire summarizer, every other
/// provider gets the quiet one. This is a boot-time decision: history lives on
/// `AppState`, not on the hot-swapped stack.
fn build_history_store(
    config: &Config,
    provider: ProviderKind,
) -> Option<Arc<dyn ConversationStore>> {
    #[cfg(feature = "history")]
    {
        use crate::adapters::history::SqliteConversationStore;

        let path = crate::core::paths::get_history_db_path();
        let summarizer = build_summarizer(provider, &config.ollama_url, &config.default_model);
        match SqliteConversationStore::with_summarizer(&path, summarizer) {
            Ok(store) => {
                tracing::info!(path = %path.display(), "History store (SQLite) initialized");
                Some(Arc::new(store))
            }
            Err(err) => {
                tracing::warn!(error = %err, "Failed to open history store; history disabled");
                None
            }
        }
    }

    #[cfg(not(feature = "history"))]
    {
        let _ = (config, provider);
        tracing::info!("History feature disabled at compile time");
        None
    }
}

/// Build the durable execution store from the same `history.db` the conversation
/// store uses, so an older database keeps its legacy rows untouched.
///
/// Never fatal: an unopenable store degrades to the no-op substitute with a
/// warning, so boot and chats keep working without durability (Fase 7 adds no
/// new hard failure mode). Under `--no-default-features` there is no `rusqlite`
/// at all and the no-op store is the only option.
fn build_execution_store() -> Arc<dyn ExecutionStore> {
    #[cfg(feature = "history")]
    {
        use crate::adapters::execution::SqliteExecutionStore;

        let path = crate::core::paths::get_history_db_path();
        match SqliteExecutionStore::new(&path) {
            Ok(store) => {
                tracing::info!(path = %path.display(), "Execution store (SQLite) initialized");
                Arc::new(store)
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "Failed to open execution store; durable execution disabled (chat still works)"
                );
                Arc::new(NoOpExecutionStore)
            }
        }
    }

    #[cfg(not(feature = "history"))]
    {
        tracing::info!("Execution store disabled: history feature not compiled in");
        Arc::new(NoOpExecutionStore)
    }
}

/// Boot recovery: mark every turn left mid-flight by a crash (`Running` or
/// `WaitingApproval`) as `Interrupted`, so it can be retried from scratch with
/// tool effects still protected by their recorded `operation_id`.
///
/// Returns the number of turns marked. A turn that cannot be transitioned is
/// logged and skipped rather than failing boot — recovery is best effort, like
/// the store itself. Called once from [`build_runtime`]; no background task.
pub async fn mark_interrupted(store: &dyn ExecutionStore) -> Result<usize, String> {
    let interrupted = store.list_interrupted().await?;
    let mut marked = 0usize;
    for execution in interrupted {
        match store
            .set_status(execution.id, ExecutionStatus::Interrupted, None)
            .await
        {
            Ok(()) => marked += 1,
            Err(err) => tracing::warn!(
                execution_id = %execution.id,
                error = %err,
                "Failed to mark an interrupted execution at boot"
            ),
        }
    }
    if marked > 0 {
        tracing::warn!(
            count = marked,
            "Boot recovery marked interrupted agent turns; they can be retried"
        );
    }
    Ok(marked)
}

/// Handles from `AppState` that the provider-dependent companions need. Kept as
/// a struct so the SAME builder runs at boot (from the freshly built `AppState`)
/// and on a settings Apply (from a live `AppState`) without either path building
/// a stack twice or swapping a half-built stack into the running process.
#[derive(Clone)]
pub struct CompanionHandles {
    pub mcp_registry: Arc<dyn McpServerRegistry>,
    pub agent_registry: Arc<AgentRegistry>,
    pub skill_manager: Arc<SkillManager>,
    pub context_store: Arc<ContextStore>,
    pub history_store: Option<Arc<dyn ConversationStore>>,
    /// The boot-owned gate, read from `AppState` on a settings Apply so pending
    /// approvals are never dropped by a stack rebuild.
    pub approval_gate: Arc<ApprovalGate>,
    /// The boot-owned automatic-approval decider, read from `AppState` on a
    /// settings Apply just like the gate, so an engine rebuild keeps the same
    /// `Auto` behaviour (and survives Apply).
    pub auto_decider: Option<Arc<dyn AutoDecider>>,
    /// The context-compaction strategy, read from `AppState` on a settings Apply
    /// so an engine rebuild keeps the same assembler inputs.
    pub compactor: Arc<dyn ContextCompactor>,
    /// The complexity model selector, read from `AppState` on a settings Apply
    /// so an engine rebuild keeps the same selector (and the same router Arc).
    pub selector: Arc<dyn ModelSelector>,
    /// The app-wide counters, read from `AppState` on a settings Apply like the
    /// gate and decider, so an engine rebuild keeps accumulating into the same
    /// observability snapshot.
    pub observability: Arc<AppObservability>,
    /// The boot-owned execution store, read from `AppState` on a settings Apply
    /// so completed tool operations survive an engine rebuild.
    pub execution_store: Arc<dyn ExecutionStore>,
    /// The bounded delegator, installed on `AppState` at boot. Read here on a
    /// settings Apply so an engine rebuild keeps the same handle (and its
    /// `Weak<AppState>` still points at the live state).
    pub delegator: Option<Arc<dyn Delegator>>,
    /// The boot-owned trace exporter, read from `AppState` on a settings Apply
    /// just like the compactor and selector, so a provider swap never changes
    /// where finished-turn traces are sent (or that they are disabled).
    pub exporter: Arc<dyn TraceExporter>,
}

impl CompanionHandles {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            mcp_registry: state.mcp_registry.clone(),
            agent_registry: state.agent_registry.clone(),
            skill_manager: state.skill_manager.clone(),
            context_store: state.context_store.clone(),
            history_store: state.history_store.clone(),
            approval_gate: state.approval_gate.clone(),
            auto_decider: state.auto_decider.clone(),
            compactor: state.compactor.clone(),
            selector: state.selector.clone(),
            observability: state.observability.clone(),
            execution_store: state.execution_store.clone(),
            delegator: state.delegator(),
            exporter: state.exporter.clone(),
        }
    }
}

/// Build the provider-dependent companions onto `stack` from the selected
/// provider. Called once at boot and once per settings Apply. RAG uses the
/// stack's embedder (or is absent), and the Rig engine is attached for every
/// provider with a completion client (all but OpenCode). The boot target is a
/// starting point: each request rebuilds it from its resolved provider, so an
/// agent may override `PROVIDER` with its own `provider` in TOML.
/// A capability the provider cannot offer is reported, never silently faked.
pub fn build_companions(handles: &CompanionHandles, config: &Config, stack: &mut ProviderStack) {
    let (rag_store, rag_ingest) = build_rag(config, stack.embeddings.clone());

    #[cfg(feature = "rig-engine")]
    let agent_runtime = {
        use crate::adapters::rig_engine::{
            engine_target_for, local_tools::LocalToolRegistry, RigAgentEngine,
        };

        // The registry is loaded once at boot (and again on a settings Apply):
        // a malformed `tools/*.toml` is a path-naming config error the operator
        // should see, but it is never fatal — the built-ins always exist.
        let local_tools = Arc::new(match LocalToolRegistry::load() {
            Ok(registry) => registry,
            Err(err) => {
                tracing::warn!(error = %err, "failed to load local tool manifests; falling back to the built-ins");
                LocalToolRegistry::builtins()
            }
        });

        let engine_target = engine_target_for(config);
        stack.engine_target = engine_target.as_ref().map(|target| target.kind());
        engine_target.map(|target| {
            let engine: Arc<dyn AgentEngine> = Arc::new(
                RigAgentEngine::from_target(
                    target,
                    handles.mcp_registry.clone(),
                    rag_store.clone(),
                    config.clone(),
                    handles.skill_manager.clone(),
                    local_tools.clone(),
                    handles.approval_gate.clone(),
                )
                .with_execution_store(handles.execution_store.clone())
                .with_auto_decider(handles.auto_decider.clone())
                .with_observability(Some(handles.observability.clone()))
                .with_delegator(handles.delegator.clone()),
            );
            Arc::new(AgentRuntime {
                registry: handles.agent_registry.clone(),
                engine,
                history: handles.history_store.clone(),
                rag: rag_store.clone(),
                // Same constructor as boot and the AppState enricher: the
                // selected stack's embedder ranks this agent's skills.
                enricher: build_context_enricher(
                    handles.context_store.clone(),
                    handles.skill_manager.clone(),
                    config.default_model.clone(),
                    config.provider,
                    stack.embeddings.clone(),
                ),
                // Default NoopCompactor, shared by boot and Apply through
                // AppState so a provider swap cannot drop the strategy.
                compactor: handles.compactor.clone(),
                // Same selector (and router Arc) on boot and Apply, so a
                // provider swap cannot drop complexity selection.
                selector: handles.selector.clone(),
            })
        })
    };

    #[cfg(not(feature = "rig-engine"))]
    let agent_runtime = {
        let _ = handles;
        stack.engine_target = None;
        None
    };

    if agent_runtime.is_none() {
        // Capability loss is never silent: name the provider and what is lost.
        tracing::warn!(
            provider = %config.provider,
            "no agent engine for this provider: agent tools, RAG context and MCP are unavailable; agent-scoped chats degrade to a direct chat answer"
        );
    }

    stack.agent_runtime = agent_runtime;
    stack.rag_store = rag_store;
    stack.rag_ingest = rag_ingest;
}

/// Convenience: build companions using the handles on a live `AppState`. Used
/// by a settings Apply and by tests that install companions onto a running
/// state; boot uses [`build_companions`] directly (no `AppState` yet).
pub fn build_companions_for_state(state: &AppState, config: &Config, stack: &mut ProviderStack) {
    build_companions(&CompanionHandles::from_state(state), config, stack);
}

/// Rebuild the `AppState` skill-ranking enricher from a selected stack and
/// install it on a running state. Called on every settings Apply right after
/// the companions are rebuilt: `chat_core` reads this enricher, so leaving the
/// boot-time one in place would keep ranking skills with the old provider's
/// embedder after a swap. No lock is held across an await here — the guard in
/// [`AppState::replace_context_enricher`] is released as soon as the value is
/// installed.
pub fn install_state_enricher(state: &AppState, config: &Config, stack: &ProviderStack) {
    let enricher = build_context_enricher(
        state.context_store.clone(),
        state.skill_manager.clone(),
        config.default_model.clone(),
        config.provider,
        stack.embeddings.clone(),
    );
    state.replace_context_enricher(enricher);
}

/// Load MCP server configs from the `mcp-servers/` directory.
/// Returns (server_ids, configs) for enabled servers.
fn load_mcp_server_configs() -> (Vec<String>, Vec<McpServerConfig>) {
    let dir = crate::core::paths::get_base_dir().join("mcp-servers");
    if !dir.exists() {
        return (Vec::new(), Vec::new());
    }
    let mut configs = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(&path) {
                match toml::from_str::<McpServerConfig>(&content) {
                    Ok(cfg) => {
                        if cfg.enabled {
                            configs.push(cfg);
                        }
                    }
                    Err(err) => {
                        tracing::warn!(path = %path.display(), error = %err, "Failed to parse MCP server config");
                    }
                }
            }
        }
    }

    let ids: Vec<String> = configs.iter().map(|c| c.id.clone()).collect();
    tracing::info!(count = configs.len(), server_ids = ?ids, "Loaded MCP server configs");
    (ids, configs)
}

pub fn start_grpc_server(runtime: &Runtime) -> tokio::task::JoinHandle<()> {
    let state = runtime.state.clone();
    let grpc_addr = runtime.grpc_addr;
    let grpc_service = GrpcService::new(state.clone());
    tokio::spawn(async move {
        state.grpc_running.store(true, Ordering::SeqCst);
        let serve_result = TonicServer::builder()
            .add_service(LlamaGatewayServer::new(grpc_service))
            .serve_with_shutdown(grpc_addr, shutdown_signal())
            .await;
        state.grpc_running.store(false, Ordering::SeqCst);
        if let Err(err) = serve_result {
            tracing::error!(error = %err, addr = %grpc_addr, "gRPC server stopped unexpectedly");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::router::{DecisionRequest, DecisionResponse, DecisionRouter};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn auto_decider_wraps_the_routing_router_at_the_configured_threshold() {
        struct CountingRouter(AtomicUsize);

        #[async_trait::async_trait]
        impl DecisionRouter for CountingRouter {
            async fn decide(&self, _req: DecisionRequest) -> Result<DecisionResponse, AppError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(DecisionResponse {
                    answers: Default::default(),
                })
            }
        }

        let counting = Arc::new(CountingRouter(AtomicUsize::new(0)));
        let router: Arc<dyn DecisionRouter> = counting.clone();
        let config = RouterConfig {
            enabled: true,
            min_confidence: 0.73,
            ..RouterConfig::default()
        };

        let decider =
            build_auto_decider(&config, Some(&router)).expect("routing on and router present");
        assert_eq!(
            decider.min_confidence(),
            0.73,
            "configured default is carried"
        );
        decider
            .should_allow("tool", &serde_json::json!({}), "hash", 0.9)
            .await;
        assert_eq!(
            counting.0.load(Ordering::SeqCst),
            1,
            "the decider must call the very router the ladder uses"
        );

        let disabled = RouterConfig {
            enabled: false,
            ..config.clone()
        };
        assert!(
            build_auto_decider(&disabled, Some(&router)).is_none(),
            "routing off means every Auto tool still asks a human"
        );
        assert!(
            build_auto_decider(&config, None).is_none(),
            "no router means no decider"
        );
    }

    #[test]
    fn router_origin_extracts_host_and_port() {
        assert_eq!(
            router_origin("http://127.0.0.1:8080").as_deref(),
            Some("127.0.0.1:8080")
        );
        assert_eq!(
            router_origin("http://127.0.0.1:8080/").as_deref(),
            Some("127.0.0.1:8080"),
            "a trailing slash is the documented spelling and must not leak into the origin"
        );
        assert_eq!(
            router_origin("https://router.local").as_deref(),
            Some("router.local:443")
        );
        assert_eq!(
            router_origin("router.local:9000").as_deref(),
            Some("router.local:9000"),
            "a scheme-less host:port still probes"
        );
        assert_eq!(
            router_origin("http://[::1]:8080/v1").as_deref(),
            Some("[::1]:8080"),
            "an IPv6 literal keeps its brackets, the spelling ToSocketAddrs accepts"
        );
        assert_eq!(
            router_origin("http://user:pass@host:8080").as_deref(),
            Some("host:8080")
        );
    }

    #[test]
    fn a_url_without_a_usable_authority_has_no_origin() {
        assert_eq!(router_origin(""), None);
        assert_eq!(router_origin("http://"), None);
        assert_eq!(
            router_origin("http://host:not-a-port"),
            None,
            "a non-numeric port is refused, not guessed"
        );
    }

    /// Draw an ephemeral port, release the listener, and let the probe answer
    /// whether the port is now closed. `true` means the probe DID report the
    /// released port unreachable; `false` means every drawn port was grabbed
    /// by another test before a probe could see it closed.
    ///
    /// The bind-drop-probe sequence is the retryable unit. A fresh port is
    /// drawn per attempt because once a listener is dropped its port is
    /// genuinely free — `adapters::rag::embeddings` tests bind ephemeral
    /// listeners in this same lib binary and the kernel may hand them the
    /// just-released port. Retrying the sequence instead of asserting on a
    /// single draw is what keeps contention from ever failing this test.
    fn released_port_probes_as_closed(attempts: u32) -> bool {
        for _ in 0..attempts {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("the kernel hands out an ephemeral port on request");
            let port = listener
                .local_addr()
                .expect("listener has an address")
                .port();
            drop(listener);
            if !probe_router_origin(&format!("http://127.0.0.1:{port}"), 1000) {
                return true;
            }
        }
        false
    }

    /// The probe must see a listener and must report a closed port as
    /// unreachable rather than hang or panic.
    ///
    /// The held-listener half is uncontended by construction, not by luck:
    /// the kernel never assigns a bound, listening port to another `:0` bind,
    /// rebinding a live port needs `SO_REUSEPORT` (std and tokio set only
    /// `SO_REUSEADDR`, which does not allow two live listeners), and no test
    /// in this lib binary binds a fixed port. The probe connects to a port
    /// this test exclusively owns for the whole half.
    ///
    /// The released-port half IS contended, so the observation that the probe
    /// reports the port closed and the test's pass condition are one event:
    /// a second, later probe of the same port would reopen exactly the race
    /// window this replaces — another test could draw the port between the
    /// two probes. Contention instead retries the whole sequence with a
    /// fresh port (`released_port_probes_as_closed`), and only a bounded run
    /// of collisions fails the test, with the port-acquisition message it
    /// deserves rather than a misleading "probe saw a listener".
    #[test]
    fn the_probe_sees_a_listener_and_misses_a_closed_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(probe_router_origin(
            &format!("http://127.0.0.1:{port}"),
            1000
        ));
        drop(listener);
        assert!(
            released_port_probes_as_closed(8),
            "could not observe a released port as closed in 8 attempts — \
             ephemeral-port contention in this test binary never relented"
        );
    }
}
