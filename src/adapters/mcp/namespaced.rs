//! Decorator for namespacing MCP tools per server configuration.

use crate::ports::mcp::{McpCallRequest, McpCallResult, McpClient, McpToolDef};
use async_trait::async_trait;
use std::sync::Arc;

/// Namespacing decorator over an MCP client.
///
/// Prepends `{namespace}_` (or `{namespace}/`) to tool names returned by `list_tools`,
/// and strips `{namespace}_` or `{namespace}/` from tool names passed to `call_tool`.
pub struct NamespacedMcpClient {
    inner: Arc<dyn McpClient>,
    namespace: String,
}

impl NamespacedMcpClient {
    pub fn new(inner: Arc<dyn McpClient>, namespace: impl Into<String>) -> Self {
        Self {
            inner,
            namespace: namespace.into(),
        }
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }
}

#[async_trait]
impl McpClient for NamespacedMcpClient {
    async fn list_tools(&self, server_id: &str) -> Result<Vec<McpToolDef>, String> {
        let mut tools = self.inner.list_tools(server_id).await?;
        let prefix_underscore = format!("{}_", self.namespace);
        let prefix_slash = format!("{}/", self.namespace);

        for tool in &mut tools {
            if !tool.name.starts_with(&prefix_underscore) && !tool.name.starts_with(&prefix_slash)
            {
                tool.name = format!("{}_{}", self.namespace, tool.name);
            }
        }
        Ok(tools)
    }

    async fn call_tool(&self, mut req: McpCallRequest) -> Result<McpCallResult, String> {
        let prefix_underscore = format!("{}_", self.namespace);
        let prefix_slash = format!("{}/", self.namespace);

        if req.tool_name.starts_with(&prefix_underscore) {
            req.tool_name = req.tool_name[prefix_underscore.len()..].to_string();
        } else if req.tool_name.starts_with(&prefix_slash) {
            req.tool_name = req.tool_name[prefix_slash.len()..].to_string();
        }

        self.inner.call_tool(req).await
    }

    async fn health(&self, server_id: &str) -> Result<(), String> {
        self.inner.health(server_id).await
    }
}
