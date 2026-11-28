//! A real gRPC round trip through the generated client and server.
//!
//! `tonic` 0.14 moved protobuf support out into `tonic-prost`/`tonic-prost-build`.
//! A build can succeed while the generated code is inconsistent — the earlier
//! attempt with a stale `tonic-build` compiled the *old* generated file and
//! failed only when the service was actually used. Unit tests that call
//! `execute_chat` directly cannot see that: they never cross the codec.
//!
//! This test binds a real port, starts the real server, and calls it with the
//! generated client, so a broken codec or service registration fails here.

use axum::{
    body::{to_bytes, Body},
    extract::Request as AxumRequest,
    http::StatusCode,
};
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::grpc::pb::llama_gateway_client::LlamaGatewayClient;
use llama_r::api::grpc::pb::ChatStreamEvent as PbChatStreamEvent;
use llama_r::api::grpc::pb::{ChatMessage, ChatRequest, McpMessage as PbMcpMessage};
use llama_r::api::grpc::{grpc_code, to_proto_stream_event};
use llama_r::api::handlers::AppState;
use llama_r::context::store::ContextStore;
use llama_r::domain::models::{
    ApprovalInfo, ChatMessage as DomainChatMessage, ChatStreamEvent as DomainChatStreamEvent,
    ToolStreamInfo,
};
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::providers::LLMProvider;
use llama_r::runtime::build_app_state;
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::skill_manager::SkillManager;
use std::collections::VecDeque;
use std::error::Error;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_stream::{wrappers::ReceiverStream, Stream};
use tonic::transport::{Channel, Server};
use tower::ServiceExt;

struct EchoProvider;

#[async_trait::async_trait]
impl LLMProvider for EchoProvider {
    fn get_base_url(&self) -> String {
        "http://echo".to_string()
    }
    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }
    async fn list_models(
        &self,
    ) -> Result<Vec<llama_r::domain::models::ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
    async fn chat(
        &self,
        request: llama_r::domain::models::ChatRequest,
    ) -> Result<llama_r::domain::models::ChatResponse, Box<dyn Error + Send + Sync>> {
        Ok(llama_r::domain::models::ChatResponse {
            model: request.model,
            created_at: "now".to_string(),
            message: llama_r::domain::models::ChatMessage {
                role: "assistant".to_string(),
                content: "respuesta".to_string(),
            },
            done: true,
            usage: Some(llama_r::domain::models::Usage {
                input_tokens: 12,
                output_tokens: 34,
                cost_usd: Some(0.5),
            }),
            served_by: Some("profile".to_string()),
            debug_prompt: None,
            debug_route: None,
            conversation_id: Some("c-1".to_string()),
        })
    }
    async fn chat_stream(
        &self,
        _request: llama_r::domain::models::ChatRequest,
    ) -> Result<
        Pin<
            Box<
                dyn Stream<
                        Item = Result<
                            llama_r::domain::models::ChatStreamEvent,
                            Box<dyn Error + Send + Sync>,
                        >,
                    > + Send,
            >,
        >,
        Box<dyn Error + Send + Sync>,
    > {
        Err("not used".into())
    }
}

/// A free port, with the same TOCTOU caveat as the other wire tests.
fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

