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

use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::grpc::pb::llama_gateway_client::LlamaGatewayClient;
use llama_r::api::grpc::pb::ChatStreamEvent as PbChatStreamEvent;
use llama_r::api::grpc::pb::{ChatMessage, ChatRequest};
use llama_r::api::grpc::to_proto_stream_event;
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
use tokio_stream::Stream;
use tonic::transport::{Channel, Server};

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
            usage: None,
            served_by: None,
            debug_prompt: None,
            debug_route: None,
            conversation_id: None,
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

    let message = response
        .into_inner()
        .message
        .expect("a chat response always carries a message");
    assert_eq!(message.role, "assistant");
    assert_eq!(message.content, "respuesta");

    server.abort();
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
