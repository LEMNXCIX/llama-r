use crate::api::chat_core::{execute_chat, execute_chat_stream};
use crate::api::handlers::AppState;
use crate::api::route_resolution::AgentSelection;
use crate::domain::models::{
    ChatMessage as DomainChatMessage, ChatRequest, ChatStreamEvent as DomainChatStreamEvent,
};
use crate::error::AppError;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

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

fn to_domain_request(req: pb::ChatRequest, stream: bool) -> ChatRequest {
    ChatRequest {
        model: req.model,
        messages: req
            .messages
            .into_iter()
            .map(|msg| DomainChatMessage {
                role: msg.role,
                content: msg.content,
            })
            .collect(),
        stream,
    }
}

/// Map one domain stream event to its protobuf counterpart.
///
/// This is the exact mapping used by `chat_stream`, made public so the wire
/// test can exercise it without standing up an agent with tools. Only hashes
/// and identifiers cross the wire: raw tool arguments and output previews are
/// not part of the domain event at all.
pub fn to_proto_stream_event(event: DomainChatStreamEvent) -> pb::ChatStreamEvent {
    pb::ChatStreamEvent {
        model: event.model,
        created_at: event.created_at,
        message: Some(pb::ChatMessage {
            role: event.message.role,
            content: event.message.content,
        }),
        done: event.done,
        approval: event.approval.map(|a| pb::ApprovalStreamInfo {
            id: a.id,
            tool_name: a.tool_name,
            args_hash: a.args_hash,
            task_id: a.task_id,
            depth: a.depth,
        }),
        tool: event.tool.map(|t| pb::ToolStreamInfo {
            kind: t.kind,
            tool_name: t.tool_name,
            is_error: t.is_error,
            args_hash: t.args_hash,
        }),
    }
}

fn metadata_value(metadata: &tonic::metadata::MetadataMap, key: &str) -> Option<String> {
    metadata
        .get(key)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn to_status(state: &AppState, error: AppError) -> Status {
    state.observability.record_grpc_error();
    match error {
        AppError::Validation(message) => Status::invalid_argument(message),
        AppError::NotFound(message) => Status::not_found(message),
        AppError::Conflict(message) => Status::already_exists(message),
        other => Status::internal(other.to_string()),
    }
}

#[tonic::async_trait]
impl LlamaGateway for GrpcService {
    async fn chat(
        &self,
        request: Request<pb::ChatRequest>,
    ) -> Result<Response<pb::ChatResponse>, Status> {
        let project_id = metadata_value(request.metadata(), "x-project");
        let agent_id = metadata_value(request.metadata(), "x-agent");
        let conversation_id = metadata_value(request.metadata(), "x-conversation-id");
        let domain_req = to_domain_request(request.into_inner(), false);
        let selection = AgentSelection {
            project_id: project_id.as_deref(),
            agent_id: agent_id.as_deref(),
            conversation_id: conversation_id.as_deref(),
            debug: false,
        };
        let response = execute_chat(&self.state, domain_req, selection)
            .await
            .map_err(|error| to_status(&self.state, error))?;

        Ok(Response::new(pb::ChatResponse {
            model: response.model,
            created_at: response.created_at,
            message: Some(pb::ChatMessage {
                role: response.message.role,
                content: response.message.content,
            }),
            done: response.done,
        }))
    }

    type ChatStreamStream = Pin<Box<dyn Stream<Item = Result<pb::ChatStreamEvent, Status>> + Send>>;

    async fn chat_stream(
        &self,
        request: Request<pb::ChatRequest>,
    ) -> Result<Response<Self::ChatStreamStream>, Status> {
        let project_id = metadata_value(request.metadata(), "x-project");
        let agent_id = metadata_value(request.metadata(), "x-agent");
        let conversation_id = metadata_value(request.metadata(), "x-conversation-id");
        let domain_req = to_domain_request(request.into_inner(), true);
        let selection = AgentSelection {
            project_id: project_id.as_deref(),
            agent_id: agent_id.as_deref(),
            conversation_id: conversation_id.as_deref(),
            debug: false,
        };
        let stream = execute_chat_stream(&self.state, domain_req, selection)
            .await
            .map_err(|error| to_status(&self.state, error))?;
        let state = self.state.clone();
        let grpc_stream = stream.map(move |res| match res {
            Ok(event) => Ok(to_proto_stream_event(event)),
            Err(error) => Err(to_status(&state, error)),
        });

        Ok(Response::new(
            Box::pin(grpc_stream) as Self::ChatStreamStream
        ))
    }

    type ConnectMcpStream = Pin<Box<dyn Stream<Item = Result<pb::McpMessage, Status>> + Send>>;

    async fn connect_mcp(
        &self,
        request: Request<Streaming<pb::McpMessage>>,
    ) -> Result<Response<Self::ConnectMcpStream>, Status> {
        let mut in_stream = request.into_inner();
        let output = async_stream::try_stream! {
            while let Some(msg) = in_stream.next().await {
                let msg = msg?;
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
