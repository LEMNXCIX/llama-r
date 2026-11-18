#![allow(clippy::await_holding_lock)] // env guard held deliberately across .await to serialise tests
use super::{execute_chat, execute_chat_stream};
use crate::adapters::mcp::StaticMcpRegistry;
use crate::api::handlers::AppState;
use crate::api::observability::AppObservability;
use crate::api::route_resolution::AgentSelection;
use crate::config::ProviderKind;
use crate::context::analyzer::ContextEnricher;
use crate::context::store::{ContextStore, ProjectContext};
use crate::domain::agent::{AgentConfig, OptimizeConfig};
use crate::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use crate::error::AppError;
use crate::optimizer::metrics::TokenMetrics;
use crate::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult};
use crate::providers::registry::ProviderRegistry;
use crate::providers::LLMProvider;
use crate::services::agent_registry::AgentRegistry;
use crate::services::agent_runtime::AgentRuntime;
use crate::services::provider_stack::ProviderStack;
use crate::services::router_config::RouterConfig;
use crate::services::skill_manager::SkillManager;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::error::Error;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use tempfile::TempDir;
use tokio::sync::{mpsc, RwLock as TokioRwLock};
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

/// An engine that records whether its streaming entry point was used, so a
/// test can prove the non-Ollama provider gate kept `chat_stream` out of the
/// path. `run_stream` would produce a valid-looking Token+Completed stream if
/// it were called — the pseudo-stream the design forbids for OpenRouter.
struct StreamSpyEngine {
    response: String,
    stream_called: Arc<AtomicBool>,
}

#[async_trait]
impl AgentEngine for StreamSpyEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
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
        self.stream_called.store(true, Ordering::SeqCst);
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

/// The enricher a test-built runtime needs.
///
/// The real thing, not a stub: `AgentRuntime` enriches whenever the caller
/// brings no system prompt of its own, and a fake here would have kept these
/// tests green while the production runtime quietly dropped the agent's rules.
fn test_enricher() -> Arc<crate::context::analyzer::ContextEnricher> {
    Arc::new(crate::context::analyzer::ContextEnricher::new(
        Arc::new(crate::context::store::ContextStore::new()),
        Arc::new(crate::services::skill_manager::SkillManager::new()),
        "test-model".to_string(),
        Arc::new(ProviderRegistry::default()),
        ProviderKind::Ollama,
    ))
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
        if request.model == "agent-model" && self.fail_primary_once.swap(false, Ordering::SeqCst) {
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
            debug_route: None,
        })
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        if request.model == "agent-model" && self.fail_primary_once.swap(false, Ordering::SeqCst) {
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
    test_state_with_runtime_and_provider(provider, agent_runtime, "ollama")
}

