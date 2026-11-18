//! Wire tests for typed MCP connections: headers, basic auth, auth precedence and OpenAPI.

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use llama_r::adapters::mcp::http::ResolvedAuth;
use llama_r::adapters::mcp::openapi::OpenApiConnection;
use llama_r::adapters::mcp::{McpServerConfig, StaticMcpRegistry};
use llama_r::ports::mcp::{McpCallRequest, McpClient, McpServerRegistry};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Default)]
struct Captured {
    method: String,
    path: String,
    query: Option<String>,
    authorization: Vec<String>,
    headers: Vec<(String, String)>,
    body: String,
}

#[derive(Clone)]
struct CaptureState {
    captured: Arc<Mutex<Vec<Captured>>>,
    tools: Value,
}

async fn capture_handler(State(state): State<CaptureState>, req: Request) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    let authorization = req
        .headers()
        .get_all("authorization")
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_string))
        .collect();
    let headers = req
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = to_bytes(req.into_body(), 1_000_000)
        .await
        .unwrap_or_default();
    let body = String::from_utf8_lossy(&bytes).to_string();

    state.captured.lock().expect("capture lock").push(Captured {
        method,
        path: path.clone(),
        query,
        authorization,
        headers,
        body: body.clone(),
    });

    if path == "/" {
        if let Ok(value) = serde_json::from_str::<Value>(&body) {
            if value.get("method").and_then(|m| m.as_str()) == Some("tools/list") {
                return Json(json!({
                    "jsonrpc": "2.0",
                    "id": value.get("id").cloned().unwrap_or(Value::Null),
                    "result": { "tools": state.tools }
                }))
                .into_response();
            }
        }
    }

    Json(json!({ "ok": true, "echo": body })).into_response()
}

async fn spawn_capture_server(tools: Value) -> (SocketAddr, Arc<Mutex<Vec<Captured>>>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let state = CaptureState {
        captured: captured.clone(),
        tools,
    };
    let app = Router::new()
        .route("/", post(capture_handler))
        .fallback(capture_handler)
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (addr, captured)
}

fn mcp_config(id: &str, url: String) -> McpServerConfig {
    McpServerConfig {
        id: id.into(),
        transport: "http".into(),
        url: Some(url),
        ..Default::default()
    }
}

fn write_spec(dir: &std::path::Path, name: &str, spec: &Value) -> String {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_vec(spec).expect("serialize spec")).expect("write spec");
    path.to_string_lossy().into_owned()
}

#[tokio::test]
async fn bearer_connection_sends_auth_and_custom_headers() {
    std::env::set_var("TEST_CONN_BEARER_TOKEN", "real-bearer-secret");
    std::env::set_var("TEST_CONN_HEADER_TOKEN", "resolved-header-secret");

    let (addr, captured) = spawn_capture_server(json!([{
        "name": "ping", "description": "ping", "inputSchema": { "type": "object" }
    }]))
    .await;

    let config = McpServerConfig {
        auth_env: Some("TEST_CONN_BEARER_TOKEN".into()),
        headers: vec![
            ("X-Tenant".into(), "acme".into()),
            ("X-Api-Key".into(), "$TEST_CONN_HEADER_TOKEN".into()),
        ],
        ..mcp_config("bearer", format!("http://{addr}/"))
    };
    let registry = StaticMcpRegistry::from_configs(&[config]).expect("registry");
    let client = registry.client_for("bearer").await.expect("client");
    let tools = client.list_tools("bearer").await.expect("list_tools");
    assert_eq!(tools.len(), 1);

    let seen = captured.lock().expect("lock").clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].authorization, vec!["Bearer real-bearer-secret"]);
    let get = |name: &str| -> Vec<String> {
        seen[0]
            .headers
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .collect()
    };
    assert_eq!(get("x-tenant"), vec!["acme"]);
    assert_eq!(get("x-api-key"), vec!["resolved-header-secret"]);
}

