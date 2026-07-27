use crate::api::handlers::AppState;
use crate::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent};
use crate::error::AppError;
use crate::optimizer::TokenOptimizer;
use std::pin::Pin;
use std::time::Instant;
use tokio_stream::{Stream, StreamExt};

pub type AppChatStream = Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, AppError>> + Send>>;

#[derive(Debug, Clone, Copy, Default)]
pub struct AgentSelection<'a> {
    pub project_id: Option<&'a str>,
    pub agent_id: Option<&'a str>,
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
}

fn prepare_request(
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
        let sys_prompt_raw = state.context_enricher.build_system_prompt(&agent);
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
    let had_agent_headers = selection.project_id.is_some() || selection.agent_id.is_some();
    let prepared = prepare_request(state, payload, selection)?;

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

    // Use agent engine when project/agent headers resolve an agent
    if had_agent_headers {
        if let Some(runtime) = &state.agent_runtime {
            let system = prepared
                .messages
                .iter()
                .find(|m| m.role == "system")
                .map(|m| m.content.clone());
            let user_message = prepared
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "user")
                .map(|m| m.content.clone())
                .ok_or_else(|| AppError::Validation("missing user message".into()))?;

            let rt_req = crate::services::agent_runtime::RuntimeChatRequest {
                project_id: selection.project_id.map(str::to_string),
                agent_id: selection.agent_id.map(str::to_string),
                conversation_id: None,
                user_message,
                model_override: Some(prepared.model.clone()),
                system_prompt: system,
                default_model: state.default_model.clone(),
            };

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
                    state.observability.record_chat_request(0);
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
    let prepared = prepare_request(state, payload, selection)?;
    let started_at = Instant::now();
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
            Ok(AgentRunResult {
                text: format!("{}|{}", self.response, req.user_message),
                tool_calls: 0,
                iterations: 1,
                model: req.model,
            })
        }

        async fn run_stream(
            &self,
            req: AgentRunRequest,
        ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
            let (tx, rx) = mpsc::channel(1);
            let result = self.run(req).await?;
            let _ = tx.send(AgentRunEvent::Completed { result }).await;
            Ok(rx)
        }
    }

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap()
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
                debug: false,
            },
        )
        .await
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("Agent 'ghost-agent' not found for project 'ghost-project'"));
    }
}
