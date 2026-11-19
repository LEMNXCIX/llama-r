//! Task 2: bounded validation for subagent delegation.
//!
//! These tests drive the pure `validate_delegation` function against an
//! in-memory `AgentRegistry` fixture. They pin each rejection branch to a
//! named error and confirm that an allowed target resolves to its `Agent`.

use llama_r::adapters::rig_engine::limits::ToolCallBudget;
use llama_r::domain::agent::{Agent, AgentConfig, RagWritePolicy, SubagentsConfig};
use llama_r::domain::policy::PermissionPolicy;
use llama_r::domain::scope::{AgentScope, ToolsAllow};
use llama_r::ports::delegation::{DelegateRequest, ToolBudget};
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::delegation::validate_delegation;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

fn scope(agent_id: &str, project: Option<&str>) -> AgentScope {
    AgentScope {
        agent_id: agent_id.into(),
        project_id: project.map(str::to_string),
        mcp_sources: HashSet::new(),
        tools_allow: ToolsAllow::DenyAll,
        local_tools_allow: ToolsAllow::DenyAll,
        rag_sources: HashSet::new(),
        rag_write: RagWritePolicy::None,
        max_tool_calls: 8,
        max_iterations: 6,
        timeout_secs: 90,
        permission: PermissionPolicy::default(),
    }
}

fn agent(id: &str, project: Option<&str>, subagents: Option<SubagentsConfig>) -> Agent {
    let config = AgentConfig {
        name: id.to_string(),
        subagents,
        ..Default::default()
    };
    Agent {
        id: id.into(),
        project_id: project.map(str::to_string),
        config,
    }
}

/// A "fake registry" fixture: a real registry populated with in-memory agents.
fn registry_with(agents: Vec<Agent>) -> AgentRegistry {
    let registry = AgentRegistry::new();
    for a in agents {
        let s = scope(&a.id, a.project_id.as_deref());
        registry.register(a, s);
    }
    registry
}

fn budget() -> Arc<dyn ToolBudget> {
    Arc::new(Mutex::new(ToolCallBudget::new(8)))
}

fn request(target: &str, caller: &str, project: Option<&str>) -> DelegateRequest {
    request_with(target, caller, project, Vec::new(), 0, 1)
}

fn request_with(
    target: &str,
    caller: &str,
    project: Option<&str>,
    chain: Vec<String>,
    depth: u32,
    effective_max_depth: u32,
) -> DelegateRequest {
    DelegateRequest {
        agent_id: target.into(),
        caller_agent_id: caller.into(),
        project_id: project.map(str::to_string),
        message: "do the thing".into(),
        task_id: None,
        chain,
        depth,
        effective_max_depth,
        deadline: Instant::now() + std::time::Duration::from_secs(30),
        budget: budget(),
        event_tx: None,
    }
}

#[test]
fn allowed_id_resolves() {
    let caller = agent(
        "parent",
        Some("proj"),
        Some(SubagentsConfig {
            allowed: vec!["child".into()],
            ..Default::default()
        }),
    );
    let child = agent("child", Some("proj"), None);
    let registry = registry_with(vec![caller, child]);

    let resolved = validate_delegation(&registry, &request("child", "parent", Some("proj")))
        .expect("allowed same-project agent must resolve");

    assert_eq!(resolved.id, "child");
    assert_eq!(resolved.qualified_id(), "proj/child");
}

#[test]
fn id_outside_allowlist_rejected() {
    let caller = agent(
        "parent",
        Some("proj"),
        Some(SubagentsConfig {
            allowed: vec!["child".into()],
            ..Default::default()
        }),
    );
    let child = agent("child", Some("proj"), None);
    let other = agent("outsider", Some("proj"), None);
    let registry = registry_with(vec![caller, child, other]);

    let err = validate_delegation(&registry, &request("outsider", "parent", Some("proj")))
        .expect_err("id not in the allowlist must be rejected");

    assert!(err.contains("outsider"), "error must name the agent: {err}");
    assert!(
        err.contains("allowlist"),
        "error must name the reason: {err}"
    );
}

