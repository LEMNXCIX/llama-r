//! MCP client port: discover and execute tools from external MCP servers.

use crate::domain::scope::AgentScope;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub server_id: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct McpCallRequest {
    pub server_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct McpCallResult {
    pub content: Value,
    pub is_error: bool,
}

#[async_trait]
pub trait McpClient: Send + Sync {
    async fn list_tools(&self, server_id: &str) -> Result<Vec<McpToolDef>, String>;
    async fn call_tool(&self, req: McpCallRequest) -> Result<McpCallResult, String>;
    async fn health(&self, server_id: &str) -> Result<(), String>;
}

/// Filter discovered tools through an agent scope. Always apply before the engine sees tools.
pub fn filter_tools_for_scope(tools: Vec<McpToolDef>, scope: &AgentScope) -> Vec<McpToolDef> {
    tools
        .into_iter()
        .filter(|tool| scope.allows_tool(&tool.server_id, &tool.name))
        .collect()
}

#[async_trait]
pub trait McpServerRegistry: Send + Sync {
    async fn list_server_ids(&self) -> Vec<String>;

    async fn client_for(&self, server_id: &str) -> Result<Arc<dyn McpClient>, String>;

    /// Discover tools from every `mcp_source` and apply scope allowlists.
    async fn discover_scoped_tools(&self, scope: &AgentScope) -> Result<Vec<McpToolDef>, String> {
        let mut out = Vec::new();
        for server_id in &scope.mcp_sources {
            let client = self.client_for(server_id).await?;
            let tools = client.list_tools(server_id).await?;
            out.extend(filter_tools_for_scope(tools, scope));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::RagWritePolicy;
    use crate::domain::scope::{AgentScope, ToolRef, ToolsAllow};
    use std::collections::HashSet;

    #[test]
    fn filter_respects_allowlist_and_server() {
        let scope = AgentScope {
            agent_id: "ops".into(),
            project_id: None,
            mcp_sources: HashSet::from(["fudi".into()]),
            tools_allow: ToolsAllow::Allowlist(HashSet::from([ToolRef::parse("fudi/list")])),
            rag_sources: HashSet::new(),
            rag_write: RagWritePolicy::None,
            max_tool_calls: 8,
            max_iterations: 6,
            timeout_secs: 90,
        };

        let tools = vec![
            McpToolDef {
                server_id: "fudi".into(),
                name: "list".into(),
                description: String::new(),
                input_schema: Value::Null,
            },
            McpToolDef {
                server_id: "fudi".into(),
                name: "delete".into(),
                description: String::new(),
                input_schema: Value::Null,
            },
            McpToolDef {
                server_id: "other".into(),
                name: "list".into(),
                description: String::new(),
                input_schema: Value::Null,
            },
        ];

        let filtered = filter_tools_for_scope(tools, &scope);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "list");
        assert_eq!(filtered[0].server_id, "fudi");
    }
}
