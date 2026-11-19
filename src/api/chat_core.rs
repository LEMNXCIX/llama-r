use crate::api::handlers::AppState;
use crate::api::route_resolution::{resolve_selection, AgentSelection, ResolvedSelection};
use crate::domain::execution::{AgentExecution, EventKind, ExecutionEvent, ExecutionStatus};
use crate::domain::models::{
    ApprovalInfo, ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, Usage,
};
use crate::error::AppError;
use crate::optimizer::TokenOptimizer;
use crate::ports::delegation::{DelegateRequest, DelegateResult, ToolBudget};
use crate::ports::engine::AgentRunEvent;
use crate::ports::execution::ExecutionStore;
use crate::services::agent_runtime::RuntimeChatRequest;
use crate::services::approval::args_hash_hex;
use crate::services::redaction::redact_trace_payload;
use serde_json::{json, Value};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use tokio_stream::{Stream, StreamExt};
use uuid::Uuid;

pub type AppChatStream = Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, AppError>> + Send>>;

/// The raw stream a `LLMProvider` returns, before its errors are mapped into
/// `AppError`. Named so the fallback's stream type stays readable.
type ProviderChatStream = Pin<
    Box<
        dyn Stream<Item = Result<ChatStreamEvent, Box<dyn std::error::Error + Send + Sync>>> + Send,
    >,
>;

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
    execution_id: Uuid,
    shared_budget: Option<Arc<dyn ToolBudget>>,
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
        execution_id,
        shared_budget,
    })
}

/// The one identity shared by the execution row, its event log and every tool
/// operation in the turn. Created as soon as an agent resolves — before the
/// engine runs — so the turn is observable even if the engine fails and the
/// answer comes from the legacy fallback.
fn new_execution(
    id: Uuid,
    agent_qualified_id: String,
    provider: String,
    model: String,
    conversation_id: Option<String>,
    user_message: &str,
) -> AgentExecution {
    let now = chrono::Utc::now();
    AgentExecution {
        id,
        conversation_id,
        agent_qualified_id,
        provider,
        model,
        status: ExecutionStatus::Running,
        current_step: 0,
        user_message: Some(user_message.to_string()),
        user_message_hash: args_hash_hex(&serde_json::Value::String(user_message.to_string())),
        created_at: now,
        updated_at: now,
        finished_at: None,
        error: None,
    }
}

