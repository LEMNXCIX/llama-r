//! Task 2: local (in-process) tools are authorized independently of MCP tools.
//!
//! `tools_local` travels from the agent manifest into `AgentScope.local_tools_allow`
//! and is checked by `allows_local_tool`. An MCP permission must never grant a
//! local tool, and a local permission must never require an MCP source.

use llama_r::domain::agent::{Agent, AgentConfig};
use llama_r::domain::scope::{ToolRef, ToolsAllow};
use llama_r::services::scope_builder::ScopeBuilder;

fn agent_with(tools_local: &[&str], tools_override: &[&str], mcp_sources: &[&str]) -> Agent {
    Agent {
        id: "demo".into(),
        project_id: None,
        config: AgentConfig {
            name: "Demo".into(),
            system_prompt: "hi".into(),
            tools_local: tools_local.iter().map(|s| (*s).to_string()).collect(),
            tools_override: tools_override.iter().map(|s| (*s).to_string()).collect(),
            mcp_sources: mcp_sources.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        },
    }
}

#[test]
fn empty_tools_local_is_deny_all() {
    let agent = agent_with(&[], &[], &[]);
    let scope = ScopeBuilder::build(&agent).unwrap();

    assert!(matches!(scope.local_tools_allow, ToolsAllow::DenyAll));
    assert!(!scope.allows_local_tool("llama_r__skill"));
}

#[test]
fn wildcard_allows_all_local_tools() {
    // No MCP sources at all: the wildcard is a local grant, not an MCP one.
    let agent = agent_with(&["*"], &[], &[]);
    let scope = ScopeBuilder::build(&agent).unwrap();

    assert!(matches!(
        scope.local_tools_allow,
        ToolsAllow::AllFromSources
    ));
    assert!(scope.allows_local_tool("llama_r__skill"));
    assert!(scope.allows_local_tool("any_local_id"));
    assert!(!scope.allows_tool("any", "tool"));
}

#[test]
fn an_explicit_local_allowlist_works() {
    let agent = agent_with(&["llama_r__skill", "llama_r__rag_query"], &[], &[]);
    let scope = ScopeBuilder::build(&agent).unwrap();

    match &scope.local_tools_allow {
        ToolsAllow::Allowlist(set) => {
            // Local ids have no server: they are bare ToolRefs by construction.
            assert!(set.contains(&ToolRef {
                server_id: None,
                name: "llama_r__skill".into(),
            }));
        }
        other => panic!("expected an allowlist, got {other:?}"),
    }
    assert!(scope.allows_local_tool("llama_r__skill"));
    assert!(scope.allows_local_tool("llama_r__rag_query"));
    assert!(!scope.allows_local_tool("unknown"));
}

#[test]
fn mcp_permissions_do_not_grant_local_tools() {
    let agent = agent_with(&[], &["*"], &["fudi"]);
    let scope = ScopeBuilder::build(&agent).unwrap();

    assert!(scope.allows_tool("fudi", "list_orders"));
    assert!(!scope.allows_local_tool("list_orders"));
}