#[tokio::test]
async fn basic_connection_sends_basic_auth_from_env_names() {
    std::env::set_var("TEST_CONN_BASIC_USER", "alice");
    std::env::set_var("TEST_CONN_BASIC_PASS", "s3cr3t");

    let (addr, captured) = spawn_capture_server(json!([{
        "name": "ping", "description": "ping", "inputSchema": { "type": "object" }
    }]))
    .await;

    let config = McpServerConfig {
        auth: llama_r::adapters::mcp::registry::AuthKind::Basic,
        auth_env: Some("TEST_CONN_BASIC_PASS".into()),
        auth_user_env: Some("TEST_CONN_BASIC_USER".into()),
        ..mcp_config("basic", format!("http://{addr}/"))
    };
    let registry = StaticMcpRegistry::from_configs(&[config]).expect("registry");
    let client = registry.client_for("basic").await.expect("client");
    client.list_tools("basic").await.expect("list_tools");

    let seen = captured.lock().expect("lock").clone();
    // base64("alice:s3cr3t") == "YWxpY2U6czNjcjN0"
    assert_eq!(seen[0].authorization, vec!["Basic YWxpY2U6czNjcjN0"]);
}

#[tokio::test]
async fn streaming_connection_sends_auth_and_custom_headers() {
    std::env::set_var("TEST_CONN_STREAM_TOKEN", "stream-bearer-secret");

    let (addr, captured) = spawn_capture_server(json!([{
        "name": "ping", "description": "ping", "inputSchema": { "type": "object" }
    }]))
    .await;

    let config = McpServerConfig {
        id: "stream".into(),
        transport: "streaming".into(),
        url: Some(format!("http://{addr}/")),
        auth_env: Some("TEST_CONN_STREAM_TOKEN".into()),
        headers: vec![("X-Stream".into(), "yes".into())],
        ..Default::default()
    };
    let registry = StaticMcpRegistry::from_configs(&[config]).expect("registry");
    let client = registry.client_for("stream").await.expect("client");
    assert_eq!(
        client.list_tools("stream").await.expect("list_tools").len(),
        1
    );

    let seen = captured.lock().expect("lock").clone();
    assert_eq!(seen[0].authorization, vec!["Bearer stream-bearer-secret"]);
    assert!(seen[0]
        .headers
        .iter()
        .any(|(k, v)| k == "x-stream" && v == "yes"));
}

#[test]
fn openapi_connection_debug_redacts_resolved_header_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = json!({
        "openapi": "3.0.3",
        "info": { "title": "redaction", "version": "1.0" },
        "servers": [{ "url": "http://127.0.0.1:1" }],
        "paths": { "/ping": { "get": { "operationId": "ping" } } }
    });
    let spec_path = write_spec(dir.path(), "redaction.json", &spec);
    let secret = "super-secret-api-key-value";
    let connection = OpenApiConnection::from_spec_path(
        "redaction",
        spec_path,
        ResolvedAuth::Bearer("super-secret-bearer-token".into()),
        vec![("X-Api-Key".into(), secret.into())],
        Duration::from_secs(5),
    )
    .expect("openapi connection");

    let debug = format!("{:?}", connection);
    assert!(
        !debug.contains(secret),
        "resolved header value leaked: {debug}"
    );
    assert!(
        debug.contains("X-Api-Key"),
        "header name should be visible: {debug}"
    );
    assert!(debug.contains("[redacted]"), "marker missing: {debug}");
    assert!(
        !debug.contains("super-secret-bearer-token"),
        "auth token leaked: {debug}"
    );
}

#[tokio::test]
async fn a_request_spoofed_authorization_cannot_override_connection_auth() {
    let (addr, captured) = spawn_capture_server(json!([])).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = json!({
        "openapi": "3.0.3",
        "info": { "title": "spoof", "version": "1.0" },
        "servers": [{ "url": format!("http://{addr}") }],
        "paths": {
            "/search": {
                "get": {
                    "operationId": "search",
                    "parameters": [
                        { "name": "Authorization", "in": "header", "schema": { "type": "string" } },
                        { "name": "q", "in": "query", "schema": { "type": "string" } }
                    ]
                }
            }
        }
    });
    let spec_path = write_spec(dir.path(), "spoof.json", &spec);

    let client = OpenApiConnection::from_spec_path(
        "spoof",
        spec_path,
        ResolvedAuth::Bearer("real-connection-token".into()),
        vec![],
        Duration::from_secs(5),
    )
    .expect("openapi connection");

    let result = client
        .call_tool(McpCallRequest::new(
            "spoof",
            "search",
            json!({ "Authorization": "Bearer ATTACKER-CONTROLLED", "q": "hello" }),
        ))
        .await
        .expect("call_tool");
    assert!(!result.is_error);

    let seen = captured.lock().expect("lock").clone();
    assert_eq!(seen.len(), 1);
    // Exactly one Authorization, carrying the connection's credential.
    assert_eq!(
        seen[0].authorization,
        vec!["Bearer real-connection-token"],
        "a request-supplied Authorization must not win or be appended"
    );
    assert!(seen[0].query.as_deref().unwrap_or("").contains("q=hello"));
}