#[test]
fn other_project_rejected() {
    let caller = agent(
        "parent",
        Some("proj"),
        Some(SubagentsConfig {
            allowed: vec!["elsewhere".into()],
            ..Default::default()
        }),
    );
    let elsewhere = agent("elsewhere", Some("other"), None);
    let registry = registry_with(vec![caller, elsewhere]);

    let err = validate_delegation(&registry, &request("elsewhere", "parent", Some("proj")))
        .expect_err("an agent in another project must be rejected");

    assert!(
        err.contains("elsewhere"),
        "error must name the agent: {err}"
    );
    assert!(err.contains("project"), "error must name the reason: {err}");
}

#[test]
fn other_project_rejected_even_with_allow_global() {
    // `allow_global` opens the door to global agents only. An agent that lives
    // in a different project is never reachable, even with the flag set.
    let caller = agent(
        "parent",
        Some("proj"),
        Some(SubagentsConfig {
            allowed: vec!["elsewhere".into()],
            allow_global: true,
            ..Default::default()
        }),
    );
    let elsewhere = agent("elsewhere", Some("other"), None);
    let registry = registry_with(vec![caller, elsewhere]);

    let err = validate_delegation(&registry, &request("elsewhere", "parent", Some("proj")))
        .expect_err("another project's agent must stay rejected even with allow_global");

    assert!(
        err.contains("cross-project"),
        "error must name the cross-project reason: {err}"
    );
    assert!(
        !err.contains("allow_global"),
        "must not be mistaken for a global agent: {err}"
    );
}

#[test]
fn global_without_allow_global_rejected() {
    let caller = agent(
        "parent",
        Some("proj"),
        Some(SubagentsConfig {
            allowed: vec!["global_helper".into()],
            allow_global: false,
            ..Default::default()
        }),
    );
    let global_helper = agent("global_helper", None, None);
    let registry = registry_with(vec![caller, global_helper]);

    let err = validate_delegation(&registry, &request("global_helper", "parent", Some("proj")))
        .expect_err("a global id must be rejected without allow_global");

    assert!(
        err.contains("allow_global"),
        "error must name the reason: {err}"
    );
    assert!(
        err.contains("global_helper"),
        "error must name the agent: {err}"
    );
}

#[test]
fn global_with_allow_global_resolves() {
    let caller = agent(
        "parent",
        Some("proj"),
        Some(SubagentsConfig {
            allowed: vec!["global_helper".into()],
            allow_global: true,
            ..Default::default()
        }),
    );
    let global_helper = agent("global_helper", None, None);
    let registry = registry_with(vec![caller, global_helper]);

    let resolved =
        validate_delegation(&registry, &request("global_helper", "parent", Some("proj")))
            .expect("a global id must resolve when allow_global is set");

    assert_eq!(resolved.id, "global_helper");
    assert_eq!(resolved.qualified_id(), "global_helper");
}

#[test]
fn depth_exceeded_rejected() {
    let caller = agent(
        "parent",
        Some("proj"),
        Some(SubagentsConfig {
            allowed: vec!["child".into()],
            ..Default::default()
        }),
    );
    let child = agent("child", Some("proj"), None);
    let registry = registry_with(vec![caller, child]);

    let req = request_with("child", "parent", Some("proj"), Vec::new(), 2, 1);
    let err = validate_delegation(&registry, &req)
        .expect_err("a depth above the effective maximum must be rejected");

    assert!(err.contains("depth"), "error must name the reason: {err}");
}

#[test]
fn chain_cycle_rejected_naming_id() {
    let caller = agent(
        "parent",
        Some("proj"),
        Some(SubagentsConfig {
            allowed: vec!["child".into()],
            ..Default::default()
        }),
    );
    let child = agent("child", Some("proj"), None);
    let registry = registry_with(vec![caller, child]);

    let req = request_with(
        "child",
        "parent",
        Some("proj"),
        vec!["proj/child".into()],
        0,
        1,
    );
    let err = validate_delegation(&registry, &req)
        .expect_err("a target already in the chain must be rejected");

    assert!(err.contains("child"), "error must name the agent: {err}");
    assert!(err.contains("cycle"), "error must name the reason: {err}");
}