#[tokio::test]
async fn a_real_grpc_chat_round_trip_uses_the_generated_codec() {
    let addr = free_addr();

    let stack = llama_r::services::provider_stack::ProviderStack::new(
        Arc::new(EchoProvider) as Arc<dyn LLMProvider + Send + Sync>,
        "fallback-model",
    );
    let state: Arc<AppState> = build_app_state(
        stack,
        Arc::new(AgentRegistry::new()),
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        Arc::new(Mutex::new(VecDeque::new())),
        Vec::new(),
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
    );

    let service = llama_r::api::grpc::GrpcService::new(state);
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(
                llama_r::api::grpc::pb::llama_gateway_server::LlamaGatewayServer::new(service),
            )
            .serve(addr)
            .await
            .expect("gRPC server failed to serve");
    });

    // Wait for the listener rather than sleeping a fixed amount: on a loaded
    // CI runner the bind can take longer than any arbitrary timeout.
    let mut client = None;
    for _ in 0..80 {
        match Channel::from_shared(format!("http://{addr}"))
            .expect("valid URI")
            .connect()
            .await
        {
            Ok(channel) => {
                client = Some(LlamaGatewayClient::new(channel));
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    let mut client = client.expect("gRPC server never accepted a connection");

    // A direct model request (no project header) exercises the legacy path,
    // which is all the EchoProvider implements. It still crosses the full
    // tonic-prost encode/decode round trip — the thing this test is about.
    let response = client
        .chat(ChatRequest {
            model: "fallback-model".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "hola".to_string(),
            }],
            stream: false,
        })
        .await
        .expect("the gRPC call must succeed through the new codec");

    let response = response.into_inner();

    // Fields 1-4 are conserved byte-for-byte.
    let message = response
        .message
        .as_ref()
        .expect("a chat response always carries a message");
    assert_eq!(message.role, "assistant");
    assert_eq!(message.content, "respuesta");
    assert_eq!(response.model, "fallback-model");
    assert!(!response.created_at.is_empty());
    assert!(response.done);

    // The three new fields must survive the full tonic-prost encode/decode
    // round trip, not just exist in the generated struct.
    assert_eq!(response.conversation_id.as_deref(), Some("c-1"));
    assert_eq!(response.served_by.as_deref(), Some("profile"));
    let usage = response.usage.expect("usage survives the gRPC wire");
    assert_eq!(usage.input_tokens, 12);
    assert_eq!(usage.output_tokens, 34);
    assert_eq!(usage.cost_usd, Some(0.5));

    server.abort();
}

/// Every `AppError` variant maps to one deterministic `tonic::Code`. The table
/// is exhaustive on purpose: adding a variant to `AppError` must force this
/// test to be updated, which is what keeps the gRPC contract from silently
/// collapsing a deliberate 501 or a 502 into a generic Internal.
#[test]
fn grpc_code_maps_all_app_error_variants() {
    use llama_r::error::AppError;
    use tonic::Code;

    let reqwest_error = reqwest::Client::new().get("http://[").build().unwrap_err();
    let cases: Vec<(AppError, Code, &'static str)> = vec![
        (AppError::Config("x".into()), Code::Internal, "Config"),
        (
            AppError::Validation("x".into()),
            Code::InvalidArgument,
            "Validation",
        ),
        (AppError::NotFound("x".into()), Code::NotFound, "NotFound"),
        (
            AppError::Unauthorized("x".into()),
            Code::Unauthenticated,
            "Unauthorized",
        ),
        (
            AppError::Conflict("x".into()),
            Code::AlreadyExists,
            "Conflict",
        ),
        (
            AppError::Forbidden("x".into()),
            Code::PermissionDenied,
            "Forbidden",
        ),
        (
            AppError::Unavailable("x".into()),
            Code::Unimplemented,
            "Unavailable",
        ),
        (
            AppError::Provider("x".into()),
            Code::Unavailable,
            "Provider",
        ),
        (
            AppError::Io(std::io::Error::other("x")),
            Code::Internal,
            "Io",
        ),
        (
            AppError::SerdeJson(serde_json::from_str::<()>("{").unwrap_err()),
            Code::Internal,
            "SerdeJson",
        ),
        (
            AppError::Toml(toml::from_str::<()>("x = [").unwrap_err()),
            Code::Internal,
            "Toml",
        ),
        (AppError::Reqwest(reqwest_error), Code::Internal, "Reqwest"),
        (
            AppError::Notify(notify::Error::generic("x")),
            Code::Internal,
            "Notify",
        ),
        (AppError::Runtime("x".into()), Code::Internal, "Runtime"),
        (
            AppError::ApprovalNotFound("x".into()),
            Code::NotFound,
            "ApprovalNotFound",
        ),
        (
            AppError::ApprovalExpired("x".into()),
            Code::DeadlineExceeded,
            "ApprovalExpired",
        ),
        (
            AppError::ApprovalAlreadyDecided("x".into()),
            Code::AlreadyExists,
            "ApprovalAlreadyDecided",
        ),
        (
            AppError::ApprovalArgsChanged("x".into()),
            Code::InvalidArgument,
            "ApprovalArgsChanged",
        ),
    ];
    assert_eq!(
        cases.len(),
        18,
        "the table must cover all 18 AppError variants"
    );
    for (error, expected, label) in cases {
        assert_eq!(
            grpc_code(&error),
            expected,
            "variant {label} (code {}) mapped incorrectly",
            error.code()
        );
    }
}

