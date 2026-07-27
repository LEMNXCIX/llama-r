use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

// --- Mock MCP HTTP server ---

#[derive(Clone)]
struct MockMcpState {
    tools: Vec<Value>,
    call_counter: Arc<AtomicUsize>,
}

async fn mock_mcp_handler(
    State(state): State<Arc<MockMcpState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let method = body.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let id = body.get("id");

    match method {
        "initialize" => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} }
            }
        })),
        "tools/list" => {
            state.call_counter.fetch_add(1, Ordering::SeqCst);
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": state.tools }
            }))
        }
        "tools/call" => {
            state.call_counter.fetch_add(1, Ordering::SeqCst);
            let name = body
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            match name {
                "error_tool" => Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": "Something went wrong"}],
                        "isError": true
                    }
                })),
                _ => Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": format!("called {}", name)}]
                    }
                })),
            }
        }
        _ => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "Method not found" }
        })),
    }
}

async fn spawn_mock_server(tools: Vec<Value>) -> (SocketAddr, Arc<AtomicUsize>) {
    let call_counter = Arc::new(AtomicUsize::new(0));
    let state = Arc::new(MockMcpState {
        tools,
        call_counter: call_counter.clone(),
    });
    let app = Router::new()
        .route("/", post(mock_mcp_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind");
    let addr = listener.local_addr().expect("failed to get addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server error");
    });
    (addr, call_counter)
}

fn make_tool(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {}
        }
    })
}

// --- Test imports ---

use llama_r::adapters::mcp::CachedMcpClient;
use llama_r::adapters::mcp::HttpMcpClient;
use llama_r::adapters::mcp::NamespacedMcpClient;
use llama_r::adapters::mcp::McpServerConfig;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::adapters::mcp::StdioMcpClient;
use llama_r::adapters::mcp::StreamingHttpMcpClient;
use llama_r::adapters::mcp::TimeoutMcpClient;
use llama_r::domain::agent::RagWritePolicy;
use llama_r::domain::scope::{AgentScope, ToolRef, ToolsAllow};
use llama_r::ports::mcp::{
    filter_tools_for_scope, CancellationToken, McpCallRequest, McpClient, McpServerRegistry,
    McpToolDef,
};
use std::collections::HashSet;

// --- Tests ---

#[tokio::test]
async fn http_client_list_tools() {
    let tools = vec![
        make_tool("greet", "Greets a user"),
        make_tool("ping", "Pings the server"),
    ];
    let (addr, _) = spawn_mock_server(tools).await;

    let client = HttpMcpClient::new("test-server", format!("http://{}/", addr));
    let result = client
        .list_tools("test-server")
        .await
        .expect("list_tools failed");

    assert_eq!(result.len(), 2);
    assert_eq!(result[0].name, "greet");
    assert_eq!(result[1].name, "ping");
}

#[tokio::test]
async fn http_client_call_tool() {
    let tools = vec![make_tool("echo", "Echoes input")];
    let (addr, _) = spawn_mock_server(tools).await;

    let client = HttpMcpClient::new("test-server", format!("http://{}/", addr));
    let result = client
        .call_tool(McpCallRequest::new("test-server", "echo", json!({"msg": "hello"})))
        .await
        .expect("call_tool failed");

    assert!(!result.is_error);
    assert!(result.content.to_string().contains("called echo"));
}

#[tokio::test]
async fn http_client_call_tool_error() {
    let tools = vec![make_tool("error_tool", "Always errors")];
    let (addr, _) = spawn_mock_server(tools).await;

    let client = HttpMcpClient::new("test-server", format!("http://{}/", addr));
    let result = client
        .call_tool(McpCallRequest::new("test-server", "error_tool", json!({})))
        .await
        .expect("call_tool failed");

    assert!(result.is_error);
}

