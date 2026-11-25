use crate::api::handlers::AppState;
use crate::api::observability::ObservabilitySnapshot;
use axum::{extract::State, response::IntoResponse};
use std::fmt::Write;
use std::sync::Arc;

/// Render an `ObservabilitySnapshot` as Prometheus text format 0.0.4.
///
/// Each of the 16 exposed metrics gets three lines: `# HELP`, `# TYPE` and the
/// value line. `completed_chat_requests` is intentionally not exposed.
pub fn render(snapshot: &ObservabilitySnapshot) -> String {
    let mut out = String::new();

    // (field value, metric name, type, help text)
    // Build manually to keep field order and types exact.
    macro_rules! emit {
        ($name:expr, $ty:expr, $help:expr, $value:expr) => {
            let _ = writeln!(out, "# HELP {} {}", $name, $help);
            let _ = writeln!(out, "# TYPE {} {}", $name, $ty);
            let _ = writeln!(out, "{} {}", $name, $value);
        };
    }

    emit!(
        "llamar_http_requests_total",
        "counter",
        "Total HTTP requests handled.",
        snapshot.http_requests
    );
    emit!(
        "llamar_chat_requests_total",
        "counter",
        "Total chat requests handled.",
        snapshot.chat_requests
    );
    emit!(
        "llamar_fallback_count_total",
        "counter",
        "Total fallback responses served.",
        snapshot.fallback_count
    );
    emit!(
        "llamar_provider_errors_total",
        "counter",
        "Total provider errors.",
        snapshot.provider_errors
    );
    emit!(
        "llamar_grpc_errors_total",
        "counter",
        "Total gRPC errors.",
        snapshot.grpc_errors
    );
    emit!(
        "llamar_mcp_errors_total",
        "counter",
        "Total MCP errors.",
        snapshot.mcp_errors
    );
    emit!(
        "llamar_chat_latency_ms_total",
        "counter",
        "Total chat latency in milliseconds.",
        snapshot.chat_latency_ms_total
    );
    emit!(
        "llamar_avg_chat_latency_ms",
        "gauge",
        "Average chat latency in milliseconds.",
        snapshot.avg_chat_latency_ms
    );
    emit!(
        "llamar_routed_to_agent_total",
        "counter",
        "Total requests routed to a specialised agent.",
        snapshot.routed_to_agent
    );
    emit!(
        "llamar_routed_to_general_total",
        "counter",
        "Total requests routed to the general agent.",
        snapshot.routed_to_general
    );
    emit!(
        "llamar_router_errors_total",
        "counter",
        "Total router errors.",
        snapshot.router_errors
    );
    emit!(
        "llamar_router_latency_ms_total",
        "counter",
        "Total router latency in milliseconds.",
        snapshot.router_latency_ms_total
    );
    emit!(
        "llamar_auto_approved_total",
        "counter",
        "Total automatically approved tool calls.",
        snapshot.auto_approved
    );
    emit!(
        "llamar_auto_denied_total",
        "counter",
        "Total automatically denied tool calls.",
        snapshot.auto_denied
    );
    emit!(
        "llamar_auto_unavailable_total",
        "counter",
        "Total tool calls with no automatic verdict.",
        snapshot.auto_unavailable
    );
    emit!(
        "llamar_cost_usd_total",
        "gauge",
        "Total cost in USD.",
        snapshot.cost_usd_total_micros as f64 / 1e6
    );

    out
}

#[utoipa::path(
    get,
    path = "/metrics",
    tag = "Health",
    responses((status = 200, description = "Prometheus text metrics"))
)]
pub async fn metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.observability.record_http_request();
    let snapshot = state.observability.snapshot();
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        render(&snapshot),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_emits_all_sixteen_metrics_with_prometheus_shape() {
        let snapshot = ObservabilitySnapshot {
            http_requests: 10,
            chat_requests: 2,
            fallback_count: 1,
            provider_errors: 0,
            grpc_errors: 0,
            mcp_errors: 0,
            chat_latency_ms_total: 500,
            avg_chat_latency_ms: 250,
            routed_to_agent: 1,
            routed_to_general: 1,
            router_errors: 0,
            router_latency_ms_total: 40,
            auto_approved: 1,
            auto_denied: 0,
            auto_unavailable: 0,
            cost_usd_total_micros: 1_234_567,
        };
        let text = render(&snapshot);
        assert!(text.contains("# HELP llamar_chat_requests_total"));
        assert!(text.contains("# TYPE llamar_chat_requests_total counter"));
        assert!(text.contains("llamar_chat_requests_total 2"));
        assert!(text.contains("llamar_avg_chat_latency_ms 250"));
        assert!(text.contains("llamar_cost_usd_total 1.234567"));
        assert_eq!(text.lines().filter(|l| l.starts_with("llamar_")).count(), 16);
        assert!(!text.to_ascii_lowercase().contains("key"));
        assert!(!text.to_ascii_lowercase().contains("token"));
    }

    #[test]
    fn render_does_not_expose_internal_completed_counter() {
        let text = render(&ObservabilitySnapshot::default());
        assert!(!text.contains("completed_chat_requests"));
    }
}
