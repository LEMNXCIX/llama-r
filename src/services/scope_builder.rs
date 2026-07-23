//! Builds an immutable [`AgentScope`] from an agent manifest.

use crate::domain::agent::Agent;
use crate::domain::scope::{AgentScope, ToolRef, ToolsAllow};
use crate::error::AppError;
use std::collections::HashSet;

pub struct ScopeBuilder;

impl ScopeBuilder {
    pub fn build(agent: &Agent) -> Result<AgentScope, AppError> {
        let config = &agent.config;
        let tools_allow = Self::parse_tools_override(&config.tools_override);

        if let ToolsAllow::Allowlist(ref allowlist) = tools_allow {
            for tool in allowlist {
                if let Some(ref server) = tool.server_id {
                    if !config.mcp_sources.iter().any(|source| source == server) {
                        return Err(AppError::Validation(format!(
                            "tools_override references server '{}' not listed in mcp_sources for agent '{}'",
                            server, agent.id
                        )));
                    }
                }
            }
        }

        Ok(AgentScope {
            agent_id: agent.id.clone(),
            project_id: agent.project_id.clone(),
            mcp_sources: config.mcp_sources.iter().cloned().collect(),
            tools_allow,
            rag_sources: config.rag_sources.iter().cloned().collect(),
            rag_write: config.rag_write.clone(),
            max_tool_calls: config.max_tool_calls,
            max_iterations: config.max_iterations,
            timeout_secs: config.timeout_secs,
        })
    }

    fn parse_tools_override(list: &[String]) -> ToolsAllow {
        if list.is_empty() {
            return ToolsAllow::DenyAll;
        }
        if list.iter().any(|entry| entry == "*") {
            return ToolsAllow::AllFromSources;
        }
        let set = list
            .iter()
            .map(|entry| ToolRef::parse(entry))
            .collect::<HashSet<_>>();
        ToolsAllow::Allowlist(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentConfig;
    use crate::domain::scope::ToolsAllow;

    #[test]
    fn empty_tools_override_is_deny_all() {
        let agent = Agent {
            id: "demo".into(),
            project_id: None,
            config: AgentConfig {
                name: "Demo".into(),
                system_prompt: "hi".into(),
                ..Default::default()
            },
        };
        let scope = ScopeBuilder::build(&agent).unwrap();
        assert!(matches!(scope.tools_allow, ToolsAllow::DenyAll));
    }

    #[test]
    fn rejects_tool_server_not_in_mcp_sources() {
        let agent = Agent {
            id: "demo".into(),
            project_id: None,
            config: AgentConfig {
                name: "Demo".into(),
                system_prompt: "hi".into(),
                mcp_sources: vec!["fudi".into()],
                tools_override: vec!["other/list".into()],
                ..Default::default()
            },
        };
        let err = ScopeBuilder::build(&agent).unwrap_err();
        assert!(err.to_string().contains("not listed in mcp_sources"));
    }

    #[test]
    fn star_means_all_from_sources() {
        let agent = Agent {
            id: "demo".into(),
            project_id: None,
            config: AgentConfig {
                name: "Demo".into(),
                system_prompt: "hi".into(),
                mcp_sources: vec!["fudi".into()],
                tools_override: vec!["*".into()],
                ..Default::default()
            },
        };
        let scope = ScopeBuilder::build(&agent).unwrap();
        assert!(matches!(scope.tools_allow, ToolsAllow::AllFromSources));
        assert!(scope.allows_tool("fudi", "anything"));
    }
}
