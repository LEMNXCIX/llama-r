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
