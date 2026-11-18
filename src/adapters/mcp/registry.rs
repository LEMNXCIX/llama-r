//! In-memory MCP server registry loaded from config structs.

use crate::adapters::mcp::http::{HttpMcpClient, ResolvedAuth};
use crate::adapters::mcp::namespaced::NamespacedMcpClient;
use crate::adapters::mcp::openapi::OpenApiConnection;
use crate::adapters::mcp::stdio::StdioMcpClient;
use crate::adapters::mcp::streaming::StreamingHttpMcpClient;
use crate::ports::mcp::{McpClient, McpServerRegistry};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// How a connection authenticates against its upstream.
///
/// Defaults to `Bearer` so a v1 config that only sets `auth_env` keeps working.
/// `None` must be requested explicitly (`auth = "none"`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthKind {
    #[default]
    Bearer,
    Basic,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub id: String,
    #[serde(default = "default_transport")]
    pub transport: String,
    pub url: Option<String>,
    /// Environment variable holding the bearer token / basic password (never store secrets in TOML).
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
    /// Extra request headers. A value of the form `$VAR` resolves from the environment
    /// at build time. Connection headers/auth are applied last so callers cannot spoof them.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub auth: AuthKind,
    /// Environment variable holding the Basic username (password comes from `auth_env`).
    #[serde(default)]
    pub auth_user_env: Option<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            transport: default_transport(),
            url: None,
            auth_env: None,
            command: None,
            args: Vec::new(),
            timeout_secs: default_timeout(),
            tool_namespace: None,
            enabled: default_enabled(),
            headers: Vec::new(),
            auth: AuthKind::default(),
            auth_user_env: None,
        }
    }
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

/// Resolve custom headers, expanding `$VAR` values from the environment. A missing
/// variable is skipped with a warning naming only the variable (never a secret value).
fn resolve_headers(config: &McpServerConfig) -> Vec<(String, String)> {
    config
        .headers
        .iter()
        .filter_map(|(name, value)| {
            let resolved = match value.strip_prefix('$') {
                Some(var) => match std::env::var(var) {
                    Ok(value) => value,
                    Err(_) => {
                        tracing::warn!(
                            server = %config.id,
                            header = %name,
                            env_var = %var,
                            "MCP header environment variable is not set; skipping header"
                        );
                        return None;
                    }
                },
                None => value.clone(),
            };
            Some((name.clone(), resolved))
        })
        .collect()
}

/// Resolve auth from environment variable *names*. No secret is logged.
fn resolve_auth(config: &McpServerConfig) -> ResolvedAuth {
    let from_env = |name: Option<&String>| -> Option<String> {
        name.and_then(|name| std::env::var(name).ok())
            .filter(|value| !value.is_empty())
    };
    match config.auth {
        AuthKind::Bearer => match from_env(config.auth_env.as_ref()) {
            Some(token) => ResolvedAuth::Bearer(token),
            None => ResolvedAuth::None,
        },
        AuthKind::Basic => {
            let username = from_env(config.auth_user_env.as_ref()).unwrap_or_default();
            let password = from_env(config.auth_env.as_ref()).unwrap_or_default();
            if username.is_empty() && password.is_empty() {
                ResolvedAuth::None
            } else {
                ResolvedAuth::Basic { username, password }
            }
        }
        AuthKind::None => ResolvedAuth::None,
    }
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
            if let Some(client) = Self::build_client(config)? {
                clients.insert(config.id.clone(), client);
            }
        }
        let mut guard = self
            .clients
            .write()
            .map_err(|_| "StaticMcpRegistry lock poisoned".to_string())?;
        *guard = clients;
        Ok(())
    }

    /// Returns `None` when a connection is deliberately skipped (invalid OpenAPI spec).
    /// Other configuration errors remain fatal so typos are not silently swallowed.
    fn build_client(config: &McpServerConfig) -> Result<Option<Arc<dyn McpClient>>, String> {
        let timeout = Duration::from_secs(config.timeout_secs);
        let client: Arc<dyn McpClient> = match config.transport.as_str() {
            "http" => {
                let url = config
                    .url
                    .clone()
                    .ok_or_else(|| format!("MCP server '{}' missing url", config.id))?;
                let mut client = HttpMcpClient::new(config.id.clone(), url)
                    .with_timeout(timeout)
                    .with_headers(resolve_headers(config));
                client = match resolve_auth(config) {
                    ResolvedAuth::Basic { username, password } => {
                        client.with_basic_auth(username, password)
                    }
                    ResolvedAuth::Bearer(token) => client.with_auth(Some(token)),
                    ResolvedAuth::None => client,
                };
                Arc::new(client)
            }
            "sse" | "streaming" => {
                let url = config
                    .url
                    .clone()
                    .ok_or_else(|| format!("MCP server '{}' missing url", config.id))?;
                let mut client = StreamingHttpMcpClient::new(config.id.clone(), url)
                    .with_timeout(timeout)
                    .with_headers(resolve_headers(config));
                client = match resolve_auth(config) {
                    ResolvedAuth::Basic { username, password } => {
                        client.with_basic_auth(username, password)
                    }
                    ResolvedAuth::Bearer(token) => client.with_auth(Some(token)),
                    ResolvedAuth::None => client,
                };
                Arc::new(client)
            }
            "openapi" => {
                let spec_path = config.url.clone().ok_or_else(|| {
                    format!("MCP OpenAPI server '{}' missing spec path (url)", config.id)
                })?;
                match OpenApiConnection::from_spec_path(
                    config.id.clone(),
                    spec_path,
                    resolve_auth(config),
                    resolve_headers(config),
                    timeout,
                ) {
                    Ok(client) => Arc::new(client),
                    Err(err) => {
                        tracing::warn!(
                            server = %config.id,
                            error = %err,
                            "Skipping MCP connection with invalid OpenAPI spec"
                        );
                        return Ok(None);
                    }
                }
            }
            "stdio" => {
                let command = config
                    .command
                    .clone()
                    .ok_or_else(|| format!("MCP stdio server '{}' missing command", config.id))?;
                let args = config.args.clone();
                let client = StdioMcpClient::new(config.id.clone(), command, args, timeout);
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
                return Ok(Some(Arc::new(NamespacedMcpClient::new(client, ns.clone()))));
            }
        }

        Ok(Some(client))
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
