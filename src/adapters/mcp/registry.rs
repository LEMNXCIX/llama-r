//! In-memory MCP server registry loaded from config structs.

use crate::adapters::mcp::http::HttpMcpClient;
use crate::adapters::mcp::namespaced::NamespacedMcpClient;
use crate::adapters::mcp::stdio::StdioMcpClient;
use crate::adapters::mcp::streaming::StreamingHttpMcpClient;
use crate::ports::mcp::{McpClient, McpServerRegistry};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub id: String,
    #[serde(default = "default_transport")]
    pub transport: String,
    pub url: Option<String>,
    /// Environment variable holding the bearer token (never store secrets in TOML).
    pub auth_env: Option<String>,
    /// Command for stdio transport (e.g. "npx", "python").
    pub command: Option<String>,
    /// Arguments passed to the stdio command.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    pub tool_namespace: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_transport() -> String {
    "http".to_string()
}

fn default_timeout() -> u64 {
    30
}

fn default_enabled() -> bool {
    true
}

/// Static registry: server id → live client. Thread-safe for concurrent reads and reloads.
pub struct StaticMcpRegistry {
    clients: RwLock<HashMap<String, Arc<dyn McpClient>>>,
}

impl StaticMcpRegistry {
    pub fn new() -> Self {
        Self {
            clients: RwLock::new(HashMap::new()),
        }
    }

    pub fn from_configs(configs: &[McpServerConfig]) -> Result<Self, String> {
        let registry = Self::new();
        registry.reload_from_configs(configs)?;
        Ok(registry)
    }

    /// Replace all clients from configs (used on startup and hot-reload).
    pub fn reload_from_configs(&self, configs: &[McpServerConfig]) -> Result<(), String> {
        let mut clients = HashMap::new();
        for config in configs {
            if !config.enabled {
                continue;
            }
            let client = Self::build_client(config)?;
            clients.insert(config.id.clone(), client);
        }
        let mut guard = self
            .clients
            .write()
            .map_err(|_| "StaticMcpRegistry lock poisoned".to_string())?;
        *guard = clients;
        Ok(())
    }

    fn build_client(config: &McpServerConfig) -> Result<Arc<dyn McpClient>, String> {
        let client: Arc<dyn McpClient> = match config.transport.as_str() {
            "http" => {
                let url = config
                    .url
                    .clone()
                    .ok_or_else(|| format!("MCP server '{}' missing url", config.id))?;
                let token = config
                    .auth_env
                    .as_ref()
                    .and_then(|name| std::env::var(name).ok());
                let client = HttpMcpClient::new(config.id.clone(), url)
                    .with_auth(token)
                    .with_timeout(Duration::from_secs(config.timeout_secs));
                Arc::new(client)
            }
            "sse" | "streaming" => {
                let url = config
                    .url
                    .clone()
                    .ok_or_else(|| format!("MCP server '{}' missing url", config.id))?;
                let token = config
                    .auth_env
                    .as_ref()
                    .and_then(|name| std::env::var(name).ok());
                let client = StreamingHttpMcpClient::new(config.id.clone(), url)
                    .with_auth(token)
                    .with_timeout(Duration::from_secs(config.timeout_secs));
                Arc::new(client)
            }
            "stdio" => {
                let command = config
                    .command
                    .clone()
                    .ok_or_else(|| format!("MCP stdio server '{}' missing command", config.id))?;
                let args = config.args.clone();
                let client = StdioMcpClient::new(
                    config.id.clone(),
                    command,
                    args,
                    Duration::from_secs(config.timeout_secs),
                );
                Arc::new(client)
            }
            other => {
                return Err(format!(
                    "unsupported MCP transport '{other}' for server '{}'",
                    config.id
                ))
            }
        };

        if let Some(ref ns) = config.tool_namespace {
            if !ns.is_empty() {
                return Ok(Arc::new(NamespacedMcpClient::new(client, ns.clone())));
            }
        }

        Ok(client)
    }

    pub fn insert_client(&self, server_id: impl Into<String>, client: Arc<dyn McpClient>) {
        if let Ok(mut guard) = self.clients.write() {
            guard.insert(server_id.into(), client);
        }
    }
}

impl Default for StaticMcpRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl McpServerRegistry for StaticMcpRegistry {
    async fn list_server_ids(&self) -> Vec<String> {
        self.clients
            .read()
            .map(|guard| guard.keys().cloned().collect())
            .unwrap_or_default()
    }

    async fn client_for(&self, server_id: &str) -> Result<Arc<dyn McpClient>, String> {
        self.clients
            .read()
            .map_err(|_| "StaticMcpRegistry lock poisoned".to_string())?
            .get(server_id)
            .cloned()
            .ok_or_else(|| format!("MCP server '{server_id}' is not registered"))
    }
}
