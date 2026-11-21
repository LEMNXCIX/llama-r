//! Fase 6c: structured turn traces over the Fase 7 execution log.
//!
//! Every finished agent turn emits exactly one `TraceRecorded` event whose
//! payload carries the resolved provider/model, token counts, cost and latency.
//! The payload is redacted with the shared helper before it reaches the store,
//! and observability is best-effort: a failing store never fails the chat.

#![allow(clippy::await_holding_lock)] // env guard held deliberately across .await to serialise tests

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use llama_r::adapters::context::NoopCompactor;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::adapters::telemetry::NoopExporter;
use llama_r::api::chat_core::{execute_chat, execute_chat_stream, record_trace, trace_payload};
use llama_r::api::handlers::AppState;
use llama_r::api::route_resolution::AgentSelection;
use llama_r::config::ProviderKind;
use llama_r::context::analyzer::ContextEnricher;
use llama_r::context::store::ContextStore;
use llama_r::domain::agent::AgentConfig;
use llama_r::domain::execution::{AgentExecution, EventKind, ExecutionEvent, ExecutionStatus};
use llama_r::domain::models::{
    ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo, Usage,
};
use llama_r::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult};
use llama_r::ports::execution::{ApprovalDecisionRecord, ExecutionStore, PendingApprovalRecord};
use llama_r::providers::registry::ProviderRegistry;
use llama_r::providers::LLMProvider;
use llama_r::runtime::build_app_state_with_gate;
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::approval::ApprovalGate;
use llama_r::services::provider_stack::ProviderStack;
use llama_r::services::skill_manager::SkillManager;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_stream::{Stream, StreamExt};
use uuid::Uuid;

// ── env serialisation (integration tests cannot use cfg(test) helpers) ──────

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ── fake store ──────────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeStore {
    events: Mutex<Vec<ExecutionEvent>>,
    executions: Mutex<HashMap<Uuid, AgentExecution>>,
    append_attempts: AtomicU32,
    fail_append: AtomicBool,
}

#[async_trait]
impl ExecutionStore for FakeStore {
    async fn create(&self, execution: &AgentExecution) -> Result<(), String> {
        self.executions
            .lock()
            .unwrap()
            .insert(execution.id, execution.clone());
        Ok(())
    }
    async fn append_event(&self, _id: Uuid, event: &ExecutionEvent) -> Result<(), String> {
        self.append_attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail_append.load(Ordering::SeqCst) {
            return Err("store unavailable".into());
        }
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
    async fn set_status(
        &self,
        id: Uuid,
        status: ExecutionStatus,
        error: Option<&str>,
    ) -> Result<(), String> {
        if let Some(e) = self.executions.lock().unwrap().get_mut(&id) {
            e.status = status;
            e.error = error.map(str::to_string);
        }
        Ok(())
    }
    async fn get(&self, id: Uuid) -> Result<Option<AgentExecution>, String> {
        Ok(self.executions.lock().unwrap().get(&id).cloned())
    }
    async fn list_interrupted(&self) -> Result<Vec<AgentExecution>, String> {
        Ok(Vec::new())
    }
    async fn list(&self, limit: usize) -> Result<Vec<AgentExecution>, String> {
        Ok(self
            .executions
            .lock()
            .unwrap()
            .values()
            .take(limit)
            .cloned()
            .collect())
    }
    async fn get_events(&self, id: Uuid) -> Result<Vec<ExecutionEvent>, String> {
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.execution_id == id)
            .cloned()
            .collect())
    }
    async fn record_operation(&self, _id: &str, _result: &Value) -> Result<(), String> {
        Ok(())
    }
    async fn get_operation(&self, _id: &str) -> Result<Option<Value>, String> {
        Ok(None)
    }
    async fn save_pending_approval(&self, _record: &PendingApprovalRecord) -> Result<(), String> {
        Ok(())
    }
    async fn load_pending_approvals(&self) -> Result<Vec<PendingApprovalRecord>, String> {
        Ok(Vec::new())
    }
    async fn decide_pending(
        &self,
        _id: Uuid,
        _decision: ApprovalDecisionRecord,
        _at: DateTime<Utc>,
    ) -> Result<(), String> {
        Ok(())
    }
}

// ── fake provider / engine ─────────────────────────────────────────────────