#[tokio::test]
async fn http_client_rejects_wrong_server_id() {
    let tools = vec![make_tool("ping", "")];
    let (addr, _) = spawn_mock_server(tools).await;

    let client = HttpMcpClient::new("server-a", format!("http://{}/", addr));
    let err = client.list_tools("server-b").await.unwrap_err();
    assert!(err.contains("bound to 'server-a', requested 'server-b'"));
}

#[tokio::test]
async fn http_client_health_check() {
    let tools = vec![make_tool("ping", "")];
    let (addr, _) = spawn_mock_server(tools).await;

    let client = HttpMcpClient::new("test-server", format!("http://{}/", addr));
    client.health("test-server").await.expect("health failed");
}

#[tokio::test]
async fn http_client_health_rejects_wrong_id() {
    let tools = vec![make_tool("ping", "")];
    let (addr, _) = spawn_mock_server(tools).await;

    let client = HttpMcpClient::new("server-a", format!("http://{}/", addr));
    let err = client.health("server-b").await.unwrap_err();
    assert!(err.contains("server_id mismatch"));
}

#[tokio::test]
async fn cached_client_caches_list_tools() {
    let tools = vec![make_tool("ping", "Pings")];
    let (addr, call_counter) = spawn_mock_server(tools).await;

    let inner = HttpMcpClient::new("test-server", format!("http://{}/", addr));
    let cached = CachedMcpClient::new(Arc::new(inner), Duration::from_secs(60));

    // First call — should hit the server
    let r1 = cached.list_tools("test-server").await.expect("first call");
    assert_eq!(r1.len(), 1);
    let first_count = call_counter.load(Ordering::SeqCst);

    // Second call — should hit the cache (no increment)
    let r2 = cached.list_tools("test-server").await.expect("second call");
    assert_eq!(r2.len(), 1);
    assert_eq!(call_counter.load(Ordering::SeqCst), first_count);
}

#[tokio::test]
async fn cached_client_invalidate_forces_refetch() {
    let tools = vec![make_tool("ping", "Pings")];
    let (addr, call_counter) = spawn_mock_server(tools).await;

    let inner = HttpMcpClient::new("test-server", format!("http://{}/", addr));
    let cached = CachedMcpClient::new(Arc::new(inner), Duration::from_secs(60));

    let _ = cached.list_tools("test-server").await.expect("first call");
    let first_count = call_counter.load(Ordering::SeqCst);

    cached.invalidate("test-server");

    let _ = cached
        .list_tools("test-server")
        .await
        .expect("after invalidate");
    assert!(call_counter.load(Ordering::SeqCst) > first_count);
}

#[tokio::test]
async fn timeout_client_applies_timeout() {
    // Use a client with a very short timeout against a deliberately slow server
    // For this test we use a client that connects to a non-routable address
    // to trigger a connection timeout
    let client = TimeoutMcpClient::new(
        Arc::new(HttpMcpClient::new("slow-server", "http://192.0.2.1:9999/")),
        Duration::from_millis(10),
    );

    let err = client.list_tools("slow-server").await.unwrap_err();
    assert!(err.contains("timeout") || err.contains("Timeout"));
}

#[tokio::test]
async fn filter_tools_for_scope_deny_all() {
    let scope = AgentScope {
        agent_id: "test".into(),
        project_id: None,
        mcp_sources: HashSet::from(["server-a".into()]),
        tools_allow: ToolsAllow::DenyAll,
        rag_sources: HashSet::new(),
        rag_write: RagWritePolicy::None,
        max_tool_calls: 8,
        max_iterations: 6,
        timeout_secs: 90,
    };

    let tools = vec![
        McpToolDef {
            server_id: "server-a".into(),
            name: "ping".into(),
            description: String::new(),
            input_schema: json!({}),
        },
        McpToolDef {
            server_id: "server-a".into(),
            name: "exec".into(),
            description: String::new(),
            input_schema: json!({}),
        },
    ];

    let filtered = filter_tools_for_scope(tools, &scope);
    assert!(filtered.is_empty());
}

