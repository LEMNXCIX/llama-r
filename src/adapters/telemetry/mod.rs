//! Telemetry adapters: an opt-in OTLP/HTTP span exporter and its disabled noop.
//!
//! Nothing here knows about the chat domain. The core hands us the same
//! redacted `Value` it persists, and we turn it into one OTLP span (or, when
//! disabled, do nothing at all).

use crate::ports::telemetry::TraceExporter;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub mod otlp_http;

pub use otlp_http::OtlpHttpExporter;

/// The default OTLP/HTTP traces endpoint (the local collector's standard port).
pub const DEFAULT_OTEL_ENDPOINT: &str = "http://127.0.0.1:4318/v1/traces";
/// The default `service.name` resource attribute.
pub const DEFAULT_OTEL_SERVICE_NAME: &str = "llama-r";

/// The disabled exporter. It issues zero requests and drops the trace — the
/// exact pre-Task-4 behaviour.
pub struct NoopExporter;

#[async_trait]
impl TraceExporter for NoopExporter {
    async fn export(&self, _trace: &Value) {}
}

fn env_truthy(name: &str) -> bool {
    matches!(
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_ascii_lowercase()),
        Some(value) if matches!(value.as_str(), "1" | "true" | "yes" | "on")
    )
}

fn env_nonempty(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Build the boot-time exporter from the environment.
///
/// `OTEL_ENABLED` is opt-in and defaults to false, so an update that only ships
/// this exporter changes nothing until an operator enables it. The other two
/// variables are read only on the enabled path. No credential is read or
/// logged here: the endpoint is a plain URL and the service name is a label.
pub fn exporter_from_env() -> Arc<dyn TraceExporter> {
    if !env_truthy("OTEL_ENABLED") {
        return Arc::new(NoopExporter);
    }
    Arc::new(OtlpHttpExporter::new(
        env_nonempty("OTEL_ENDPOINT", DEFAULT_OTEL_ENDPOINT),
        env_nonempty("OTEL_SERVICE_NAME", DEFAULT_OTEL_SERVICE_NAME),
    ))
}
