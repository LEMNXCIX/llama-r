//! Task 2: bounded validation for subagent delegation.
//!
//! These tests drive the pure `validate_delegation` function against an
//! in-memory `AgentRegistry` fixture. They pin each rejection branch to a
//! named error and confirm that an allowed target resolves to its `Agent`.

use llama_r::adapters::rig_engine::limits::ToolCallBudget;
use llama_r::domain::agent::{Agent, AgentConfig, RagWritePolicy, SubagentsConfig};
use llama_r::domain::policy::PermissionPolicy;
use llama_r::domain::scope::{AgentScope, ToolsAllow};
use llama_r::ports::delegation::{DelegateRequest, DelegationContext, ToolBudget};
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
        parent_execution_id: Uuid::nil(),
        root_execution_id: Uuid::nil(),
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
use llama_r::adapters::context::NoopCompactor;
use llama_r::adapters::execution::NoOpExecutionStore;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::adapters::rig_engine::local_tools::{
    DelegateBounds, LocalToolBridge, LocalToolDeps, LocalToolRegistry, BUILTIN_DELEGATE_ID,
};
use llama_r::adapters::rig_engine::operations::OperationContext;
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
            conversation_id: None,
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
            conversation_id: None,
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
        compactor: Arc::new(NoopCompactor),
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
        compactor: Arc::new(NoopCompactor),
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

