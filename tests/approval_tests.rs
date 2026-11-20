//! Task 3 + Task 4: the approval gate primitive and its wiring into the tool
//! bridges. The bridge tests use a real `McpToolBridge` / `LocalToolBridge`, a
//! real `ApprovalGate` and a real event channel — no mocked gate.

use async_trait::async_trait;
use llama_r::adapters::rig_engine::builder::build_tool_bridges;
use llama_r::adapters::rig_engine::limits::ToolCallBudget;
use llama_r::adapters::rig_engine::local_tools::{
    LocalToolBridge, LocalToolDeps, LocalToolRegistry, BUILTIN_SKILL_ID,
};
use llama_r::adapters::rig_engine::tools::McpToolBridge;
use llama_r::adapters::rig_engine::{ScopedMcpTool, ScopedTool};
use llama_r::api::chat_core::engine_events_to_chat_stream;
use llama_r::domain::agent::RagWritePolicy;
use llama_r::domain::models::ChatStreamEvent;
use llama_r::domain::policy::{ApprovalPolicy, ApprovalRule, PermissionPolicy};
use llama_r::domain::scope::{AgentScope, ToolsAllow};
use llama_r::ports::engine::{AgentRunEvent, AgentRunResult};
use llama_r::ports::mcp::{McpCallRequest, McpCallResult, McpClient, McpToolDef};
use llama_r::services::approval::{
    args_hash, args_hash_hex, ApprovalDecision, ApprovalError, ApprovalGate,
};
use llama_r::services::skill_manager::SkillManager;
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio_stream::StreamExt;
use uuid::Uuid;

// Task 5: approval API + end-to-end SSE proof.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::adapters::rig_engine::{EngineTarget, RigAgentEngine};
use llama_r::api::approval_api::REDACTED_MARKER;
use llama_r::api::handlers::AppState;
use llama_r::config::{Config, ProviderKind};
use llama_r::context::store::ContextStore;
use llama_r::domain::agent::{Agent, AgentConfig};
use llama_r::domain::models::{ChatRequest, ChatResponse, ModelInfo};
use llama_r::ports::engine::{AgentEngine, AgentRunRequest};
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::providers::capabilities::capabilities_for;
use llama_r::providers::LLMProvider;
use llama_r::runtime::{build_app_state_with_gate, build_router};
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::provider_stack::ProviderStack;
use std::error::Error;
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_stream::Stream;
use tower::ServiceExt;

#[test]
fn request_returns_id_and_receiver() {
    let gate = ApprovalGate::default();
    let (id, mut rx) = gate.request("db__drop_table", &json!({"table": "users"}));
    let (other_id, _other_rx) = gate.request("db__drop_table", &json!({"table": "posts"}));

    assert_ne!(id, other_id, "each pending approval gets a fresh id");
    // The pending has not been decided, so the oneshot is still open.
    assert!(rx.try_recv().is_err());
}

#[test]
fn decide_approve_resolves_the_receiver() {
    let gate = ApprovalGate::default();
    let args = json!({"table": "users"});
    let (id, mut rx) = gate.request("db__drop_table", &args);

    gate.decide(id, true, args_hash(&args))
        .expect("approving an existing pending must resolve it");
    assert!(matches!(rx.try_recv(), Ok(ApprovalDecision::Approved)));

    // Atomic decide: the entry was taken under the write lock, so a second
    // decide cannot resolve the same oneshot. The decided entry is retained
    // briefly so a repeat is reported as a conflict, not a miss.
    assert!(matches!(
        gate.decide(id, true, args_hash(&args)),
        Err(ApprovalError::AlreadyDecided(_))
    ));
}

#[test]
fn decide_deny_resolves_with_a_reason() {
    let gate = ApprovalGate::default();
    let args = json!({"amount": 100});
    let (id, mut rx) = gate.request("pay__send", &args);

    gate.decide(id, false, args_hash(&args)).unwrap();
    match rx.try_recv() {
        Ok(ApprovalDecision::Denied { reason }) => assert!(!reason.is_empty()),
        other => panic!("expected Denied with a reason, got {other:?}"),
    }
}

#[test]
fn decide_with_wrong_hash_rejects() {
    let gate = ApprovalGate::default();
    let args = json!({"amount": 100, "to": "alice"});
    let (id, mut rx) = gate.request("pay__send", &args);

    let tampered = json!({"amount": 200, "to": "alice"});
    assert!(matches!(
        gate.decide(id, true, args_hash(&tampered)),
        Err(ApprovalError::ArgsChanged)
    ));
    // A mismatch does not resolve the pending: the human can retry with the
    // correct hash, still bound to the original arguments.
    assert!(rx.try_recv().is_err());

    gate.decide(id, true, args_hash(&args))
        .expect("the correct hash resolves the original pending");
    assert!(matches!(rx.try_recv(), Ok(ApprovalDecision::Approved)));
}

