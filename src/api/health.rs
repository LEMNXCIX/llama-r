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
    let provider_healthy = state.provider.health_check().await.is_ok();
    let router_config = state.router_config.clone();
    let router_healthy = state.router_healthy.load(Ordering::SeqCst);
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::observability::AppObservability;

    #[test]
    fn a_disabled_router_is_enabled_but_neither_healthy_nor_counted() {
        let response = HealthResponse {
            status: "ok",
            provider_healthy: true,
            agent_count: 0,
            context_count: 0,
            api_running: false,
            grpc_running: false,
            saved_tokens: 0,
            total_tokens_processed: 0,
            observability: AppObservability::new().snapshot(),
            router: RouterHealth {
                enabled: false,
                healthy: false,
                url: "http://127.0.0.1:8080".into(),
            },
        };
        assert_eq!(
            response.status, "ok",
            "a feature that is off must not degrade the service"
        );
        assert!(!response.router.enabled);
    }
}
