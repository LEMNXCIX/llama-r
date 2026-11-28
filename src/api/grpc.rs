use crate::api::chat_core::{execute_chat, execute_chat_stream};
use crate::api::handlers::AppState;
use crate::api::mcp_api;
use crate::api::route_resolution::AgentSelection;
use crate::domain::models::{
    ChatMessage as DomainChatMessage, ChatRequest, ChatStreamEvent as DomainChatStreamEvent,
    JsonRpcRequest,
};
use crate::error::AppError;
use axum::{body::to_bytes, http::StatusCode};
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::{Stream, StreamExt};
use tonic::{Code, Request, Response, Status, Streaming};

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

/// Map a domain error to the gRPC code matching its HTTP counterpart in
/// `AppError::status_code()` (`error.rs`). The match is exhaustive on purpose:
/// adding an `AppError` variant must force a decision here instead of silently
/// collapsing a deliberate 501 or a 502 into a generic Internal.
///
/// The message is `AppError::Display`, which never carries a credential value.
pub fn grpc_code(error: &AppError) -> Code {
    match error {
        // 400 Bad Request
        AppError::Validation(_) | AppError::ApprovalArgsChanged(_) => Code::InvalidArgument,
        // 401 Unauthorized
        AppError::Unauthorized(_) => Code::Unauthenticated,
        // 403 Forbidden
        AppError::Forbidden(_) => Code::PermissionDenied,
        // 404 Not Found
        AppError::NotFound(_) | AppError::ApprovalNotFound(_) => Code::NotFound,
        // 409 Conflict
        AppError::Conflict(_) | AppError::ApprovalAlreadyDecided(_) => Code::AlreadyExists,
        // 410 Gone — the pending expired and must be re-requested.
        AppError::ApprovalExpired(_) => Code::DeadlineExceeded,
        // 501 Not Implemented — a deliberately disabled feature is not a server
        // fault. Kept distinct from Provider below so a client can tell "this
        // capability is off" from "the upstream went away".
        AppError::Unavailable(_) => Code::Unimplemented,
        // 502 Bad Gateway — the upstream failed.
        AppError::Provider(_) => Code::Unavailable,
        // 500 Internal Server Error
        AppError::Config(_)
        | AppError::Io(_)
        | AppError::SerdeJson(_)
        | AppError::Toml(_)
        | AppError::Reqwest(_)
        | AppError::Notify(_)
        | AppError::Runtime(_) => Code::Internal,
    }
}

fn to_status(state: &AppState, error: AppError) -> Status {
    state.observability.record_grpc_error();
    Status::new(grpc_code(&error), error.to_string())
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
            conversation_id: response.conversation_id,
            served_by: response.served_by,
            usage: response.usage.map(|usage| pb::Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cost_usd: usage.cost_usd,
            }),
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
        let state = self.state.clone();
        let output = async_stream::try_stream! {
            while let Some(msg) = in_stream.next().await {
                let msg = msg?;
                // `json_rpc` is the single source of truth, just like the body
                // of POST /api/mcp. The separate method/params fields are a
                // fallback for legacy clients that only send those two.
                let req: JsonRpcRequest = if msg.json_rpc.is_empty() {
                    JsonRpcRequest {
                        jsonrpc: "2.0".to_string(),
                        id: None,
                        method: msg.method,
                        params: serde_json::from_str(&msg.params)
                            .unwrap_or(serde_json::Value::Null),
                    }
                } else {
                    match serde_json::from_str(&msg.json_rpc) {
                        Ok(req) => req,
                        Err(_) => {
                            // A bad envelope is a per-message protocol error,
                            // not a stream error: answer with the standard
                            // -32700 parse error and keep reading the stream.
                            yield pb::McpMessage {
                                json_rpc: serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": serde_json::Value::Null,
                                    "error": { "code": -32700, "message": "Parse error" },
                                })
                                .to_string(),
                                method: String::new(),
                                params: String::new(),
                            };
                            continue;
                        }
                    }
                };
                // Same dispatch as POST /api/mcp (`mcp_message` is a thin
                // wrapper around this call), so validation, redaction and tool
                // behaviour cannot drift between the two surfaces.
                //
                // A `tools/call` with an approval rule can park this stream
                // until a human decides: the HTTP park-and-ack in
                // `handlers.rs` is chat-only and does not apply here. That is
                // inherited MCP behaviour, not a regression introduced here.
                let resp = mcp_api::handle_message(state.clone(), req).await;
                // Notifications carry no id; the handler answers 204 and there
                // is deliberately no frame to forward.
                if resp.status() == StatusCode::NO_CONTENT {
                    continue;
                }
                let bytes = to_bytes(resp.into_body(), 2 * 1024 * 1024)
                    .await
                    .map_err(|error| {
                        Status::internal(format!("failed to read MCP response body: {error}"))
                    })?;
                yield pb::McpMessage {
                    json_rpc: String::from_utf8_lossy(&bytes).into_owned(),
                    method: String::new(),
                    params: String::new(),
                };
            }
        };
        Ok(Response::new(Box::pin(output) as Self::ConnectMcpStream))
    }
}
