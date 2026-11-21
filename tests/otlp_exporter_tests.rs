//! OTLP/HTTP trace exporter wiring (Task 4).
//!
//! Every test talks to a real one-shot HTTP listener — no mocking of reqwest —
//! so the path, the OTLP JSON shape and the best-effort contract are observed
//! on the wire. The exporter is deliberately built from the same `Config` the
//! boot path uses, so "disabled means noop" is tested end to end.

#![allow(clippy::await_holding_lock)] // env guard held deliberately across .await to serialise tests

use chrono::Utc;
use llama_r::adapters::execution::NoOpExecutionStore;
use llama_r::adapters::telemetry::{exporter_from_config, OtlpHttpExporter};
use llama_r::api::chat_core::record_trace;
use llama_r::config::Config;
use llama_r::domain::execution::{AgentExecution, ExecutionStatus};
use llama_r::ports::telemetry::TraceExporter;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing_subscriber::prelude::*;
use uuid::Uuid;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Save and restore the three OTEL variables so a test cannot leak config into
/// another test in the same binary.
struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        let mut saved = Vec::new();
        for &(name, value) in vars {
            saved.push((name, std::env::var_os(name)));
            std::env::set_var(name, value);
        }
        Self(saved)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }
}

struct OneShot {
    url: String,
    requests: Arc<Mutex<Vec<(String, String)>>>, // (request line, body)
    _task: JoinHandle<()>,
}

impl OneShot {
    async fn start(status_line: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let Some((head, body)) = read_request(&mut socket).await else {
                    continue;
                };
                let request_line = head.lines().next().unwrap_or("").to_string();
                sink.lock().unwrap().push((request_line, body));
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        Self {
            url: format!("http://{addr}"),
            requests,
            _task: task,
        }
    }

    fn bodies(&self) -> Vec<(String, String)> {
        self.requests.lock().unwrap().clone()
    }
}

