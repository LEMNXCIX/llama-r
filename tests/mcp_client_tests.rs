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
use llama_r::adapters::mcp::TimeoutMcpClient;
use llama_r::domain::agent::RagWritePolicy;
use llama_r::domain::scope::{AgentScope, ToolRef, ToolsAllow};
use llama_r::ports::mcp::{filter_tools_for_scope, McpCallRequest, McpClient, McpToolDef};
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
        .call_tool(McpCallRequest {
            server_id: "test-server".to_string(),
            tool_name: "echo".to_string(),
            arguments: json!({"msg": "hello"}),
        })
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
        .call_tool(McpCallRequest {
            server_id: "test-server".to_string(),
            tool_name: "error_tool".to_string(),
            arguments: json!({}),
        })
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
        .call_tool(McpCallRequest {
            server_id: "test-server".to_string(),
            tool_name: "ping".to_string(),
            arguments: json!({}),
        })
        .await
        .expect("call_tool failed");

    assert_eq!(call_counter.load(Ordering::SeqCst), 1);

    let _ = client
        .call_tool(McpCallRequest {
            server_id: "test-server".to_string(),
            tool_name: "ping".to_string(),
            arguments: json!({}),
        })
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
        .call_tool(McpCallRequest {
            server_id: "test-server".to_string(),
            tool_name: "error_tool".to_string(),
            arguments: json!({}),
        })
        .await
        .expect("call_tool should succeed");

    assert!(result.is_error);
    assert!(result.content.to_string().contains("Something went wrong"));
}