#[test]
fn boot_and_apply_keep_the_delegator_on_the_engine() {
    let registry = Arc::new(AgentRegistry::new());
    let state = test_state(
        registry.clone(),
        Arc::new(RecordingEngine {
            runs: RecordedRuns::default(),
            answer: "ok".into(),
            fail_on: None,
            spend_budget: false,
        }),
        Arc::new(FakeProvider { fail: false }),
    );
    let delegator: Arc<dyn Delegator> =
        Arc::new(RuntimeDelegator::new(registry, Arc::downgrade(&state)));
    state.set_delegator(delegator);

    // Boot and a settings Apply both read the same installed handle from state.
    let boot_handles = llama_r::runtime::CompanionHandles::from_state(&state);
    let apply_handles = llama_r::runtime::CompanionHandles::from_state(&state);
    let boot = boot_handles
        .delegator
        .as_ref()
        .expect("boot has the delegator");
    let apply = apply_handles
        .delegator
        .as_ref()
        .expect("Apply keeps the delegator");
    assert!(Arc::ptr_eq(boot, apply), "the same Arc survives an Apply");

    // `build_companions` installs that handle on the engine with this setter on
    // both paths; the engine field is what `LocalToolDeps` reads per run.
    let engine = llama_r::adapters::rig_engine::RigAgentEngine::from_target(
        llama_r::adapters::rig_engine::EngineTarget::Ollama {
            url: "http://localhost:11434".into(),
        },
        Arc::new(StaticMcpRegistry::new()),
        None,
        llama_r::config::Config {
            port: 3000,
            provider: ProviderKind::Ollama,
            ollama_url: "http://localhost:11434".into(),
            default_model: "llama3.2".into(),
            embedding_model: "nomic-embed-text".into(),
            embedding_dimensions: 768,
            rag_enabled: false,
            history_retention_days: 0,
            openrouter_api_key: None,
            opencode_api_key: None,
            opencode_url: "http://127.0.0.1:4096".into(),
            openai_base_url: None,
        },
        Arc::new(SkillManager::new()),
        Arc::new(llama_r::adapters::rig_engine::local_tools::LocalToolRegistry::builtins()),
        Arc::new(ApprovalGate::default()),
    )
    .with_delegator(boot_handles.delegator.clone());
    assert!(
        engine.delegator.is_some(),
        "the engine holds the delegator the tool reads"
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

// ---------------------------------------------------------------------------
// Task 4 fix round 1: specialist selection and tree-wide recursion bounds.
// ---------------------------------------------------------------------------

/// A real delegate-tool bridge backed by `RuntimeDelegator`, so the tool call
/// exercises the same allowlist / chain / depth validation a model would hit.
fn delegate_bridge(
    state: &Arc<AppState>,
    registry: &Arc<AgentRegistry>,
    caller_id: &str,
    context: DelegationContext,
) -> LocalToolBridge {
    let caller = registry
        .get_project_agent("proj", caller_id)
        .unwrap_or_else(|| panic!("caller {caller_id} must be registered in proj"));
    let bounds = DelegateBounds::for_agent(&caller)
        .unwrap_or_else(|| panic!("caller {caller_id} must have a [subagents] policy"));
    let delegator: Arc<dyn Delegator> = Arc::new(RuntimeDelegator::new(
        registry.clone(),
        Arc::downgrade(state),
    ));
    let deps = LocalToolDeps {
        skill_manager: Arc::new(SkillManager::new()),
        rag: None,
        gate: state.approval_gate.clone(),
        auto_decider: None,
        observability: None,
        delegator: Some(delegator),
        delegate_bounds: Some(bounds),
        delegation_context: Some(context),
        event_tx: None,
        tool_event_tx: None,
        operations: OperationContext::disabled(),
    };
    let tool = LocalToolRegistry::builtins()
        .tools()
        .iter()
        .find(|t| t.id == BUILTIN_DELEGATE_ID)
        .unwrap()
        .clone();
    let mut scoped = scope(caller_id, Some("proj"));
    scoped.local_tools_allow = ToolsAllow::AllFromSources;
    LocalToolBridge::new(
        tool,
        scoped,
        Arc::new(Mutex::new(ToolCallBudget::new(8))),
        deps,
    )
}

fn root_context(max_depth: u32) -> DelegationContext {
    DelegationContext {
        chain: Vec::new(),
        depth: 0,
        effective_max_depth: max_depth,
        parent_execution_id: None,
        root_execution_id: None,
    }
}

fn with_subagents(id: &str, allowed: &[&str], max_depth: u32) -> Agent {
    Agent {
        id: id.into(),
        project_id: Some("proj".into()),
        config: AgentConfig {
            name: id.into(),
            subagents: Some(SubagentsConfig {
                allowed: allowed.iter().map(|s| s.to_string()).collect(),
                max_depth: Some(max_depth),
                ..Default::default()
            }),
            ..Default::default()
        },
    }
}

#[tokio::test]
async fn explicit_agent_id_selects_the_named_specialist() {
    let registry = Arc::new(AgentRegistry::new());
    register_real(&registry, parent_with(&["a", "b"], AgentConfig::default()));
    register_real(&registry, agent("a", Some("proj"), None));
    register_real(&registry, agent("b", Some("proj"), None));
    let runs = RecordedRuns::default();
    let state = test_state(
        registry.clone(),
        Arc::new(RecordingEngine {
            runs: runs.clone(),
            answer: "B_ANSWER".into(),
            fail_on: None,
            spend_budget: false,
        }),
        Arc::new(FakeProvider { fail: false }),
    );
    let bridge = delegate_bridge(&state, &registry, "parent", root_context(1));

    let result = bridge
        .invoke_json(serde_json::json!({"message": "x", "agent_id": "b"}))
        .await
        .expect("an allowed explicit target runs");
    assert!(result.contains("B_ANSWER"));
    let guard = runs.0.lock().unwrap();
    assert_eq!(guard.len(), 1, "exactly one child turn ran");
    assert_eq!(guard[0].agent.id, "b", "the named specialist was chosen");
}

#[tokio::test]
async fn non_allowed_agent_id_is_rejected_by_name() {
    let registry = Arc::new(AgentRegistry::new());
    register_real(&registry, parent_with(&["a"], AgentConfig::default()));
    register_real(&registry, agent("a", Some("proj"), None));
    register_real(&registry, agent("outsider", Some("proj"), None));
    let runs = RecordedRuns::default();
    let state = test_state(
        registry.clone(),
        Arc::new(RecordingEngine {
            runs: runs.clone(),
            answer: "unused".into(),
            fail_on: None,
            spend_budget: false,
        }),
        Arc::new(FakeProvider { fail: false }),
    );
    let bridge = delegate_bridge(&state, &registry, "parent", root_context(1));

    let err = bridge
        .invoke_json(serde_json::json!({"message": "x", "agent_id": "outsider"}))
        .await
        .expect_err("a non-allowed id must be refused");
    assert!(err.contains("outsider"), "error names the agent: {err}");
    assert!(err.contains("allowlist"), "error names the reason: {err}");
    assert!(
        runs.0.lock().unwrap().is_empty(),
        "no child turn ran for a non-allowed id"
    );
}

#[tokio::test]
async fn grandchild_request_carries_depth_two_and_the_full_ancestor_chain() {
    let registry = Arc::new(AgentRegistry::new());
    register_real(&registry, parent_with(&["b"], AgentConfig::default()));
    register_real(&registry, with_subagents("b", &["c"], 2));
    register_real(&registry, agent("c", Some("proj"), None));
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

    // Level 1: the coordinator delegates to B.
    delegate_bridge(&state, &registry, "parent", root_context(2))
        .invoke_json(serde_json::json!({"message": "level-1"}))
        .await
        .expect("the first delegation runs");
    let child_context = {
        let guard = runs.0.lock().unwrap();
        assert_eq!(guard.len(), 1);
        assert_eq!(guard[0].agent.id, "b");
        guard[0]
            .delegation_context
            .clone()
            .expect("the child turn carries a context")
    };
    assert_eq!(child_context.depth, 1);
    assert_eq!(child_context.chain, vec!["proj/parent".to_string()]);

    // Level 2: B's own tool uses the context the runtime actually produced.
    delegate_bridge(&state, &registry, "b", child_context)
        .invoke_json(serde_json::json!({"message": "level-2"}))
        .await
        .expect("the second delegation runs");
    let guard = runs.0.lock().unwrap();
    assert_eq!(guard.len(), 2, "two child turns ran");
    let grandchild = &guard[1];
    assert_eq!(grandchild.agent.id, "c");
    let context = grandchild
        .delegation_context
        .as_ref()
        .expect("the grandchild turn carries a context");
    assert_eq!(context.depth, 2, "depth increments at every level");
    assert_eq!(
        context.chain,
        vec!["proj/parent".to_string(), "proj/b".to_string()]
    );
    assert!(
        context.chain.iter().any(|id| id == "proj/parent"),
        "the coordinator's qualified id is still in the chain"
    );
}

#[tokio::test]
async fn cycle_back_to_the_coordinator_is_rejected_at_level_two() {
    let registry = Arc::new(AgentRegistry::new());
    register_real(&registry, parent_with(&["b"], AgentConfig::default()));
    register_real(&registry, with_subagents("b", &["parent"], 2));
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

    delegate_bridge(&state, &registry, "parent", root_context(2))
        .invoke_json(serde_json::json!({"message": "level-1"}))
        .await
        .expect("the first delegation runs");
    let child_context = runs.0.lock().unwrap()[0]
        .delegation_context
        .clone()
        .expect("child context");

    let err = delegate_bridge(&state, &registry, "b", child_context)
        .invoke_json(serde_json::json!({"message": "level-2", "agent_id": "parent"}))
        .await
        .expect_err("delegating back to the coordinator must be refused");
    assert!(err.contains("cycle"), "error names the reason: {err}");
    assert!(
        err.contains("proj/parent"),
        "error names the cycle id: {err}"
    );
    assert_eq!(runs.0.lock().unwrap().len(), 1, "no third turn ran");
}

// ---------------------------------------------------------------------------
// Task 5: child approvals reach the root SSE; tree hierarchy is threaded.
// ---------------------------------------------------------------------------

use llama_r::api::chat_core::engine_events_to_chat_stream;
use llama_r::ports::delegation::{ChildEvent, DelegateResult, EventForwarder};
use llama_r::services::approval::args_hash;
use tokio_stream::StreamExt;

/// A delegator that fires a synthetic child `ApprovalRequired` into the
/// request's forwarder before returning. Stands in for a real child turn whose
/// tool hit an approval rule deep in its own Rig loop.
struct FiringDelegator {
    /// When set, registers a real pending in the gate with no SSE channel
    /// (the non-streaming child path) instead of firing a child event.
    poll_gate: Option<Arc<ApprovalGate>>,
}

#[async_trait]
impl Delegator for FiringDelegator {
    async fn delegate(&self, req: DelegateRequest) -> Result<DelegateResult, String> {
        if let Some(gate) = &self.poll_gate {
            // A non-streaming child has no event channel: the approval is only
            // discoverable through the gate's list (GET /api/approvals).
            let args = serde_json::json!({"query": "drop table users", "api_key": "SECRET"});
            let _rx = gate.request_for(Some(req.parent_execution_id), "child__danger", &args);
        } else {
            let forwarder: EventForwarder = req
                .event_tx
                .clone()
                .expect("a streaming child carries a forwarder");
            let id = Uuid::new_v4();
            forwarder(ChildEvent {
                event: AgentRunEvent::ApprovalRequired {
                    id,
                    tool_name: "child__danger".into(),
                    args: serde_json::json!({"query": "delete all"}),
                    task_id: None,
                    depth: None,
                },
                task_id: req.task_id.clone().unwrap_or_else(|| "task-42".into()),
                depth: req.depth,
                agent_qualified_id: "proj/child".into(),
            });
        }
        Ok(DelegateResult {
            answer: "ok".into(),
            task_id: req.task_id.unwrap_or_else(|| "task-42".into()),
        })
    }
}

/// A delegate bridge with a caller-supplied delegator and an optional SSE
/// sender (the parent's stream). Mirrors `delegate_bridge` but lets the test
/// observe child events and the pending gate directly.
fn delegate_bridge_observing(
    state: &Arc<AppState>,
    registry: &Arc<AgentRegistry>,
    caller_id: &str,
    context: DelegationContext,
    delegator: Arc<dyn Delegator>,
    event_tx: Option<tokio::sync::mpsc::Sender<AgentRunEvent>>,
) -> LocalToolBridge {
    let caller = registry
        .get_project_agent("proj", caller_id)
        .unwrap_or_else(|| panic!("caller {caller_id} must be registered in proj"));
    let bounds = DelegateBounds::for_agent(&caller).expect("caller has a [subagents] policy");
    let deps = LocalToolDeps {
        skill_manager: Arc::new(SkillManager::new()),
        rag: None,
        gate: state.approval_gate.clone(),
        auto_decider: None,
        observability: None,
        delegator: Some(delegator),
        delegate_bounds: Some(bounds),
        delegation_context: Some(context),
        event_tx,
        tool_event_tx: None,
        operations: OperationContext::disabled(),
    };
    let tool = LocalToolRegistry::builtins()
        .tools()
        .iter()
        .find(|t| t.id == BUILTIN_DELEGATE_ID)
        .unwrap()
        .clone();
    let mut scoped = scope(caller_id, Some("proj"));
    scoped.local_tools_allow = ToolsAllow::AllFromSources;
    LocalToolBridge::new(
        tool,
        scoped,
        Arc::new(Mutex::new(ToolCallBudget::new(8))),
        deps,
    )
}

#[tokio::test]
async fn child_approval_reaches_parent_sse_with_task_id_and_depth() {
    let registry = Arc::new(AgentRegistry::new());
    register_real(&registry, parent_with(&["child"], AgentConfig::default()));
    register_real(&registry, agent("child", Some("proj"), None));
    let state = test_state(
        registry.clone(),
        Arc::new(RecordingEngine {
            runs: RecordedRuns::default(),
            answer: "unused".into(),
            fail_on: None,
            spend_budget: false,
        }),
        Arc::new(FakeProvider { fail: false }),
    );

    // The parent's SSE channel: the root stream reads this receiver.
    let (parent_tx, parent_rx) = tokio::sync::mpsc::channel(32);
    let bridge = delegate_bridge_observing(
        &state,
        &registry,
        "parent",
        root_context(2),
        Arc::new(FiringDelegator { poll_gate: None }),
        Some(parent_tx),
    );

    bridge
        .invoke_json(serde_json::json!({"message": "x", "task_id": "task-42"}))
        .await
        .expect("the delegate tool runs");

    let mut stream = engine_events_to_chat_stream("model".into(), parent_rx);
    let event = stream
        .next()
        .await
        .expect("the child approval reaches the parent stream")
        .expect("a valid SSE event");
    let approval = event
        .approval
        .as_ref()
        .expect("the event carries approval info");
    assert_eq!(approval.task_id.as_deref(), Some("task-42"));
    assert_eq!(approval.depth, Some(1));
    // The approval id remains the global UUID; it is not prefixed with the task.
    assert_eq!(approval.id.len(), 36, "approval id is a bare UUID");
}

#[tokio::test]
async fn child_approval_is_pollable_without_stream() {
    let registry = Arc::new(AgentRegistry::new());
    register_real(&registry, parent_with(&["child"], AgentConfig::default()));
    register_real(&registry, agent("child", Some("proj"), None));
    let gate = Arc::new(ApprovalGate::default());
    let state = test_state(
        registry.clone(),
        Arc::new(RecordingEngine {
            runs: RecordedRuns::default(),
            answer: "unused".into(),
            fail_on: None,
            spend_budget: false,
        }),
        Arc::new(FakeProvider { fail: false }),
    );

    // No SSE sender: a non-streaming child has no channel to emit into.
    let bridge = delegate_bridge_observing(
        &state,
        &registry,
        "parent",
        root_context(2),
        Arc::new(FiringDelegator {
            poll_gate: Some(gate.clone()),
        }),
        None,
    );
    bridge
        .invoke_json(serde_json::json!({"message": "x", "task_id": "task-poll"}))
        .await
        .expect("the delegate tool runs");

    // GET /api/approvals lists exactly this pending, with secrets redacted.
    let pending = gate.list_pending();
    assert_eq!(pending.len(), 1, "one discoverable pending approval");
    let view = &pending[0];
    assert_eq!(view.tool_name, "child__danger");
    let redacted = llama_r::services::redaction::redact_secrets(&view.args);
    assert_eq!(redacted["api_key"], "[REDACTED]");
    // A decision with the stored hash unblocks the waiting child call.
    gate.decide(view.id, true, args_hash(&view.args))
        .expect("a poll-path decision is accepted");
}

#[tokio::test]
async fn delegate_turn_threads_parent_and_root_ids_into_the_child_context() {
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

    let root_id = Uuid::new_v4();
    let parent_id = Uuid::new_v4();
    let child_context = DelegationContext {
        chain: vec!["proj/parent".into()],
        depth: 1,
        effective_max_depth: 2,
        parent_execution_id: Some(parent_id),
        root_execution_id: Some(root_id),
    };
    let mut req = request_child("level-1");
    req.parent_execution_id = parent_id;
    req.root_execution_id = root_id;
    let child_agent = registry.get_project_agent("proj", "child").unwrap();

    llama_r::api::chat_core::delegate_turn(&state, req, child_agent, child_context)
        .await
        .expect("the child turn runs");

    let guard = runs.0.lock().unwrap();
    let seen = guard[0]
        .delegation_context
        .as_ref()
        .expect("the engine sees the child context");
    assert_eq!(seen.depth, 1);
    assert_eq!(seen.parent_execution_id, Some(parent_id));
    assert_eq!(seen.root_execution_id, Some(root_id));
}

/// An engine that emits an `ApprovalRequired` into the child turn's forwarded
/// event channel. Lets a test verify `delegate_turn` really drives the forwarder
/// it was handed, not just that the tool builds one.
struct ApprovalEmittingEngine;

#[async_trait]
impl AgentEngine for ApprovalEmittingEngine {
    async fn run(
        &self,
        req: llama_r::ports::engine::AgentRunRequest,
    ) -> Result<AgentRunResult, String> {
        if let Some(tx) = &req.approval_event_tx {
            let _ = tx
                .send(AgentRunEvent::ApprovalRequired {
                    id: Uuid::new_v4(),
                    tool_name: "child__danger".into(),
                    args: serde_json::json!({"query": "delete all"}),
                    task_id: None,
                    depth: None,
                })
                .await;
        }
        Ok(AgentRunResult {
            text: "ok".into(),
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

#[tokio::test]
async fn delegate_turn_drives_the_parent_forwarder_with_task_id_and_depth() {
    let registry = Arc::new(AgentRegistry::new());
    register_real(&registry, parent_with(&["child"], AgentConfig::default()));
    register_real(&registry, agent("child", Some("proj"), None));
    let state = test_state(
        registry.clone(),
        Arc::new(ApprovalEmittingEngine),
        Arc::new(FakeProvider { fail: false }),
    );

    let (parent_tx, mut parent_rx) = tokio::sync::mpsc::channel(32);
    let forwarder: EventForwarder = Arc::new(move |child: ChildEvent| {
        if let AgentRunEvent::ApprovalRequired {
            id,
            tool_name,
            args,
            ..
        } = child.event
        {
            let _ = parent_tx.try_send(AgentRunEvent::ApprovalRequired {
                id,
                tool_name,
                args,
                task_id: Some(child.task_id),
                depth: Some(child.depth),
            });
        }
    });

    let root_id = Uuid::new_v4();
    let parent_id = Uuid::new_v4();
    let child_context = DelegationContext {
        chain: vec!["proj/parent".into()],
        depth: 2,
        effective_max_depth: 2,
        parent_execution_id: Some(parent_id),
        root_execution_id: Some(root_id),
    };
    let mut req = request_child("level-2-task");
    req.event_tx = Some(forwarder);
    req.parent_execution_id = parent_id;
    req.root_execution_id = root_id;
    let child_agent = registry.get_project_agent("proj", "child").unwrap();

    llama_r::api::chat_core::delegate_turn(&state, req, child_agent, child_context)
        .await
        .expect("the child turn runs");

    let event = tokio::time::timeout(std::time::Duration::from_secs(2), parent_rx.recv())
        .await
        .expect("the forwarded approval arrives")
        .expect("a forwarded event");
    match event {
        AgentRunEvent::ApprovalRequired { task_id, depth, .. } => {
            assert_eq!(task_id.as_deref(), Some("task-42"));
            assert_eq!(depth, Some(2));
        }
        other => panic!("expected a forwarded approval, got {other:?}"),
    }
}

// Fix round 1: a grandchild approval must keep its origin through two hops.

/// Mirrors the forwarder `exec_delegate` builds: re-emit a child approval into
/// the given channel with the `ChildEvent`'s (origin) tags attached.
fn channel_forwarder(tx: tokio::sync::mpsc::Sender<AgentRunEvent>) -> EventForwarder {
    Arc::new(move |child: ChildEvent| {
        if let AgentRunEvent::ApprovalRequired {
            id,
            tool_name,
            args,
            ..
        } = child.event
        {
            let _ = tx.try_send(AgentRunEvent::ApprovalRequired {
                id,
                tool_name,
                args,
                task_id: Some(child.task_id),
                depth: Some(child.depth),
            });
        }
    }) as EventForwarder
}

/// The middle turn's engine: it delegates one level deeper using the channel
/// its own `delegate_turn` created, so the grandchild's approval is forwarded
/// through the real forwarder task under test.
struct DelegatingEngine {
    state: Arc<AppState>,
    agent: Agent,
    context: DelegationContext,
    task_id: String,
    parent_id: Uuid,
    root_id: Uuid,
}

#[async_trait]
impl AgentEngine for DelegatingEngine {
    async fn run(
        &self,
        req: llama_r::ports::engine::AgentRunRequest,
    ) -> Result<AgentRunResult, String> {
        let channel = req
            .approval_event_tx
            .clone()
            .ok_or("a delegated turn must carry its approval channel")?;
        let child_req = DelegateRequest {
            agent_id: self.agent.id.clone(),
            caller_agent_id: "a".into(),
            project_id: Some("proj".into()),
            message: "go".into(),
            task_id: Some(self.task_id.clone()),
            chain: self.context.chain.clone(),
            depth: self.context.depth,
            effective_max_depth: self.context.effective_max_depth,
            deadline: Instant::now() + std::time::Duration::from_secs(10),
            budget: budget(),
            event_tx: Some(channel_forwarder(channel)),
            parent_execution_id: self.parent_id,
            root_execution_id: self.root_id,
        };
        let result = llama_r::api::chat_core::delegate_turn(
            &self.state,
            child_req,
            self.agent.clone(),
            self.context.clone(),
        )
        .await?;
        Ok(AgentRunResult {
            text: result.answer,
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

/// The root's delegate tool hands its `DelegateRequest` (whose `event_tx` is
/// the forwarder built by `exec_delegate`) straight into the middle turn.
struct RootDelegator {
    state: Arc<AppState>,
    agent: Agent,
    context: DelegationContext,
}

#[async_trait]
impl Delegator for RootDelegator {
    async fn delegate(&self, req: DelegateRequest) -> Result<DelegateResult, String> {
        llama_r::api::chat_core::delegate_turn(
            &self.state,
            req,
            self.agent.clone(),
            self.context.clone(),
        )
        .await
    }
}

#[tokio::test]
async fn deep_approval_keeps_origin_tags_through_two_forwarding_hops() {
    let root_id = Uuid::new_v4();

    // Root (depth 0): the delegate tool builds the first forwarder from the
    // SSE sender we hand it via `delegate_bridge_observing`.
    let root_registry = Arc::new(AgentRegistry::new());
    register_real(&root_registry, parent_with(&["a"], AgentConfig::default()));
    register_real(&root_registry, agent("a", Some("proj"), None));
    let root_state = test_state(
        root_registry.clone(),
        Arc::new(RecordingEngine {
            runs: RecordedRuns::default(),
            answer: "unused".into(),
            fail_on: None,
            spend_budget: false,
        }),
        Arc::new(FakeProvider { fail: false }),
    );

    // Deepest level (depth 2, agent B): its engine emits an UNTAGGED approval.
    let b_registry = Arc::new(AgentRegistry::new());
    register_real(&b_registry, agent("b", Some("proj"), None));
    let b_state = test_state(
        b_registry.clone(),
        Arc::new(ApprovalEmittingEngine),
        Arc::new(FakeProvider { fail: false }),
    );
    let b_agent = b_registry.get_project_agent("proj", "b").unwrap();
    let b_context = DelegationContext {
        chain: vec!["proj/parent".into(), "proj/a".into()],
        depth: 2,
        effective_max_depth: 3,
        parent_execution_id: Some(Uuid::new_v4()),
        root_execution_id: Some(root_id),
    };

    // Middle level (depth 1, agent A): delegating to B here forces the
    // grandchild's tagged approval back through A's real forwarder task.
    let a_registry = Arc::new(AgentRegistry::new());
    register_real(&a_registry, agent("a", Some("proj"), None));
    let a_state = test_state(
        a_registry.clone(),
        Arc::new(DelegatingEngine {
            state: b_state,
            agent: b_agent,
            context: b_context,
            task_id: "task-B".into(),
            parent_id: Uuid::new_v4(),
            root_id,
        }),
        Arc::new(FakeProvider { fail: false }),
    );
    let a_agent = a_registry.get_project_agent("proj", "a").unwrap();
    let a_context = DelegationContext {
        chain: vec!["proj/parent".into()],
        depth: 1,
        effective_max_depth: 3,
        parent_execution_id: Some(Uuid::new_v4()),
        root_execution_id: Some(root_id),
    };

    let (root_tx, mut root_rx) = tokio::sync::mpsc::channel(32);
    let bridge = delegate_bridge_observing(
        &root_state,
        &root_registry,
        "parent",
        root_context(3),
        Arc::new(RootDelegator {
            state: a_state,
            agent: a_agent,
            context: a_context,
        }),
        Some(root_tx),
    );
    bridge
        .invoke_json(serde_json::json!({"message": "go", "task_id": "task-A"}))
        .await
        .expect("the root delegate tool runs");

    // The bridge also emits ToolCall/ToolResult into the sender; keep reading
    // until the forwarded approval (the SSE mapper ignores those other events).
    let event = loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), root_rx.recv())
            .await
            .expect("the deep approval reaches the root SSE")
            .expect("a forwarded event");
        if matches!(event, AgentRunEvent::ApprovalRequired { .. }) {
            break event;
        }
    };
    match event {
        AgentRunEvent::ApprovalRequired { task_id, depth, .. } => {
            assert_eq!(
                task_id.as_deref(),
                Some("task-B"),
                "origin task wins over hop"
            );
            assert_eq!(depth, Some(2), "origin depth wins over hop");
        }
        other => panic!("expected a forwarded approval, got {other:?}"),
    }
}
