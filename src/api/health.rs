use crate::api::handlers::AppState;
use axum::{extract::State, Json};
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub provider_healthy: bool,
    pub agent_count: usize,
    pub context_count: usize,
    pub api_running: bool,
    pub grpc_running: bool,
    pub saved_tokens: usize,
    pub total_tokens_processed: usize,
}

pub async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let provider_healthy = state.provider.health_check().await.is_ok();
    Json(HealthResponse {
        status: if provider_healthy { "ok" } else { "degraded" },
        provider_healthy,
        agent_count: state.agent_manager.list_agents().len(),
        context_count: state.context_store.list_contexts().len(),
        api_running: state.api_running.load(Ordering::SeqCst),
        grpc_running: state.grpc_running.load(Ordering::SeqCst),
        saved_tokens: state.metrics.get_saved_tokens(),
        total_tokens_processed: state.metrics.get_total_processed(),
    })
}
