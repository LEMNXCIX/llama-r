use crate::adapters::mcp::{McpServerConfig, StaticMcpRegistry};
#[cfg(feature = "rag")]
use crate::adapters::rag::{FileRagStore, OllamaEmbeddings};
use crate::api::agent_api::{
    create_agent, delete_agent, get_agent, get_agent_scope, list_agents_api, update_agent,
};
use crate::api::context_api::{
    analyze_project, create_context, delete_context, get_context, list_contexts, update_context,
};
use crate::api::docs::{serve_openapi_json, serve_scalar_ui};
use crate::api::grpc::{pb::llama_gateway_server::LlamaGatewayServer, GrpcService};
use crate::api::handlers::{chat, list_models, mcp_message, openai_chat, AppState};
use crate::api::health::health;
use crate::api::history_api::{
    delete_conversation_handler, export_conversation_handler, get_conversation_handler,
    get_conversation_messages_handler, list_conversations_handler,
};
use crate::api::observability::AppObservability;
use crate::api::rag_api::{rag_delete_document, rag_ingest, rag_query};
use crate::config::Config;
use crate::context::analyzer::ContextEnricher;
use crate::context::store::ContextStore;
use crate::core::hot_reload::HotReloader;
use crate::error::AppError;
use crate::optimizer::metrics::TokenMetrics;
#[cfg(feature = "rig-engine")]
use crate::ports::engine::AgentEngine;
use crate::ports::history::ConversationStore;
use crate::ports::mcp::McpServerRegistry;
#[cfg(feature = "rag")]
use crate::ports::rag::EmbeddingProvider;
use crate::ports::rag::RagStore;
use crate::providers::ollama::OllamaProvider;
use crate::providers::LLMProvider;
use crate::services::agent_registry::AgentRegistry;
use crate::services::agent_runtime::AgentRuntime;
use crate::services::rag_ingest::RagIngestService;
use crate::services::skill_manager::SkillManager;
use axum::{
    routing::{get, post},
    Router,
};
use std::collections::VecDeque;
use std::fs;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tonic::transport::Server as TonicServer;
use tower_http::cors::{Any, CorsLayer};

pub struct Runtime {
    pub config: Config,
    pub state: Arc<AppState>,
    pub router: Router,
    pub http_addr: SocketAddr,
    pub grpc_addr: SocketAddr,
}