#[test]
fn expired_pending_is_pruned_and_rejected() {
    let gate = ApprovalGate::new(Duration::from_millis(20));
    let args = json!({"table": "users"});
    let (id, mut rx) = gate.request("db__drop_table", &args);

    std::thread::sleep(Duration::from_millis(60));
    assert!(matches!(
        gate.decide(id, true, args_hash(&args)),
        Err(ApprovalError::Expired(_))
    ));
    // Rejecting an expired pending drops its sender, closing the receiver.
    assert!(rx.try_recv().is_err());

    // The reaper also runs on `request`: a stale pending is gone by the time
    // the next request arrives, while a live one is untouched.
    let gate = ApprovalGate::new(Duration::from_millis(20));
    let (stale_id, _stale_rx) = gate.request("old__tool", &json!({}));
    std::thread::sleep(Duration::from_millis(60));
    let (fresh_id, mut fresh_rx) = gate.request("new__tool", &json!({}));
    assert!(matches!(
        gate.decide(stale_id, true, args_hash(&json!({}))),
        Err(ApprovalError::NotFound(_))
    ));
    gate.decide(fresh_id, true, args_hash(&json!({})))
        .expect("the fresh pending survives the reaper");
    assert!(matches!(
        fresh_rx.try_recv(),
        Ok(ApprovalDecision::Approved)
    ));
}

#[test]
fn args_hash_is_deterministic_for_reordered_keys() {
    let a = json!({"b": 2, "a": 1, "nested": {"y": true, "x": "s"}});
    let b = json!({"nested": {"x": "s", "y": true}, "a": 1, "b": 2});
    assert_eq!(args_hash(&a), args_hash(&b));
    assert_eq!(args_hash_hex(&a), args_hash_hex(&b));

    let hex = args_hash_hex(&a);
    assert_eq!(hex.len(), 64, "SHA-256 is 32 bytes as lowercase hex");
    assert!(hex
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_ne!(args_hash(&a), args_hash(&json!({"a": 1})));
}

#[test]
fn approval_errors_map_to_the_expected_http_statuses() {
    use axum::http::StatusCode;
    use llama_r::error::AppError;

    let id = Uuid::new_v4();
    let missing: AppError = ApprovalError::NotFound(id).into();
    assert_eq!(missing.status_code(), StatusCode::NOT_FOUND);
    assert_eq!(missing.code(), "approval_not_found");

    let expired: AppError = ApprovalError::Expired(id).into();
    assert_eq!(expired.status_code(), StatusCode::GONE);
    assert_eq!(expired.code(), "approval_expired");

    let decided: AppError = ApprovalError::AlreadyDecided(id).into();
    assert_eq!(decided.status_code(), StatusCode::CONFLICT);
    assert_eq!(decided.code(), "approval_already_decided");

    let changed: AppError = ApprovalError::ArgsChanged.into();
    assert_eq!(changed.status_code(), StatusCode::BAD_REQUEST);
    assert_eq!(changed.code(), "approval_args_changed");
}

#[tokio::test]
async fn approval_required_event_maps_to_sse_without_breaking_other_events() {
    let id = Uuid::new_v4();
    let args = json!({"table": "users"});
    let expected_hash = args_hash_hex(&args);
    let (tx, rx) = mpsc::channel(8);

    tx.send(AgentRunEvent::Token {
        text: "working".to_string(),
    })
    .await
    .unwrap();
    tx.send(AgentRunEvent::ApprovalRequired {
        id,
        tool_name: "db__drop_table".to_string(),
        args: args.clone(),
        task_id: None,
        depth: None,
    })
    .await
    .unwrap();
    tx.send(AgentRunEvent::Completed {
        result: AgentRunResult {
            text: String::new(),
            tool_calls: 0,
            iterations: 0,
            model: "m".to_string(),
        },
    })
    .await
    .unwrap();
    drop(tx);

    let mut stream = engine_events_to_chat_stream("m".to_string(), rx);
    let mut events: Vec<ChatStreamEvent> = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.expect("stream must not error"));
    }
    assert_eq!(events.len(), 3);

    let token = &events[0];
    assert!(token.approval.is_none());
    assert_eq!(token.message.content, "working");
    assert!(!token.done);
    let token_json = serde_json::to_value(token).unwrap();
    assert!(
        !token_json.as_object().unwrap().contains_key("approval"),
        "existing SSE clients must not see a new key on ordinary events"
    );

    let approval = &events[1];
    let info = approval
        .approval
        .as_ref()
        .expect("ApprovalRequired carries approval info");
    assert_eq!(info.id, id.to_string());
    assert_eq!(info.tool_name, "db__drop_table");
    assert_eq!(info.args_hash, expected_hash);
    assert!(
        !approval.done,
        "the request stays open while awaiting a human"
    );
    let approval_json = serde_json::to_value(approval).unwrap();
    assert!(approval_json["approval"].is_object());

    let done = &events[2];
    assert!(done.approval.is_none());
    assert!(done.done);
}

// ---------------------------------------------------------------------------
// Task 4: real bridges gated on a real approval decision
// ---------------------------------------------------------------------------

/// MCP client that counts dispatches. A gated tool must not reach `call_tool`
/// until a human approves.
#[derive(Default)]
struct CountingMcp {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl McpClient for CountingMcp {
    async fn list_tools(&self, _server_id: &str) -> Result<Vec<McpToolDef>, String> {
        Ok(Vec::new())
    }
    async fn call_tool(&self, _req: McpCallRequest) -> Result<McpCallResult, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(McpCallResult {
            content: json!("mcp ok"),
            is_error: false,
        })
    }
    async fn health(&self, _server_id: &str) -> Result<(), String> {
        Ok(())
    }
}