#[tokio::test]
async fn filter_tools_for_scope_allowlist() {
    let scope = AgentScope {
        agent_id: "test".into(),
        project_id: None,
        mcp_sources: HashSet::from(["server-a".into()]),
        tools_allow: ToolsAllow::Allowlist(HashSet::from([ToolRef::parse("server-a/ping")])),
        rag_sources: HashSet::new(),
        rag_write: RagWritePolicy::None,
        max_tool_calls: 8,
        max_iterations: 6,
        timeout_secs: 90,
    };

    let tools = vec![
        McpToolDef {
            server_id: "server-a".into(),
            name: "ping".into(),
            description: String::new(),
            input_schema: json!({}),
        },
        McpToolDef {
            server_id: "server-a".into(),
            name: "exec".into(),
            description: String::new(),
            input_schema: json!({}),
        },
    ];

    let filtered = filter_tools_for_scope(tools, &scope);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].name, "ping");
}

#[tokio::test]
async fn filter_tools_for_scope_all_from_sources() {
    let scope = AgentScope {
        agent_id: "test".into(),
        project_id: None,
        mcp_sources: HashSet::from(["server-a".into()]),
        tools_allow: ToolsAllow::AllFromSources,
        rag_sources: HashSet::new(),
        rag_write: RagWritePolicy::None,
        max_tool_calls: 8,
        max_iterations: 6,
        timeout_secs: 90,
    };

    let tools = vec![
        McpToolDef {
            server_id: "server-a".into(),
            name: "ping".into(),
            description: String::new(),
            input_schema: json!({}),
        },
        McpToolDef {
            server_id: "server-a".into(),
            name: "exec".into(),
            description: String::new(),
            input_schema: json!({}),
        },
    ];

    let filtered = filter_tools_for_scope(tools, &scope);
    assert_eq!(filtered.len(), 2);
}

#[tokio::test]
async fn filter_rejects_tools_from_non_scoped_servers() {
    let scope = AgentScope {
        agent_id: "test".into(),
        project_id: None,
        mcp_sources: HashSet::from(["server-a".into()]),
        tools_allow: ToolsAllow::AllFromSources,
        rag_sources: HashSet::new(),
        rag_write: RagWritePolicy::None,
        max_tool_calls: 8,
        max_iterations: 6,
        timeout_secs: 90,
    };

    let tools = vec![
        McpToolDef {
            server_id: "server-a".into(),
            name: "allowed".into(),
            description: String::new(),
            input_schema: json!({}),
        },
        McpToolDef {
            server_id: "server-b".into(),
            name: "blocked".into(),
            description: String::new(),
            input_schema: json!({}),
        },
    ];

    let filtered = filter_tools_for_scope(tools, &scope);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].name, "allowed");
}

