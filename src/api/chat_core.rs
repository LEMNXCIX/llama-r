use crate::api::handlers::AppState;
use crate::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent};
use crate::error::AppError;
use crate::optimizer::TokenOptimizer;
use crate::ports::engine::AgentRunEvent;
use crate::services::agent_runtime::RuntimeChatRequest;
use std::pin::Pin;
use std::time::Instant;
use tokio_stream::{Stream, StreamExt};

pub type AppChatStream = Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, AppError>> + Send>>;

#[derive(Debug, Clone, Copy, Default)]
pub struct AgentSelection<'a> {
    pub project_id: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub conversation_id: Option<&'a str>,
    pub debug: bool,
}

impl<'a> AgentSelection<'a> {
    fn requested_target(&self, fallback_model: &str) -> String {
        match (self.project_id, self.agent_id) {
            (Some(project_id), Some(agent_id)) => format!("{}/{}", project_id, agent_id),
            (Some(project_id), None) => format!("{}/{}", project_id, project_id),
            (None, Some(agent_id)) => agent_id.to_string(),
            (None, None) => fallback_model.to_string(),
        }
    }

    fn has_agent_headers(&self) -> bool {
        self.project_id.is_some() || self.agent_id.is_some()
    }
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
/// Engine is used when headers select an agent, or when `model` matches a global agent id.
fn engine_target(
    state: &AppState,
    selection: AgentSelection<'_>,
    original_model: &str,
) -> Option<(Option<String>, Option<String>)> {
    if selection.has_agent_headers() {
        return Some((
            selection.project_id.map(str::to_string),
            selection.agent_id.map(str::to_string),
        ));
    }

    if state.agent_registry.get_agent(original_model).is_some() {
        return Some((None, Some(original_model.to_string())));
    }

    None
}

fn build_runtime_request(
    state: &AppState,
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
        default_model: state.default_model.clone(),
    })
}