fn test_state_with_runtime_and_provider(
    provider: Arc<dyn LLMProvider + Send + Sync>,
    agent_runtime: Option<Arc<AgentRuntime>>,
    provider_name: &str,
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
    let global_provider = provider_name.parse::<ProviderKind>().unwrap_or_default();
    let context_enricher = Arc::new(ContextEnricher::new(
        context_store.clone(),
        skill_manager.clone(),
        "fallback-model".to_string(),
        Arc::new(ProviderRegistry::default()),
        global_provider,
    ));
    let mut stack = ProviderStack::new(provider, "fallback-model");
    stack.provider_name = provider_name.to_string();
    stack.agent_runtime = agent_runtime;
    // No test here consults the router; the routing ladder is injected into the
    // stack by the tests that do.
    let state = Arc::new(AppState {
        services: Arc::new(TokioRwLock::new(Arc::new(stack))),
        agent_registry,
        skill_manager,
        context_store,
        context_enricher,
        metrics: Arc::new(TokenMetrics::new()),
        observability: Arc::new(AppObservability::new()),
        api_running: AtomicBool::new(false),
        grpc_running: AtomicBool::new(false),
        logs: Arc::new(Mutex::new(VecDeque::new())),
        known_mcp_servers: RwLock::new(Vec::new()),
        mcp_registry: Arc::new(StaticMcpRegistry::new()),
        history_store: None,
        router_config: Arc::new(RouterConfig::default()),
        router_healthy: AtomicBool::new(false),
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
        enricher: test_enricher(),
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
        enricher: test_enricher(),
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
        enricher: test_enricher(),
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
        enricher: test_enricher(),
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
        enricher: test_enricher(),
    });
    let provider = Arc::new(FakeProvider {
        fail_primary_once: AtomicBool::new(false),
        fail_always: true,
    });
    let (_dir, state) = test_state_with_runtime(provider, Some(runtime));

    // A model the capability catalog marks as streaming: the new gate reads
    // resolved capabilities, so this agent may enter the engine stream path.
    let config = AgentConfig {
        name: "stream-agent".to_string(),
        model: "llama3.2".to_string(),
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
        enricher: test_enricher(),
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

/// A provider that rejects streaming the way the non-Ollama adapters do:
/// a deliberate `AppError::Unavailable`, decided before any I/O.
struct UnavailableStreamProvider;

#[async_trait]
impl LLMProvider for UnavailableStreamProvider {
    fn get_base_url(&self) -> String {
        "http://unused".to_string()
    }

    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(Vec::new())
    }

    async fn chat(
        &self,
        _request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        unreachable!("this test only streams")
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        Err(Box::new(AppError::Unavailable(
            "streaming is not supported by openrouter".to_string(),
        )))
    }
}

#[tokio::test]
async fn an_unavailable_stream_error_stays_a_501_not_a_502() {
    let _guard = lock_env();
    let (_dir, state) = test_state(Arc::new(UnavailableStreamProvider));
    let err = execute_chat_stream(
        &state,
        ChatRequest {
            model: "fallback-model".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            stream: true,
        },
        AgentSelection::default(),
    )
    .await
    .err()
    .expect("streaming must be rejected");

    assert!(
        matches!(err, AppError::Unavailable(_)),
        "a deliberate Unavailable must not be wrapped into Provider (502): {err:?}"
    );
    assert_eq!(err.status_code(), axum::http::StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn non_streaming_model_stream_is_rejected_with_501_without_calling_engine() {
    let _guard = lock_env();
    let stream_called = Arc::new(AtomicBool::new(false));
    let engine = Arc::new(StreamSpyEngine {
        response: "or-spy".to_string(),
        stream_called: stream_called.clone(),
    });
    let registry = Arc::new(AgentRegistry::new());
    let runtime = Arc::new(AgentRuntime {
        registry: registry.clone(),
        engine,
        history: None,
        rag: None,
        enricher: test_enricher(),
    });
    // The agent resolves against OpenRouter (whose catalog says streaming is
    // false), and the provider's chat_stream rejects streaming the way the real
    // OpenRouter/OpenCode adapters do. The capability gate must skip the engine
    // and let that 501 through.
    let (_dir, state) = test_state_with_runtime_and_provider(
        Arc::new(UnavailableStreamProvider),
        Some(runtime),
        "openrouter",
    );

    let config = AgentConfig {
        name: "or-agent".to_string(),
        model: "agent-model".to_string(),
        system_prompt: "You are helpful".to_string(),
        context_project: Some("or-proj".to_string()),
        ..Default::default()
    };
    let dir = crate::core::paths::get_project_agents_dir("or-proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("or-agent.toml"), toml::to_string(&config).unwrap()).unwrap();
    registry.reload_all(&[]).unwrap();

    let err = execute_chat_stream(
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
            project_id: Some("or-proj"),
            agent_id: Some("or-agent"),
            conversation_id: None,
            debug: false,
        },
    )
    .await
    .err()
    .expect("a non-Ollama provider must reject streaming with Unavailable");

    assert!(
        matches!(err, AppError::Unavailable(_)),
        "an engine pseudo-stream must not turn a 501 into a 200: {err:?}"
    );
    assert_eq!(err.status_code(), axum::http::StatusCode::NOT_IMPLEMENTED);
    assert!(
        !stream_called.load(Ordering::SeqCst),
        "the engine's chat_stream must not be called for a non-streaming model"
    );
}

#[tokio::test]
async fn ollama_engine_stream_is_still_taken() {
    let _guard = lock_env();
    let stream_called = Arc::new(AtomicBool::new(false));
    let engine = Arc::new(StreamSpyEngine {
        response: "ollama-spy".to_string(),
        stream_called: stream_called.clone(),
    });
    let registry = Arc::new(AgentRegistry::new());
    let runtime = Arc::new(AgentRuntime {
        registry: registry.clone(),
        engine,
        history: None,
        rag: None,
        enricher: test_enricher(),
    });
    let (_dir, state) = test_state_with_runtime_and_provider(
        Arc::new(UnavailableStreamProvider),
        Some(runtime),
        "ollama",
    );

    // A streaming-capable resolved model (Ollama/llama3.2) must keep the
    // engine stream path under the capability gate.
    let config = AgentConfig {
        name: "ol-agent".to_string(),
        model: "llama3.2".to_string(),
        system_prompt: "You are helpful".to_string(),
        context_project: Some("ol-proj".to_string()),
        ..Default::default()
    };
    let dir = crate::core::paths::get_project_agents_dir("ol-proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ol-agent.toml"), toml::to_string(&config).unwrap()).unwrap();
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
            project_id: Some("ol-proj"),
            agent_id: Some("ol-agent"),
            conversation_id: None,
            debug: false,
        },
    )
    .await
    .expect("an Ollama provider must keep the engine streaming path");

    let mut saw_engine = false;
    while let Some(item) = stream.next().await {
        if item.unwrap().message.content.contains("ollama-spy") {
            saw_engine = true;
        }
    }
    assert!(saw_engine, "Ollama stream should contain engine text");
    assert!(
        stream_called.load(Ordering::SeqCst),
        "the engine's chat_stream must be called for a streaming-capable model"
    );
}