struct StubProvider {
    usage: Option<Usage>,
    fail: bool,
}

#[async_trait]
impl LLMProvider for StubProvider {
    fn get_base_url(&self) -> String {
        "http://stub".into()
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
        if self.fail {
            return Err("provider down".into());
        }
        Ok(ChatResponse {
            model: request.model,
            created_at: "now".into(),
            message: ChatMessage {
                role: "assistant".into(),
                content: "direct answer".into(),
            },
            done: true,
            usage: self.usage.clone(),
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
                content: "streamed".into(),
            },
            done: true,
            approval: None,
        })])))
    }
}

struct StubEngine {
    fail: bool,
}

#[async_trait]
impl AgentEngine for StubEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        if self.fail {
            return Err("engine down".into());
        }
        Ok(AgentRunResult {
            text: "engine answer".into(),
            tool_calls: 0,
            iterations: 1,
            model: req.model,
        })
    }
    async fn run_stream(
        &self,
        req: AgentRunRequest,
    ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
        let result = self.run(req).await?;
        let (tx, rx) = mpsc::channel(2);
        let _ = tx.send(AgentRunEvent::Completed { result }).await;
        Ok(rx)
    }
}

// ── rig ─────────────────────────────────────────────────────────────────────

fn test_enricher() -> Arc<ContextEnricher> {
    Arc::new(ContextEnricher::new(
        Arc::new(ContextStore::new()),
        Arc::new(SkillManager::new()),
        "model-a".into(),
        Arc::new(ProviderRegistry::default()),
        ProviderKind::Ollama,
    ))
}

fn install_agent(registry: &Arc<AgentRegistry>, project: &str, agent: &str) {
    let dir = llama_r::core::paths::get_project_agents_dir(project);
    std::fs::create_dir_all(&dir).unwrap();
    let config = AgentConfig {
        name: agent.into(),
        model: "model-a".into(),
        system_prompt: "you are helpful".into(),
        context_project: Some(project.into()),
        ..Default::default()
    };
    std::fs::write(
        dir.join(format!("{agent}.toml")),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    registry.reload_all(&[]).unwrap();
}

fn build_state(
    engine: Arc<dyn AgentEngine>,
    provider: Arc<dyn LLMProvider + Send + Sync>,
    store: Arc<FakeStore>,
) -> (TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());
    let registry = Arc::new(AgentRegistry::new());
    install_agent(&registry, "trace-proj", "trace-agent");
    let runtime = llama_r::services::agent_runtime::AgentRuntime {
        registry: registry.clone(),
        engine,
        history: None,
        rag: None,
        enricher: test_enricher(),
        compactor: Arc::new(NoopCompactor),
        selector: Arc::new(llama_r::adapters::context::CurrentModelSelector),
    };
    let mut stack = ProviderStack::new(provider, "model-a");
    stack.provider_name = "ollama".into();
    stack.agent_runtime = Some(Arc::new(runtime));
    let state = build_app_state_with_gate(
        Arc::new(ApprovalGate::default()),
        stack,
        registry,
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        Arc::new(Mutex::new(VecDeque::new())),
        vec![],
        Arc::new(StaticMcpRegistry::new()),
        None,
        store,
    );
    (dir, state)
}

fn selection() -> AgentSelection<'static> {
    AgentSelection {
        project_id: Some("trace-proj"),
        agent_id: Some("trace-agent"),
        conversation_id: None,
        debug: false,
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "model-a".into(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: "hello".into(),
        }],
        stream: false,
    }
}

fn sample_execution() -> AgentExecution {
    AgentExecution {
        id: Uuid::new_v4(),
        conversation_id: None,
        agent_qualified_id: "trace-proj::trace-agent".into(),
        provider: "ollama".into(),
        model: "model-a".into(),
        status: ExecutionStatus::Completed,
        current_step: 0,
        user_message: Some("hello".into()),
        user_message_hash: "cd".repeat(32),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        finished_at: None,
        error: None,
    }
}

fn only_trace(events: &[ExecutionEvent]) -> &ExecutionEvent {
    let traces: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::TraceRecorded)
        .collect();
    assert_eq!(
        traces.len(),
        1,
        "exactly one trace per finished turn: {events:?}"
    );
    traces[0]
}

