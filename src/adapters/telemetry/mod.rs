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
// The defaults live in `Config` — the single source of truth for every env
// var. Re-exported so callers can keep importing them from this adapter.
pub use crate::config::{DEFAULT_OTEL_ENDPOINT, DEFAULT_OTEL_SERVICE_NAME};

/// The disabled exporter. It issues zero requests and drops the trace — the
/// exact pre-Task-4 behaviour.
pub struct NoopExporter;

#[async_trait]
impl TraceExporter for NoopExporter {
    async fn export(&self, _trace: &Value) {}
}

/// Build the boot-time exporter from `Config`.
///
/// `OTEL_ENABLED` is opt-in and defaults to false, so an update that only ships
/// this exporter changes nothing until an operator enables it. The other two
/// settings are read only on the enabled path. No credential is read or logged
/// here: the endpoint is a plain URL and the service name is a label.
pub fn exporter_from_config(config: &crate::config::Config) -> Arc<dyn TraceExporter> {
    if !config.otel_enabled {
        return Arc::new(NoopExporter);
    }
    Arc::new(OtlpHttpExporter::new(
        config.otel_endpoint.clone(),
        config.otel_service_name.clone(),
    ))
}
