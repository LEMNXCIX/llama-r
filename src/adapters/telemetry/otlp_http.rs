//! OTLP/HTTP JSON trace exporter.
//!
//! One finished turn becomes one span in one `resourceSpans` payload. The
//! trace id comes from the execution UUID carried in the redacted payload, so
//! it matches the id already visible through the execution API. Attribute
//! values are copied only from the known scalar fields of that payload — there
//! is no path for tool arguments, headers, tokens or any other secret to reach
//! the collector.

use crate::ports::telemetry::TraceExporter;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// A tiny client timeout keeps a stalled collector from holding a chat turn
/// open. Export is best-effort; the warning is emitted locally and dropped.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct OtlpHttpExporter {
    pub endpoint: String,
    pub service_name: String,
    pub client: reqwest::Client,
}

impl OtlpHttpExporter {
    pub fn new(endpoint: String, service_name: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(EXPORT_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            endpoint,
            service_name,
            client,
        }
    }

    /// The execution UUID is the source of truth for both ids: the full 16
    /// bytes give the 32-hex trace id, the first 8 bytes the 16-hex span id.
    /// A malformed id falls back to a fresh UUID so an export never fails
    /// because of a payload the core could not have produced.
    fn ids(trace: &Value) -> (String, String) {
        let id = trace
            .get("trace_id")
            .and_then(Value::as_str)
            .and_then(|raw| Uuid::parse_str(raw).ok())
            .unwrap_or_else(Uuid::new_v4);
        let trace_id = id.simple().to_string();
        let span_id = id.as_bytes()[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        (trace_id, span_id)
    }

    /// Map the known scalar fields of the redacted payload to typed OTLP
    /// attributes. Nulls are skipped; unknown fields are ignored, which is what
    /// keeps a sown secret from ever reaching the wire.
    fn attributes(trace: &Value) -> Vec<Value> {
        let mut attrs = Vec::new();
        for key in [
            "agent_qualified_id",
            "provider",
            "model",
            "parent_execution_id",
            "root_execution_id",
        ] {
            if let Some(value) = trace.get(key).and_then(Value::as_str) {
                attrs.push(json!({"key": key, "value": {"stringValue": value}}));
            }
        }
        for key in ["input_tokens", "output_tokens", "latency_ms", "depth"] {
            if let Some(value) = trace.get(key).and_then(Value::as_u64) {
                attrs.push(json!({"key": key, "value": {"intValue": value}}));
            }
        }
        if let Some(value) = trace.get("cost_usd").and_then(Value::as_f64) {
            attrs.push(json!({"key": "cost_usd", "value": {"doubleValue": value}}));
        }
        attrs
    }

    /// Derive the span's time window from the turn's latency. The payload has
    /// no absolute timestamps (only `latency_ms`), so the window ends now and
    /// reaches back by exactly the recorded latency.
    fn time_window(trace: &Value) -> (u64, u64) {
        let end = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let latency_ms = trace.get("latency_ms").and_then(Value::as_u64).unwrap_or(0);
        let start = end.saturating_sub(Duration::from_millis(latency_ms));
        let to_nanos =
            |duration: Duration| -> u64 { u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX) };
        (to_nanos(start), to_nanos(end))
    }

    /// Build the OTLP/HTTP JSON body for one redacted trace. Public so the wire
    /// shape is inspectable in isolation, independent of the HTTP transport.
    pub fn span_body(&self, trace: &Value) -> Value {
        let (trace_id, span_id) = Self::ids(trace);
        let (start_time, end_time) = Self::time_window(trace);
        json!({
            "resourceSpans": [{
                "resource": {
                    "attributes": [{
                        "key": "service.name",
                        "value": {"stringValue": self.service_name}
                    }]
                },
                "scopeSpans": [{
                    "scope": {"name": "llama-r"},
                    "spans": [{
                        "traceId": trace_id,
                        "spanId": span_id,
                        "name": "agent.turn",
                        "kind": 1,
                        "startTimeUnixNano": start_time,
                        "endTimeUnixNano": end_time,
                        "attributes": Self::attributes(trace),
                    }]
                }]
            }]
        })
    }
}

#[async_trait]
impl TraceExporter for OtlpHttpExporter {
    async fn export(&self, trace: &Value) {
        let body = self.span_body(trace);
        match self.client.post(&self.endpoint).json(&body).send().await {
            Ok(response) => {
                if let Err(err) = response.error_for_status() {
                    tracing::warn!(error = %err, "could not export trace over OTLP/HTTP: collector rejected the span");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "could not export trace over OTLP/HTTP");
            }
        }
    }
}