/// Read one full HTTP/1.1 request (headers plus `Content-Length` body).
async fn read_request(socket: &mut TcpStream) -> Option<(String, String)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let header_end = loop {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let len = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    while buf.len() < header_end + len {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = String::from_utf8_lossy(&buf[header_end..header_end + len]).to_string();
    Some((headers, body))
}

fn sample_execution() -> AgentExecution {
    AgentExecution {
        id: Uuid::parse_str("12345678-1234-5678-1234-567812345678").unwrap(),
        conversation_id: None,
        agent_qualified_id: "proj::agent".into(),
        provider: "ollama".into(),
        model: "model-a".into(),
        status: ExecutionStatus::Completed,
        current_step: 0,
        user_message: Some("hello".into()),
        user_message_hash: "cd".repeat(32),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        finished_at: None,
        error: None,
    }
}

fn payload_with(execution: &AgentExecution, latency_ms: u64) -> Value {
    json!({
        "trace_id": execution.id,
        "agent_qualified_id": "proj::agent",
        "provider": "ollama",
        "model": "model-a",
        "input_tokens": 10,
        "output_tokens": 20,
        "cost_usd": null,
        "latency_ms": latency_ms,
        "parent_execution_id": null,
        "root_execution_id": execution.id,
        "depth": 0,
    })
}

#[tokio::test]
async fn otel_disabled_sends_nothing() {
    let _lock = ENV_LOCK.lock().unwrap();
    let server = OneShot::start("200 OK").await;
    let _env = EnvGuard::set(&[
        ("OTEL_ENABLED", "false"),
        ("OTEL_ENDPOINT", &format!("{}/v1/traces", server.url)),
    ]);

    let config = Config::from_env().expect("config reads OTEL_* from the env");
    let exporter = exporter_from_config(&config);
    exporter.export(&json!({"trace_id": Uuid::new_v4()})).await;

    assert!(
        server.bodies().is_empty(),
        "a disabled exporter must issue zero HTTP requests"
    );

    // Control: the same factory with the flag on DOES post to this endpoint,
    // so the empty result above is the noop, not a misconfigured URL.
    std::env::set_var("OTEL_ENABLED", "true");
    let enabled = exporter_from_config(&Config::from_env().expect("reload config"));
    enabled.export(&json!({"trace_id": Uuid::new_v4()})).await;
    assert_eq!(server.bodies().len(), 1, "the enabled control must post");
}

#[tokio::test]
async fn otel_export_posts_expected_span() {
    let server = OneShot::start("200 OK").await;
    let exporter = OtlpHttpExporter::new(
        format!("{}/v1/traces", server.url),
        "test-service".to_string(),
    );
    let execution = sample_execution();

    exporter.export(&payload_with(&execution, 250)).await;

    let requests = server.bodies();
    assert_eq!(requests.len(), 1, "exactly one POST per finished turn");
    assert_eq!(requests[0].0, "POST /v1/traces HTTP/1.1");
    let body: Value = serde_json::from_str(&requests[0].1).unwrap();
    let resource = &body["resourceSpans"][0];
    let service_name = resource["resource"]["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attr| attr["key"] == "service.name")
        .expect("service.name attribute")
        .clone();
    assert_eq!(service_name["value"]["stringValue"], "test-service");

    let scope = &resource["scopeSpans"][0];
    let span = &scope["spans"][0];
    assert_eq!(span["name"], "agent.turn");
    assert_eq!(span["traceId"], "12345678123456781234567812345678");
    assert_eq!(span["spanId"], "1234567812345678");

    let start = span["startTimeUnixNano"].as_u64().unwrap();
    let end = span["endTimeUnixNano"].as_u64().unwrap();
    assert!(end >= start);
    assert!((end - start).abs_diff(250_000_000) < 5_000_000);

    let attrs: std::collections::BTreeMap<&str, &Value> = span["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|attr| (attr["key"].as_str().unwrap(), &attr["value"]))
        .collect();
    assert_eq!(attrs["agent_qualified_id"]["stringValue"], "proj::agent");
    assert_eq!(attrs["provider"]["stringValue"], "ollama");
    assert_eq!(attrs["model"]["stringValue"], "model-a");
    assert_eq!(attrs["input_tokens"]["intValue"], 10);
    assert_eq!(attrs["output_tokens"]["intValue"], 20);
    assert_eq!(attrs["latency_ms"]["intValue"], 250);
    assert_eq!(attrs["depth"]["intValue"], 0);
    assert_eq!(
        attrs["root_execution_id"]["stringValue"],
        "12345678-1234-5678-1234-567812345678"
    );
    assert!(!attrs.contains_key("parent_execution_id"));
    assert!(!attrs.contains_key("cost_usd"));
}

/// A capture layer for warnings, mirroring the pattern used by other wire tests.
#[derive(Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuffer {
    type Writer = SharedBuffer;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn otel_failure_is_best_effort() {
    let server = OneShot::start("500 Internal Server Error").await;
    let exporter = OtlpHttpExporter::new(
        format!("{}/v1/traces", server.url),
        "test-service".to_string(),
    );
    let execution = sample_execution();
    let captured = SharedBuffer::default();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(captured.clone())
            .with_filter(tracing_subscriber::filter::LevelFilter::WARN),
    );
    // `set_default` is task-local and works across `.await` on this runtime.
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    // The exact call a finished chat makes. A 500 must not propagate: the
    // function returns unit and the warning is the only observable effect.
    record_trace(
        &NoOpExecutionStore,
        &exporter,
        &execution,
        &payload_with(&execution, 10),
    )
    .await;

    let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("OTLP") || log.contains("otlp"),
        "a failed export must be warned about; got: {log}"
    );
    assert_eq!(
        server.bodies().len(),
        1,
        "the failed POST was still attempted"
    );
}

#[tokio::test]
async fn otel_payload_has_no_secret() {
    let server = OneShot::start("200 OK").await;
    let exporter = OtlpHttpExporter::new(
        format!("{}/v1/traces", server.url),
        "test-service".to_string(),
    );
    let execution = sample_execution();
    let mut payload = payload_with(&execution, 5);
    payload["api_key"] = json!("sk-SUPER-SECRET-DO-NOT-LEAK");
    payload["nested"] = json!({"token": "ANOTHER-SECRET"});

    record_trace(&NoOpExecutionStore, &exporter, &execution, &payload).await;

    let requests = server.bodies();
    assert_eq!(requests.len(), 1);
    let raw = &requests[0].1;
    assert!(!raw.contains("sk-SUPER-SECRET-DO-NOT-LEAK"));
    assert!(!raw.contains("ANOTHER-SECRET"));
    // The counters survive redaction; they are counts, not credentials.
    let body: Value = serde_json::from_str(raw).unwrap();
    let attrs = &body["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["attributes"];
    assert!(attrs.to_string().contains("input_tokens"));
}