#[tokio::test]
async fn call_counter_increments_on_tool_call() {
    let tools = vec![make_tool("ping", "")];
    let (addr, call_counter) = spawn_mock_server(tools).await;

    let client = HttpMcpClient::new("test-server", format!("http://{}/", addr));

    let _ = client
        .call_tool(McpCallRequest::new("test-server", "ping", json!({})))
        .await
        .expect("call_tool failed");

    assert_eq!(call_counter.load(Ordering::SeqCst), 1);

    let _ = client
        .call_tool(McpCallRequest::new("test-server", "ping", json!({})))
        .await
        .expect("call_tool failed");

    assert_eq!(call_counter.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn http_client_server_returns_error_object() {
    let tools = vec![make_tool("ping", "")];
    let (addr, _) = spawn_mock_server(tools).await;

    let client = HttpMcpClient::new("test-server", format!("http://{}/", addr));

    // Send a method the mock doesn't handle
    // Actually, the mock handles unknown methods as JSON-RPC errors
    // Let's test that a server error is propagated
    let result = client
        .call_tool(McpCallRequest::new("test-server", "error_tool", json!({})))
        .await
        .expect("call_tool should succeed");

    assert!(result.is_error);
    assert!(result.content.to_string().contains("Something went wrong"));
}

// --- SSE Mock Server Helper ---

async fn mock_mcp_sse_handler(
    State(state): State<Arc<MockMcpState>>,
    Json(body): Json<Value>,
) -> ([(axum::http::HeaderName, &'static str); 1], String) {
    let method = body.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let id = body.get("id").cloned().unwrap_or(json!(1));

    match method {
        "tools/list" => {
            state.call_counter.fetch_add(1, Ordering::SeqCst);
            let payload = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": state.tools }
            });
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                format!("data: {}\n\n", payload),
            )
        }
        "tools/call" => {
            state.call_counter.fetch_add(1, Ordering::SeqCst);
            let name = body
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let payload = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{"type": "text", "text": format!("sse called {}", name)}]
                }
            });
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                format!("data: {}\n\n", payload),
            )
        }
        _ => {
            let payload = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {}
            });
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                format!("data: {}\n\n", payload),
            )
        }
    }
}

