//! MCP client over SSE / Streamable HTTP.
//!
//! Handles streaming responses and Server-Sent Events (SSE) from streamable MCP servers.

use crate::ports::mcp::{CancellationToken, McpCallRequest, McpCallResult, McpClient, McpToolDef};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_stream::StreamExt;

#[derive(Clone)]
pub struct StreamingHttpMcpClient {
    http: Client,
    server_id: String,
    base_url: String,
    auth_token: Option<String>,
    timeout: Duration,
}

impl StreamingHttpMcpClient {
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

    async fn rpc(
        &self,
        method: &str,
        params: Value,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<Value, String> {
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
            .header("Accept", "text/event-stream, application/json")
            .json(&body);

        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }

        let send_fut = req.send();

        let response = if let Some(ct) = cancel_token {
            tokio::select! {
                res = send_fut => res.map_err(|err| err.to_string())?,
                _ = ct.cancelled() => return Err(format!("MCP streaming RPC request cancelled for '{}'", self.server_id)),
            }
        } else {
            send_fut.await.map_err(|err| err.to_string())?
        };

        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("text/event-stream") {
            let mut stream = response.bytes_stream();
            let mut buffer = String::new();
            let mut last_result: Option<Value> = None;

            loop {
                let chunk_opt = if let Some(ct) = cancel_token {
                    tokio::select! {
                        chunk = stream.next() => chunk,
                        _ = ct.cancelled() => return Err(format!("MCP streaming read cancelled for '{}'", self.server_id)),
                    }
                } else {
                    stream.next().await
                };

                let chunk = match chunk_opt {
                    Some(Ok(bytes)) => bytes,
                    Some(Err(e)) => return Err(e.to_string()),
                    None => break,
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].trim().to_string();
                    buffer = buffer[pos + 1..].to_string();

                    if let Some(data) = line.strip_prefix("data:") {
                        let data = data.trim();
                        if !data.is_empty() {
                            if let Ok(parsed) = serde_json::from_str::<Value>(data) {
                                if let Some(error) = parsed.get("error") {
                                    return Err(format!("MCP RPC error: {error}"));
                                }
                                if let Some(res) = parsed.get("result") {
                                    last_result = Some(res.clone());
                                } else {
                                    last_result = Some(parsed);
                                }
                            }
                        }
                    }
                }
            }

            if let Some(result) = last_result {
                return Ok(result);
            }
            Err("SSE stream ended without result data".to_string())
        } else {
            let value: Value = response.json().await.map_err(|err| err.to_string())?;

            if !status.is_success() {
                return Err(format!("MCP Streaming HTTP {status}: {value}"));
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
}

#[async_trait]
impl McpClient for StreamingHttpMcpClient {
    async fn list_tools(&self, server_id: &str) -> Result<Vec<McpToolDef>, String> {
        if server_id != self.server_id {
            return Err(format!(
                "client bound to '{}', requested '{}'",
                self.server_id, server_id
            ));
        }

        let result = self.rpc("tools/list", json!({}), None).await?;
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

        let result = self
            .rpc(
                "tools/call",
                json!({
                    "name": req.tool_name,
                    "arguments": req.arguments,
                }),
                req.cancel_token.as_ref(),
            )
            .await?;

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
                None,
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
