use crate::api::handlers::AppState;
use crate::domain::models::{ChatMessage as DomainChatMessage, ChatRequest};
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

// Auto-generated protobuf traits
pub mod pb {
    tonic::include_proto!("llamar");
}

use pb::llama_gateway_server::LlamaGateway;

pub struct GrpcService {
    state: Arc<AppState>,
}

impl GrpcService {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl LlamaGateway for GrpcService {
    async fn chat(
        &self,
        request: Request<pb::ChatRequest>,
    ) -> Result<Response<pb::ChatResponse>, Status> {
        let req = request.into_inner();

        let mut domain_messages = vec![];
        for msg in req.messages {
            domain_messages.push(DomainChatMessage {
                role: msg.role,
                content: msg.content,
            });
        }

        let mut domain_req = ChatRequest {
            model: req.model,
            messages: domain_messages,
            stream: false,
        };

        // Reuse optimizer logic (simplified for brevity)
        if let Some(agent) = self.state.agent_manager.get_agent(&domain_req.model) {
            domain_req.model = agent.config.model.clone();
            // In a full implementation, we'd apply TokenOptimizer here
        }

        match self.state.provider.chat(domain_req).await {
            Ok(resp) => {
                let grpc_msg = pb::ChatMessage {
                    role: resp.message.role,
                    content: resp.message.content,
                };

                Ok(Response::new(pb::ChatResponse {
                    model: resp.model,
                    created_at: resp.created_at,
                    message: Some(grpc_msg),
                    done: resp.done,
                }))
            }
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    type ChatStreamStream = Pin<Box<dyn Stream<Item = Result<pb::ChatStreamEvent, Status>> + Send>>;

    async fn chat_stream(
        &self,
        request: Request<pb::ChatRequest>,
    ) -> Result<Response<Self::ChatStreamStream>, Status> {
        let req = request.into_inner();

        // ... (convert messages similarly to `chat`)
        let mut domain_messages = vec![];
        for msg in req.messages {
            domain_messages.push(DomainChatMessage {
                role: msg.role,
                content: msg.content,
            });
        }

        let mut domain_req = ChatRequest {
            model: req.model,
            messages: domain_messages,
            stream: true,
        };

        if let Some(agent) = self.state.agent_manager.get_agent(&domain_req.model) {
            domain_req.model = agent.config.model.clone();
        }

        match self.state.provider.chat_stream(domain_req).await {
            Ok(stream) => {
                let grpc_stream = stream.map(|res| match res {
                    Ok(event) => {
                        let msg = pb::ChatMessage {
                            role: event.message.role,
                            content: event.message.content,
                        };
                        Ok(pb::ChatStreamEvent {
                            model: event.model,
                            created_at: event.created_at,
                            message: Some(msg),
                            done: event.done,
                        })
                    }
                    Err(e) => Err(Status::internal(e.to_string())),
                });

                Ok(Response::new(
                    Box::pin(grpc_stream) as Self::ChatStreamStream
                ))
            }
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    // Bidirectional
    type ConnectMcpStream = Pin<Box<dyn Stream<Item = Result<pb::McpMessage, Status>> + Send>>;

    async fn connect_mcp(
        &self,
        request: Request<Streaming<pb::McpMessage>>,
    ) -> Result<Response<Self::ConnectMcpStream>, Status> {
        let mut in_stream = request.into_inner();

        let output = async_stream::try_stream! {
            while let Some(msg) = in_stream.next().await {
                let msg = msg?;
                // Parse MCP payload (future task)

                // Echo back for now
                yield pb::McpMessage {
                    json_rpc: msg.json_rpc,
                    method: msg.method,
                    params: msg.params,
                };
            }
        };

        Ok(Response::new(Box::pin(output) as Self::ConnectMcpStream))
    }
}