// ── tests ───────────────────────────────────────────────────────────────────

#[test]
fn trace_payload_cost_is_null_without_prices_and_computed_with_injected_prices() {
    let execution = sample_execution();

    let no_prices = Usage::from_counts(10, 10, None, None);
    let p = trace_payload(&execution, Some(&no_prices), 5);
    assert!(p["cost_usd"].is_null());
    assert_eq!(p["input_tokens"], 10);
    assert_eq!(p["output_tokens"], 10);
    assert_eq!(p["latency_ms"], 5);

    let priced = Usage::from_counts(2_000_000, 1_000_000, Some(3.0), Some(6.0));
    let p = trace_payload(&execution, Some(&priced), 7);
    assert_eq!(p["cost_usd"].as_f64().unwrap(), 12.0);
    assert_eq!(p["input_tokens"], 2_000_000);
    assert_eq!(p["output_tokens"], 1_000_000);
}

#[test]
fn trace_payload_carries_turn_identity() {
    let execution = sample_execution();
    let p = trace_payload(&execution, None, 3);
    assert_eq!(p["trace_id"], execution.id.to_string());
    assert_eq!(p["agent_qualified_id"], execution.agent_qualified_id);
    assert_eq!(p["provider"], execution.provider);
    assert_eq!(p["model"], execution.model);
    assert!(p["cost_usd"].is_null());
}

#[tokio::test]
async fn record_trace_redacts_secrets_before_appending() {
    let store = Arc::new(FakeStore::default());
    let execution = sample_execution();
    let payload = json!({
        "trace_id": execution.id,
        "api_key": "SUPER-SECRET-KEY",
        "nested": { "token": "ANOTHER-SECRET" },
        "provider": "ollama"
    });

    record_trace(store.as_ref(), &NoopExporter, &execution, &payload).await;

    let events = store.get_events(execution.id).await.unwrap();
    let event = only_trace(&events);
    assert_eq!(event.payload["api_key"], "[REDACTED]");
    assert_eq!(event.payload["nested"]["token"], "[REDACTED]");
    let serialized = serde_json::to_string(&event.payload).unwrap();
    assert!(!serialized.contains("SUPER-SECRET-KEY"));
    assert!(!serialized.contains("ANOTHER-SECRET"));
}

#[tokio::test]
async fn a_successful_turn_emits_one_trace_with_resolved_identity() {
    let _guard = lock_env();
    let store = Arc::new(FakeStore::default());
    let (_dir, state) = build_state(
        Arc::new(StubEngine { fail: false }),
        Arc::new(StubProvider {
            usage: None,
            fail: false,
        }),
        store.clone(),
    );

    let response = execute_chat(&state, request(), selection()).await.unwrap();
    assert_eq!(response.message.content, "engine answer");

    let executions = store.list(10).await.unwrap();
    assert_eq!(executions.len(), 1);
    let execution = &executions[0];
    let events = store.get_events(execution.id).await.unwrap();
    let trace = only_trace(&events).payload.clone();

    assert_eq!(trace["trace_id"], execution.id.to_string());
    assert_eq!(trace["agent_qualified_id"], execution.agent_qualified_id);
    assert_eq!(trace["provider"], "ollama");
    assert_eq!(trace["model"], execution.model);
    assert!(trace["latency_ms"].as_u64().is_some());
    assert!(
        trace["cost_usd"].is_null(),
        "engine path has no token accounting"
    );
}

#[tokio::test]
async fn a_fallback_turn_propagates_injected_cost() {
    let _guard = lock_env();
    let store = Arc::new(FakeStore::default());
    let priced = Usage::from_counts(2_000_000, 1_000_000, Some(3.0), Some(6.0));
    let (_dir, state) = build_state(
        Arc::new(StubEngine { fail: true }),
        Arc::new(StubProvider {
            usage: Some(priced),
            fail: false,
        }),
        store.clone(),
    );

    let response = execute_chat(&state, request(), selection()).await.unwrap();
    assert_eq!(response.message.content, "direct answer");

    let executions = store.list(10).await.unwrap();
    let execution = &executions[0];
    let events = store.get_events(execution.id).await.unwrap();
    let trace = only_trace(&events).payload.clone();

    assert_eq!(trace["cost_usd"].as_f64().unwrap(), 12.0);
    assert_eq!(trace["input_tokens"], 2_000_000);
    assert_eq!(trace["output_tokens"], 1_000_000);
}

