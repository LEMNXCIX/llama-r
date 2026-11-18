//! Basic MCP client over HTTP JSON-RPC.
//!
//! Compatible with servers that accept POST JSON-RPC at a single endpoint
//! (`tools/list`, `tools/call`, `initialize`). Streamable-HTTP/SSE refinements
//! can layer on top without changing the [`McpClient`] port.

use crate::ports::mcp::{McpCallRequest, McpCallResult, McpClient, McpToolDef};
use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue, AUTHORIZATION};
use reqwest::Client;
use serde_json::{json, Value};
use std::fmt;
use std::time::Duration;

/// A connection credential resolved from environment variable *names* at build time.
/// The secret value never lives in config or logs — only the resolved header does.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum ResolvedAuth {
    Bearer(String),
    Basic {
        username: String,
        password: String,
    },
    #[default]
    None,
}

/// Never let a derived `Debug` put a credential into a log line.
impl fmt::Debug for ResolvedAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bearer(_) => f.write_str("Bearer(<redacted>)"),
            Self::Basic { .. } => f.write_str("Basic(<redacted>)"),
            Self::None => f.write_str("None"),
        }
    }
}

/// Apply a connection's auth and custom headers to an outgoing HTTP request.
///
/// Custom headers are inserted first and the connection's auth is inserted last, so a
/// value supplied by a caller (for example an OpenAPI `Authorization` header parameter
/// coming from tool arguments) can never override or append to the configured credential.
/// `HeaderMap::insert` replaces every previous value, guaranteeing exactly one value.
pub fn apply_connection_headers(
    request: &mut reqwest::Request,
    auth: &ResolvedAuth,
    headers: &[(String, String)],
) -> Result<(), String> {
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|err| format!("invalid MCP header name '{name}': {err}"))?;
        let value = HeaderValue::from_str(value)
            .map_err(|err| format!("invalid value for MCP header '{name}': {err}"))?;
        request.headers_mut().insert(name, value);
    }

    match auth {
        ResolvedAuth::Bearer(token) => {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| "invalid bearer token for MCP connection".to_string())?;
            request.headers_mut().insert(AUTHORIZATION, value);
        }
        ResolvedAuth::Basic { username, password } => {
            let credentials = format!("{username}:{password}");
            let value =
                HeaderValue::from_str(&format!("Basic {}", base64_encode(credentials.as_bytes())))
                    .map_err(|_| "invalid basic auth for MCP connection".to_string())?;
            request.headers_mut().insert(AUTHORIZATION, value);
        }
        ResolvedAuth::None => {}
    }
    Ok(())
}

/// Minimal RFC 4648 base64 encoder (no extra dependency needed for Basic auth).
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[derive(Clone)]
pub struct HttpMcpClient {
    http: Client,
    server_id: String,
    base_url: String,
    auth: ResolvedAuth,
    headers: Vec<(String, String)>,
    timeout: Duration,
}

impl HttpMcpClient {
    pub fn new(server_id: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            server_id: server_id.into(),
            base_url: base_url.into(),
            auth: ResolvedAuth::None,
            headers: Vec::new(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Legacy bearer-token setter, kept for callers that only have a token.
    pub fn with_auth(mut self, token: Option<String>) -> Self {
        self.auth = match token {
            Some(token) if !token.is_empty() => ResolvedAuth::Bearer(token),
            _ => ResolvedAuth::None,
        };
        self
    }

    pub fn with_basic_auth(mut self, username: String, password: String) -> Self {
        self.auth = ResolvedAuth::Basic { username, password };
        self
    }

    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
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

        let builder = self
            .http
            .post(&self.base_url)
            .timeout(self.timeout)
            .header("Content-Type", "application/json")
            .json(&body);
        let mut request = builder.build().map_err(|err| err.to_string())?;
        apply_connection_headers(&mut request, &self.auth, &self.headers)?;

        let response = self
            .http
            .execute(request)
            .await
            .map_err(|err| err.to_string())?;
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