/// A deliberately disabled feature is NOT the same failure as an upstream
/// that went away. The client distinguishes 501 from 502, so the gRPC codes
/// must not collapse the two into Internal or into each other.
#[test]
fn grpc_code_distinguishes_deliberate_unavailable_from_provider_failure() {
    use llama_r::error::AppError;
    use tonic::Code;

    let disabled = grpc_code(&AppError::Unavailable("feature off".into()));
    let upstream = grpc_code(&AppError::Provider("connection refused".into()));
    assert_eq!(disabled, Code::Unimplemented);
    assert_eq!(upstream, Code::Unavailable);
    assert_ne!(disabled, upstream);
}

fn domain_stream_event(
    approval: Option<ApprovalInfo>,
    tool: Option<ToolStreamInfo>,
) -> DomainChatStreamEvent {
    DomainChatStreamEvent {
        model: "m".to_string(),
        created_at: "now".to_string(),
        message: DomainChatMessage {
            role: "assistant".to_string(),
            content: String::new(),
        },
        done: false,
        approval,
        tool,
    }
}

/// The same mapper the gRPC stream uses must carry both new fields across the
/// wire, with only non-secret hashes and identifiers.
#[test]
fn grpc_stream_event_carries_approval_and_tool_without_raw_arguments() {
    let mapped: PbChatStreamEvent = to_proto_stream_event(domain_stream_event(
        Some(ApprovalInfo {
            id: "ap-1".to_string(),
            tool_name: "srv__t".to_string(),
            args_hash: "deadbeef".to_string(),
            task_id: Some("task-7".to_string()),
            depth: Some(2),
        }),
        Some(ToolStreamInfo {
            kind: "call".to_string(),
            tool_name: "srv__t".to_string(),
            is_error: None,
            args_hash: Some("abc".to_string()),
        }),
    ));

    // Fields 1-4 are conserved byte-for-byte.
    assert_eq!(mapped.model, "m");
    assert_eq!(mapped.created_at, "now");
    assert!(!mapped.done);
    let message = mapped
        .message
        .as_ref()
        .expect("a message is always present");
    assert_eq!(message.role, "assistant");
    assert_eq!(message.content, "");

    let tool = mapped
        .tool
        .as_ref()
        .expect("tool field survives the gRPC wire");
    assert_eq!(tool.kind, "call");
    assert_eq!(tool.tool_name, "srv__t");
    assert_eq!(tool.is_error, None);
    assert_eq!(tool.args_hash.as_deref(), Some("abc"));

    let approval = mapped
        .approval
        .as_ref()
        .expect("approval field survives the gRPC wire");
    assert_eq!(approval.id, "ap-1");
    assert_eq!(approval.tool_name, "srv__t");
    assert_eq!(approval.args_hash, "deadbeef");
    assert_eq!(approval.task_id.as_deref(), Some("task-7"));
    assert_eq!(approval.depth, Some(2));

    // Raw arguments were never part of the domain event, so no secret can be
    // present in the protobuf that crosses the gRPC codec.
    let wire = format!("{mapped:?}");
    assert!(!wire.contains("SECRET_VALUE"));
}

#[test]
fn grpc_stream_event_maps_tool_result_without_args_hash() {
    let mapped = to_proto_stream_event(domain_stream_event(
        None,
        Some(ToolStreamInfo {
            kind: "result".to_string(),
            tool_name: "search".to_string(),
            is_error: Some(true),
            args_hash: None,
        }),
    ));

    let tool = mapped.tool.as_ref().expect("tool result must be mapped");
    assert_eq!(tool.kind, "result");
    assert_eq!(tool.tool_name, "search");
    assert_eq!(tool.is_error, Some(true));
    assert_eq!(tool.args_hash, None);
    assert!(mapped.approval.is_none());
}

#[test]
fn grpc_stream_event_leaves_optional_fields_unset_on_ordinary_events() {
    let mapped = to_proto_stream_event(domain_stream_event(None, None));

    assert_eq!(mapped.model, "m");
    assert!(mapped.message.is_some());
    assert!(!mapped.done);
    assert!(mapped.approval.is_none());
    assert!(mapped.tool.is_none());
}

#[test]
fn grpc_approval_without_delegation_fields_leaves_them_unset() {
    let mapped = to_proto_stream_event(domain_stream_event(
        Some(ApprovalInfo {
            id: "ap-2".to_string(),
            tool_name: "local_tool".to_string(),
            args_hash: "cafe".to_string(),
            task_id: None,
            depth: None,
        }),
        None,
    ));

    let approval = mapped.approval.as_ref().expect("approval must be mapped");
    assert_eq!(approval.id, "ap-2");
    assert_eq!(approval.task_id, None);
    assert_eq!(approval.depth, None);
    assert!(mapped.tool.is_none());
}