#[tokio::test]
async fn an_error_turn_still_emits_one_trace_with_no_cost() {
    let _guard = lock_env();
    let store = Arc::new(FakeStore::default());
    let (_dir, state) = build_state(
        Arc::new(StubEngine { fail: true }),
        Arc::new(StubProvider {
            usage: None,
            fail: true,
        }),
        store.clone(),
    );

    let result = execute_chat(&state, request(), selection()).await;
    assert!(result.is_err(), "both engine and provider failed");

    let executions = store.list(10).await.unwrap();
    let execution = &executions[0];
    assert_eq!(execution.status, ExecutionStatus::Failed);
    let events = store.get_events(execution.id).await.unwrap();
    let trace = only_trace(&events).payload.clone();
    assert!(trace["cost_usd"].is_null());
    assert_eq!(trace["trace_id"], execution.id.to_string());
}

#[tokio::test]
async fn a_streaming_turn_emits_one_trace() {
    let _guard = lock_env();
    let store = Arc::new(FakeStore::default());
    let (_dir, state) = build_state(
        Arc::new(StubEngine { fail: false }),
        Arc::new(StubProvider {
            usage: None,
            fail: false,
        }),
        store.clone(),
    );

    let mut req = request();
    req.stream = true;
    let mut stream = execute_chat_stream(&state, req, selection()).await.unwrap();
    while stream.next().await.is_some() {}

    let executions = store.list(10).await.unwrap();
    assert_eq!(executions.len(), 1);
    let execution = &executions[0];
    let events = store.get_events(execution.id).await.unwrap();
    let trace = only_trace(&events).payload.clone();
    assert_eq!(trace["trace_id"], execution.id.to_string());
    assert_eq!(trace["provider"], "ollama");
    assert!(trace["cost_usd"].is_null());
}

#[cfg(feature = "history")]
#[tokio::test]
async fn the_real_sqlite_store_keeps_numeric_counters_and_redacts_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        llama_r::adapters::execution::SqliteExecutionStore::new(dir.path().join("history.db"))
            .unwrap(),
    );
    let execution = sample_execution();
    store.create(&execution).await.unwrap();
    let event = ExecutionEvent::new(
        execution.id,
        EventKind::TraceRecorded,
        json!({
            "trace_id": execution.id,
            "input_tokens": 2_000_000,
            "output_tokens": 1000,
            "cost_usd": 12.0,
            "api_key": "sk-SHOULD-NOT-PERSIST"
        }),
    );
    store.append_event(execution.id, &event).await.unwrap();

    let events = store.get_events(execution.id).await.unwrap();
    let stored = &events[0];
    assert_eq!(stored.payload["input_tokens"], 2_000_000);
    assert_eq!(stored.payload["output_tokens"], 1000);
    assert_eq!(stored.payload["cost_usd"], 12.0);
    assert_eq!(stored.payload["api_key"], "[REDACTED]");

    let conn = rusqlite::Connection::open(dir.path().join("history.db")).unwrap();
    let on_disk: String = conn
        .query_row(
            "SELECT payload FROM execution_events WHERE id = ?1",
            rusqlite::params![event.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!on_disk.contains("sk-SHOULD-NOT-PERSIST"));
    assert!(on_disk.contains("[REDACTED]"));
}

#[tokio::test]
async fn a_failing_store_does_not_fail_the_chat() {
    let _guard = lock_env();
    let store = Arc::new(FakeStore::default());
    store.fail_append.store(true, Ordering::SeqCst);
    let (_dir, state) = build_state(
        Arc::new(StubEngine { fail: false }),
        Arc::new(StubProvider {
            usage: None,
            fail: false,
        }),
        store.clone(),
    );

    let response = execute_chat(&state, request(), selection()).await;
    assert!(
        response.is_ok(),
        "observability is best-effort: {}",
        response.err().map(|e| e.to_string()).unwrap_or_default()
    );
    assert!(
        store.append_attempts.load(Ordering::SeqCst) >= 2,
        "the terminal event and the trace were both attempted"
    );
}

// ── Task 5: hierarchical traces ─────────────────────────────────────────────

