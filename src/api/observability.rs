use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use utoipa::ToSchema;

#[derive(Debug, Default)]
pub struct AppObservability {
    http_requests: AtomicU64,
    chat_requests: AtomicU64,
    fallback_count: AtomicU64,
    provider_errors: AtomicU64,
    grpc_errors: AtomicU64,
    mcp_errors: AtomicU64,
    chat_latency_ms_total: AtomicU64,
    completed_chat_requests: AtomicU64,
    routed_to_agent: AtomicU64,
    routed_to_general: AtomicU64,
    router_errors: AtomicU64,
    router_latency_ms_total: AtomicU64,
}

impl AppObservability {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_http_request(&self) {
        self.http_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_chat_request(&self, latency_ms: u64) {
        self.chat_requests.fetch_add(1, Ordering::Relaxed);
        self.chat_latency_ms_total
            .fetch_add(latency_ms, Ordering::Relaxed);
        self.completed_chat_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_fallback(&self) {
        self.fallback_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_provider_error(&self) {
        self.provider_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_grpc_error(&self) {
        self.grpc_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_mcp_error(&self) {
        self.mcp_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// The router picked a specialised agent. A misconfigured threshold shows
    /// up here as `routed_to_agent` stuck at zero while `router_errors` climbs.
    pub fn record_route_to_agent(&self) {
        self.routed_to_agent.fetch_add(1, Ordering::Relaxed);
    }

    /// The router deliberately chose the general agent (below threshold, the
    /// general candidate won, or no route applied). Distinct from an error:
    /// this is the router working, that is the router failing.
    pub fn record_route_to_general(&self) {
        self.routed_to_general.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_router_error(&self) {
        self.router_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_router_latency(&self, latency_ms: u64) {
        self.router_latency_ms_total
            .fetch_add(latency_ms, Ordering::Relaxed);
    }

    /// Decisions that reached an outcome (agent or general). The dashboard
    /// shows a routing *rate* from this rather than an absolute count.
    pub fn router_decisions(&self) -> u64 {
        self.routed_to_agent.load(Ordering::Relaxed)
            + self.routed_to_general.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> ObservabilitySnapshot {
        let completed = self.completed_chat_requests.load(Ordering::Relaxed);
        let total_latency = self.chat_latency_ms_total.load(Ordering::Relaxed);
        ObservabilitySnapshot {
            http_requests: self.http_requests.load(Ordering::Relaxed),
            chat_requests: self.chat_requests.load(Ordering::Relaxed),
            fallback_count: self.fallback_count.load(Ordering::Relaxed),
            provider_errors: self.provider_errors.load(Ordering::Relaxed),
            grpc_errors: self.grpc_errors.load(Ordering::Relaxed),
            mcp_errors: self.mcp_errors.load(Ordering::Relaxed),
            chat_latency_ms_total: total_latency,
            avg_chat_latency_ms: if completed == 0 {
                0
            } else {
                total_latency / completed
            },
            routed_to_agent: self.routed_to_agent.load(Ordering::Relaxed),
            routed_to_general: self.routed_to_general.load(Ordering::Relaxed),
            router_errors: self.router_errors.load(Ordering::Relaxed),
            router_latency_ms_total: self.router_latency_ms_total.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ObservabilitySnapshot {
    pub http_requests: u64,
    pub chat_requests: u64,
    pub fallback_count: u64,
    pub provider_errors: u64,
    pub grpc_errors: u64,
    pub mcp_errors: u64,
    pub chat_latency_ms_total: u64,
    pub avg_chat_latency_ms: u64,
    pub routed_to_agent: u64,
    pub routed_to_general: u64,
    pub router_errors: u64,
    pub router_latency_ms_total: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snapshot_counts_each_routing_outcome_separately() {
        let obs = AppObservability::new();
        obs.record_route_to_agent();
        obs.record_route_to_agent();
        obs.record_route_to_general();
        obs.record_router_error();
        obs.record_router_latency(30);
        let snap = obs.snapshot();
        assert_eq!(snap.routed_to_agent, 2);
        assert_eq!(snap.routed_to_general, 1);
        assert_eq!(snap.router_errors, 1);
        assert_eq!(snap.router_latency_ms_total, 30);
        // The dashboard's rate numerator: the two outcomes, and nothing else —
        // an error is not a decision, so it must not inflate the count.
        assert_eq!(obs.router_decisions(), 3);
    }
}
