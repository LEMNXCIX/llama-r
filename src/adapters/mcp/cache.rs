use crate::ports::mcp::{McpCallRequest, McpCallResult, McpClient, McpToolDef};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

struct CachedEntry {
    tools: Vec<McpToolDef>,
    cached_at: Instant,
}

/// Caching decorator over an MCP client.
///
/// Caches `list_tools` results per server with configurable TTL.
/// `call_tool` and `health` are always passed through.
pub struct CachedMcpClient {
    inner: Arc<dyn McpClient>,
    cache: RwLock<HashMap<String, CachedEntry>>,
    ttl: Duration,
}

impl CachedMcpClient {
    pub fn new(inner: Arc<dyn McpClient>, ttl: Duration) -> Self {
        Self {
            inner,
            cache: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// Invalidate cache for a specific server (e.g., after config change).
    pub fn invalidate(&self, server_id: &str) {
        if let Ok(mut guard) = self.cache.write() {
            guard.remove(server_id);
        }
    }

    /// Invalidate entire cache.
    pub fn invalidate_all(&self) {
        if let Ok(mut guard) = self.cache.write() {
            guard.clear();
        }
    }
}

#[async_trait]
impl McpClient for CachedMcpClient {
    async fn list_tools(&self, server_id: &str) -> Result<Vec<McpToolDef>, String> {
        // Check cache
        {
            let guard = self
                .cache
                .read()
                .map_err(|_| "cache lock poisoned".to_string())?;
            if let Some(entry) = guard.get(server_id) {
                if entry.cached_at.elapsed() < self.ttl {
                    return Ok(entry.tools.clone());
                }
            }
        }

        // Miss — fetch from inner
        let tools = self.inner.list_tools(server_id).await?;

        // Store in cache
        if let Ok(mut guard) = self.cache.write() {
            guard.insert(
                server_id.to_string(),
                CachedEntry {
                    tools: tools.clone(),
                    cached_at: Instant::now(),
                },
            );
        }

        Ok(tools)
    }

    async fn call_tool(&self, req: McpCallRequest) -> Result<McpCallResult, String> {
        self.inner.call_tool(req).await
    }

    async fn health(&self, server_id: &str) -> Result<(), String> {
        self.inner.health(server_id).await
    }
}
