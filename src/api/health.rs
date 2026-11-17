use crate::api::handlers::AppState;
use crate::api::observability::ObservabilitySnapshot;
use axum::{extract::State, Json};
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use utoipa::ToSchema;

/// Router reachability as last observed. `healthy` is a last known value, not
/// a live probe: `health()` already probes the provider on every call, and a
/// second HTTP round trip per health check is a bad trade. A router that is
/// switched off reports `healthy: false` — nothing has checked it, and an
/// off feature must not be dressed up as a working one.
#[derive(Debug, Serialize, ToSchema)]
pub struct RouterHealth {
    pub enabled: bool,
    pub healthy: bool,
    pub url: String,
}

/// Which upstream one `PROVIDER` selected and which of its capabilities are
/// actually available. `reason` is `Some` only when a capability the operator
/// would expect is missing — never a place for credentials.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProviderHealth {
    pub name: String,
    pub embeddings: bool,
    /// Whether a Rig engine is actually attached for this provider. `false`
    /// for providers rig-core cannot serve (e.g. OpenCode).
    pub agent_engine: bool,
    /// Embeddings capability explanation (kept separate from the engine one so
    /// a stack that simply has no engine attached is not conflated with a
    /// provider that exposes no embeddings at all).
    pub reason: Option<String>,
    /// Names the lost capabilities (tools/RAG/MCP) when no engine is attached.
    pub agent_engine_reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HealthResponse {
    pub status: &'static str,
    pub provider_healthy: bool,
    pub agent_count: usize,
    pub context_count: usize,
    pub api_running: bool,
    pub grpc_running: bool,
    pub saved_tokens: usize,
    pub total_tokens_processed: usize,
    pub observability: ObservabilitySnapshot,
    pub router: RouterHealth,
    pub provider: ProviderHealth,
}

