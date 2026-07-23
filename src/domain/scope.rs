//! Per-agent isolation boundary enforced at runtime.
//!
//! An [`AgentScope`] is built from the agent manifest and is immutable for the
//! duration of a request. All tool discovery, tool invocation, and RAG access
//! must go through these checks (defense in depth).

use crate::domain::agent::RagWritePolicy;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Qualified tool reference: `server_id/tool_name` or bare `tool_name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolRef {
    pub server_id: Option<String>,
    pub name: String,
}

impl ToolRef {
    pub fn parse(raw: &str) -> Self {
        if let Some((server, name)) = raw.split_once('/') {
            if !server.is_empty() && !name.is_empty() && server != "*" {
                return Self {
                    server_id: Some(server.to_string()),
                    name: name.to_string(),
                };
            }
        }
        Self {
            server_id: None,
            name: raw.to_string(),
        }
    }

    pub fn qualified(&self) -> String {
        match &self.server_id {
            Some(server) => format!("{server}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// How tools are authorized for an agent.
#[derive(Debug, Clone)]
pub enum ToolsAllow {
    /// No tools (safe default when `tools_override` is empty).
    DenyAll,
    /// Every tool discovered from the agent's `mcp_sources`.
    AllFromSources,
    /// Only the listed tools (matched by qualified or bare name).
    Allowlist(HashSet<ToolRef>),
}

/// Immutable execution scope for one agent invocation.
#[derive(Debug, Clone)]
pub struct AgentScope {
    pub agent_id: String,
    pub project_id: Option<String>,
    pub mcp_sources: HashSet<String>,
    pub tools_allow: ToolsAllow,
    pub rag_sources: HashSet<String>,
    pub rag_write: RagWritePolicy,
    pub max_tool_calls: u32,
    pub max_iterations: u32,
    pub timeout_secs: u64,
}

impl AgentScope {
    /// Own-memory collection id used by [`RagWritePolicy::OwnMemoryOnly`].
    pub fn own_memory_source_id(&self) -> String {
        match &self.project_id {
            Some(project_id) => format!("agent:{project_id}/{}/memory", self.agent_id),
            None => format!("agent:{}/memory", self.agent_id),
        }
    }

    /// Whether this scope may use a tool discovered on `server_id`.
    pub fn allows_tool(&self, server_id: &str, tool_name: &str) -> bool {
        if !self.mcp_sources.contains(server_id) {
            return false;
        }
        match &self.tools_allow {
            ToolsAllow::DenyAll => false,
            ToolsAllow::AllFromSources => true,
            ToolsAllow::Allowlist(set) => {
                let exact = ToolRef {
                    server_id: Some(server_id.to_string()),
                    name: tool_name.to_string(),
                };
                let bare = ToolRef {
                    server_id: None,
                    name: tool_name.to_string(),
                };
                set.contains(&exact) || set.contains(&bare)
            }
        }
    }

    pub fn allows_rag_read(&self, source_id: &str) -> bool {
        self.rag_sources.contains(source_id)
    }

    pub fn allows_rag_write(&self, source_id: &str) -> bool {
        match self.rag_write {
            RagWritePolicy::None => false,
            RagWritePolicy::OwnMemoryOnly => {
                source_id == self.own_memory_source_id()
                    || source_id == format!("agent:{}/memory", self.agent_id)
            }
            RagWritePolicy::Listed => self.rag_sources.contains(source_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::RagWritePolicy;

    fn base_scope(tools: ToolsAllow) -> AgentScope {
        AgentScope {
            agent_id: "ops".into(),
            project_id: Some("fudi".into()),
            mcp_sources: HashSet::from(["fudi".into(), "other".into()]),
            tools_allow: tools,
            rag_sources: HashSet::from(["fudi/policies".into(), "agent:fudi/ops/memory".into()]),
            rag_write: RagWritePolicy::OwnMemoryOnly,
            max_tool_calls: 8,
            max_iterations: 6,
            timeout_secs: 90,
        }
    }

    #[test]
    fn deny_all_blocks_even_known_servers() {
        let scope = base_scope(ToolsAllow::DenyAll);
        assert!(!scope.allows_tool("fudi", "list_orders"));
    }

    #[test]
    fn all_from_sources_requires_mcp_source() {
        let scope = base_scope(ToolsAllow::AllFromSources);
        assert!(scope.allows_tool("fudi", "list_orders"));
        assert!(!scope.allows_tool("unknown", "list_orders"));
    }

    #[test]
    fn allowlist_matches_qualified_and_bare_names() {
        let set = HashSet::from([
            ToolRef::parse("fudi/list_orders"),
            ToolRef::parse("get_order"),
        ]);
        let scope = base_scope(ToolsAllow::Allowlist(set));
        assert!(scope.allows_tool("fudi", "list_orders"));
        assert!(scope.allows_tool("fudi", "get_order"));
        assert!(!scope.allows_tool("fudi", "delete_all"));
        assert!(!scope.allows_tool("other", "list_orders"));
    }

    #[test]
    fn rag_write_own_memory_only() {
        let scope = base_scope(ToolsAllow::DenyAll);
        assert!(scope.allows_rag_write("agent:fudi/ops/memory"));
        assert!(!scope.allows_rag_write("fudi/policies"));
        assert!(scope.allows_rag_read("fudi/policies"));
        assert!(!scope.allows_rag_read("secret/other"));
    }

    #[test]
    fn tool_ref_parse() {
        let q = ToolRef::parse("fudi/list_orders");
        assert_eq!(q.server_id.as_deref(), Some("fudi"));
        assert_eq!(q.name, "list_orders");
        assert_eq!(q.qualified(), "fudi/list_orders");

        let bare = ToolRef::parse("ping");
        assert_eq!(bare.server_id, None);
        assert_eq!(bare.name, "ping");
    }
}
