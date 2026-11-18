use crate::api::handlers::AppState;
use crate::api::route_resolution::{resolve_selection, AgentSelection, ResolvedSelection};
use crate::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent};
use crate::error::AppError;
use crate::optimizer::TokenOptimizer;
use crate::ports::engine::AgentRunEvent;
use crate::services::agent_runtime::RuntimeChatRequest;
use std::pin::Pin;
use std::time::Instant;
use tokio_stream::{Stream, StreamExt};

pub type AppChatStream = Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, AppError>> + Send>>;

/// The one description of the agent-engine capability gap, shared by the
/// chat-time warning and the health field so the two can never disagree about
/// what is lost. It names the provider and the lost capabilities, and never a
/// key. A direct-answer fallback must never be silent.
pub fn agent_engine_unavailable_message(provider: &str) -> String {
    format!(
        "no agent engine for the '{provider}' provider: tools, RAG context and MCP are unavailable; agent-scoped chats degrade to a direct chat answer"
    )
}

/// Split prepared messages into system prompt, prior turns, and current user message.
fn split_engine_messages(
    messages: &[ChatMessage],
) -> Result<(Option<String>, Vec<ChatMessage>, String), AppError> {
    let system = messages
        .iter()
        .find(|m| m.role == "system")
        .map(|m| m.content.clone());

    let last_user_idx = messages
        .iter()
        .rposition(|m| m.role == "user")
        .ok_or_else(|| AppError::Validation("missing user message".into()))?;

    let user_message = messages[last_user_idx].content.clone();
    let history: Vec<ChatMessage> = messages
        .iter()
        .enumerate()
        .filter(|(i, m)| *i != last_user_idx && m.role != "system")
        .map(|(_, m)| m.clone())
        .collect();

    Ok((system, history, user_message))
}

/// Decide whether the engine path should run, and which project/agent ids to resolve.
///
/// Engine is used when the resolved selection names a project or an agent —
/// header-pinned or router-chosen, indistinguishable here on purpose — or
/// when `model` matches a global agent id. That first arm is the whole
/// trick: because a routed selection produces the same `(project_id,
/// agent_id)` pair an `X-Agent` header would have, nothing below this
/// function can tell the two apart, which is why routing had to change no
/// code downstream of it.
fn engine_target(
    state: &AppState,
    selection: &ResolvedSelection,
    original_model: &str,
) -> Option<(Option<String>, Option<String>)> {
    if selection.project_id.is_some() || selection.agent_id.is_some() {
        return Some((selection.project_id.clone(), selection.agent_id.clone()));
    }

    if state.agent_registry.get_agent(original_model).is_some() {
        return Some((None, Some(original_model.to_string())));
    }

    None
}

/// Today's "agent not found" complaint, in one place: `prepare_request`
/// says it for a header naming a missing agent, and the ladder's
/// `NoGeneralAgent` says it for a project with no general agent to degrade
/// to. Same words, so a client cannot tell which path produced it — and a
/// project that has no general agent failed exactly like this before
/// routing existed.
pub(crate) fn agent_not_found(target: &str, project_id: &str) -> AppError {
    AppError::Validation(format!(
        "Agent '{}' not found for project '{}'. Ensure the agent exists in 'contextos/projects/{}/agents/'.",
        target, project_id, project_id
    ))
}

/// Build the engine request from the caller's single stack snapshot.
///
/// `services` is the one snapshot the whole chat request took, threaded here
/// rather than re-read: the engine's `default_model` and the provider used if
/// the engine falls back must come from the same stack. A settings Apply that
/// swaps both mid-request must not pair the old model with the new provider.
async fn build_runtime_request(
    services: &crate::services::provider_stack::ProviderStack,
    prepared: &ChatRequest,
    project_id: Option<String>,
    agent_id: Option<String>,
    conversation_id: Option<String>,
) -> Result<RuntimeChatRequest, AppError> {
    let (system, history, user_message) = split_engine_messages(&prepared.messages)?;
    Ok(RuntimeChatRequest {
        project_id,
        agent_id,
        conversation_id,
        user_message,
        history,
        model_override: Some(prepared.model.clone()),
        system_prompt: system,
        default_model: services.default_model.clone(),
    })
}