/// A provider that identifies itself by the content it answers, so a test can
/// tell which stack served a fallback without inspecting URLs.
struct TagProvider(&'static str);

#[async_trait]
impl LLMProvider for TagProvider {
    fn get_base_url(&self) -> String {
        format!("http://provider-{}", self.0)
    }
    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
    async fn chat(
        &self,
        _request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        Ok(ChatResponse {
            model: "whatever".into(),
            created_at: "now".into(),
            message: ChatMessage {
                role: "assistant".into(),
                content: self.0.into(),
            },
            done: true,
            debug_prompt: None,
            debug_route: None,
        })
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        Err("not used".into())
    }
}

/// An engine that installs a replacement stack the first time it runs, then
/// fails so dispatch falls back. The slot is filled after `AppState` exists, so
/// the engine need not hold a strong reference to the state that owns it.
struct SwapOnRunEngine {
    state_slot: Arc<Mutex<Option<Arc<AppState>>>>,
    replacement: Arc<Mutex<Option<ProviderStack>>>,
}

#[async_trait]
impl AgentEngine for SwapOnRunEngine {
    async fn run(&self, _req: AgentRunRequest) -> Result<AgentRunResult, String> {
        let (state, stack) = {
            let state = self.state_slot.lock().unwrap().clone();
            let stack = self.replacement.lock().unwrap().take();
            (state, stack)
        };
        if let (Some(state), Some(stack)) = (state, stack) {
            // A settings Apply landing mid-request: provider AND model change
            // together, while this one chat is still in flight.
            state.replace_services(stack).await;
        }
        Err("engine failed on purpose".into())
    }
    async fn run_stream(
        &self,
        _req: AgentRunRequest,
    ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
        Err("engine failed on purpose".into())
    }
}

/// A settings Apply mid-request cannot change the provider/model pair a single
/// chat uses. The engine installs stack B before failing; the fallback must
/// still answer from stack A, the snapshot the request took at entry. Without
/// the one-snapshot discipline the fallback would take a fresh snapshot and
/// answer with provider B — the old model from one snapshot, the new provider
/// from the next, exactly the incoherence Task 7's hot swap can cause.
#[tokio::test]
async fn a_mid_request_swap_cannot_change_the_fallback_stack() {
    let _guard = lock_env();
    let state_slot: Arc<Mutex<Option<Arc<AppState>>>> = Arc::new(Mutex::new(None));
    let replacement: Arc<Mutex<Option<ProviderStack>>> = Arc::new(Mutex::new(None));
    let engine = Arc::new(SwapOnRunEngine {
        state_slot: state_slot.clone(),
        replacement: replacement.clone(),
    });
    let registry = Arc::new(AgentRegistry::new());
    let runtime = Arc::new(AgentRuntime {
        registry: registry.clone(),
        engine,
        history: None,
        rag: None,
        enricher: test_enricher(),
    });
    let (_dir, state) = test_state_with_runtime(Arc::new(TagProvider("A")), Some(runtime));

    // The request's stack: provider A, default model "model-a".
    {
        let mut stack = (*state.services_snapshot().await).clone();
        stack.default_model = "model-a".into();
        state.replace_services(stack).await;
    }
    *state_slot.lock().unwrap() = Some(state.clone());
    *replacement.lock().unwrap() = Some(ProviderStack::new(Arc::new(TagProvider("B")), "model-b"));

    // An agent makes dispatch take the engine path; its model differs from the
    // stack's default so the engine failure triggers the legacy fallback.
    let config = AgentConfig {
        name: "swap-agent".into(),
        model: "agent-model".into(),
        system_prompt: "You are helpful".into(),
        context_project: Some("swap-proj".into()),
        ..Default::default()
    };
    let dir = crate::core::paths::get_project_agents_dir("swap-proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("swap-agent.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    registry.reload_all(&[]).unwrap();

    let response = execute_chat(
        &state,
        ChatRequest {
            model: "fallback-model".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "question".into(),
            }],
            stream: false,
        },
        AgentSelection {
            project_id: Some("swap-proj"),
            agent_id: Some("swap-agent"),
            conversation_id: None,
            debug: false,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        response.message.content, "A",
        "the fallback must use the stack the request snapshotted at entry, not the one \
         a mid-request Apply installed: {response:?}"
    );
    // The swap did take effect for later requests — it just cannot reach back
    // into one already in flight.
    let after = state.services_snapshot().await;
    assert_eq!(after.default_model, "model-b");
    assert_eq!(after.provider.get_base_url(), "http://provider-B");
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