// ---------------------------------------------------------------------------
// Task 3: RuntimeDelegator, child-turn re-entry and one shared budget.
// ---------------------------------------------------------------------------

use async_trait::async_trait;
use llama_r::adapters::execution::NoOpExecutionStore;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::chat_core::execute_chat;
use llama_r::api::handlers::AppState;
use llama_r::api::observability::AppObservability;
use llama_r::api::route_resolution::AgentSelection;
use llama_r::config::ProviderKind;
use llama_r::context::analyzer::ContextEnricher;
use llama_r::context::store::ContextStore;
use llama_r::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use llama_r::optimizer::metrics::TokenMetrics;
use llama_r::ports::delegation::Delegator;
use llama_r::ports::engine::{AgentEngine, AgentRunEvent, AgentRunResult};
use llama_r::providers::registry::ProviderRegistry;
use llama_r::providers::LLMProvider;
use llama_r::services::agent_runtime::AgentRuntime;
use llama_r::services::approval::ApprovalGate;
use llama_r::services::delegation::RuntimeDelegator;
use llama_r::services::provider_stack::ProviderStack;
use llama_r::services::router_config::RouterConfig;
use llama_r::services::scope_builder::ScopeBuilder;
use llama_r::services::skill_manager::SkillManager;
use std::collections::VecDeque;
use std::error::Error;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex as StdMutex, OnceLock, RwLock};
use tokio::sync::RwLock as TokioRwLock;
use tokio_stream::Stream;
use uuid::Uuid;

/// Every engine request the child turn produced, in order. Cloned handles let
/// the test inspect what the delegated turn actually carried after `delegate`
/// returned.
#[derive(Clone, Default)]
struct RecordedRuns(Arc<StdMutex<Vec<llama_r::ports::engine::AgentRunRequest>>>);

struct RecordingEngine {
    runs: RecordedRuns,
    answer: String,
    /// When set, only a run whose user message equals this string fails. Lets
    /// one state demonstrate a failing child *and* a still-working parent.
    fail_on: Option<String>,
    /// Spend one unit from the request's shared budget, standing in for the
    /// tool calls a real child would make.
    spend_budget: bool,
}

#[async_trait]
impl AgentEngine for RecordingEngine {
    async fn run(
        &self,
        req: llama_r::ports::engine::AgentRunRequest,
    ) -> Result<AgentRunResult, String> {
        if self.spend_budget {
            let budget = req
                .shared_budget
                .as_ref()
                .ok_or_else(|| "child turn did not receive a shared budget".to_string())?;
            budget.try_consume().await?;
        }
        self.runs.0.lock().unwrap().push(req.clone());
        if self.fail_on.as_deref() == Some(req.user_message.as_str()) {
            return Err("child engine exploded".into());
        }
        Ok(AgentRunResult {
            text: self.answer.clone(),
            tool_calls: 0,
            iterations: 1,
            model: req.model,
        })
    }

    async fn run_stream(
        &self,
        req: llama_r::ports::engine::AgentRunRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<AgentRunEvent>, String> {
        let result = self.run(req).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let _ = tx.send(AgentRunEvent::Completed { result }).await;
        Ok(rx)
    }
}

struct FakeProvider {
    fail: bool,
}

#[async_trait]
impl LLMProvider for FakeProvider {
    fn get_base_url(&self) -> String {
        "http://fake-provider".into()
    }

    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(vec![ModelInfo {
            name: "fallback-model".into(),
            modified_at: "now".into(),
            size: 1,
        }])
    }

