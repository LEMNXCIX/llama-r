//! Basic MCP client over HTTP JSON-RPC.
//!
//! Compatible with servers that accept POST JSON-RPC at a single endpoint
//! (`tools/list`, `tools/call`, `initialize`). Streamable-HTTP/SSE refinements
//! can layer on top without changing the [`McpClient`] port.

use crate::ports::mcp::{McpCallRequest, McpCallResult, McpClient, McpToolDef};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct HttpMcpClient {
    http: Client,
    server_id: String,
    base_url: String,
    auth_token: Option<String>,
    timeout: Duration,
}

impl HttpMcpClient {
    pub fn new(server_id: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            server_id: server_id.into(),
            base_url: base_url.into(),
            auth_token: None,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_auth(mut self, token: Option<String>) -> Self {
        self.auth_token = token;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": request_id(),
            "method": method,
            "params": params,
        });

        let mut req = self
            .http
            .post(&self.base_url)
            .timeout(self.timeout)
            .header("Content-Type", "application/json")
            .json(&body);

        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }

        let response = req.send().await.map_err(|err| err.to_string())?;
        let status = response.status();
        let value: Value = response.json().await.map_err(|err| err.to_string())?;

        if !status.is_success() {
            return Err(format!("MCP HTTP {status}: {value}"));
        }
        if let Some(error) = value.get("error") {
            return Err(format!("MCP RPC error: {error}"));
        }

        value
            .get("result")
            .cloned()
            .ok_or_else(|| "MCP response missing result".to_string())
    }
}

#[async_trait]
impl McpClient for HttpMcpClient {
    async fn list_tools(&self, server_id: &str) -> Result<Vec<McpToolDef>, String> {
        if server_id != self.server_id {
            return Err(format!(
                "client bound to '{}', requested '{}'",
                self.server_id, server_id
            ));
        }

        let result = self.rpc("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|value| value.as_array())
            .ok_or_else(|| "invalid tools/list result".to_string())?;

        Ok(tools
            .iter()
            .filter_map(|tool| {
                Some(McpToolDef {
                    server_id: self.server_id.clone(),
                    name: tool.get("name")?.as_str()?.to_string(),
                    description: tool
                        .get("description")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                })
            })
            .collect())
    }

    async fn call_tool(&self, req: McpCallRequest) -> Result<McpCallResult, String> {
        if req.server_id != self.server_id {
            return Err("server_id mismatch".to_string());
        }

        let fut = self.rpc(
            "tools/call",
            json!({
                "name": req.tool_name,
                "arguments": req.arguments,
            }),
        );

        let result = if let Some(token) = &req.cancel_token {
            tokio::select! {
                res = fut => res?,
                _ = token.cancelled() => return Err(format!("MCP call_tool cancelled for server '{}'", self.server_id)),
            }
        } else {
            fut.await?
        };

        let is_error = result
            .get("isError")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);

        Ok(McpCallResult {
            content: result.get("content").cloned().unwrap_or(result),
            is_error,
        })
    }

    async fn health(&self, server_id: &str) -> Result<(), String> {
        if server_id != self.server_id {
            return Err("server_id mismatch".to_string());
        }

        let _ = self
            .rpc(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "llama-r", "version": "0.1.0" }
                }),
            )
            .await?;
        Ok(())
    }
}

fn request_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("llama-r-{nanos}")
}