use llama_r::api::chat_core::trace_payload_for;
use llama_r::ports::delegation::DelegationContext;

fn trace_context(
    chain: Vec<String>,
    depth: u32,
    parent: Option<Uuid>,
    root: Option<Uuid>,
) -> DelegationContext {
    DelegationContext {
        chain,
        depth,
        effective_max_depth: 2,
        parent_execution_id: parent,
        root_execution_id: root,
    }
}

#[test]
fn hierarchical_trace_fields_reconstruct_a_two_level_tree() {
    let root = sample_execution();
    let child = sample_execution();
    let grandchild = sample_execution();

    // Root: no parent, its own root, depth 0.
    let root_ctx = trace_context(Vec::new(), 0, None, None);
    let root_payload = trace_payload_for(&root, None, 1, Some(&root_ctx));
    assert!(root_payload["parent_execution_id"].is_null());
    assert_eq!(root_payload["root_execution_id"], root.id.to_string());
    assert_eq!(root_payload["depth"], 0);

    // Child of the root.
    let child_ctx = trace_context(
        vec![root.agent_qualified_id.clone()],
        1,
        Some(root.id),
        Some(root.id),
    );
    let child_payload = trace_payload_for(&child, None, 2, Some(&child_ctx));
    assert_eq!(child_payload["parent_execution_id"], root.id.to_string());
    assert_eq!(child_payload["root_execution_id"], root.id.to_string());
    assert_eq!(child_payload["depth"], 1);

    // Grandchild: parent is the child, root is still the root.
    let grandchild_ctx = trace_context(
        vec![
            root.agent_qualified_id.clone(),
            child.agent_qualified_id.clone(),
        ],
        2,
        Some(child.id),
        Some(root.id),
    );
    let grandchild_payload = trace_payload_for(&grandchild, None, 3, Some(&grandchild_ctx));
    assert_eq!(
        grandchild_payload["parent_execution_id"],
        child.id.to_string()
    );
    assert_eq!(grandchild_payload["root_execution_id"], root.id.to_string());
    assert_eq!(grandchild_payload["depth"], 2);

    // The whole tree is reconstructable from the three payloads alone.
    let by_id: HashMap<String, &Value> = [&root_payload, &child_payload, &grandchild_payload]
        .into_iter()
        .map(|p| (p["trace_id"].as_str().unwrap().to_string(), p))
        .collect();
    assert_eq!(by_id.len(), 3);
    let gc = &by_id[&grandchild.id.to_string()];
    let parent = &by_id[gc["parent_execution_id"].as_str().unwrap()];
    let root_of_gc = &by_id[gc["root_execution_id"].as_str().unwrap()];
    assert_eq!(parent["trace_id"], child.id.to_string());
    assert_eq!(root_of_gc["trace_id"], root.id.to_string());
    assert_eq!(parent["depth"], 1);
    assert_eq!(root_of_gc["depth"], 0);
}

#[tokio::test]
async fn hierarchical_trace_payload_is_redacted_before_reaching_the_store() {
    let store = Arc::new(FakeStore::default());
    let child = sample_execution();
    let ctx = trace_context(
        vec!["root".into()],
        1,
        Some(Uuid::new_v4()),
        Some(Uuid::new_v4()),
    );
    let mut payload = trace_payload_for(&child, None, 5, Some(&ctx));
    // A secret smuggled into the payload must not survive persistence, while
    // the hierarchy counters (plain scalars) are kept.
    payload["api_key"] = json!("sk-SHOULD-NOT-PERSIST");
    payload["depth"] = json!(1);

    record_trace(store.as_ref(), &NoopExporter, &child, &payload).await;

    let events = store.get_events(child.id).await.unwrap();
    let trace = only_trace(&events);
    assert_eq!(trace.payload["api_key"], "[REDACTED]");
    assert_eq!(trace.payload["depth"], 1);
    assert_eq!(
        trace.payload["parent_execution_id"],
        ctx.parent_execution_id.unwrap().to_string()
    );
    assert_eq!(
        trace.payload["root_execution_id"],
        ctx.root_execution_id.unwrap().to_string()
    );
    let serialized = serde_json::to_string(&trace.payload).unwrap();
    assert!(!serialized.contains("sk-SHOULD-NOT-PERSIST"));
}
