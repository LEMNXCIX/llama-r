use crate::ports::mcp::{McpCallRequest, McpCallResult, McpClient, McpToolDef};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

/// Timeout decorator over an MCP client.
///
/// Applies `tokio::time::timeout` to every operation so that a stuck
/// MCP server never blocks the runtime indefinitely.
pub struct TimeoutMcpClient {
    inner: Arc<dyn McpClient>,
    timeout: Duration,
}

impl TimeoutMcpClient {
    pub fn new(inner: Arc<dyn McpClient>, timeout: Duration) -> Self {
        Self { inner, timeout }
    }
}

#[async_trait]
impl McpClient for TimeoutMcpClient {
    async fn list_tools(&self, server_id: &str) -> Result<Vec<McpToolDef>, String> {
        let sid = server_id.to_string();
        tokio::time::timeout(self.timeout, self.inner.list_tools(&sid))
            .await
            .map_err(|_| format!("MCP list_tools timeout for server '{sid}'"))?
    }

    async fn call_tool(&self, req: McpCallRequest) -> Result<McpCallResult, String> {
        let sid = req.server_id.clone();
        let tool_name = req.tool_name.clone();
        let token = req.cancel_token.clone();

        if let Some(token) = token {
            tokio::select! {
                res = tokio::time::timeout(self.timeout, self.inner.call_tool(req)) => {
                    res.map_err(|_| format!("MCP call_tool timeout for server '{sid}' tool '{tool_name}'"))?
                }
                _ = token.cancelled() => {
                    Err(format!("MCP call_tool cancelled for server '{sid}' tool '{tool_name}'"))
                }
            }
        } else {
            tokio::time::timeout(self.timeout, self.inner.call_tool(req))
                .await
                .map_err(|_| {
                    format!("MCP call_tool timeout for server '{sid}' tool '{tool_name}'")
                })?
        }
    }

    async fn health(&self, server_id: &str) -> Result<(), String> {
        let sid = server_id.to_string();
        tokio::time::timeout(self.timeout, self.inner.health(&sid))
            .await
            .map_err(|_| format!("MCP health timeout for server '{sid}'"))?
    }
}