// --- connect_mcp: the bidi stream must dispatch exactly like POST /api/mcp ---

/// A real server plus generated client for the bidirectional MCP RPC, sharing
/// one `AppState` with the HTTP router the parity assertions compare against.
async fn spawn_mcp_grpc_client() -> (
    LlamaGatewayClient<Channel>,
    tokio::task::JoinHandle<()>,
    Arc<AppState>,
) {
    let stack = llama_r::services::provider_stack::ProviderStack::new(
        Arc::new(EchoProvider) as Arc<dyn LLMProvider + Send + Sync>,
        "fallback-model",
    );
    let state: Arc<AppState> = build_app_state(
        stack,
        Arc::new(AgentRegistry::new()),
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        Arc::new(Mutex::new(VecDeque::new())),
        Vec::new(),
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
    );
    let addr = free_addr();
    let service = llama_r::api::grpc::GrpcService::new(state.clone());
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(
                llama_r::api::grpc::pb::llama_gateway_server::LlamaGatewayServer::new(service),
            )
            .serve(addr)
            .await
            .expect("gRPC server failed to serve");
    });
    let mut client = None;
    for _ in 0..80 {
        match Channel::from_shared(format!("http://{addr}"))
            .expect("valid URI")
            .connect()
            .await
        {
            Ok(channel) => {
                client = Some(LlamaGatewayClient::new(channel));
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    (
        client.expect("gRPC server never accepted a connection"),
        server,
        state,
    )
}

/// The real HTTP handler at `/api/mcp`, with no admin middleware: the parity
/// target is the same `mcp_api::handle_message` the production router calls.
fn http_mcp_router(state: Arc<AppState>) -> axum::Router {
    axum::Router::new()
        .route(
            "/api/mcp",
            axum::routing::post(llama_r::api::handlers::mcp_message),
        )
        .with_state(state)
}

async fn http_mcp_response(
    app: &axum::Router,
    body: serde_json::Value,
) -> axum::response::Response {
    app.clone()
        .oneshot(
            AxumRequest::builder()
                .method("POST")
                .uri("/api/mcp")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn http_mcp_value(app: &axum::Router, body: serde_json::Value) -> serde_json::Value {
    let resp = http_mcp_response(app, body).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 2 * 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).expect("HTTP /api/mcp must answer JSON")
}

async fn next_mcp_frame(stream: &mut tonic::Streaming<PbMcpMessage>) -> PbMcpMessage {
    tokio::time::timeout(Duration::from_secs(5), stream.message())
        .await
        .expect("connect_mcp did not yield a frame in time")
        .expect("gRPC stream error")
        .expect("connect_mcp stream ended")
}

#[tokio::test]
async fn connect_mcp_answers_initialize_and_tools_list_like_http() {
    let (mut client, server, state) = spawn_mcp_grpc_client().await;
    let http = http_mcp_router(state);
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let mut stream = client
        .connect_mcp(tonic::Request::new(ReceiverStream::new(rx)))
        .await
        .expect("connect_mcp call must be accepted")
        .into_inner();

    for (id, method) in [(1, "initialize"), (2, "tools/list")] {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": {},
        });
        let expected = http_mcp_value(&http, request.clone()).await;

        tx.send(PbMcpMessage {
            json_rpc: request.to_string(),
            method: String::new(),
            params: String::new(),
        })
        .await
        .expect("inbound stream still open");

        let frame = next_mcp_frame(&mut stream).await;
        let actual: serde_json::Value = serde_json::from_str(&frame.json_rpc)
            .unwrap_or_else(|e| panic!("frame is not JSON-RPC ({e}): {}", frame.json_rpc));
        assert_eq!(
            actual, expected,
            "{method} must dispatch identically on gRPC and HTTP"
        );
        assert_eq!(frame.method, "");
        assert_eq!(frame.params, "");
    }
    server.abort();
}

#[tokio::test]
async fn connect_mcp_falls_back_to_method_and_params_when_json_rpc_is_empty() {
    let (mut client, server, state) = spawn_mcp_grpc_client().await;
    let http = http_mcp_router(state);
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let mut stream = client
        .connect_mcp(tonic::Request::new(ReceiverStream::new(rx)))
        .await
        .expect("connect_mcp call must be accepted")
        .into_inner();

    // A legacy frame has no id, so ordinary methods answer 204 (no frame).
    // Use an invalid create_agent argument instead: the handler returns an
    // error response even without an id, which proves the method/params were
    // actually dispatched rather than echoed. No agent file is written.
    let params = serde_json::json!({
        "name": "create_agent",
        "arguments": { "id": "bad id!", "name": "x", "system_prompt": "y" },
    });
    let expected = http_mcp_response(
        &http,
        serde_json::json!({
            "jsonrpc": "2.0", "method": "tools/call", "params": params.clone(),
        }),
    )
    .await;
    assert_eq!(expected.status(), StatusCode::BAD_REQUEST);
    let expected_bytes = to_bytes(expected.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();

    tx.send(PbMcpMessage {
        json_rpc: String::new(),
        method: "tools/call".to_string(),
        params: params.to_string(),
    })
    .await
    .expect("inbound stream still open");

    let frame = next_mcp_frame(&mut stream).await;
    assert_eq!(
        frame.json_rpc,
        String::from_utf8_lossy(&expected_bytes),
        "a legacy method/params frame must use the very same handler"
    );
    assert_eq!(frame.method, "");
    assert_eq!(frame.params, "");
    server.abort();
}

#[tokio::test]
async fn connect_mcp_notification_without_id_emits_no_frame() {
    let (mut client, server, state) = spawn_mcp_grpc_client().await;
    let http = http_mcp_router(state);
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let mut stream = client
        .connect_mcp(tonic::Request::new(ReceiverStream::new(rx)))
        .await
        .expect("connect_mcp call must be accepted")
        .into_inner();

    let notification = serde_json::json!({
        "jsonrpc": "2.0", "method": "notifications/initialized",
    });
    // HTTP returns 204 for a notification: the gRPC stream must skip that
    // frame entirely rather than echo the input or send an empty message.
    assert_eq!(
        http_mcp_response(&http, notification.clone())
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    tx.send(PbMcpMessage {
        json_rpc: notification.to_string(),
        method: String::new(),
        params: String::new(),
    })
    .await
    .expect("inbound stream still open");

    let no_frame = tokio::time::timeout(Duration::from_millis(250), stream.message()).await;
    assert!(no_frame.is_err(), "a notification must not produce a frame");

    // The stream is still usable: a following request gets its own answer.
    tx.send(PbMcpMessage {
        json_rpc: r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}"#.to_string(),
        method: String::new(),
        params: String::new(),
    })
    .await
    .expect("inbound stream still open");
    let frame = next_mcp_frame(&mut stream).await;
    let actual: serde_json::Value = serde_json::from_str(&frame.json_rpc).unwrap();
    assert_eq!(actual["id"], serde_json::json!(7));
    assert!(actual["result"].is_object());
    server.abort();
}

#[tokio::test]
async fn connect_mcp_malformed_json_rpc_yields_parse_error_and_stays_open() {
    let (mut client, server, _state) = spawn_mcp_grpc_client().await;
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let mut stream = client
        .connect_mcp(tonic::Request::new(ReceiverStream::new(rx)))
        .await
        .expect("connect_mcp call must be accepted")
        .into_inner();

    tx.send(PbMcpMessage {
        json_rpc: "{not json".to_string(),
        method: String::new(),
        params: String::new(),
    })
    .await
    .expect("inbound stream still open");

    let frame = next_mcp_frame(&mut stream).await;
    let actual: serde_json::Value = serde_json::from_str(&frame.json_rpc)
        .unwrap_or_else(|e| panic!("frame is not JSON-RPC ({e}): {}", frame.json_rpc));
    assert_eq!(actual["jsonrpc"], "2.0");
    assert_eq!(actual["id"], serde_json::Value::Null);
    assert_eq!(actual["error"]["code"], -32700);
    assert!(actual["error"]["message"]
        .as_str()
        .is_some_and(|m| !m.is_empty()));

    // A parse error is per message, not fatal to the stream.
    tx.send(PbMcpMessage {
        json_rpc: r#"{"jsonrpc":"2.0","id":8,"method":"initialize","params":{}}"#.to_string(),
        method: String::new(),
        params: String::new(),
    })
    .await
    .expect("inbound stream still open");
    let frame = next_mcp_frame(&mut stream).await;
    let actual: serde_json::Value = serde_json::from_str(&frame.json_rpc).unwrap();
    assert_eq!(actual["id"], serde_json::json!(8));
    assert!(actual["error"].is_null());
    server.abort();
}