fn mcp_scope(permission: PermissionPolicy) -> AgentScope {
    AgentScope {
        agent_id: "t".into(),
        project_id: None,
        mcp_sources: HashSet::from(["mock".into()]),
        tools_allow: ToolsAllow::AllFromSources,
        local_tools_allow: ToolsAllow::DenyAll,
        rag_sources: HashSet::new(),
        rag_write: RagWritePolicy::None,
        max_tool_calls: 10,
        max_iterations: 5,
        timeout_secs: 30,
        permission,
    }
}

fn local_scope(permission: PermissionPolicy) -> AgentScope {
    AgentScope {
        agent_id: "t".into(),
        project_id: None,
        mcp_sources: HashSet::new(),
        tools_allow: ToolsAllow::DenyAll,
        local_tools_allow: ToolsAllow::AllFromSources,
        rag_sources: HashSet::new(),
        rag_write: RagWritePolicy::None,
        max_tool_calls: 10,
        max_iterations: 5,
        timeout_secs: 30,
        permission,
    }
}

fn always_for(pattern: &str) -> PermissionPolicy {
    PermissionPolicy {
        connections_allow: Vec::new(),
        approvals: vec![ApprovalRule {
            tool_pattern: pattern.into(),
            policy: ApprovalPolicy::Always,
        }],
        auto_threshold: None,
    }
}

fn mcp_bridge(
    permission: PermissionPolicy,
    gate: Arc<ApprovalGate>,
    event_tx: Option<mpsc::Sender<AgentRunEvent>>,
    budget: Arc<AsyncMutex<ToolCallBudget>>,
) -> (McpToolBridge, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = ScopedMcpTool {
        def: McpToolDef {
            server_id: "mock".into(),
            name: "drop".into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
        },
        client: Arc::new(CountingMcp {
            calls: calls.clone(),
        }),
        scope: mcp_scope(permission),
    };
    (McpToolBridge::new(inner, budget, gate, event_tx), calls)
}

fn local_bridge(
    permission: PermissionPolicy,
    gate: Arc<ApprovalGate>,
    event_tx: Option<mpsc::Sender<AgentRunEvent>>,
    budget: Arc<AsyncMutex<ToolCallBudget>>,
    skill_root: &Path,
) -> LocalToolBridge {
    let manager = SkillManager::with_paths(vec![skill_root.to_path_buf()]);
    manager.scan_and_load();
    let deps = LocalToolDeps {
        skill_manager: Arc::new(manager),
        rag: None,
        gate,
        auto_decider: None,
        observability: None,
        delegator: None,
        delegate_bounds: None,
        delegation_context: None,
        event_tx,
        tool_event_tx: None,
        operations: llama_r::adapters::rig_engine::operations::OperationContext::disabled(),
    };
    let tool = LocalToolRegistry::builtins()
        .tools()
        .iter()
        .find(|t| t.id == BUILTIN_SKILL_ID)
        .unwrap()
        .clone();
    LocalToolBridge::new(tool, local_scope(permission), budget, deps)
}

fn write_skill(dir: &Path, id: &str) {
    let skill_dir = dir.join(id);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {id}\ndescription: demo\n---\n\nbody\n"),
    )
    .unwrap();
}

fn approval_id(event: Option<AgentRunEvent>) -> Uuid {
    match event.expect("an ApprovalRequired event must be emitted") {
        AgentRunEvent::ApprovalRequired { id, tool_name, .. } => {
            assert!(!tool_name.is_empty());
            id
        }
        other => panic!("expected ApprovalRequired, got {other:?}"),
    }
}

#[tokio::test]
async fn always_tool_blocks_until_decide_approve_then_executes() {
    let gate = Arc::new(ApprovalGate::default());
    let (tx, mut rx) = mpsc::channel(8);
    let budget = Arc::new(AsyncMutex::new(ToolCallBudget::new(5)));
    let (bridge, calls) = mcp_bridge(
        always_for("mock__drop"),
        gate.clone(),
        Some(tx),
        budget.clone(),
    );
    let args = json!({"table": "users"});

    let decide_gate = gate.clone();
    let expected_args = args.clone();
    let decider = tokio::spawn(async move {
        let id = approval_id(rx.recv().await);
        decide_gate
            .decide(id, true, args_hash(&expected_args))
            .unwrap();
    });

    let result = bridge.invoke_json(args).await.expect("approved tool runs");
    decider.await.unwrap();
    assert_eq!(result, "\"mcp ok\"");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "tool executed exactly once"
    );
    assert_eq!(
        budget.lock().await.used(),
        1,
        "budget charged once, on execution"
    );
}