pub fn build_app_state(
    provider: Arc<dyn LLMProvider + Send + Sync>,
    agent_registry: Arc<AgentRegistry>,
    skill_manager: Arc<SkillManager>,
    context_store: Arc<ContextStore>,
    default_model: String,
    logs: Arc<Mutex<VecDeque<String>>>,
    known_mcp_servers: Vec<String>,
    mcp_registry: Arc<dyn McpServerRegistry>,
    agent_runtime: Option<Arc<AgentRuntime>>,
    rag_store: Option<Arc<dyn RagStore>>,
    rag_ingest: Option<Arc<RagIngestService>>,
    history_store: Option<Arc<dyn ConversationStore>>,
) -> Arc<AppState> {
    let metrics = Arc::new(TokenMetrics::new());
    let context_enricher = Arc::new(ContextEnricher::new(
        context_store.clone(),
        skill_manager.clone(),
        default_model.clone(),
    ));

    Arc::new(AppState {
        provider,
        agent_registry,
        skill_manager,
        context_store,
        context_enricher,
        metrics,
        observability: Arc::new(AppObservability::new()),
        default_model,
        api_running: AtomicBool::new(false),
        grpc_running: AtomicBool::new(false),
        logs,
        known_mcp_servers: RwLock::new(known_mcp_servers),
        mcp_registry,
        agent_runtime,
        rag_store,
        rag_ingest,
        history_store,
    })
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderName::from_static("x-project"),
            axum::http::HeaderName::from_static("x-agent"),
            axum::http::HeaderName::from_static("x-debug"),
            axum::http::HeaderName::from_static("x-conversation-id"),
        ]);

    Router::new()
        .route("/", get(|| async { "Llama-R API is running" }))
        .route("/health", get(health))
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
        .route("/api/agents", get(list_agents_api).post(create_agent))
        .route("/api/agents/:id", get(get_agent).put(update_agent).delete(delete_agent))
        .route("/api/agents/:id/scope", get(get_agent_scope))
        .route("/api/contexts", get(list_contexts).post(create_context))
        .route("/api/contexts/:id", get(get_context).put(update_context).delete(delete_context))
        .route("/api/contexts/:id/analyze", post(analyze_project))
        .route("/api/rag/ingest", post(rag_ingest))
        .route("/api/rag/query", post(rag_query))
        .route("/api/rag/delete-document", post(rag_delete_document))
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
        .route("/openapi.json", get(serve_openapi_json))
        .route("/docs", get(serve_scalar_ui))
        .layer(cors)
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

    let mut provider_impl: Arc<dyn LLMProvider + Send + Sync> =
        Arc::new(OllamaProvider::new(config.ollama_url.clone()));

    tracing::info!(provider_url = %config.ollama_url, "Verifying LLM provider health");
    let health_ok = provider_impl.health_check().await.is_ok();
    let is_terminal = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let plan = startup_plan::plan(startup_plan::PlanInput {
        health_ok,
        model_configured: config.is_configured(),
        is_terminal,
    });

    match plan {
        startup_plan::StartupPlan::Proceed => {}
        startup_plan::StartupPlan::Degraded => {
            // Never fatal: serve the API and let /health report "degraded".
            tracing::warn!(
                provider_url = %config.ollama_url,
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
            match crate::cli::interactive::run_interactive_setup(config.ollama_url.clone()).await {
                Ok((new_provider, selected_model)) => {
                    config.ollama_url = new_provider.get_base_url();
                    config.default_model = selected_model;
                    if let Err(err) = config.save_to_env() {
                        tracing::error!(error = %err, "Failed to persist configuration to .env");
                    }
                    provider_impl = new_provider;
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

    let (rag_store, rag_ingest) = build_rag(&config);
    let history_store = build_history_store(&config);

    if let Some(history_arc) = history_store.clone() {
        let retention = config.history_retention_days;
        let h1 = history_arc.clone();
        tokio::spawn(async move {
            match h1.purge_old_conversations(retention).await {
                Ok(n) => tracing::info!(deleted = n, "History purge on startup completed"),
                Err(e) => tracing::warn!(error = %e, "History purge on startup failed"),
            }
        });

        let h2 = history_arc.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(86_400));
            interval.tick().await;
            loop {
                interval.tick().await;
                match h2.purge_old_conversations(retention).await {
                    Ok(n) => tracing::info!(deleted = n, "History purge completed"),
                    Err(e) => tracing::warn!(error = %e, "History purge failed"),
                }
            }
        });
    }

    let agent_runtime = build_agent_runtime(
        &config,
        mcp_registry_inner.clone(),
        agent_registry.clone(),
        rag_store.clone(),
        history_store.clone(),
    );
    let state = build_app_state(
        provider_impl,
        agent_registry.clone(),
        skill_manager,
        context_store,
        config.default_model.clone(),
        logs,
        known_mcp_servers,
        mcp_registry,
        agent_runtime,
        rag_store,
        rag_ingest,
        history_store,
    );
    let router = build_router(state.clone());

    Ok(Runtime {
        http_addr: SocketAddr::from(([127, 0, 0, 1], config.port)),
        grpc_addr: SocketAddr::from(([127, 0, 0, 1], 50051)),
        config,
        state,
        router,
    })
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
        if let Err(err) = axum::serve(listener, app).await {
            state.api_running.store(false, Ordering::SeqCst);
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

/// Build embeddings + persistent RAG store when enabled.
fn build_rag(config: &Config) -> (Option<Arc<dyn RagStore>>, Option<Arc<RagIngestService>>) {
    if !config.rag_enabled {
        tracing::info!("RAG disabled via RAG_ENABLED=false");
        return (None, None);
    }

    #[cfg(feature = "rag")]
    {
        let embeddings: Arc<dyn EmbeddingProvider> = Arc::new(OllamaEmbeddings::new(
            config.ollama_url.clone(),
            config.embedding_model.clone(),
            config.embedding_dimensions,
        ));
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

    #[cfg(not(feature = "rag"))]
    {
        tracing::warn!("RAG feature disabled at compile time; store unavailable");
        let _ = config;
        (None, None)
    }
}

/// Build history store (SQLite) when feature is enabled.
fn build_history_store(config: &Config) -> Option<Arc<dyn ConversationStore>> {
    #[cfg(feature = "history")]
    {
        use crate::adapters::history::SqliteConversationStore;

        let path = crate::core::paths::get_history_db_path();
        match SqliteConversationStore::open(
            &path,
            config.ollama_url.clone(),
            config.default_model.clone(),
        ) {
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
        let _ = config;
        tracing::info!("History feature disabled at compile time");
        None
    }
}

/// Build the agent runtime (Rig engine) when the feature is enabled.
fn build_agent_runtime(
    config: &Config,
    mcp_registry: Arc<StaticMcpRegistry>,
    agent_registry: Arc<AgentRegistry>,
    rag: Option<Arc<dyn RagStore>>,
    history_store: Option<Arc<dyn ConversationStore>>,
) -> Option<Arc<AgentRuntime>> {
    #[cfg(feature = "rig-engine")]
    {
        use crate::adapters::rig_engine::RigAgentEngine;

        let engine: Arc<dyn AgentEngine> = Arc::new(RigAgentEngine::new(
            config.ollama_url.clone(),
            mcp_registry,
            rag.clone(),
        ));

        Some(Arc::new(AgentRuntime {
            registry: agent_registry,
            engine,
            history: history_store,
            rag,
        }))
    }

    #[cfg(not(feature = "rig-engine"))]
    {
        let _ = (config, mcp_registry, agent_registry, rag, history_store);
        None
    }
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
        if let Err(err) = TonicServer::builder()
            .add_service(LlamaGatewayServer::new(grpc_service))
            .serve(grpc_addr)
            .await
        {
            state.grpc_running.store(false, Ordering::SeqCst);
            tracing::error!(error = %err, addr = %grpc_addr, "gRPC server stopped unexpectedly");
        }
    })
}