async fn spawn_mock_sse_server(tools: Vec<Value>) -> (SocketAddr, Arc<AtomicUsize>) {
    let call_counter = Arc::new(AtomicUsize::new(0));
    let state = Arc::new(MockMcpState {
        tools,
        call_counter: call_counter.clone(),
    });
    let app = Router::new()
        .route("/", post(mock_mcp_sse_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind sse mock");
    let addr = listener.local_addr().expect("failed to get addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("sse server error");
    });
    (addr, call_counter)
}

// --- Additional Component Tests ---

#[tokio::test]
async fn stdio_client_list_and_call_tools() {
    let script = r#"
while read line; do
    if echo "$line" | grep -q '"method":"initialize"'; then
        echo '{"jsonrpc":"2.0","id":"1","result":{"protocolVersion":"2024-11-05"}}'
    elif echo "$line" | grep -q '"method":"tools/list"'; then
        echo '{"jsonrpc":"2.0","id":"2","result":{"tools":[{"name":"stdio_ping","description":"ping tool"}]}}'
    elif echo "$line" | grep -q '"method":"tools/call"'; then
        echo '{"jsonrpc":"2.0","id":"3","result":{"content":[{"type":"text","text":"stdio pong"}],"isError":false}}'
    fi
done
"#;

    let client = StdioMcpClient::new(
        "stdio-server",
        "sh",
        vec!["-c".to_string(), script.to_string()],
        Duration::from_secs(5),
    );

    let tools = client.list_tools("stdio-server").await.expect("stdio list_tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "stdio_ping");

    let result = client
        .call_tool(McpCallRequest::new(
            "stdio-server",
            "stdio_ping",
            json!({}),
        ))
        .await
        .expect("stdio call_tool");

    assert!(!result.is_error);
    assert!(result.content.to_string().contains("stdio pong"));
}

#[tokio::test]
async fn registry_from_configs_and_namespacing() {
    let tools = vec![make_tool("list_items", "Lists items")];
    let (addr, _) = spawn_mock_server(tools).await;

    let configs = vec![
        McpServerConfig {
            id: "server1".into(),
            transport: "http".into(),
            url: Some(format!("http://{}/", addr)),
            auth_env: None,
            command: None,
            args: vec![],
            timeout_secs: 30,
            tool_namespace: Some("items".into()),
            enabled: true,
        },
        McpServerConfig {
            id: "disabled_server".into(),
            transport: "http".into(),
            url: Some("http://127.0.0.1:9999/".into()),
            auth_env: None,
            command: None,
            args: vec![],
            timeout_secs: 30,
            tool_namespace: None,
            enabled: false,
        },
    ];

    let registry = StaticMcpRegistry::from_configs(&configs).expect("registry init");
    let ids = registry.list_server_ids().await;
    assert_eq!(ids, vec!["server1".to_string()]);

    let client = registry.client_for("server1").await.expect("client_for");
    let tools = client.list_tools("server1").await.expect("list_tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "items_list_items");

    let call_res = client
        .call_tool(McpCallRequest::new(
            "server1",
            "items_list_items",
            json!({}),
        ))
        .await
        .expect("call_tool");
    assert!(!call_res.is_error);
    assert!(call_res.content.to_string().contains("called list_items"));
}

#[tokio::test]
async fn registry_discover_scoped_tools() {
    let tools = vec![make_tool("query", "Query DB")];
    let (addr, _) = spawn_mock_server(tools).await;

    let configs = vec![McpServerConfig {
        id: "db_server".into(),
        transport: "http".into(),
        url: Some(format!("http://{}/", addr)),
        auth_env: None,
        command: None,
        args: vec![],
        timeout_secs: 30,
        tool_namespace: None,
        enabled: true,
    }];

    let registry = StaticMcpRegistry::from_configs(&configs).expect("registry init");

    let scope = AgentScope {
        agent_id: "test".into(),
        project_id: None,
        mcp_sources: HashSet::from(["db_server".into()]),
        tools_allow: ToolsAllow::AllFromSources,
        rag_sources: HashSet::new(),
        rag_write: RagWritePolicy::None,
        max_tool_calls: 5,
        max_iterations: 5,
        timeout_secs: 60,
    };

    let discovered = registry
        .discover_scoped_tools(&scope)
        .await
        .expect("discover_scoped_tools");
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].name, "query");
    assert_eq!(discovered[0].server_id, "db_server");
}

#[tokio::test]
async fn streaming_http_client_sse_events() {
    let tools = vec![make_tool("stream_tool", "Stream test tool")];
    let (addr, _) = spawn_mock_sse_server(tools).await;

    let client = StreamingHttpMcpClient::new("sse-server", format!("http://{}/", addr));
    let list_res = client
        .list_tools("sse-server")
        .await
        .expect("list_tools via SSE");
    assert_eq!(list_res.len(), 1);
    assert_eq!(list_res[0].name, "stream_tool");

    let call_res = client
        .call_tool(McpCallRequest::new(
            "sse-server",
            "stream_tool",
            json!({}),
        ))
        .await
        .expect("call_tool via SSE");
    assert!(!call_res.is_error);
    assert!(call_res.content.to_string().contains("sse called stream_tool"));
}

#[tokio::test]
async fn cancellation_token_cancels_tool_call() {
    let token = CancellationToken::new();
    assert!(!token.is_cancelled());

    token.cancel();
    assert!(token.is_cancelled());

    let tools = vec![make_tool("slow", "")];
    let (addr, _) = spawn_mock_server(tools).await;

    let client = HttpMcpClient::new("test-server", format!("http://{}/", addr));
    let req = McpCallRequest::new("test-server", "slow", json!({})).with_cancellation(token);

    let err = client.call_tool(req).await.unwrap_err();
    assert!(err.contains("cancelled"));
}

#[tokio::test]
async fn namespaced_client_wraps_tool_names() {
    let tools = vec![make_tool("fetch", "Fetch data")];
    let (addr, _) = spawn_mock_server(tools).await;

    let inner = Arc::new(HttpMcpClient::new("server", format!("http://{}/", addr)));
    let ns_client = NamespacedMcpClient::new(inner, "my_prefix");

    let list_res = ns_client.list_tools("server").await.expect("list_tools");
    assert_eq!(list_res.len(), 1);
    assert_eq!(list_res[0].name, "my_prefix_fetch");

    let call_res = ns_client
        .call_tool(McpCallRequest::new(
            "server",
            "my_prefix_fetch",
            json!({}),
        ))
        .await
        .expect("call_tool");
    assert!(!call_res.is_error);
    assert!(call_res.content.to_string().contains("called fetch"));
}