#[tokio::test]
async fn openapi_spec_generates_one_tool_per_operation() {
    let (addr, captured) = spawn_capture_server(json!([])).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = json!({
        "openapi": "3.0.3",
        "info": { "title": "things", "version": "1.0" },
        "servers": [{ "url": format!("http://{addr}") }],
        "paths": {
            "/items": {
                "get": {
                    "operationId": "list_items",
                    "summary": "List all items",
                    "parameters": [
                        { "name": "limit", "in": "query", "required": false, "schema": { "type": "integer" } }
                    ]
                },
                "post": {
                    "operationId": "create_item",
                    "description": "Create an item",
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": { "type": "object" } } }
                    }
                }
            }
        }
    });
    let spec_path = write_spec(dir.path(), "openapi.json", &spec);
    let config = McpServerConfig {
        id: "things".into(),
        transport: "openapi".into(),
        url: Some(spec_path.clone()),
        ..Default::default()
    };
    let registry = StaticMcpRegistry::from_configs(&[config]).expect("registry");
    let client = registry.client_for("things").await.expect("client");
    let tools = client.list_tools("things").await.expect("list_tools");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "list_items");
    assert_eq!(tools[0].description, "List all items");
    assert_eq!(tools[1].name, "create_item");
    assert_eq!(tools[1].description, "Create an item");

    let get_result = client
        .call_tool(McpCallRequest::new(
            "things",
            "list_items",
            json!({ "limit": 3 }),
        ))
        .await
        .expect("get call");
    assert!(!get_result.is_error);
    let seen = captured.lock().expect("lock").clone();
    let get_seen = seen.iter().find(|c| c.method == "GET").expect("GET seen");
    assert_eq!(get_seen.path, "/items");
    assert!(get_seen.query.as_deref().unwrap_or("").contains("limit=3"));

    let post_result = client
        .call_tool(McpCallRequest::new(
            "things",
            "create_item",
            json!({ "body": { "name": "widget" } }),
        ))
        .await
        .expect("post call");
    assert!(!post_result.is_error);
    let seen = captured.lock().expect("lock").clone();
    let post_seen = seen.iter().find(|c| c.method == "POST").expect("POST seen");
    assert_eq!(post_seen.path, "/items");
    assert!(post_seen.body.contains("widget"));
}

#[tokio::test]
async fn invalid_openapi_spec_names_path_and_skips_connection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let spec_path = write_spec(dir.path(), "broken.json", &json!({ "not": "a spec" }));

    // Direct construction errors, naming the spec path.
    let err = OpenApiConnection::from_spec_path(
        "broken",
        spec_path.clone(),
        ResolvedAuth::None,
        vec![],
        Duration::from_secs(5),
    )
    .expect_err("invalid spec must fail");
    assert!(err.contains(&spec_path), "error must name spec path: {err}");

    // Boot via registry is non-fatal: bad connection skipped, registry still built.
    let config = McpServerConfig {
        id: "broken".into(),
        transport: "openapi".into(),
        url: Some(spec_path),
        ..Default::default()
    };
    let registry = StaticMcpRegistry::from_configs(&[config]).expect("boot must not fail");
    assert!(registry.list_server_ids().await.is_empty());
}

#[test]
fn a_v1_server_config_without_new_fields_loads() {
    let v1 = r#"
        id = "legacy"
        transport = "http"
        url = "http://127.0.0.1:9999/"
        auth_env = "LEGACY_TOKEN"
        timeout_secs = 12
        enabled = true
    "#;
    let cfg: McpServerConfig = toml::from_str(v1).expect("v1 config loads");
    assert_eq!(cfg.id, "legacy");
    assert!(cfg.headers.is_empty());
    assert_eq!(cfg.auth, llama_r::adapters::mcp::registry::AuthKind::Bearer);
    assert!(cfg.auth_user_env.is_none());
    assert_eq!(cfg.timeout_secs, 12);
}