fn engine_events_to_chat_stream(
    model: String,
    mut rx: tokio::sync::mpsc::Receiver<AgentRunEvent>,
) -> AppChatStream {
    Box::pin(async_stream::stream! {
        let mut final_text = String::new();
        let mut saw_completed = false;

        while let Some(event) = rx.recv().await {
            match event {
                AgentRunEvent::Token { text } => {
                    final_text = text.clone();
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
                    // If we already streamed the same text as a Token, only emit the done marker.
                    let content = if final_text == result.text && !final_text.is_empty() {
                        String::new()
                    } else {
                        result.text
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

        if !saw_completed && !final_text.is_empty() {
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

async fn prepare_request(
    state: &AppState,
    mut payload: ChatRequest,
    selection: AgentSelection<'_>,
) -> Result<ChatRequest, AppError> {
    if payload.messages.is_empty() {
        return Err(AppError::Validation(
            "Missing required field: 'messages' cannot be empty.".to_string(),
        ));
    }

    let selected_agent = if selection.project_id.is_some() || selection.agent_id.is_some() {
        let agent = state
            .agent_registry
            .resolve_agent(selection.project_id, selection.agent_id);

        if agent.is_none() && selection.project_id.is_some() {
            let target = selection.agent_id.unwrap_or(selection.project_id.unwrap());
            return Err(AppError::Validation(format!(
                "Agent '{}' not found for project '{}'. Ensure the agent exists in 'contextos/projects/{}/agents/'.",
                target, selection.project_id.unwrap(), selection.project_id.unwrap()
            )));
        }
        agent
    } else {
        state.agent_registry.get_agent(&payload.model)
    };

    if let Some(agent) = selected_agent {
        let optimizer = TokenOptimizer::new(agent.config.optimize.clone());
        let mut optimized_messages = Vec::with_capacity(payload.messages.len() + 1);
        let sys_prompt_raw = state.context_enricher.build_system_prompt(&agent).await;
        let sys_prompt = optimizer.optimize(&sys_prompt_raw);
        state
            .metrics
            .record_optimization(sys_prompt_raw.len(), sys_prompt.len());
        optimized_messages.push(ChatMessage {
            role: "system".to_string(),
            content: sys_prompt,
        });

        for mut msg in payload.messages {
            let original_len = msg.content.len();
            msg.content = optimizer.optimize(&msg.content);
            state
                .metrics
                .record_optimization(original_len, msg.content.len());
            optimized_messages.push(msg);
        }

        let resolved_model = state.context_enricher.resolve_model(&agent);
        tracing::info!(
            agent_id = %agent.qualified_id(),
            resolved_model = %resolved_model,
            "Resolved chat request to configured agent"
        );
        payload.model = resolved_model;
        payload.messages = optimized_messages;
    }

    Ok(payload)
}

async fn run_with_fallback(
    state: &AppState,
    mut request: ChatRequest,
    requested_model: &str,
) -> Result<ChatResponse, AppError> {
    match state.provider.chat(request.clone()).await {
        Ok(response) => Ok(response),
        Err(error) => {
            state.observability.record_provider_error();
            let fallback_model = state.default_model.trim();
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
                state
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
    mut request: ChatRequest,
    requested_model: &str,
) -> Result<AppChatStream, AppError> {
    match state.provider.chat_stream(request.clone()).await {
        Ok(stream) => Ok(Box::pin(stream.map(
            |event: Result<ChatStreamEvent, Box<dyn std::error::Error + Send + Sync>>| {
                event.map_err(|error| AppError::Provider(error.to_string()))
            },
        ))),
        Err(error) => {
            state.observability.record_provider_error();
            let fallback_model = state.default_model.trim();
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
                state
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
    let requested_model = selection.requested_target(&payload.model);
    let original_model = payload.model.clone();
    let engine_ids = engine_target(state, selection, &original_model);
    let prepared = prepare_request(state, payload, selection).await?;

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

    // Use agent engine when headers or model resolve an agent and runtime is available.
    if let Some((project_id, agent_id)) = engine_ids {
        if let Some(runtime) = &state.agent_runtime {
            let conversation_id = selection.conversation_id.map(|s| s.to_string());
            let rt_req =
                build_runtime_request(state, &prepared, project_id, agent_id, conversation_id)?;
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
    let mut response = run_with_fallback(state, prepared, &requested_model).await?;
    response.debug_prompt = debug_prompt;

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
    payload.stream = true;
    let requested_model = selection.requested_target(&payload.model);
    let original_model = payload.model.clone();
    let engine_ids = engine_target(state, selection, &original_model);
    let prepared = prepare_request(state, payload, selection).await?;
    let started_at = Instant::now();

    if let Some((project_id, agent_id)) = engine_ids {
        if let Some(runtime) = &state.agent_runtime {
            let conversation_id = selection.conversation_id.map(|s| s.to_string());
            let rt_req =
                build_runtime_request(state, &prepared, project_id, agent_id, conversation_id)?;
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

    let stream = run_stream_with_fallback(state, prepared, &requested_model).await?;
    state
        .observability
        .record_chat_request(started_at.elapsed().as_millis() as u64);
    tracing::info!(requested_model = %requested_model, "Started streaming chat request");
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::{execute_chat, execute_chat_stream, AgentSelection};
    use crate::adapters::mcp::StaticMcpRegistry;
    use crate::api::handlers::AppState;
    use crate::api::observability::AppObservability;
    use crate::context::analyzer::ContextEnricher;
    use crate::context::store::{ContextStore, ProjectContext};
    use crate::domain::agent::{AgentConfig, OptimizeConfig};
    use crate::domain::models::{
        ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo,
    };
    use crate::optimizer::metrics::TokenMetrics;
    use crate::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult};
    use crate::providers::LLMProvider;
    use crate::services::agent_registry::AgentRegistry;
    use crate::services::agent_runtime::AgentRuntime;
    use crate::services::skill_manager::SkillManager;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::error::Error;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, RwLock};
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio_stream::{Stream, StreamExt};

    struct FakeEngine {
        response: String,
        fail: bool,
    }

    #[async_trait]
    impl AgentEngine for FakeEngine {
        async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
            if self.fail {
                return Err("engine down".into());
            }
            let hist_len = req.history.len();
            Ok(AgentRunResult {
                text: format!("{}|{}|h{}", self.response, req.user_message, hist_len),
                tool_calls: 0,
                iterations: 1,
                model: req.model,
            })
        }

        async fn run_stream(
            &self,
            req: AgentRunRequest,
        ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
            let (tx, rx) = mpsc::channel(4);
            let result = self.run(req).await?;
            let _ = tx
                .send(AgentRunEvent::Token {
                    text: result.text.clone(),
                })
                .await;
            let _ = tx.send(AgentRunEvent::Completed { result }).await;
            Ok(rx)
        }
    }

    fn lock_env() -> MutexGuard<'static, ()> {
        crate::core::paths::lock_env_for_tests()
    }

    struct FakeProvider {
        fail_primary_once: AtomicBool,
        fail_always: bool,
    }

    #[async_trait]
    impl LLMProvider for FakeProvider {
        fn get_base_url(&self) -> String {
            "http://fake-provider".to_string()
        }

        async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
            Ok(())
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
            Ok(vec![
                ModelInfo {
                    name: "fallback-model".to_string(),
                    modified_at: "now".to_string(),
                    size: 1,
                },
                ModelInfo {
                    name: "agent-model".to_string(),
                    modified_at: "now".to_string(),
                    size: 1,
                },
            ])
        }

        async fn chat(
            &self,
            request: ChatRequest,
        ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
            if self.fail_always {
                return Err("always fail".into());
            }
            if request.model == "agent-model"
                && self.fail_primary_once.swap(false, Ordering::SeqCst)
            {
                return Err("fail once".into());
            }
            Ok(ChatResponse {
                model: request.model,
                created_at: "now".to_string(),
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: request
                        .messages
                        .last()
                        .map(|msg| msg.content.clone())
                        .unwrap_or_default(),
                },
                done: true,
                debug_prompt: None,
            })
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<
                Box<
                    dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send,
                >,
            >,
            Box<dyn Error + Send + Sync>,
        > {
            if request.model == "agent-model"
                && self.fail_primary_once.swap(false, Ordering::SeqCst)
            {
                return Err("fail once".into());
            }
            Ok(Box::pin(tokio_stream::iter(vec![Ok(ChatStreamEvent {
                model: request.model,
                created_at: "now".to_string(),
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: request
                        .messages
                        .last()
                        .map(|msg| msg.content.clone())
                        .unwrap_or_default(),
                },
                done: true,
            })])))
        }
    }

    fn test_state(provider: Arc<dyn LLMProvider + Send + Sync>) -> (TempDir, Arc<AppState>) {
        test_state_with_runtime(provider, None)
    }

    fn test_state_with_runtime(
        provider: Arc<dyn LLMProvider + Send + Sync>,
        agent_runtime: Option<Arc<AgentRuntime>>,
    ) -> (TempDir, Arc<AppState>) {
        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_var("LLAMA_R_DIR", temp_dir.path());
        let agent_registry = agent_runtime
            .as_ref()
            .map(|rt| rt.registry.clone())
            .unwrap_or_else(|| Arc::new(AgentRegistry::new()));
        let _ = agent_registry.reload_all(&[]);
        let skill_manager = Arc::new(SkillManager::new());
        let context_store = Arc::new(ContextStore::new());
        let context_enricher = Arc::new(ContextEnricher::new(
            context_store.clone(),
            skill_manager.clone(),
            "fallback-model".to_string(),
        ));
        let state = Arc::new(AppState {
            provider,
            agent_registry,
            skill_manager,
            context_store,
            context_enricher,
            metrics: Arc::new(TokenMetrics::new()),
            observability: Arc::new(AppObservability::new()),
            default_model: "fallback-model".to_string(),
            api_running: AtomicBool::new(false),
            grpc_running: AtomicBool::new(false),
            logs: Arc::new(Mutex::new(VecDeque::new())),
            known_mcp_servers: RwLock::new(Vec::new()),
            mcp_registry: Arc::new(StaticMcpRegistry::new()),
            agent_runtime,
            rag_store: None,
            rag_ingest: None,
            history_store: None,
        });
        (temp_dir, state)
    }

    #[tokio::test]
    async fn direct_request_should_succeed_without_agent() {
        let _guard = lock_env();
        let (_dir, state) = test_state(Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        }));
        let response = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                stream: false,
            },
            AgentSelection::default(),
        )
        .await
        .unwrap();
        assert_eq!(response.model, "fallback-model");
    }

    #[tokio::test]
    async fn project_header_without_agent_should_use_project_general_agent() {
        let _guard = lock_env();
        let (_dir, state) = test_state(Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        }));
        state
            .context_store
            .save_context(ProjectContext {
                project_id: "demo".to_string(),
                path: ".".to_string(),
                context_md: "project context".to_string(),
                project_type: "rust".to_string(),
                skills_injected: vec![],
                last_analyzed: chrono::Utc::now(),
                custom_rules: String::new(),
            })
            .unwrap();
        let config = AgentConfig {
            name: "Demo".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "You are helpful".to_string(),
            context_project: Some("demo".to_string()),
            optimize: OptimizeConfig::default(),
            ..Default::default()
        };
        let dir = crate::core::paths::get_project_agents_dir("demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("demo.toml"), toml::to_string(&config).unwrap()).unwrap();
        state.agent_registry.reload_all(&[]).unwrap();

        let response = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "question".to_string(),
                }],
                stream: false,
            },
            AgentSelection {
                project_id: Some("demo"),
                agent_id: None,
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(response.model, "agent-model");
    }

    #[tokio::test]
    async fn project_and_agent_headers_should_resolve_specific_project_agent() {
        let _guard = lock_env();
        let (_dir, state) = test_state(Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        }));
        let config = AgentConfig {
            name: "Reviewer".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "Review this code".to_string(),
            context_project: Some("demo".to_string()),
            optimize: OptimizeConfig::default(),
            ..Default::default()
        };
        let dir = crate::core::paths::get_project_agents_dir("demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("reviewer.toml"), toml::to_string(&config).unwrap()).unwrap();
        state.agent_registry.reload_all(&[]).unwrap();

        let response = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "question".to_string(),
                }],
                stream: false,
            },
            AgentSelection {
                project_id: Some("demo"),
                agent_id: Some("reviewer"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(response.model, "agent-model");
    }

    #[tokio::test]
    async fn project_with_non_existent_agent_should_fail() {
        let _guard = lock_env();
        let (_dir, state) = test_state(Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        }));

        // We don't even need to create the project dir, resolve_agent will return None
        let err = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                stream: false,
            },
            AgentSelection {
                project_id: Some("non-existent-project"),
                agent_id: Some("ghost-agent"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("Agent 'ghost-agent' not found for project 'non-existent-project'"));
    }

    #[tokio::test]
    async fn fallback_failure_should_return_error() {
        let _guard = lock_env();
        let (_dir, state) = test_state(Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: true,
        }));
        let err = execute_chat(
            &state,
            ChatRequest {
                model: "agent-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                stream: false,
            },
            AgentSelection::default(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Provider error"));
    }

    #[tokio::test]
    async fn streaming_request_should_apply_same_resolution_path() {
        let _guard = lock_env();
        let (_dir, state) = test_state(Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        }));
        let mut stream = execute_chat_stream(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                stream: true,
            },
            AgentSelection::default(),
        )
        .await
        .unwrap();
        let event = stream.next().await.unwrap().unwrap();
        assert_eq!(event.model, "fallback-model");
    }

    #[tokio::test]
    async fn agent_runtime_returns_engine_text() {
        let _guard = lock_env();
        let engine = Arc::new(FakeEngine {
            response: "engine-echo".to_string(),
            fail: false,
        });
        let registry = Arc::new(AgentRegistry::new());
        let runtime = Arc::new(AgentRuntime {
            registry: registry.clone(),
            engine,
            history: None,
            rag: None,
        });
        let provider = Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        });
        let (_dir, state) = test_state_with_runtime(provider, Some(runtime));

        // Create a project agent
        use crate::domain::agent::AgentConfig;
        let config = AgentConfig {
            name: "test-agent".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "You are helpful".to_string(),
            context_project: Some("test-proj".to_string()),
            ..Default::default()
        };
        let dir = crate::core::paths::get_project_agents_dir("test-proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("test-agent.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        registry.reload_all(&[]).unwrap();

        let response = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "ping".to_string(),
                }],
                stream: false,
            },
            AgentSelection {
                project_id: Some("test-proj"),
                agent_id: Some("test-agent"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap();
        assert!(response.message.content.contains("engine-echo"));
        assert!(response.message.content.contains("ping"));
    }

    #[tokio::test]
    async fn chat_uses_engine_when_agent_present() {
        let _guard = lock_env();
        // Provider panics if called (engine should handle it)
        let engine = Arc::new(FakeEngine {
            response: "from-engine".to_string(),
            fail: false,
        });
        let registry = Arc::new(AgentRegistry::new());
        let runtime = Arc::new(AgentRuntime {
            registry: registry.clone(),
            engine,
            history: None,
            rag: None,
        });
        let provider = Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: true,
        });
        let (_dir, state) = test_state_with_runtime(provider, Some(runtime));

        let config = AgentConfig {
            name: "engine-agent".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "You are helpful".to_string(),
            context_project: Some("engine-proj".to_string()),
            ..Default::default()
        };
        let dir = crate::core::paths::get_project_agents_dir("engine-proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("engine-agent.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        registry.reload_all(&[]).unwrap();

        let response = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                stream: false,
            },
            AgentSelection {
                project_id: Some("engine-proj"),
                agent_id: Some("engine-agent"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap();
        assert!(response.message.content.contains("from-engine"));
    }

    #[tokio::test]
    async fn chat_falls_back_when_engine_fails() {
        let _guard = lock_env();
        let engine = Arc::new(FakeEngine {
            response: "".to_string(),
            fail: true,
        });
        let registry = Arc::new(AgentRegistry::new());
        let runtime = Arc::new(AgentRuntime {
            registry: registry.clone(),
            engine,
            history: None,
            rag: None,
        });
        let provider = Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        });
        let (_dir, state) = test_state_with_runtime(provider, Some(runtime));

        let config = AgentConfig {
            name: "fallback-agent".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "You are helpful".to_string(),
            context_project: Some("fallback-proj".to_string()),
            ..Default::default()
        };
        let dir = crate::core::paths::get_project_agents_dir("fallback-proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fallback-agent.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        registry.reload_all(&[]).unwrap();

        // Engine fails -> falls back to FakeProvider
        let response = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "fallback-msg".to_string(),
                }],
                stream: false,
            },
            AgentSelection {
                project_id: Some("fallback-proj"),
                agent_id: Some("fallback-agent"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap();
        assert!(response.message.content.contains("fallback-msg"));
    }

    #[tokio::test]
    async fn project_missing_agent_still_400() {
        let _guard = lock_env();
        let (_dir, state) = test_state(Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        }));

        let err = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                stream: false,
            },
            AgentSelection {
                project_id: Some("ghost-project"),
                agent_id: Some("ghost-agent"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("Agent 'ghost-agent' not found for project 'ghost-project'"));
    }

    #[tokio::test]
    async fn engine_receives_prior_history_turns() {
        let _guard = lock_env();
        let engine = Arc::new(FakeEngine {
            response: "hist".to_string(),
            fail: false,
        });
        let registry = Arc::new(AgentRegistry::new());
        let runtime = Arc::new(AgentRuntime {
            registry: registry.clone(),
            engine,
            history: None,
            rag: None,
        });
        let provider = Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: true,
        });
        let (_dir, state) = test_state_with_runtime(provider, Some(runtime));

        let config = AgentConfig {
            name: "hist-agent".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "You are helpful".to_string(),
            context_project: Some("hist-proj".to_string()),
            ..Default::default()
        };
        let dir = crate::core::paths::get_project_agents_dir("hist-proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("hist-agent.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        registry.reload_all(&[]).unwrap();

        let response = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![
                    ChatMessage {
                        role: "user".to_string(),
                        content: "first".to_string(),
                    },
                    ChatMessage {
                        role: "assistant".to_string(),
                        content: "reply".to_string(),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: "second".to_string(),
                    },
                ],
                stream: false,
            },
            AgentSelection {
                project_id: Some("hist-proj"),
                agent_id: Some("hist-agent"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap();
        // FakeEngine encodes history length as hN; two prior turns (user+assistant).
        assert!(
            response.message.content.contains("|h2"),
            "expected history length 2, got {}",
            response.message.content
        );
        assert!(response.message.content.contains("second"));
    }

    #[tokio::test]
    async fn streaming_uses_engine_when_agent_present() {
        let _guard = lock_env();
        let engine = Arc::new(FakeEngine {
            response: "stream-engine".to_string(),
            fail: false,
        });
        let registry = Arc::new(AgentRegistry::new());
        let runtime = Arc::new(AgentRuntime {
            registry: registry.clone(),
            engine,
            history: None,
            rag: None,
        });
        let provider = Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: true,
        });
        let (_dir, state) = test_state_with_runtime(provider, Some(runtime));

        let config = AgentConfig {
            name: "stream-agent".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "You are helpful".to_string(),
            context_project: Some("stream-proj".to_string()),
            ..Default::default()
        };
        let dir = crate::core::paths::get_project_agents_dir("stream-proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("stream-agent.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        registry.reload_all(&[]).unwrap();

        let mut stream = execute_chat_stream(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hi".to_string(),
                }],
                stream: true,
            },
            AgentSelection {
                project_id: Some("stream-proj"),
                agent_id: Some("stream-agent"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap();

        let mut saw_engine = false;
        let mut saw_done = false;
        while let Some(item) = stream.next().await {
            let event = item.unwrap();
            if event.message.content.contains("stream-engine") {
                saw_engine = true;
            }
            if event.done {
                saw_done = true;
            }
        }
        assert!(saw_engine, "stream should include engine text");
        assert!(saw_done, "stream should complete");
    }

    #[tokio::test]
    async fn engine_used_when_model_is_global_agent_id() {
        let _guard = lock_env();
        let engine = Arc::new(FakeEngine {
            response: "by-model".to_string(),
            fail: false,
        });
        let registry = Arc::new(AgentRegistry::new());
        let runtime = Arc::new(AgentRuntime {
            registry: registry.clone(),
            engine,
            history: None,
            rag: None,
        });
        let provider = Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: true,
        });
        let (_dir, state) = test_state_with_runtime(provider, Some(runtime));

        // Global agent (no project) whose id matches the request model field.
        let config = AgentConfig {
            name: "global-bot".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "You are helpful".to_string(),
            ..Default::default()
        };
        let dir = crate::core::paths::get_agents_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("global-bot.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        registry.reload_all(&[]).unwrap();

        let response = execute_chat(
            &state,
            ChatRequest {
                model: "global-bot".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "ping".to_string(),
                }],
                stream: false,
            },
            AgentSelection::default(),
        )
        .await
        .unwrap();
        assert!(response.message.content.contains("by-model"));
    }

    #[test]
    fn split_engine_messages_extracts_history() {
        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: "sys".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "u1".into(),
            },
            ChatMessage {
                role: "assistant".into(),
                content: "a1".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "u2".into(),
            },
        ];
        let (system, history, user) = super::split_engine_messages(&messages).unwrap();
        assert_eq!(system.as_deref(), Some("sys"));
        assert_eq!(user, "u2");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "u1");
        assert_eq!(history[1].content, "a1");
    }
}
