//! Signed inbound webhook channels.
//!
//! `POST /api/webhooks/:channel` resolves a channel from `channels.toml`,
//! verifies its signature over the RAW bytes (before any JSON parsing), then
//! dispatches one agent turn built strictly from the channel config. Nothing
//! from the emitter's headers can widen that scope: project, agent and the
//! debug gate all come from the config, never from the request.

use crate::adapters::channel::load_channels;
use crate::api::chat_core::{agent_not_found, execute_chat, execute_chat_with_new_id};
use crate::api::handlers::AppState;
use crate::api::route_resolution::AgentSelection;
use crate::core::paths::get_base_dir;
use crate::domain::channel::ChannelMode;
use crate::domain::models::{ChatMessage, ChatRequest};
use crate::error::AppError;
use crate::services::marker_template::message_from_template;
use axum::{
    body::to_bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

/// A body larger than this is refused before parsing. Webhook payloads are
/// small; an unbounded read would let an emitter exhaust the server.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// The `202` acknowledgement. The id is minted before the turn is spawned so
/// the client can poll `GET /api/executions/:id` immediately.
#[derive(Debug, Serialize)]
struct WebhookAccepted {
    execution_id: String,
}

/// Verify the raw body of a signed channel request, then run one agent turn.
///
/// The order is deliberate: raw bytes → resolve channel → verify → parse →
/// render → scope → dispatch. The signature is over the exact bytes the sender
/// signed, so nothing may be parsed, trimmed or re-serialised before the check.
/// A failure at any gate is a generic error — no body, signature or secret is
/// ever echoed into a response or a log line.
pub async fn webhook(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<Response, AppError> {
    // Load per request so an operator editing channels.toml gets the change
    // without a restart, exactly like agents and contexts.
    let channel = load_channels(&get_base_dir())
        .into_iter()
        .find(|channel| channel.id == channel_id && channel.enabled)
        .ok_or_else(|| AppError::NotFound("unknown or disabled channel".into()))?;

    // HMAC over the raw bytes, before any parsing. Resolving the channel first
    // is what tells us which header and which env var to use; nothing here can
    // read a secret from the request.
    let bytes = to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| AppError::Validation("invalid body".into()))?;
    // Fail closed: a missing or empty secret must never become an empty HMAC
    // key, which an attacker could use to forge a valid signature. The warning
    // names only the variable, never its value (there is no value here).
    let secret = match std::env::var(&channel.auth_env) {
        Ok(value) if !value.is_empty() => value,
        _ => {
            tracing::warn!(
                auth_env = %channel.auth_env,
                "webhook channel has no shared secret; refusing request"
            );
            return Err(AppError::Unauthorized("invalid signature".into()));
        }
    };
    let signature = headers
        .get(channel.signature_header.as_str())
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !state.channel_verifier.verify(&bytes, signature, &secret) {
        // One generic refusal for a missing, malformed or wrong signature: it
        // says nothing about the body, the header or the shared secret.
        return Err(AppError::Unauthorized("invalid signature".into()));
    }

    // Only now is the body parsed. Both a parse failure and a template failure
    // are 400s whose text names the template path at most, never body content.
    let payload: Value =
        serde_json::from_slice(&bytes).map_err(|_| AppError::Validation("invalid_json".into()))?;
    let raw_body = String::from_utf8_lossy(&bytes);
    let message = message_from_template(channel.template.as_deref(), &payload, &raw_body)
        .map_err(AppError::Validation)?;

    // The scope comes from config, so validate it BEFORE either mode
    // acknowledges the request: a missing agent is the same 400 a
    // header-pinned request gets, and an async turn must never answer 202 for
    // a channel that cannot dispatch. Prefer the agent's own model so the
    // channel answers on the model it would use on any other surface.
    let resolved_agent = state
        .agent_registry
        .resolve_agent(channel.project_id.as_deref(), Some(&channel.agent_id));
    let model = resolved_agent
        .as_ref()
        .map(|agent| agent.config.model.clone())
        .unwrap_or_default();
    if resolved_agent.is_none() {
        return Err(agent_not_found(
            &channel.agent_id,
            channel.project_id.as_deref().unwrap_or(""),
        ));
    }
    let request = ChatRequest {
        model,
        messages: vec![ChatMessage {
            role: "user".into(),
            content: message,
        }],
        stream: false,
    };

    let crate::domain::channel::ChannelConfig {
        agent_id,
        project_id,
        mode,
        ..
    } = channel;
    match mode {
        ChannelMode::Sync => {
            let selection = AgentSelection {
                project_id: project_id.as_deref(),
                agent_id: Some(agent_id.as_str()),
                conversation_id: None,
                debug: false,
            };
            let response = execute_chat(&state, request, selection).await?;
            Ok(Json(response).into_response())
        }
        ChannelMode::Async => {
            // The id is observed by GET /api/executions/:id, so it must be
            // minted HERE, before the 202 is sent — not inside the spawned
            // task. `execute_chat_with_new_id` creates the row under this id
            // (it is a fresh turn, not a retry).
            let execution_id = Uuid::new_v4();
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let selection = AgentSelection {
                    project_id: project_id.as_deref(),
                    agent_id: Some(agent_id.as_str()),
                    conversation_id: None,
                    debug: false,
                };
                if execute_chat_with_new_id(&state, request, selection, execution_id)
                    .await
                    .is_err()
                {
                    // execute_chat already records the Failed turn; this log
                    // deliberately names neither the body nor the error text.
                    tracing::warn!(
                        execution_id = %execution_id,
                        "async webhook turn failed"
                    );
                }
            });
            Ok((
                StatusCode::ACCEPTED,
                Json(WebhookAccepted {
                    execution_id: execution_id.to_string(),
                }),
            )
                .into_response())
        }
    }
}
