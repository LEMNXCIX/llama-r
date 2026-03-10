use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

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
        self.completed_chat_requests
            .fetch_add(1, Ordering::Relaxed);
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
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ObservabilitySnapshot {
    pub http_requests: u64,
    pub chat_requests: u64,
    pub fallback_count: u64,
    pub provider_errors: u64,
    pub grpc_errors: u64,
    pub mcp_errors: u64,
    pub chat_latency_ms_total: u64,
    pub avg_chat_latency_ms: u64,
}