    async fn chat(
        &self,
        request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        if self.fail {
            return Err("provider down".into());
        }
        Ok(ChatResponse {
            model: request.model,
            created_at: "now".into(),
            message: ChatMessage {
                role: "assistant".into(),
                content: request
                    .messages
                    .last()
                    .map(|m| m.content.clone())
                    .unwrap_or_default(),
            },
            done: true,
            usage: None,
            served_by: None,
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
        if self.fail {
            return Err("provider down".into());
        }
        Ok(Box::pin(tokio_stream::iter(vec![Ok(ChatStreamEvent {
            model: request.model,
            created_at: "now".into(),
            message: ChatMessage {
                role: "assistant".into(),
                content: request
                    .messages
                    .last()
                    .map(|m| m.content.clone())
                    .unwrap_or_default(),
            },
            done: true,
            approval: None,
        })])))
    }
}

/// The `ChatRequest`s a fallback provider saw. `stream` lives on this type, not
/// on `AgentRunRequest`, so a provider is the only place a delegated turn's
/// `stream` flag is observable without changing production code.
#[derive(Clone, Default)]
struct SeenChatRequests(Arc<StdMutex<Vec<ChatRequest>>>);

struct RecordingProvider {
    seen: SeenChatRequests,
}

#[async_trait]
impl LLMProvider for RecordingProvider {
    fn get_base_url(&self) -> String {
        "http://fake-provider".into()
    }

    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }

    async fn chat(
        &self,
        request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        self.seen.0.lock().unwrap().push(request.clone());
        Ok(ChatResponse {
            model: request.model,
            created_at: "now".into(),
            message: ChatMessage {
                role: "assistant".into(),
                content: "FALLBACK_ANSWER".into(),
            },
            done: true,
            usage: None,
            served_by: None,
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
        Err("a child turn must never stream".into())
    }
}

fn register_real(registry: &AgentRegistry, agent: Agent) {
    let scope = ScopeBuilder::build(&agent).expect("manifest builds a scope");
    registry.register(agent, scope);
}

fn test_state(
    registry: Arc<AgentRegistry>,
    engine: Arc<dyn AgentEngine>,
    provider: Arc<dyn LLMProvider + Send + Sync>,
) -> Arc<AppState> {
    let skill_manager = Arc::new(SkillManager::new());
    let context_store = Arc::new(ContextStore::new());
    let enricher = Arc::new(ContextEnricher::new(
        context_store.clone(),
        skill_manager.clone(),
        "test-model".into(),
        Arc::new(ProviderRegistry::default()),
        ProviderKind::Ollama,
    ));
    let mut stack = ProviderStack::new(provider, "fallback-model");
    stack.agent_runtime = Some(Arc::new(AgentRuntime {
        registry: registry.clone(),
        engine,
        history: None,
        rag: None,
        enricher: enricher.clone(),
    }));
    Arc::new(AppState {
        services: Arc::new(TokioRwLock::new(Arc::new(stack))),
        agent_registry: registry,
        skill_manager,
        context_store,
        context_enricher: StdMutex::new(enricher),
        metrics: Arc::new(TokenMetrics::new()),
        observability: Arc::new(AppObservability::new()),
        api_running: AtomicBool::new(false),
        grpc_running: AtomicBool::new(false),
        logs: Arc::new(StdMutex::new(VecDeque::new())),
        known_mcp_servers: RwLock::new(Vec::new()),
        mcp_registry: Arc::new(StaticMcpRegistry::new()),
        history_store: None,
        router_config: Arc::new(RouterConfig::default()),
        router_healthy: AtomicBool::new(false),
        approval_gate: Arc::new(ApprovalGate::default()),
        auto_decider: None,
        execution_store: Arc::new(NoOpExecutionStore),
        delegator: OnceLock::new(),
    })
}

fn parent_with(allowed: &[&str], extra: AgentConfig) -> Agent {
    let mut config = extra;
    config.name = "parent".into();
    config.subagents = Some(SubagentsConfig {
        allowed: allowed.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    });
    Agent {
        id: "parent".into(),
        project_id: Some("proj".into()),
        config,
    }
}

fn agent_with_model(id: &str, project: Option<&str>, model: &str) -> Agent {
    let mut target = agent(id, project, None);
    target.config.model = model.into();
    target
}

fn request_child(message: &str) -> DelegateRequest {
    let mut req = request("child", "parent", Some("proj"));
    req.message = message.into();
    req.task_id = Some("task-42".into());
    req
}

#[tokio::test]
async fn delegated_turn_returns_answer_with_fresh_execution_and_task_id() {
    let registry = Arc::new(AgentRegistry::new());
    register_real(&registry, parent_with(&["child"], AgentConfig::default()));
    register_real(&registry, agent("child", Some("proj"), None));
    let runs = RecordedRuns::default();
    let engine = Arc::new(RecordingEngine {
        runs: runs.clone(),
        answer: "CHILD_ANSWER".into(),
        fail_on: None,
        spend_budget: false,
    });
    let state = test_state(
        registry.clone(),
        engine,
        Arc::new(FakeProvider { fail: false }),
    );
    let delegator = RuntimeDelegator::new(registry, Arc::downgrade(&state));

    let result = delegator
        .delegate(request_child("do the thing"))
        .await
        .expect("an allowed child turn must run");

    assert_eq!(result.answer, "CHILD_ANSWER");
    assert_eq!(result.task_id, "task-42");
    let guard = runs.0.lock().unwrap();
    assert_eq!(guard.len(), 1, "exactly one child turn ran");
    let child = &guard[0];
    assert_eq!(child.agent.id, "child");
    assert_eq!(child.conversation_id.as_deref(), Some("task-42"));
    assert_eq!(child.user_message, "do the thing");
    assert_ne!(
        child.execution_id,
        Uuid::nil(),
        "a fresh child execution id"
    );
}

#[tokio::test]
async fn child_scope_is_deny_all_and_prompt_has_no_parent_sections() {
    let registry = Arc::new(AgentRegistry::new());
    register_real(
        &registry,
        parent_with(
            &["child"],
            AgentConfig {
                rules: vec!["PARENT_RULE_MARKER".into()],
                context_project: Some("parent-context".into()),
                mcp_sources: vec!["parent-server".into()],
                tools_override: vec!["*".into()],
                tools_local: vec!["*".into()],
                rag_sources: vec!["parent-rag".into()],
                ..Default::default()
            },
        ),
    );
    register_real(&registry, agent("child", Some("proj"), None));
    let runs = RecordedRuns::default();
    let state = test_state(
        registry.clone(),
        Arc::new(RecordingEngine {
            runs: runs.clone(),
            answer: "ok".into(),
            fail_on: None,
            spend_budget: false,
        }),
        Arc::new(FakeProvider { fail: false }),
    );
    let delegator = RuntimeDelegator::new(registry, Arc::downgrade(&state));

    delegator
        .delegate(request_child("do the thing"))
        .await
        .expect("delegation succeeds");

    let guard = runs.0.lock().unwrap();
    let child = &guard[0];
    assert!(
        matches!(child.scope.tools_allow, ToolsAllow::DenyAll),
        "child MCP tools are deny-all"
    );
    assert!(
        matches!(child.scope.local_tools_allow, ToolsAllow::DenyAll),
        "child local tools are deny-all"
    );
    assert!(child.scope.mcp_sources.is_empty());
    assert!(child.scope.rag_sources.is_empty());
    // The parent's curated sections must not leak into the child's prompt:
    // zero inheritance, by construction.
    assert!(!child.system_prompt.contains("PARENT_RULE_MARKER"));
    assert!(!child.system_prompt.contains("parent-context"));
    assert!(!child.system_prompt.contains("parent-server"));
}

#[tokio::test]
async fn whole_tree_shares_one_budget() {
    let registry = Arc::new(AgentRegistry::new());
    register_real(&registry, parent_with(&["child"], AgentConfig::default()));
    register_real(&registry, agent("child", Some("proj"), None));
    let runs = RecordedRuns::default();
    let shared: Arc<dyn ToolBudget> = Arc::new(Mutex::new(ToolCallBudget::new(1)));
    let mut req = request_child("do the thing");
    req.budget = shared.clone();
    let state = test_state(
        registry.clone(),
        Arc::new(RecordingEngine {
            runs: runs.clone(),
            answer: "ok".into(),
            fail_on: None,
            spend_budget: true,
        }),
        Arc::new(FakeProvider { fail: false }),
    );
    let delegator = RuntimeDelegator::new(registry, Arc::downgrade(&state));

    delegator.delegate(req).await.expect("child turn succeeds");

    {
        let guard = runs.0.lock().unwrap();
        assert!(
            guard[0].shared_budget.is_some(),
            "child turn received the shared budget"
        );
    }
    // The child spent the single allowance; the parent now cannot spend it.
    assert!(
        shared.try_consume().await.is_err(),
        "one allowance covers the whole tree"
    );
}

#[tokio::test]
async fn child_failure_is_named_without_failing_the_parent() {
    let registry = Arc::new(AgentRegistry::new());
    register_real(&registry, parent_with(&["child"], AgentConfig::default()));
    register_real(&registry, agent("child", Some("proj"), None));
    let state = test_state(
        registry.clone(),
        Arc::new(RecordingEngine {
            runs: RecordedRuns::default(),
            answer: "PARENT_OK".into(),
            fail_on: Some("do the thing".into()),
            spend_budget: false,
        }),
        // The fallback must fail too, so the child turn returns an error
        // instead of quietly answering from a direct provider call.
        Arc::new(FakeProvider { fail: true }),
    );
    let delegator = RuntimeDelegator::new(registry, Arc::downgrade(&state));

    let err = delegator
        .delegate(request_child("do the thing"))
        .await
        .expect_err("a failing child returns a named error");
    assert!(err.contains("child"), "error names the child: {err}");

    // The parent's own turn still works: the child's failure is contained.
    let response = execute_chat(
        &state,
        ChatRequest {
            model: "fallback-model".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hello".into(),
            }],
            stream: false,
        },
        AgentSelection {
            project_id: Some("proj"),
            agent_id: Some("parent"),
            conversation_id: None,
            debug: false,
        },
    )
    .await
    .expect("the parent turn is unaffected by a child failure");
    assert_eq!(response.message.content, "PARENT_OK");
}

#[tokio::test]
async fn expired_deadline_is_rejected_without_executing() {
    let registry = Arc::new(AgentRegistry::new());
    register_real(&registry, parent_with(&["child"], AgentConfig::default()));
    register_real(&registry, agent("child", Some("proj"), None));
    let runs = RecordedRuns::default();
    let state = test_state(
        registry.clone(),
        Arc::new(RecordingEngine {
            runs: runs.clone(),
            answer: "ok".into(),
            fail_on: None,
            spend_budget: false,
        }),
        Arc::new(FakeProvider { fail: false }),
    );
    let delegator = RuntimeDelegator::new(registry, Arc::downgrade(&state));

    let mut req = request_child("do the thing");
    req.deadline = Instant::now() - std::time::Duration::from_secs(1);
    let err = delegator
        .delegate(req)
        .await
        .expect_err("an expired deadline is rejected");
    assert!(err.contains("timed out"), "error names the timeout: {err}");
    assert!(
        runs.0.lock().unwrap().is_empty(),
        "no child turn ran past an expired deadline"
    );
}

#[tokio::test]
async fn child_request_is_non_streaming_and_uses_the_target_model() {
    let registry = Arc::new(AgentRegistry::new());
    register_real(
        &registry,
        parent_with(
            &["child"],
            AgentConfig {
                model: "coordinator-model".into(),
                ..Default::default()
            },
        ),
    );
    register_real(
        &registry,
        agent_with_model("child", Some("proj"), "specialist-model"),
    );
    let runs = RecordedRuns::default();
    let seen = SeenChatRequests::default();
    let state = test_state(
        registry.clone(),
        Arc::new(RecordingEngine {
            runs: runs.clone(),
            answer: "unused".into(),
            // The engine records the request before failing, so the turn falls
            // through to the recording provider — the only place `ChatRequest`
            // (and therefore `stream`) is observable from a test.
            fail_on: Some("do the thing".into()),
            spend_budget: false,
        }),
        Arc::new(RecordingProvider { seen: seen.clone() }),
    );
    let delegator = RuntimeDelegator::new(registry, Arc::downgrade(&state));

    let result = delegator
        .delegate(request_child("do the thing"))
        .await
        .expect("the child turn answers through the fallback");
    assert_eq!(result.answer, "FALLBACK_ANSWER");

    {
        let guard = runs.0.lock().unwrap();
        assert_eq!(guard.len(), 1, "exactly one child engine turn ran");
        assert_eq!(guard[0].agent.id, "child");
        assert_eq!(
            guard[0].model, "specialist-model",
            "the child uses the target agent's own model, not the coordinator's"
        );
        assert_ne!(guard[0].model, "coordinator-model");
    }
    {
        let guard = seen.0.lock().unwrap();
        assert_eq!(guard.len(), 1, "one fallback chat request");
        assert!(!guard[0].stream, "the delegated turn is not streaming");
        let last = guard[0].messages.last().expect("a user message");
        assert_eq!(last.role, "user");
        assert_eq!(last.content, "do the thing");
    }
    // `debug` is consumed by chat dispatch before the engine sees the turn: it
    // appears on neither `AgentRunRequest` nor `ChatRequest`, and `delegate_turn`
    // drops the response field it feeds. Pin the literal at its one source, the
    // same source-guard pattern `tests/tui_chrome.rs` uses for unobservable UI
    // contracts.
    let source =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/api/chat_core.rs"))
            .expect("chat_core source is readable");
    let body = &source[source
        .find("pub async fn delegate_turn")
        .expect("delegate_turn exists")..];
    assert!(
        body.contains("stream: false"),
        "delegate_turn must build a non-streaming child ChatRequest"
    );
    assert!(
        body.contains("debug: false"),
        "delegate_turn must build a non-debug child AgentSelection"
    );
}

#[tokio::test]
async fn global_target_runs_unscoped_without_project_context() {
    let registry = Arc::new(AgentRegistry::new());
    let parent = Agent {
        id: "parent".into(),
        project_id: Some("proj".into()),
        config: AgentConfig {
            name: "parent".into(),
            model: "coordinator-model".into(),
            context_project: Some("parent-context".into()),
            subagents: Some(SubagentsConfig {
                allowed: vec!["glob".into()],
                allow_global: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    };
    register_real(&registry, parent);
    register_real(&registry, agent_with_model("glob", None, "global-model"));
    let runs = RecordedRuns::default();
    let state = test_state(
        registry.clone(),
        Arc::new(RecordingEngine {
            runs: runs.clone(),
            answer: "GLOBAL_OK".into(),
            fail_on: None,
            spend_budget: false,
        }),
        Arc::new(FakeProvider { fail: false }),
    );
    let delegator = RuntimeDelegator::new(registry, Arc::downgrade(&state));

    let mut req = request("glob", "parent", Some("proj"));
    req.message = "do the thing".into();
    let result = delegator
        .delegate(req)
        .await
        .expect("an allowed global target must run");
    assert_eq!(result.answer, "GLOBAL_OK");

    let guard = runs.0.lock().unwrap();
    assert_eq!(guard.len(), 1, "exactly one child turn ran");
    let child = &guard[0];
    assert_eq!(child.agent.id, "glob");
    assert_eq!(child.agent.project_id, None, "the target is a global agent");
    assert_eq!(child.scope.project_id, None, "unscoped dispatch");
    assert_eq!(child.model, "global-model", "the target's own model");
    assert!(
        !child.system_prompt.contains("parent-context"),
        "no coordinator project context leaks into a global delegate"
    );
}