#[tokio::test]
async fn denied_tool_does_not_execute_and_budget_not_charged() {
    let gate = Arc::new(ApprovalGate::default());
    let (tx, mut rx) = mpsc::channel(8);
    let budget = Arc::new(AsyncMutex::new(ToolCallBudget::new(5)));
    let (bridge, calls) = mcp_bridge(
        always_for("mock__drop"),
        gate.clone(),
        Some(tx),
        budget.clone(),
    );
    let args = json!({"table": "users"});

    let decide_gate = gate.clone();
    let expected_args = args.clone();
    let decider = tokio::spawn(async move {
        let id = approval_id(rx.recv().await);
        decide_gate
            .decide(id, false, args_hash(&expected_args))
            .unwrap();
    });

    let err = bridge
        .invoke_json(args)
        .await
        .expect_err("denied tool must not run");
    decider.await.unwrap();
    assert!(err.contains("denied"), "named refusal: {err}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "denied tool never reached MCP"
    );
    assert_eq!(budget.lock().await.used(), 0, "denial charges no budget");
}

#[tokio::test]
async fn args_changed_between_request_and_decide_rejects() {
    let gate = Arc::new(ApprovalGate::default());
    let (tx, mut rx) = mpsc::channel(8);
    let budget = Arc::new(AsyncMutex::new(ToolCallBudget::new(5)));
    let (bridge, calls) = mcp_bridge(
        always_for("mock__drop"),
        gate.clone(),
        Some(tx),
        budget.clone(),
    );
    let args = json!({"table": "users", "rows": 10});

    let decide_gate = gate.clone();
    let expected_args = args.clone();
    let seen_calls = calls.clone();
    let decider = tokio::spawn(async move {
        let id = approval_id(rx.recv().await);
        let tampered = json!({"table": "users", "rows": 999});
        assert_eq!(
            decide_gate
                .decide(id, true, args_hash(&tampered))
                .unwrap_err(),
            ApprovalError::ArgsChanged,
            "a decision bound to different args is rejected"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            seen_calls.load(Ordering::SeqCst),
            0,
            "no execution while the human submitted bad args"
        );
        decide_gate
            .decide(id, true, args_hash(&expected_args))
            .unwrap();
    });

    bridge
        .invoke_json(args)
        .await
        .expect("correct hash approves");
    decider.await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(budget.lock().await.used(), 1);
}

#[tokio::test]
async fn approval_timeout_returns_named_error() {
    let gate = Arc::new(ApprovalGate::new(Duration::from_millis(60)));
    let (tx, _rx) = mpsc::channel(8);
    let budget = Arc::new(AsyncMutex::new(ToolCallBudget::new(5)));
    let (bridge, calls) = mcp_bridge(always_for("mock__drop"), gate, Some(tx), budget.clone());

    let err = bridge
        .invoke_json(json!({"table": "users"}))
        .await
        .expect_err("no decision must time out with a named error");
    assert!(
        err.contains("timed out") || err.contains("expired"),
        "named timeout error: {err}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "timed-out tool never ran");
    assert_eq!(budget.lock().await.used(), 0, "timeout charges no budget");
}

#[tokio::test]
async fn one_budget_across_mcp_local_and_approval_path() {
    let gate = Arc::new(ApprovalGate::default());
    let (tx, mut rx) = mpsc::channel(8);
    // A single allowance shared by both bridges. The second approved tool must
    // be refused at charge time, proving the charge happens after approval and
    // on the SAME budget MCP and local tools spend.
    let budget = Arc::new(AsyncMutex::new(ToolCallBudget::new(1)));
    let (mcp, mcp_calls) = mcp_bridge(
        always_for("mock__drop"),
        gate.clone(),
        Some(tx.clone()),
        budget.clone(),
    );
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "demo");
    let local = local_bridge(
        always_for(BUILTIN_SKILL_ID),
        gate.clone(),
        Some(tx),
        budget.clone(),
        dir.path(),
    );

    let decide_gate = gate.clone();
    let decider = tokio::spawn(async move {
        // Approve both pending calls in the order they are emitted, hashing
        // the exact args carried by each event (the gate binds to them).
        // `ToolCall`/`ToolResult` are informative events from the same channel;
        // only `ApprovalRequired` carries a decision to make.
        let mut decided = 0;
        while decided < 2 {
            if let AgentRunEvent::ApprovalRequired { id, args, .. } =
                rx.recv().await.expect("bridge event")
            {
                decide_gate.decide(id, true, args_hash(&args)).unwrap();
                decided += 1;
            }
        }
    });

    mcp.invoke_json(json!({"table": "users"}))
        .await
        .expect("first approved tool executes");
    let local_err = local
        .invoke_json(json!({"name": "demo"}))
        .await
        .expect_err("the shared budget is exhausted by the first tool");
    decider.await.unwrap();
    assert!(
        local_err.contains("max_tool_calls exceeded"),
        "budget error after approval: {local_err}"
    );
    assert_eq!(mcp_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        budget.lock().await.used(),
        1,
        "only one charge across both bridges"
    );
}

