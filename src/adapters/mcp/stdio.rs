use crate::ports::mcp::{McpCallRequest, McpCallResult, McpClient, McpToolDef};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// MCP client over stdio subprocess.
///
/// Spawns a child process on first use and communicates via JSON-RPC
/// over stdin/stdout (one line per message).
pub struct StdioMcpClient {
    server_id: String,
    command: String,
    args: Vec<String>,
    timeout: Duration,
    child: Mutex<Option<Child>>,
}

impl StdioMcpClient {
    pub fn new(
        server_id: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            server_id: server_id.into(),
            command: command.into(),
            args,
            timeout,
            child: Mutex::new(None),
        }
    }

    async fn ensure_spawned(&self) -> Result<tokio::sync::MutexGuard<'_, Option<Child>>, String> {
        let mut guard = self.child.lock().await;
        if guard.is_none() {
            let child = Command::new(&self.command)
                .args(&self.args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .map_err(|e| {
                    format!(
                        "failed to spawn MCP stdio server '{}': {}",
                        self.server_id, e
                    )
                })?;
            *guard = Some(child);

            // Perform initialize handshake
            let result = self
                .rpc_inner(
                    &mut guard,
                    "initialize",
                    json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": { "name": "llama-r", "version": "0.1.0" }
                    }),
                )
                .await;
            if let Err(e) = result {
                // Clean up on failure: drop the child process
                guard.take();
                return Err(format!(
                    "MCP initialize handshake failed for '{}': {}",
                    self.server_id, e
                ));
            }
        }
        Ok(guard)
    }

    async fn rpc_inner(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, Option<Child>>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let child = guard
            .as_mut()
            .ok_or_else(|| "MCP child process not available".to_string())?;

        // Check process health
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "MCP stdio server '{}' exited with status {}",
                self.server_id, status
            ));
        }

        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "MCP stdin not available".to_string())?;
        let stdout = child
            .stdout
            .as_mut()
            .ok_or_else(|| "MCP stdout not available".to_string())?;

        let request_id = format!(
            "llama-r-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );

        let body = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        });

        let line = serde_json::to_string(&body).map_err(|e| e.to_string())?;

        // Write request + newline
        tokio::time::timeout(self.timeout, async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
            Ok::<_, std::io::Error>(())
        })
        .await
        .map_err(|_| format!("MCP stdio write timeout for '{}'", self.server_id))?
        .map_err(|e| format!("MCP stdio write error for '{}': {}", self.server_id, e))?;

        // Read response line
        let mut reader = BufReader::new(stdout);
        let mut response_line = String::new();
        tokio::time::timeout(self.timeout, reader.read_line(&mut response_line))
            .await
            .map_err(|_| format!("MCP stdio read timeout for '{}'", self.server_id))?
            .map_err(|e| format!("MCP stdio read error for '{}': {}", self.server_id, e))?;

        if response_line.is_empty() {
            return Err(format!(
                "MCP stdio server '{}' closed connection",
                self.server_id
            ));
        }

        let value: Value = serde_json::from_str(&response_line)
            .map_err(|e| format!("MCP stdio parse error: {}", e))?;

        if let Some(error) = value.get("error") {
            return Err(format!("MCP RPC error: {error}"));
        }

        value
            .get("result")
            .cloned()
            .ok_or_else(|| "MCP response missing result".to_string())
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut guard = self.ensure_spawned().await?;
        self.rpc_inner(&mut guard, method, params).await
    }
}

#[async_trait]
impl McpClient for StdioMcpClient {
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

        let result = self
            .rpc(
                "tools/call",
                json!({
                    "name": req.tool_name,
                    "arguments": req.arguments,
                }),
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

        // Re-initialize to verify health (idempotent for most servers)
        let mut guard = self.ensure_spawned().await?;
        self.rpc_inner(
            &mut guard,
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