#[utoipa::path(
    get,
    path = "/health",
    tag = "Health",
    responses(
        (status = 200, description = "Health check with provider, agents, contexts, and observability status", body = HealthResponse),
    )
)]
pub async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    state.observability.record_http_request();
    let services = state.services_snapshot().await;
    let provider_healthy = services.provider.health_check().await.is_ok();
    let router_config = state.router_config.clone();
    let router_healthy = state.router_healthy.load(Ordering::SeqCst);
    let has_engine = services.agent_runtime.is_some();
    let provider = ProviderHealth {
        name: services.provider_name.clone(),
        embeddings: services.embeddings.is_some(),
        agent_engine: has_engine,
        // A stack without embeddings is a capability gap, not an ordinary
        // state. Name it and explain why; no field here ever carries a key.
        reason: if services.embeddings.is_some() {
            None
        } else {
            Some(format!(
                "embeddings are disabled: the '{}' provider does not serve them and no OLLAMA_URL fallback is configured",
                services.provider_name
            ))
        },
        // The engine gap uses the same message chat_core logs on degradation,
        // so health and the warning can never disagree about what is lost.
        agent_engine_reason: if has_engine {
            None
        } else {
            Some(crate::api::chat_core::agent_engine_unavailable_message(
                &services.provider_name,
            ))
        },
    };
    Json(HealthResponse {
        // A switched-off router must never degrade the service: `enabled` is
        // false by default, and that default has to keep reading "ok". A
        // router that IS enabled but has not answered (or last errored) is a
        // capability the operator asked for and is not getting — degraded,
        // by the same rule the provider follows.
        status: if provider_healthy && (!router_config.enabled || router_healthy) {
            "ok"
        } else {
            "degraded"
        },
        provider_healthy,
        agent_count: state.agent_registry.list_agents().len(),
        context_count: state.context_store.list_contexts().len(),
        api_running: state.api_running.load(Ordering::SeqCst),
        grpc_running: state.grpc_running.load(Ordering::SeqCst),
        saved_tokens: state.metrics.get_saved_tokens(),
        total_tokens_processed: state.metrics.get_total_processed(),
        observability: state.observability.snapshot(),
        router: RouterHealth {
            enabled: router_config.enabled,
            healthy: router_healthy,
            url: router_config.url.clone(),
        },
        provider,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::await_holding_lock)] // tests hold an env lock across .await to serialise access
    use super::*;
    use crate::adapters::mcp::StaticMcpRegistry;
    use crate::api::observability::AppObservability;
    use crate::context::analyzer::ContextEnricher;
    use crate::context::store::ContextStore;
    use crate::domain::models::{ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
    use crate::optimizer::metrics::TokenMetrics;
    use crate::providers::LLMProvider;
    use crate::services::agent_registry::AgentRegistry;
    use crate::services::provider_stack::ProviderStack;
    use crate::services::router_config::RouterConfig;
    use crate::services::skill_manager::SkillManager;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex, RwLock};
    use tempfile::TempDir;
    use tokio::sync::RwLock as TokioRwLock;
    use tokio_stream::Stream;

    /// A provider that answers `health_check` so the *router's* contribution
    /// to `status` is the only variable under test.
    struct HealthyProvider;

    #[async_trait]
    impl LLMProvider for HealthyProvider {
        fn get_base_url(&self) -> String {
            "http://localhost:11434".to_string()
        }

        async fn health_check(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }

        async fn list_models(
            &self,
        ) -> Result<Vec<ModelInfo>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Vec::new())
        }

        async fn chat(
            &self,
            _request: ChatRequest,
        ) -> Result<ChatResponse, Box<dyn std::error::Error + Send + Sync>> {
            Err("not used by health".into())
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<
            Pin<
                Box<
                    dyn Stream<
                            Item = Result<
                                ChatStreamEvent,
                                Box<dyn std::error::Error + Send + Sync>,
                            >,
                        > + Send,
                >,
            >,
            Box<dyn std::error::Error + Send + Sync>,
        > {
            Err("not used by health".into())
        }
    }

    /// A whole `AppState` built around the two router facts under test.
    /// Constructed by literal rather than `build_app_state` so the test is
    /// hermetic: `build_app_state` reads the ambient environment for
    /// `RouterConfig`, and this test pins the condition, not the shell.
    fn state_with(router_config: RouterConfig, router_healthy: bool) -> (TempDir, Arc<AppState>) {
        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_var("LLAMA_R_DIR", temp_dir.path());
        let context_store = Arc::new(ContextStore::new());
        let skill_manager = Arc::new(SkillManager::new());
        let stack = ProviderStack::new(Arc::new(HealthyProvider), "fallback-model");
        let state = Arc::new(AppState {
            services: Arc::new(TokioRwLock::new(Arc::new(stack))),
            agent_registry: Arc::new(AgentRegistry::new()),
            context_enricher: Arc::new(ContextEnricher::new(
                context_store.clone(),
                skill_manager.clone(),
                "fallback-model".to_string(),
            )),
            skill_manager,
            context_store,
            metrics: Arc::new(TokenMetrics::new()),
            observability: Arc::new(AppObservability::new()),
            api_running: AtomicBool::new(false),
            grpc_running: AtomicBool::new(false),
            logs: Arc::new(Mutex::new(VecDeque::new())),
            known_mcp_servers: RwLock::new(Vec::new()),
            mcp_registry: Arc::new(StaticMcpRegistry::new()),
            history_store: None,
            router_config: Arc::new(router_config),
            router_healthy: AtomicBool::new(router_healthy),
        });
        (temp_dir, state)
    }

    #[tokio::test]
    async fn a_disabled_router_never_degrades_the_service() {
        let _lock = crate::core::paths::lock_env_for_tests();
        let (_dir, state) = state_with(
            RouterConfig {
                enabled: false,
                ..RouterConfig::default()
            },
            false,
        );
        let response = health(State(state)).await.0;
        assert_eq!(
            response.status, "ok",
            "a feature that is off must not degrade the service"
        );
        assert!(!response.router.enabled);
        assert!(!response.router.healthy);
    }

    /// The other half of the condition: an enabled router that has not
    /// answered is a capability the operator asked for and is not getting —
    /// degraded until the ladder records an answer, never "ok" by default.
    #[tokio::test]
    async fn an_enabled_router_degrades_until_it_has_answered() {
        let _lock = crate::core::paths::lock_env_for_tests();
        let (_dir, unheard) = state_with(
            RouterConfig {
                enabled: true,
                ..RouterConfig::default()
            },
            false,
        );
        let response = health(State(unheard)).await.0;
        assert_eq!(
            response.status, "degraded",
            "configured but silent is degraded, not ok"
        );
        assert!(response.router.enabled);
        assert!(!response.router.healthy);

        let (_dir, answered) = state_with(
            RouterConfig {
                enabled: true,
                ..RouterConfig::default()
            },
            true,
        );
        let response = health(State(answered)).await.0;
        assert_eq!(response.status, "ok");
        assert!(response.router.healthy);
    }
}