#[tokio::test]
async fn never_rule_executes_without_approval() {
    // No rules at all: the default `Never` allows. No event is emitted and the
    // tool runs immediately — the pre-HITL behavior is preserved.
    let gate = Arc::new(ApprovalGate::default());
    let (tx, mut rx) = mpsc::channel(8);
    let budget = Arc::new(AsyncMutex::new(ToolCallBudget::new(5)));
    let (bridge, calls) = mcp_bridge(PermissionPolicy::default(), gate, Some(tx), budget.clone());

    bridge
        .invoke_json(json!({"table": "users"}))
        .await
        .expect("Never-policy tool executes directly");
    // Bridges also emit informative `ToolCall`/`ToolResult` events now; what
    // must not appear is an approval request for a tool with no matching rule.
    let mut saw_approval = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, AgentRunEvent::ApprovalRequired { .. }) {
            saw_approval = true;
        }
    }
    assert!(!saw_approval, "no ApprovalRequired event for Never tools");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(budget.lock().await.used(), 1);
}

#[tokio::test]
async fn build_tool_bridges_carries_gate_to_both_bridge_kinds() {
    // Prove the production construction path (not just direct `new`) threads
    // the gate into both MCP and local bridges.
    let gate = Arc::new(ApprovalGate::default());
    let (tx, mut rx) = mpsc::channel(8);
    let budget = Arc::new(AsyncMutex::new(ToolCallBudget::new(5)));
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "demo");

    let manager = SkillManager::with_paths(vec![dir.path().to_path_buf()]);
    manager.scan_and_load();
    let deps = LocalToolDeps {
        skill_manager: Arc::new(manager),
        rag: None,
        gate: gate.clone(),
        auto_decider: None,
        observability: None,
        delegator: None,
        delegate_bounds: None,
        delegation_context: None,
        event_tx: Some(tx),
        tool_event_tx: None,
        operations: llama_r::adapters::rig_engine::operations::OperationContext::disabled(),
    };
    let mcp_tool = ScopedMcpTool {
        def: McpToolDef {
            server_id: "mock".into(),
            name: "drop".into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
        },
        client: Arc::new(CountingMcp {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        scope: mcp_scope(always_for("mock__drop")),
    };
    let local_tool = LocalToolRegistry::builtins()
        .tools()
        .iter()
        .find(|t| t.id == BUILTIN_SKILL_ID)
        .unwrap()
        .clone();
    let bridges = build_tool_bridges(
        vec![ScopedTool::Mcp(mcp_tool), ScopedTool::Local(local_tool)],
        &local_scope(always_for(BUILTIN_SKILL_ID)),
        Some(budget),
        deps,
    );
    assert_eq!(bridges.len(), 2);

    let decide_gate = gate.clone();
    let decider = tokio::spawn(async move {
        // Both bridge kinds emit on the same channel; approve each approval
        // request with the exact args it carried, ignoring informative events.
        let mut decided = 0;
        while decided < 2 {
            if let AgentRunEvent::ApprovalRequired { id, args, .. } =
                rx.recv().await.expect("bridge event")
            {
                decide_gate.decide(id, true, args_hash(&args)).unwrap();
                decided += 1;
            }
        }
    });
    // Both bridges must run through the gate that `build_tool_bridges` injected.
    bridges[0]
        .call(json!({"table": "users"}).to_string())
        .await
        .expect("gated MCP bridge runs after approval");
    bridges[1]
        .call(json!({"name": "demo"}).to_string())
        .await
        .expect("gated local bridge runs after approval");
    decider.await.unwrap();
}

// ---------------------------------------------------------------------------
// Task 5: approval API + end-to-end SSE proof
// ---------------------------------------------------------------------------

type BoxErr = Box<dyn Error + Send + Sync>;

/// A provider never reached by the approval endpoints — it only lets us build a
/// minimal `AppState` around a caller-owned gate.
struct NoopProvider;

#[async_trait]
impl LLMProvider for NoopProvider {
    fn get_base_url(&self) -> String {
        "http://noop".into()
    }
    async fn health_check(&self) -> Result<(), BoxErr> {
        Ok(())
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, BoxErr> {
        Ok(vec![])
    }
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, BoxErr> {
        Err("noop provider".into())
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, BoxErr>> + Send>>, BoxErr> {
        Err("noop provider".into())
    }
}

fn api_state(gate: Arc<ApprovalGate>) -> Arc<AppState> {
    build_app_state_with_gate(
        gate,
        ProviderStack::new(Arc::new(NoopProvider), "m"),
        Arc::new(AgentRegistry::new()),
        Arc::new(SkillManager::new()),
        Arc::new(ContextStore::new()),
        Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        Vec::new(),
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
        Arc::new(llama_r::adapters::execution::NoOpExecutionStore),
    )
}

async fn call_api(
    state: &Arc<AppState>,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> axum::response::Response {
    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(payload))
        .unwrap();
    build_router(state.clone()).oneshot(request).await.unwrap()
}

#[tokio::test]
async fn approval_routes_are_registered_and_list_redacts_secrets() {
    let gate = Arc::new(ApprovalGate::default());
    let state = api_state(gate.clone());
    let args = json!({
        "table": "users",
        "api_token": "sk-DUMMY-SECRET",
        "nested": {"password": "p"}
    });
    let (id, _rx) = gate.request("db__drop_table", &args);

    // The route is registered (not a 404) and the pending is listed with its
    // secret-bearing fields scrubbed — the gate holds the raw args, but the API
    // must never echo them back to a caller.
    let resp = call_api(&state, "GET", "/api/approvals", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let list: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    let entry = &list[0];
    assert_eq!(entry["id"], id.to_string());
    assert_eq!(entry["tool_name"], "db__drop_table");
    assert_eq!(entry["args"]["table"], "users");
    assert_eq!(entry["args"]["api_token"], REDACTED_MARKER);
    assert_eq!(entry["args"]["nested"]["password"], REDACTED_MARKER);
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains("sk-DUMMY-SECRET"), "secret leaked over HTTP");
    // The hash is bound to what the human saw, but never published on this list.
    assert!(!entry.as_object().unwrap().contains_key("args_hash"));
}

#[tokio::test]
async fn decide_endpoint_maps_bad_hash_missing_and_already_decided() {
    let gate = Arc::new(ApprovalGate::default());
    let state = api_state(gate.clone());
    let args = json!({"table": "users"});
    let correct = args_hash_hex(&args);
    let (id, _rx) = gate.request("db__drop_table", &args);

    // Wrong hash -> 400, and the pending is left intact for a retry.
    let resp = call_api(
        &state,
        "POST",
        &format!("/api/approvals/{id}"),
        Some(json!({"approve": true, "args_hash": "00".repeat(32)})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Unknown id -> 404.
    let unknown = Uuid::new_v4();
    let resp = call_api(
        &state,
        "POST",
        &format!("/api/approvals/{unknown}"),
        Some(json!({"approve": true, "args_hash": correct.clone()})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Omitted hash uses the gate's stored one -> 200 (this is how a
    // non-streaming client, with no SSE event to echo, decides).
    let resp = call_api(
        &state,
        "POST",
        &format!("/api/approvals/{id}"),
        Some(json!({"approve": true})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Deciding the same id again -> 409, distinguishable from 404 above.
    let resp = call_api(
        &state,
        "POST",
        &format!("/api/approvals/{id}"),
        Some(json!({"approve": false, "args_hash": correct})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // The decided pending is no longer offered for a decision.
    let resp = call_api(&state, "GET", "/api/approvals", None).await;
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let list: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 0);
}

// --- end-to-end: a gated tool driven through run_stream -> SSE -------------

const E2E_SKILL: &str = "x";
const E2E_SKILL_BODY: &str = "GATED SKILL BODY MARKER";
const E2E_FINAL: &str = "done after approval";

/// Fake Ollama playing both halves of the tool loop on a real socket: the first
/// completion calls the gated local tool, the one after the tool result returns
/// the final answer. Every raw request body is recorded so the test can prove
/// whether the tool actually ran.
struct GatedToolServer {
    addr: SocketAddr,
    bodies: Arc<std::sync::Mutex<Vec<String>>>,
    handle: JoinHandle<()>,
}

impl GatedToolServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = bodies.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let Some(body) = read_http_body(&mut socket).await else {
                    return;
                };
                let is_tool_result = body.contains("\"role\":\"tool\"");
                sink.lock().unwrap().push(body.clone());
                // Rig's Ollama streaming client reassembles NDJSON lines and
                // only emits a frame once its trailing newline arrives.
                let payload = if is_tool_result {
                    json!({
                        "model": "llama3.2",
                        "created_at": "2024-01-01T00:00:00Z",
                        "message": {"role": "assistant", "content": E2E_FINAL},
                        "done": true
                    })
                } else {
                    json!({
                        "model": "llama3.2",
                        "created_at": "2024-01-01T00:00:00Z",
                        "message": {
                            "role": "assistant",
                            "content": "",
                            "tool_calls": [{
                                "type": "function",
                                "function": {
                                    "name": BUILTIN_SKILL_ID,
                                    "arguments": {"name": E2E_SKILL, "api_key": "sk-E2E-SECRET"}
                                }
                            }]
                        },
                        "done": true
                    })
                };
                let payload = format!("{payload}\n");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        Self {
            addr,
            bodies,
            handle,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }
}

impl Drop for GatedToolServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn read_http_body(socket: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let header_end = loop {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let len = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let total = header_end + len;
    while buf.len() < total {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Some(String::from_utf8_lossy(&buf[header_end..total]).to_string())
}

fn e2e_config(ollama_url: &str) -> Config {
    Config {
        port: 3000,
        provider: ProviderKind::Ollama,
        ollama_url: ollama_url.to_string(),
        default_model: "llama3.2".to_string(),
        embedding_model: "nomic-embed-text".to_string(),
        embedding_dimensions: 768,
        rag_enabled: true,
        history_retention_days: 90,
        openrouter_api_key: None,
        opencode_api_key: None,
        opencode_url: "http://127.0.0.1:4096".to_string(),
        openai_base_url: None,
    }
}

fn e2e_request() -> AgentRunRequest {
    AgentRunRequest {
        agent: Agent {
            id: "a".to_string(),
            project_id: None,
            config: AgentConfig::default(),
        },
        scope: AgentScope {
            agent_id: "a".to_string(),
            project_id: None,
            mcp_sources: HashSet::new(),
            tools_allow: ToolsAllow::DenyAll,
            local_tools_allow: ToolsAllow::AllFromSources,
            rag_sources: HashSet::new(),
            rag_write: RagWritePolicy::None,
            max_tool_calls: 1,
            max_iterations: 2,
            timeout_secs: 10,
            permission: always_for(BUILTIN_SKILL_ID),
        },
        model: "llama3.2".to_string(),
        provider: ProviderKind::Ollama,
        capabilities: capabilities_for(ProviderKind::Ollama, "llama3.2"),
        system_prompt: "sys".to_string(),
        user_message: "load the skill".to_string(),
        history: Vec::new(),
        conversation_id: None,
        execution_id: Uuid::new_v4(),
        shared_budget: None,
        delegation_context: None,
        approval_event_tx: None,
        tool_event_tx: None,
    }
}

/// Drive one gated tool through the whole chain: real engine `run_stream` ->
/// real bridge -> real gate -> engine event channel -> SSE mapping -> GET/POST
/// endpoints -> tool loop continues. Returns the tokens, the raw model requests
/// and any stream errors so each test can assert its own outcome.
async fn drive_hitl(approve: bool) -> (Vec<String>, Vec<String>, Vec<String>) {
    let server = GatedToolServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("skills").join(E2E_SKILL);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {E2E_SKILL}\ndescription: demo\n---\n{E2E_SKILL_BODY}"),
    )
    .unwrap();
    let skill_manager = Arc::new(SkillManager::with_paths(vec![dir.path().join("skills")]));
    skill_manager.scan_and_load();

    let gate = Arc::new(ApprovalGate::default());
    let engine = RigAgentEngine::from_target(
        EngineTarget::Ollama { url: server.url() },
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
        e2e_config(&server.url()),
        skill_manager,
        Arc::new(LocalToolRegistry::builtins()),
        gate.clone(),
    );
    // The same gate is installed on AppState, so the HTTP endpoints see the
    // pending the engine registered and can unblock the waiting bridge.
    let state = api_state(gate);

    let rx = engine
        .run_stream(e2e_request())
        .await
        .expect("run_stream must open the event channel");
    let mut stream = engine_events_to_chat_stream("llama3.2".to_string(), rx);

    // Consume the SSE stream until the approval event arrives. The bridge is
    // blocked on the gate while the request stays open.
    let (approval_id, approval_hash) = loop {
        let item = stream
            .next()
            .await
            .expect("the stream stays open while a human decides")
            .expect("no stream error before the approval event");
        if let Some(info) = item.approval {
            break (info.id, info.args_hash);
        }
        if item.done {
            panic!("the stream finished without an approval event");
        }
    };

    // The client discovers the same pending through GET and sees a redacted
    // argument, proving the non-streaming discovery path and the redaction
    // requirement together.
    let resp = call_api(&state, "GET", "/api/approvals", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let list: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let pending = &list[0];
    assert_eq!(pending["id"], approval_id);
    assert_eq!(pending["args"]["api_key"], REDACTED_MARKER);
    assert!(!String::from_utf8_lossy(&bytes).contains("sk-E2E-SECRET"));

    // Decide through POST with the hash echo from the SSE event.
    let resp = call_api(
        &state,
        "POST",
        &format!("/api/approvals/{approval_id}"),
        Some(json!({"approve": approve, "args_hash": approval_hash})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Drain the rest of the stream.
    let mut tokens = Vec::new();
    let mut errors = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(event) => {
                if event.done {
                    break;
                }
                if !event.message.content.is_empty() {
                    tokens.push(event.message.content);
                }
            }
            Err(err) => errors.push(err.to_string()),
        }
    }
    (tokens, server.bodies(), errors)
}

#[tokio::test]
async fn gated_tool_approved_via_api_completes_the_sse_stream() {
    let (tokens, bodies, errors) = drive_hitl(true).await;
    assert!(
        errors.is_empty(),
        "the approved path must not error: {errors:?}"
    );
    assert_eq!(
        tokens.concat(),
        E2E_FINAL,
        "final answer arrives after approval"
    );
    assert_eq!(bodies.len(), 2, "a tool call then a final answer");
    assert!(
        bodies[1].contains("\"role\":\"tool\"") && bodies[1].contains(E2E_SKILL_BODY),
        "the gated tool actually executed and returned its body"
    );
}

#[tokio::test]
async fn gated_tool_denied_via_api_never_executes() {
    let (_tokens, bodies, errors) = drive_hitl(false).await;
    assert!(
        errors.is_empty(),
        "a denial is a named tool result, not a stream error: {errors:?}"
    );
    // Rig feeds a tool error back as a tool result and the model still produces
    // a closing turn. What matters is that the *tool* never ran: the result is
    // the denial, never the gated skill's body, and no secret is echoed.
    assert_eq!(bodies.len(), 2, "the denial is reported back to the model");
    assert!(
        bodies[1].contains("approval denied"),
        "the tool result names the denial"
    );
    assert!(
        !bodies[1].contains(E2E_SKILL_BODY),
        "the denied tool never loaded the skill body"
    );
}

// --- Fase 3 final fix: approval deadline must outlive the engine timeout ----
//
// The bridge waits on the gate for `gate.timeout()` (default 120s), but the
// `AgentEngine` boundary used to wrap the whole run in a shorter scope timeout
// (default 90s). When that outer deadline fired first it dropped the bridge's
// pending oneshot (a late decision became a no-op) and, non-streaming, handed
// chat_core an `Err` that triggered its legacy, ungated direct-chat fallback.
// With approval rules the outer timeout is now `max(scope, gate + margin)`, so
// the bridge's named refusal fires first and rig continues the turn.

/// Build a real engine whose gate deadline and scope deadline are both short
/// and configurable, pointed at a fake Ollama that calls the gated skill tool.
async fn short_timeout_engine(
    gate_timeout: Duration,
    scope_timeout_secs: u64,
    permission: PermissionPolicy,
    skill_root: &Path,
) -> (RigAgentEngine, GatedToolServer, AgentRunRequest) {
    let server = GatedToolServer::start().await;
    let skill_dir = skill_root.join("skills").join(E2E_SKILL);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {E2E_SKILL}\ndescription: demo\n---\n{E2E_SKILL_BODY}"),
    )
    .unwrap();
    let skill_manager = Arc::new(SkillManager::with_paths(vec![skill_root.join("skills")]));
    skill_manager.scan_and_load();
    let engine = RigAgentEngine::from_target(
        EngineTarget::Ollama { url: server.url() },
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
        e2e_config(&server.url()),
        skill_manager,
        Arc::new(LocalToolRegistry::builtins()),
        Arc::new(ApprovalGate::new(gate_timeout)),
    );
    let mut req = e2e_request();
    req.scope.timeout_secs = scope_timeout_secs;
    req.scope.permission = permission;
    (engine, server, req)
}

#[tokio::test]
async fn approval_timeout_outlives_engine_timeout_nonstreaming() {
    let dir = tempfile::tempdir().unwrap();
    // gate 2s, scope 1s, tool gated: the named approval refusal must win before
    // the engine's extended (7s) deadline. No decision is ever posted, so the
    // bridge's own 2s timeout is the only thing that can end the wait.
    let (engine, server, req) = short_timeout_engine(
        Duration::from_secs(2),
        1,
        always_for(BUILTIN_SKILL_ID),
        dir.path(),
    )
    .await;

    let result = engine.run(req).await;
    match &result {
        Ok(r) => assert!(
            !r.text.is_empty(),
            "the turn continues after the named refusal"
        ),
        Err(e) => panic!("engine must not fail with a generic timeout: {e}"),
    }
    let bodies = server.bodies();
    assert!(
        bodies.iter().any(|b| b.contains("approval timed out")),
        "the named approval timeout must be fed back as a tool result: {bodies:?}"
    );
    assert!(
        !bodies.concat().contains("agent run timed out after 1s"),
        "the short scope deadline must not have fired first"
    );
}

#[tokio::test]
async fn approval_timeout_outlives_engine_timeout_streaming() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, server, req) = short_timeout_engine(
        Duration::from_secs(2),
        1,
        always_for(BUILTIN_SKILL_ID),
        dir.path(),
    )
    .await;

    let mut rx = engine
        .run_stream(req)
        .await
        .expect("run_stream opens the event channel");
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    for event in &events {
        if let AgentRunEvent::Error { message } = event {
            assert!(
                !message.contains("agent stream timed out after 1s"),
                "the short scope deadline must not leak as a stream error: {message}"
            );
        }
    }
    let bodies = server.bodies();
    assert!(
        bodies.iter().any(|b| b.contains("approval timed out")),
        "the named approval timeout must be fed back as a tool result: {bodies:?}"
    );
}

/// A fake Ollama that answers later than the scope deadline, used to prove a
/// request with *no* approval rules keeps the unchanged short timeout.
struct SlowFinalServer {
    addr: SocketAddr,
    handle: JoinHandle<()>,
}

impl SlowFinalServer {
    async fn start(delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let Some(_body) = read_http_body(&mut socket).await else {
                return;
            };
            tokio::time::sleep(delay).await;
            let payload = format!(
                "{}\n",
                json!({
                    "model": "llama3.2",
                    "created_at": "2024-01-01T00:00:00Z",
                    "message": {"role": "assistant", "content": "slow final"},
                    "done": true
                })
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        });
        Self { addr, handle }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for SlowFinalServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[tokio::test]
async fn no_approval_rules_keep_the_short_scope_timeout() {
    let server = SlowFinalServer::start(Duration::from_millis(1500)).await;
    let engine = RigAgentEngine::from_target(
        EngineTarget::Ollama { url: server.url() },
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
        e2e_config(&server.url()),
        Arc::new(SkillManager::new()),
        Arc::new(LocalToolRegistry::builtins()),
        // A long gate timeout must NOT stretch the engine deadline when the
        // request carries no approval rules.
        Arc::new(ApprovalGate::new(Duration::from_secs(120))),
    );
    let mut req = e2e_request();
    req.scope.timeout_secs = 1;
    req.scope.permission = PermissionPolicy::default();
    req.scope.local_tools_allow = ToolsAllow::DenyAll;

    let err = engine
        .run(req)
        .await
        .expect_err("the slow answer must exceed the unchanged 1s scope deadline");
    assert_eq!(err, "agent run timed out after 1s");
}
