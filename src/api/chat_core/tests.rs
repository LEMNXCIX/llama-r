#![allow(clippy::await_holding_lock)] // env guard held deliberately across .await to serialise tests
use super::{execute_chat, execute_chat_stream};
use crate::adapters::mcp::StaticMcpRegistry;
use crate::api::handlers::AppState;
use crate::api::observability::AppObservability;
use crate::api::route_resolution::AgentSelection;
use crate::context::analyzer::ContextEnricher;
use crate::context::store::{ContextStore, ProjectContext};
use crate::domain::agent::{AgentConfig, OptimizeConfig};
use crate::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use crate::optimizer::metrics::TokenMetrics;
use crate::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult};
use crate::providers::LLMProvider;
use crate::services::agent_registry::AgentRegistry;
use crate::services::agent_runtime::AgentRuntime;
use crate::services::router_config::RouterConfig;
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
        // No test here consults the router; the routing ladder is injected
        // through these fields by the tests that do.
        router: RwLock::new(None),
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
