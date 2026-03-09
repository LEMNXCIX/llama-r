use crate::api::agent_api::{create_agent, delete_agent, get_agent, list_agents_api, update_agent};
use crate::api::context_api::{analyze_project, create_context, delete_context, get_context, list_contexts, update_context};
use crate::api::grpc::{pb::llama_gateway_server::LlamaGatewayServer, GrpcService};
use crate::api::handlers::{chat, list_models, mcp_message, openai_chat, AppState};
use crate::api::health::health;
use crate::config::Config;
use crate::context::analyzer::ContextEnricher;
use crate::context::store::ContextStore;
use crate::core::hot_reload::HotReloader;
use crate::error::AppError;
use crate::optimizer::metrics::TokenMetrics;
use crate::providers::ollama::OllamaProvider;
use crate::providers::LLMProvider;
use crate::services::agent_manager::AgentManager;
use crate::services::skill_manager::SkillManager;
use axum::{routing::{get, post}, Router};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
    agent_manager: Arc<AgentManager>,
    skill_manager: Arc<SkillManager>,
    context_store: Arc<ContextStore>,
    default_model: String,
    logs: Arc<Mutex<VecDeque<String>>>,
) -> Arc<AppState> {
    let metrics = Arc::new(TokenMetrics::new());
    let context_enricher = Arc::new(ContextEnricher::new(
        context_store.clone(),
        skill_manager.clone(),
        default_model,
    ));

    Arc::new(AppState {
        provider,
        agent_manager,
        skill_manager,
        context_store,
        context_enricher,
        metrics,
        api_running: AtomicBool::new(false),
        grpc_running: AtomicBool::new(false),
        logs,
    })
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderName::from_static("x-agent"),
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
        .route("/api/contexts", get(list_contexts).post(create_context))
        .route("/api/contexts/:id", get(get_context).put(update_context).delete(delete_context))
        .route("/api/contexts/:id/analyze", post(analyze_project))
        .layer(cors)
        .with_state(state)
}

pub async fn build_runtime(logs: Arc<Mutex<VecDeque<String>>>) -> Result<Runtime, AppError> {
    let mut config = Config::from_env()?;

    if let Err(err) = crate::core::paths::ensure_dirs() {
        tracing::error!(error = %err, "Failed to create data directories");
    }

    let agent_manager = Arc::new(AgentManager::new());
    if let Err(err) = agent_manager.load_agents() {
        tracing::error!(error = %err, "Failed to load agents on startup; continuing with partial state");
    }

    let base_dir = crate::core::paths::get_base_dir();
    let reloader = HotReloader::new(agent_manager.clone());
    if let Err(err) = reloader.watch(&base_dir) {
        tracing::error!(path = %base_dir.display(), error = %err, "Failed to watch base folder; hot reload disabled");
    }

    let skill_manager = Arc::new(SkillManager::new());
    skill_manager.scan_and_load();

    let mut provider_impl: Arc<dyn LLMProvider + Send + Sync> =
        Arc::new(OllamaProvider::new(config.ollama_url.clone()));

    tracing::info!(provider_url = %config.ollama_url, "Verifying LLM provider health");
    let health_ok = provider_impl.health_check().await.is_ok();
    let needs_setup = !health_ok || !config.is_configured();

    if needs_setup {
        if !health_ok {
            println!("WARNING: LLM Provider at {} is not reachable.", config.ollama_url);
        } else {
            println!("Welcome to Llama-R. Let's configure your default provider.");
        }

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
                return Err(AppError::Runtime(format!(
                    "Interactive setup failed and no healthy provider is available: {}",
                    err
                )));
            }
        }
    } else {
        tracing::info!(default_model = %config.default_model, "Provider healthy; validating configured agent models");
        if let Ok(models) = provider_impl.list_models().await {
            let model_names: Vec<String> = models.iter().map(|model| model.name.clone()).collect();
            for agent in agent_manager.list_agents() {
                let model_to_check = if agent.config.model.is_empty() {
                    &config.default_model
                } else {
                    &agent.config.model
                };
                if !model_names.iter().any(|candidate| candidate == model_to_check) {
                    tracing::warn!(agent_id = %agent.id, model = %model_to_check, available_models = ?model_names, "Agent references unavailable model");
                }
            }
        }
    }

    let context_store = Arc::new(ContextStore::new());
    let state = build_app_state(
        provider_impl,
        agent_manager,
        skill_manager,
        context_store,
        config.default_model.clone(),
        logs,
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
        .map_err(|err| AppError::Runtime(format!("Failed to bind HTTP listener on {}: {}", runtime.http_addr, err)))?;
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
