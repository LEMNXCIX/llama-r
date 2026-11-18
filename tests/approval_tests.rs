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
    // decide cannot resolve the same oneshot (and cannot panic on double send).
    assert!(matches!(
        gate.decide(id, true, args_hash(&args)),
        Err(ApprovalError::NotFound(_))
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
        event_tx,
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
        for _ in 0..2 {
            match rx.recv().await.expect("ApprovalRequired event") {
                AgentRunEvent::ApprovalRequired { id, args, .. } => {
                    decide_gate.decide(id, true, args_hash(&args)).unwrap();
                }
                other => panic!("expected ApprovalRequired, got {other:?}"),
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
    assert!(
        rx.try_recv().is_err(),
        "no ApprovalRequired event for Never tools"
    );
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
        event_tx: Some(tx),
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
        budget,
        deps,
    );
    assert_eq!(bridges.len(), 2);

    let decide_gate = gate.clone();
    let decider = tokio::spawn(async move {
        // Both bridge kinds emit on the same channel; approve each with the
        // exact args it carried.
        for _ in 0..2 {
            match rx.recv().await.expect("ApprovalRequired event") {
                AgentRunEvent::ApprovalRequired { id, args, .. } => {
                    decide_gate.decide(id, true, args_hash(&args)).unwrap();
                }
                other => panic!("expected ApprovalRequired, got {other:?}"),
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
