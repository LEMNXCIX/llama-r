//! Port for exporting a finished turn's already-redacted trace.
//!
//! Deliberately tiny: the core hands the exporter a `serde_json::Value` and
//! never learns about OTLP, HTTP or telemetry backends. The value is the same
//! redacted payload the execution store receives (`redact_trace_payload` in
//! `services::redaction`), so an exporter can never widen what leaves the
//! process. Export is best-effort — the trait returns no error because a
//! telemetry failure must never fail a chat.

use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait TraceExporter: Send + Sync {
    /// Export one finished turn's redacted trace. Implementations log their own
    /// failures; callers do not retry or propagate them.
    async fn export(&self, trace: &Value);
}
