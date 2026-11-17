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
use llama_r::api::grpc::pb::{ChatMessage, ChatRequest};
use llama_r::api::handlers::AppState;
use llama_r::context::store::ContextStore;
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
            debug_prompt: None,
            debug_route: None,
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