/// The last user turn, redacted by the store before it reaches disk.
fn turn_user_message(prepared: &ChatRequest) -> String {
    prepared
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

/// Create and register the execution for a resolved agent turn. `None` means
/// no agent resolved (a direct model request has no agent turn to observe).
async fn begin_turn_execution(
    state: &AppState,
    services: &crate::services::provider_stack::ProviderStack,
    engine_ids: Option<&(Option<String>, Option<String>)>,
    conversation_id: Option<&str>,
    prepared: &ChatRequest,
    resume_execution_id: Option<Uuid>,
) -> Option<AgentExecution> {
    let resuming = resume_execution_id.is_some();
    let execution = engine_ids.map(|(project_id, agent_id)| {
        let qualified_id = state
            .agent_registry
            .resolve(project_id.as_deref(), agent_id.as_deref())
            .map(|registered| registered.agent.qualified_id())
            .unwrap_or_else(|| {
                agent_id
                    .clone()
                    .unwrap_or_else(|| project_id.clone().unwrap_or_default())
            });
        new_execution(
            // A retry reuses the original id; that is the whole point — the
            // idempotency guard keys its replay cache on it. A fresh turn mints
            // a new id as before.
            resume_execution_id.unwrap_or_else(Uuid::new_v4),
            qualified_id,
            services.provider_name.clone(),
            prepared.model.clone(),
            conversation_id.map(str::to_string),
            &turn_user_message(prepared),
        )
    });
    if let Some(execution) = &execution {
        if resuming {
            record_resume(state, execution).await;
        } else {
            record_start(state, execution).await;
        }
    }
    execution
}

/// Register the start. A store failure is a warning, never a failed chat: an
/// unavailable store degrades to a no-op exactly like the tool guard does.
async fn record_start(state: &AppState, execution: &AgentExecution) {
    if let Err(err) = state.execution_store.create(execution).await {
        tracing::warn!(error = %err, "could not persist execution start; continuing");
        return;
    }
    let event = ExecutionEvent::new(execution.id, EventKind::Started, json!({}));
    if let Err(err) = state
        .execution_store
        .append_event(execution.id, &event)
        .await
    {
        tracing::warn!(error = %err, "could not append execution Started event");
    }
}

/// Re-open an interrupted/failed turn for a retry. The row already exists (the
/// retry endpoint loaded it), so this must NOT `create` again — a second insert
/// with the same id would conflict. Instead flip it back to `Running` and append
/// an `ExecutionResumed` marker. A store failure is a warning, never a failed
/// chat, exactly like [`record_start`].
async fn record_resume(state: &AppState, execution: &AgentExecution) {
    let store = state.execution_store.as_ref();
    if let Err(err) = store
        .set_status(execution.id, ExecutionStatus::Running, None)
        .await
    {
        tracing::warn!(error = %err, "could not mark execution as running on retry; continuing");
    }
    let event = ExecutionEvent::new(execution.id, EventKind::ExecutionResumed, json!({}));
    if let Err(err) = store.append_event(execution.id, &event).await {
        tracing::warn!(error = %err, "could not append execution ExecutionResumed event");
    }
}

/// Build the structured trace payload for a finished turn. Public so the
/// redaction and cost contract can be exercised directly by integration tests.
/// `cost_usd` comes straight from the response's `Usage` — never fabricated
/// here; a model with no catalog price leaves it `null`.
pub fn trace_payload(execution: &AgentExecution, usage: Option<&Usage>, latency_ms: u64) -> Value {
    let (input_tokens, output_tokens, cost_usd) = match usage {
        Some(usage) => (usage.input_tokens, usage.output_tokens, usage.cost_usd),
        None => (0, 0, None),
    };
    json!({
        "trace_id": execution.id,
        "agent_qualified_id": execution.agent_qualified_id,
        "provider": execution.provider,
        "model": execution.model,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cost_usd": cost_usd,
        "latency_ms": latency_ms,
    })
}

/// Append a trace event for a finished turn, redacting its payload with the
/// shared helper before it reaches the store. Best-effort: a store error is a
/// warning, never a failed chat.
pub async fn record_trace(store: &dyn ExecutionStore, execution: &AgentExecution, payload: &Value) {
    let event = ExecutionEvent::new(
        execution.id,
        EventKind::TraceRecorded,
        redact_trace_payload(payload),
    );
    if let Err(err) = store.append_event(execution.id, &event).await {
        tracing::warn!(error = %err, "could not append execution TraceRecorded event");
    }
}

/// Finalize the turn on every outcome — engine success, engine failure answered
/// by the fallback, or an outright error — so no turn is left `Running`.
async fn finish_execution(
    state: &AppState,
    execution: Option<&AgentExecution>,
    outcome: Result<(), String>,
    usage: Option<&Usage>,
    latency_ms: u64,
) {
    let Some(execution) = execution else {
        return;
    };
    let (status, kind, payload, error) = match &outcome {
        Ok(()) => (
            ExecutionStatus::Completed,
            EventKind::ExecutionCompleted,
            json!({}),
            None,
        ),
        Err(message) => (
            ExecutionStatus::Failed,
            EventKind::ExecutionFailed,
            json!({"error": message}),
            Some(message.as_str()),
        ),
    };
    let store = state.execution_store.as_ref();
    if let Err(err) = store.set_status(execution.id, status, error).await {
        tracing::warn!(error = %err, "could not set execution status");
    }
    let event = ExecutionEvent::new(execution.id, kind, payload);
    if let Err(err) = store.append_event(execution.id, &event).await {
        tracing::warn!(error = %err, "could not append execution terminal event");
    }
    // One trace emission -> one cost observation. Only a real upstream figure
    // is recorded (today `Usage::cost_usd` is `None` without catalog prices),
    // so no cost is ever fabricated from token counts.
    if let Some(cost_usd) = usage.and_then(|usage| usage.cost_usd) {
        state.observability.record_cost(cost_usd);
    }
    record_trace(
        store,
        execution,
        &trace_payload(execution, usage, latency_ms),
    )
    .await;
}

/// Wrap the returned stream so a turn that ends (or errors) still records its
/// terminal status and event. Wrapping at this boundary covers both the engine
/// stream and the legacy stream without either path knowing about the store.
fn finalize_stream(
    state: &AppState,
    execution: Option<AgentExecution>,
    stream: AppChatStream,
    started_at: Instant,
) -> AppChatStream {
    if execution.is_none() {
        return stream;
    }
    let store = state.execution_store.clone();
    Box::pin(async_stream::stream! {
        let mut stream = stream;
        let mut error: Option<String> = None;
        while let Some(item) = stream.next().await {
            if let Err(err) = &item {
                error = Some(err.to_string());
            }
            yield item;
        }
        let execution = execution.unwrap();
        let (status, kind, payload, error_text) = match error {
            Some(message) => (
                ExecutionStatus::Failed,
                EventKind::ExecutionFailed,
                json!({"error": message}),
                Some(message),
            ),
            None => (
                ExecutionStatus::Completed,
                EventKind::ExecutionCompleted,
                json!({}),
                None,
            ),
        };
        let _ = store
            .set_status(execution.id, status, error_text.as_deref())
            .await;
        let event = ExecutionEvent::new(execution.id, kind, payload);
        let _ = store.append_event(execution.id, &event).await;
        record_trace(
            store.as_ref(),
            &execution,
            &trace_payload(&execution, None, started_at.elapsed().as_millis() as u64),
        )
        .await;
    })
}

/// Map engine events to SSE events. Exposed so the approval-event mapping can
/// be tested without standing up a full `AppState`.
pub fn engine_events_to_chat_stream(
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
                        approval: None,
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
                        approval: None,
                    });
                }
                AgentRunEvent::Error { message } => {
                    yield Err(AppError::Runtime(message));
                }
                AgentRunEvent::ApprovalRequired { id, tool_name, args } => {
                    // Signal the pending decision without leaking the raw
                    // arguments into the stream. The client gets the id and
                    // the hash it must echo back with its decision.
                    yield Ok(ChatStreamEvent {
                        model: model.clone(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        message: ChatMessage {
                            role: "assistant".to_string(),
                            content: String::new(),
                        },
                        done: false,
                        approval: Some(ApprovalInfo {
                            id: id.to_string(),
                            tool_name,
                            args_hash: args_hash_hex(&args),
                        }),
                    });
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
                approval: None,
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
        // Snapshot once for both reads below so a hot swap landing mid-request
        // cannot make the system prompt and the model resolution disagree.
        let enricher = state.context_enricher_snapshot();
        let sys_prompt = enricher.build_system_prompt(&agent).await;
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

        let resolved = enricher.resolve(&agent);
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

/// Map an upstream stream's boxed errors into `AppError` at the boundary. Both
/// the primary and the (at most one) fallback stream go through here so a
/// provider error is reported the same way whichever answered.
fn map_provider_stream(stream: ProviderChatStream) -> AppChatStream {
    Box::pin(stream.map(|event| event.map_err(|error| AppError::Provider(error.to_string()))))
}

async fn run_with_fallback(
    state: &AppState,
    services: &crate::services::provider_stack::ProviderStack,
    request: ChatRequest,
    requested_model: &str,
) -> Result<ChatResponse, AppError> {
    // `services` is the request's one snapshot: the primary attempt, its
    // DEFAULT_MODEL retry, the one provider fallback and every field they read
    // use the same stack, even if a hot swap lands mid-call. A settings Apply
    // that changes provider *and* model together cannot pair the old model with
    // the new provider.
    let fallback_model = services.default_model.clone();
    let fallback_model = fallback_model.trim();
    let model_retry_available = !fallback_model.is_empty()
        && requested_model != fallback_model
        && request.model != fallback_model;

    let mut error = match services.provider.chat(request.clone()).await {
        Ok(response) => return Ok(response),
        Err(error) => {
            state.observability.record_provider_error();
            error
        }
    };

    // Rung 1 (pre-existing): retry the SAME provider with DEFAULT_MODEL.
    if model_retry_available {
        tracing::warn!(
            requested_model = %requested_model,
            attempted_model = %request.model,
            fallback_model = %fallback_model,
            error = %error,
            "Primary chat attempt failed; retrying with DEFAULT_MODEL"
        );
        state.observability.record_fallback();
        let mut retry_request = request.clone();
        retry_request.model = fallback_model.to_string();
        match services.provider.chat(retry_request).await {
            Ok(response) => return Ok(response),
            Err(retry_error) => {
                state.observability.record_provider_error();
                error = retry_error;
            }
        }
    }

    // Rung 2 (new): try the ONE next configured profile held by the stack. The
    // bound is structural — a single field, a single attempt — so a chain of
    // broken profiles can never turn one request into an unbounded walk. The
    // profile name is secret-free; it is the only provider identity recorded.
    if let Some(fallback) = &services.fallback_provider {
        tracing::warn!(
            provider = %fallback.name,
            error = %error,
            "Primary provider failed; trying the next configured profile"
        );
        state.observability.record_fallback();
        // Adapters prefer a non-empty `request.model` over their own default,
        // so the primary's model must not leak into the fallback rung: a
        // credentialed fallback with a different catalog would otherwise fail
        // with model-not-found. Send the stack's DEFAULT_MODEL instead.
        let mut fallback_request = request.clone();
        fallback_request.model = fallback_model.to_string();
        match fallback.provider.chat(fallback_request).await {
            Ok(mut response) => {
                tracing::info!(provider = %fallback.name, "Fallback profile answered");
                response.served_by = Some(fallback.name.clone());
                return Ok(response);
            }
            Err(fallback_error) => {
                state.observability.record_provider_error();
                return Err(AppError::Provider(format!(
                    "Fallback also failed: {}",
                    fallback_error
                )));
            }
        }
    }

    Err(AppError::Provider(error.to_string()))
}

async fn run_stream_with_fallback(
    state: &AppState,
    services: &crate::services::provider_stack::ProviderStack,
    request: ChatRequest,
    requested_model: &str,
) -> Result<AppChatStream, AppError> {
    // `services` is the request's one snapshot (see `run_with_fallback`): every
    // attempt reads the same stack, so a hot swap cannot mix providers or models
    // mid-stream. Failover happens only while *opening* a stream — once events
    // have been yielded the provider is fixed for the life of that stream.
    let fallback_model = services.default_model.clone();
    let fallback_model = fallback_model.trim();
    let model_retry_available = !fallback_model.is_empty()
        && requested_model != fallback_model
        && request.model != fallback_model;

    // The non-Ollama adapters reject streaming with a deliberate
    // `AppError::Unavailable` (501) before opening any connection. Preserve
    // that status instead of trying another rung: the feature is unsupported,
    // not a broken upstream. Ordinary errors keep the fallback semantics.
    fn preserve_unavailable(
        error: Box<dyn std::error::Error + Send + Sync>,
    ) -> Result<Box<dyn std::error::Error + Send + Sync>, AppError> {
        match error.downcast::<AppError>() {
            Ok(app_error) if matches!(&*app_error, AppError::Unavailable(_)) => Err(*app_error),
            Ok(app_error) => Ok(app_error),
            Err(error) => Ok(error),
        }
    }

    let mut error = match services.provider.chat_stream(request.clone()).await {
        Ok(stream) => return Ok(map_provider_stream(stream)),
        Err(error) => {
            let error = preserve_unavailable(error)?;
            state.observability.record_provider_error();
            error
        }
    };

    // Rung 1 (pre-existing): the same provider with DEFAULT_MODEL.
    if model_retry_available {
        tracing::warn!(
            requested_model = %requested_model,
            attempted_model = %request.model,
            fallback_model = %fallback_model,
            error = %error,
            "Primary streaming chat attempt failed; retrying with DEFAULT_MODEL"
        );
        state.observability.record_fallback();
        let mut retry_request = request.clone();
        retry_request.model = fallback_model.to_string();
        match services.provider.chat_stream(retry_request).await {
            Ok(stream) => return Ok(map_provider_stream(stream)),
            Err(retry_error) => {
                let retry_error = preserve_unavailable(retry_error)?;
                state.observability.record_provider_error();
                error = retry_error;
            }
        }
    }

    // Rung 2 (new): the ONE next configured profile. Only a failure to *open*
    // the stream reaches here, so this never switches providers mid-stream.
    if let Some(fallback) = &services.fallback_provider {
        tracing::warn!(
            provider = %fallback.name,
            error = %error,
            "Primary provider failed; trying the next configured profile for streaming"
        );
        state.observability.record_fallback();
        // Same as the blocking path: never send the primary's model to the
        // fallback, even when only opening a stream.
        let mut fallback_request = request.clone();
        fallback_request.model = fallback_model.to_string();
        match fallback.provider.chat_stream(fallback_request).await {
            Ok(stream) => {
                tracing::info!(provider = %fallback.name, "Fallback profile answered (streaming)");
                return Ok(map_provider_stream(stream));
            }
            Err(fallback_error) => {
                let fallback_error = preserve_unavailable(fallback_error)?;
                state.observability.record_provider_error();
                return Err(AppError::Provider(format!(
                    "Fallback also failed: {}",
                    fallback_error
                )));
            }
        }
    }

    Err(AppError::Provider(error.to_string()))
}

/// Dispatch a normal chat turn (a fresh execution id is minted when an agent
/// resolves). The retry endpoint calls [`execute_chat_with_id`] with the original
/// id so the operation-id replay cache engages.
pub async fn execute_chat(
    state: &AppState,
    payload: ChatRequest,
    selection: AgentSelection<'_>,
) -> Result<ChatResponse, AppError> {
    execute_chat_with_id(state, payload, selection, None).await
}

/// Same dispatch as [`execute_chat`], but with an explicit execution id to reuse.
/// `Some(id)` is the retry path: it resumes that turn instead of creating a new
/// one, so tool operations recorded under the id are replayed rather than
/// repeated. `None` is an ordinary turn.
pub async fn execute_chat_with_id(
    state: &AppState,
    payload: ChatRequest,
    selection: AgentSelection<'_>,
    resume_execution_id: Option<Uuid>,
) -> Result<ChatResponse, AppError> {
    execute_chat_with_id_and_budget(state, payload, selection, resume_execution_id, None).await
}

/// The delegation-aware core behind [`execute_chat_with_id`]. The extra field is
/// the one tool allowance a whole delegation tree shares: a child turn carries
/// its parent's `Arc<dyn ToolBudget>` here so both bridge kinds in the child
/// spend from it instead of minting a fresh one. Ordinary callers pass `None`
/// and keep the exact behaviour they always had. Kept separate so no existing
/// public signature changes — only this internal entry point knows it.
pub(crate) async fn execute_chat_with_id_and_budget(
    state: &AppState,
    payload: ChatRequest,
    selection: AgentSelection<'_>,
    resume_execution_id: Option<Uuid>,
    shared_budget: Option<Arc<dyn ToolBudget>>,
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

    // Create the execution when the agent resolves, not when the engine starts:
    // a failed engine that falls back to a direct answer is still an observable
    // turn. The store may be a no-op (degraded boot); recording then does
    // nothing, and the id still threads into the engine for tool idempotency.
    let execution = begin_turn_execution(
        state,
        &services,
        engine_ids.as_ref(),
        selection.conversation_id,
        &prepared,
        resume_execution_id,
    )
    .await;

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

    // One id for the whole turn, whether or not a store could persist it.
    let run_id = execution
        .as_ref()
        .map(|e| e.id)
        .unwrap_or_else(Uuid::new_v4);
    // One timer for the whole turn, shared by `record_chat_request` and the
    // trace so the two always report the same latency.
    let started_at = Instant::now();
    let outcome: Result<ChatResponse, AppError> = async {
        // Use agent engine when headers or model resolve an agent and runtime is available.
        if let Some((project_id, agent_id)) = engine_ids {
            if let Some(runtime) = &services.agent_runtime {
                let conversation_id = selection.conversation_id.map(str::to_string);
                let rt_req = build_runtime_request(
                    &services,
                    &prepared,
                    project_id,
                    agent_id,
                    conversation_id,
                    run_id,
                    shared_budget,
                )
                .await?;
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
                            // The engine path has no token accounting surface yet.
                            usage: None,
                            served_by: None,
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
    .await;

    // Finalize on both branches above: an engine failure that the legacy chat
    // answers is still a completed, observable turn.
    finish_execution(
        state,
        execution.as_ref(),
        outcome.as_ref().map(|_| ()).map_err(|e| e.to_string()),
        outcome
            .as_ref()
            .ok()
            .and_then(|response| response.usage.as_ref()),
        started_at.elapsed().as_millis() as u64,
    )
    .await;
    outcome
}

/// Streaming counterpart of [`execute_chat`]. A fresh id is minted per turn.
pub async fn execute_chat_stream(
    state: &AppState,
    payload: ChatRequest,
    selection: AgentSelection<'_>,
) -> Result<AppChatStream, AppError> {
    execute_chat_stream_with_id(state, payload, selection, None).await
}

/// Streaming dispatch with an explicit execution id to reuse (the retry path).
pub async fn execute_chat_stream_with_id(
    state: &AppState,
    mut payload: ChatRequest,
    selection: AgentSelection<'_>,
    resume_execution_id: Option<Uuid>,
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
    let execution = begin_turn_execution(
        state,
        &services,
        engine_ids.as_ref(),
        selection.conversation_id,
        &prepared,
        resume_execution_id,
    )
    .await;
    let run_id = execution
        .as_ref()
        .map(|e| e.id)
        .unwrap_or_else(Uuid::new_v4);
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
                .context_enricher_snapshot()
                .resolve(&registered.agent)
                .capabilities
                .streaming
        })
        .unwrap_or(false);

    let outcome: Result<AppChatStream, AppError> = async {
        if let Some((project_id, agent_id)) = engine_ids.filter(|_| streaming) {
            if let Some(runtime) = &services.agent_runtime {
                let conversation_id = selection.conversation_id.map(str::to_string);
                let rt_req = build_runtime_request(
                    &services,
                    &prepared,
                    project_id,
                    agent_id,
                    conversation_id,
                    run_id,
                    // Streaming delegation is not part of this slice: child
                    // turns are always non-streaming, so no tree budget here.
                    None,
                )
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
    .await;

    match outcome {
        Ok(stream) => Ok(finalize_stream(state, execution, stream, started_at)),
        Err(err) => {
            finish_execution(
                state,
                execution.as_ref(),
                Err(err.to_string()),
                None,
                started_at.elapsed().as_millis() as u64,
            )
            .await;
            Err(err)
        }
    }
}

/// Run one validated child turn and return its answer.
///
/// This is the re-entry point for delegation: a child is an ordinary chat turn
/// with its own `execution_id`, its own manifest/scope and its own system
/// prompt. Only three things are inherited, all of them bounds carried by the
/// request: the shared tool budget, the deadline and the `task_id` used as the
/// child conversation id. Nothing from the parent's prompt or scope is passed.
///
/// A failure is a *named* error for the caller (the delegate tool turns it into
/// a `ToolResult`) — it never escapes as an error of the parent chat turn.
pub async fn delegate_turn(
    state: &AppState,
    req: DelegateRequest,
    agent: crate::domain::agent::Agent,
) -> Result<DelegateResult, String> {
    // Reject an already-expired deadline before doing any work. This is what
    // guarantees "no child turn executed", rather than relying on tokio's
    // timeout poll order (which may drive the future once before noticing).
    if req.deadline <= Instant::now() {
        return Err(format!(
            "delegated turn to '{}' timed out: deadline expired before start",
            agent.qualified_id()
        ));
    }

    let task_id = req
        .task_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let payload = ChatRequest {
        model: agent.config.model.clone(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: req.message.clone(),
        }],
        stream: false,
    };
    // The child is resolved by its own manifest: a target in another project is
    // rejected by validation, so its own project_id is the right scope here.
    // A global target carries `None`, which dispatches outside any project.
    let selection = AgentSelection {
        project_id: agent.project_id.as_deref(),
        agent_id: Some(agent.id.as_str()),
        conversation_id: Some(task_id.as_str()),
        debug: false,
    };

    let fut =
        execute_chat_with_id_and_budget(state, payload, selection, None, Some(req.budget.clone()));
    match tokio::time::timeout_at(tokio::time::Instant::from_std(req.deadline), fut).await {
        Ok(Ok(response)) => Ok(DelegateResult {
            answer: response.message.content,
            task_id,
        }),
        Ok(Err(err)) => Err(format!(
            "delegated turn to '{}' failed: {err}",
            agent.qualified_id()
        )),
        Err(_) => Err(format!(
            "delegated turn to '{}' timed out: deadline exceeded",
            agent.qualified_id()
        )),
    }
}

#[cfg(test)]
mod tests;