fn engine_events_to_chat_stream(
    model: String,
    mut rx: tokio::sync::mpsc::Receiver<AgentRunEvent>,
) -> AppChatStream {
    Box::pin(async_stream::stream! {
        // Accumulate every delta. Once tokens have been streamed the full answer
        // has already been delivered, so the `done` marker carries no content —
        // repeating `result.text` there would duplicate the whole response.
        let mut streamed_text = String::new();
        let mut saw_completed = false;

        while let Some(event) = rx.recv().await {
            match event {
                AgentRunEvent::Token { text } => {
                    streamed_text.push_str(&text);
                    yield Ok(ChatStreamEvent {
                        model: model.clone(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        message: ChatMessage {
                            role: "assistant".to_string(),
                            content: text,
                        },
                        done: false,
                    });
                }
                AgentRunEvent::Completed { result } => {
                    saw_completed = true;
                    let content = if streamed_text.is_empty() {
                        result.text
                    } else {
                        String::new()
                    };
                    yield Ok(ChatStreamEvent {
                        model: model.clone(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        message: ChatMessage {
                            role: "assistant".to_string(),
                            content,
                        },
                        done: true,
                    });
                }
                AgentRunEvent::Error { message } => {
                    yield Err(AppError::Runtime(message));
                }
                AgentRunEvent::ToolCall { .. } | AgentRunEvent::ToolResult { .. } => {
                    // Tool progress is not mapped to ChatStreamEvent yet (Nivel-A streaming).
                }
            }
        }

        if !saw_completed && !streamed_text.is_empty() {
            yield Ok(ChatStreamEvent {
                model,
                created_at: chrono::Utc::now().to_rfc3339(),
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: String::new(),
                },
                done: true,
            });
        }
    })
}

/// The one complaint an empty message list gets, checked before resolution so
/// a request that cannot be answered at all never spends a decision.
fn require_messages(payload: &ChatRequest) -> Result<(), AppError> {
    if payload.messages.is_empty() {
        return Err(AppError::Validation(
            "Missing required field: 'messages' cannot be empty.".to_string(),
        ));
    }
    Ok(())
}

/// Prepare a request, reading the engine-presence flag from the caller's single
/// stack snapshot rather than taking another one. See `execute_chat`'s snapshot.
async fn prepare_request(
    state: &AppState,
    services: &crate::services::provider_stack::ProviderStack,
    mut payload: ChatRequest,
    selection: &ResolvedSelection,
) -> Result<ChatRequest, AppError> {
    require_messages(&payload)?;

    // Read as `Option<&str>` so this function cannot tell a header-derived
    // selection from a routed one — it sees the same pair either way. With
    // routing, a project that has a general agent always resolves, so the
    // 400 below only fires for a genuinely agentless project, which is what
    // it always meant.
    let project_id = selection.project_id.as_deref();
    let agent_id = selection.agent_id.as_deref();
    let selected_agent = if project_id.is_some() || agent_id.is_some() {
        let agent = state.agent_registry.resolve_agent(project_id, agent_id);

        if agent.is_none() {
            if let Some(project_id) = project_id {
                let target = agent_id.unwrap_or(project_id);
                return Err(agent_not_found(target, project_id));
            }
        }
        agent
    } else {
        state.agent_registry.get_agent(&payload.model)
    };

    if let Some(agent) = selected_agent {
        let mut prepared_messages = Vec::with_capacity(payload.messages.len() + 1);
        let sys_prompt = state.context_enricher.build_system_prompt(&agent).await;
        prepared_messages.push(ChatMessage {
            role: "system".to_string(),
            content: sys_prompt,
        });
        prepared_messages.append(&mut payload.messages);

        // Only for the requests this module sends itself. When an agent runtime is
        // present the request goes to `AgentRuntime`, which applies the agent's
        // tools on its way to the engine; doing it here as well ran every tool
        // twice, and the second application is the kind of thing that stays
        // invisible until a tool turns out not to be idempotent.
        //
        // The condition is the same field the dispatch below branches on — and
        // both read the one snapshot the request took, so a hot swap cannot make
        // them disagree about which engine is going to run.
        if services.agent_runtime.is_none() {
            let optimizer = TokenOptimizer::new(agent.config.optimize.clone());
            for message in prepared_messages.iter_mut() {
                let original_len = message.content.len();
                message.content = optimizer.optimize(&message.content);
                state
                    .metrics
                    .record_optimization(original_len, message.content.len());
            }
        }

        let optimized_messages = prepared_messages;

        let resolved = state.context_enricher.resolve(&agent);
        tracing::info!(
            agent_id = %agent.qualified_id(),
            resolved_provider = %resolved.provider,
            resolved_model = %resolved.model,
            "Resolved chat request to configured agent"
        );
        payload.model = resolved.model;
        payload.messages = optimized_messages;
    }

    Ok(payload)
}

async fn run_with_fallback(
    state: &AppState,
    services: &crate::services::provider_stack::ProviderStack,
    mut request: ChatRequest,
    requested_model: &str,
) -> Result<ChatResponse, AppError> {
    // `services` is the request's one snapshot: the primary attempt, its
    // fallback and every field they read use the same stack, even if a hot
    // swap lands mid-call. A settings Apply that changes provider *and* model
    // together cannot pair the old model with the new provider.
    let fallback_model = services.default_model.clone();
    match services.provider.chat(request.clone()).await {
        Ok(response) => Ok(response),
        Err(error) => {
            state.observability.record_provider_error();
            let fallback_model = fallback_model.trim();
            if !fallback_model.is_empty()
                && requested_model != fallback_model
                && request.model != fallback_model
            {
                tracing::warn!(
                    requested_model = %requested_model,
                    attempted_model = %request.model,
                    fallback_model = %fallback_model,
                    error = %error,
                    "Primary chat attempt failed; retrying with DEFAULT_MODEL"
                );
                state.observability.record_fallback();
                request.model = fallback_model.to_string();
                services
                    .provider
                    .chat(request)
                    .await
                    .map_err(|fallback_error| {
                        state.observability.record_provider_error();
                        AppError::Provider(format!("Fallback also failed: {}", fallback_error))
                    })
            } else {
                Err(AppError::Provider(error.to_string()))
            }
        }
    }
}

async fn run_stream_with_fallback(
    state: &AppState,
    services: &crate::services::provider_stack::ProviderStack,
    mut request: ChatRequest,
    requested_model: &str,
) -> Result<AppChatStream, AppError> {
    // `services` is the request's one snapshot (see `run_with_fallback`): the
    // primary attempt and its fallback read the same stack, so a hot swap
    // cannot mix the two providers or the two models mid-stream.
    let fallback_model = services.default_model.clone();
    match services.provider.chat_stream(request.clone()).await {
        Ok(stream) => Ok(Box::pin(stream.map(
            |event: Result<ChatStreamEvent, Box<dyn std::error::Error + Send + Sync>>| {
                event.map_err(|error| AppError::Provider(error.to_string()))
            },
        ))),
        Err(error) => {
            // The non-Ollama adapters reject streaming with a deliberate
            // `AppError::Unavailable` (501) before opening any connection.
            // Preserve that status instead of flattening it into a 502 — the
            // feature is unsupported, not a broken upstream. Ordinary errors
            // keep the existing fallback/no-failover semantics unchanged.
            let error: Box<dyn std::error::Error + Send + Sync> = match error.downcast::<AppError>()
            {
                Ok(app_error) if matches!(&*app_error, AppError::Unavailable(_)) => {
                    return Err(*app_error);
                }
                Ok(app_error) => app_error,
                Err(error) => error,
            };
            state.observability.record_provider_error();
            let fallback_model = fallback_model.trim();
            if !fallback_model.is_empty()
                && requested_model != fallback_model
                && request.model != fallback_model
            {
                tracing::warn!(
                    requested_model = %requested_model,
                    attempted_model = %request.model,
                    fallback_model = %fallback_model,
                    error = %error,
                    "Primary streaming chat attempt failed; retrying with DEFAULT_MODEL"
                );
                state.observability.record_fallback();
                request.model = fallback_model.to_string();
                services
                    .provider
                    .chat_stream(request)
                    .await
                    .map(|stream| {
                        Box::pin(stream.map(
                            |event: Result<
                                ChatStreamEvent,
                                Box<dyn std::error::Error + Send + Sync>,
                            >| {
                                event.map_err(|stream_error| {
                                    AppError::Provider(stream_error.to_string())
                                })
                            },
                        )) as AppChatStream
                    })
                    .map_err(|fallback_error| {
                        state.observability.record_provider_error();
                        AppError::Provider(format!("Fallback also failed: {}", fallback_error))
                    })
            } else {
                Err(AppError::Provider(error.to_string()))
            }
        }
    }
}

pub async fn execute_chat(
    state: &AppState,
    payload: ChatRequest,
    selection: AgentSelection<'_>,
) -> Result<ChatResponse, AppError> {
    // ONE snapshot for the whole request. Every provider-dependent field below
    // — the agent runtime, the engine's default model, the chat provider and
    // its fallback model — is read from this single `Arc`. A settings Apply
    // replaces provider *and* model together; without this a request that began
    // before the swap and took several snapshots could straddle it (old model
    // from one snapshot, new provider from the next) and answer incoherently.
    // Taking it before resolution also keeps the router's own snapshot from
    // racing ahead of the stack the answer is produced on.
    let services = state.services_snapshot().await;
    // Before the decision, not after: a request with no messages answers this
    // `400` whatever the router would have said, so resolving first spends a
    // forward pass on an outcome that is already known. Same check, same
    // words, one call earlier — `prepare_request` keeps it because it is the
    // function that owns the field.
    require_messages(&payload)?;
    // Resolve next: from here on the selection is the router's answer or
    // the header's, in one shape, and every step below reads that shape.
    let resolved = resolve_selection(state, selection, &payload.messages).await?;
    let requested_model = resolved.requested_target(&payload.model);
    let original_model = payload.model.clone();
    let engine_ids = engine_target(state, &resolved, &original_model);
    if engine_ids.is_some() && services.agent_runtime.is_none() {
        tracing::warn!(
            "{}",
            agent_engine_unavailable_message(&services.provider_name)
        );
    }
    let prepared = prepare_request(state, &services, payload, &resolved).await?;

    let debug_prompt = if selection.debug {
        Some(
            prepared
                .messages
                .iter()
                .map(|m| format!("[{}] {}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n---\n"),
        )
    } else {
        None
    };
    // The routing decision is debug output too, gated exactly like
    // `debug_prompt` and built beside it: without `X-Debug: true` the field
    // is `None`, so an ordinary caller never receives the project's agent
    // roster and the distribution over it.
    let debug_route = if selection.debug {
        resolved.routed
    } else {
        None
    };

    // Use agent engine when headers or model resolve an agent and runtime is available.
    if let Some((project_id, agent_id)) = engine_ids {
        if let Some(runtime) = &services.agent_runtime {
            let conversation_id = selection.conversation_id.map(|s| s.to_string());
            let rt_req =
                build_runtime_request(&services, &prepared, project_id, agent_id, conversation_id)
                    .await?;
            let started_at = Instant::now();
            match runtime.chat(rt_req).await {
                Ok(text) => {
                    let response = ChatResponse {
                        model: prepared.model.clone(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        message: ChatMessage {
                            role: "assistant".to_string(),
                            content: text,
                        },
                        done: true,
                        debug_prompt,
                        debug_route,
                    };
                    state
                        .observability
                        .record_chat_request(started_at.elapsed().as_millis() as u64);
                    tracing::info!(
                        requested_model = %requested_model,
                        final_model = %response.model,
                        "Completed chat request via agent engine"
                    );
                    return Ok(response);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "agent engine failed; falling back to legacy chat");
                }
            }
        }
    }

    let started_at = Instant::now();
    let mut response = run_with_fallback(state, &services, prepared, &requested_model).await?;
    response.debug_prompt = debug_prompt;
    // The decision rides here too — `None` unless `X-Debug: true` asked for
    // it, same as the engine path above. The streaming path has no field to
    // carry it and gRPC drops it deliberately.
    response.debug_route = debug_route;

    state
        .observability
        .record_chat_request(started_at.elapsed().as_millis() as u64);
    tracing::info!(
        requested_model = %requested_model,
        final_model = %response.model,
        "Completed chat request"
    );
    Ok(response)
}

pub async fn execute_chat_stream(
    state: &AppState,
    mut payload: ChatRequest,
    selection: AgentSelection<'_>,
) -> Result<AppChatStream, AppError> {
    // The one snapshot for this request, for the same reason as `execute_chat`:
    // every field the engine and fallback read comes from this single stack.
    let services = state.services_snapshot().await;
    payload.stream = true;
    // Same ordering as `execute_chat`: the empty-messages `400` precedes the
    // decision for the same reason, and then resolve-first — one selection
    // type feeds `requested_target`, `engine_target` and `prepare_request`
    // here too.
    require_messages(&payload)?;
    let resolved = resolve_selection(state, selection, &payload.messages).await?;
    let requested_model = resolved.requested_target(&payload.model);
    let original_model = payload.model.clone();
    let engine_ids = engine_target(state, &resolved, &original_model);
    if engine_ids.is_some() && services.agent_runtime.is_none() {
        tracing::warn!(
            "{}",
            agent_engine_unavailable_message(&services.provider_name)
        );
    }
    let prepared = prepare_request(state, &services, payload, &resolved).await?;
    let started_at = Instant::now();

    // Streaming is a capability of the *resolved model*, not a property of the
    // globally selected provider name. Ask the same resolver the non-streaming
    // path used: a model whose catalog says `streaming: false` (OpenRouter,
    // OpenCode, an unknown model) must not enter the engine branch — it would
    // either fake a stream or call a client with no streaming model. Those
    // requests fall through to `run_stream_with_fallback`, whose adapters
    // return `AppError::Unavailable` (501) before opening any connection, and
    // whose 501-preserving downcast stays intact.
    let streaming = engine_ids
        .as_ref()
        .and_then(|(project_id, agent_id)| {
            state
                .agent_registry
                .resolve(project_id.as_deref(), agent_id.as_deref())
        })
        .map(|registered| {
            state
                .context_enricher
                .resolve(&registered.agent)
                .capabilities
                .streaming
        })
        .unwrap_or(false);

    if let Some((project_id, agent_id)) = engine_ids.filter(|_| streaming) {
        if let Some(runtime) = &services.agent_runtime {
            let conversation_id = selection.conversation_id.map(|s| s.to_string());
            let rt_req =
                build_runtime_request(&services, &prepared, project_id, agent_id, conversation_id)
                    .await?;
            match runtime.chat_stream(rt_req).await {
                Ok(rx) => {
                    state
                        .observability
                        .record_chat_request(started_at.elapsed().as_millis() as u64);
                    tracing::info!(
                        requested_model = %requested_model,
                        "Started streaming chat request via agent engine"
                    );
                    return Ok(engine_events_to_chat_stream(prepared.model.clone(), rx));
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "agent engine stream failed; falling back to legacy stream"
                    );
                }
            }
        }
    }

    let stream = run_stream_with_fallback(state, &services, prepared, &requested_model).await?;
    state
        .observability
        .record_chat_request(started_at.elapsed().as_millis() as u64);
    tracing::info!(requested_model = %requested_model, "Started streaming chat request");
    Ok(stream)
}

#[cfg(test)]
mod tests;
