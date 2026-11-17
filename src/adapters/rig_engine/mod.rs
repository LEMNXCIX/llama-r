//! Rig.rs agent engine adapter (feature-gated wiring documented in ARCHITECTURE_PLAN).

#[cfg(feature = "rig-engine")]
pub mod builder;
pub mod limits;
#[cfg(feature = "rig-engine")]
pub mod tools;

use crate::config::{Config, ProviderKind, OPENROUTER_BASE_URL};
use crate::domain::scope::AgentScope;
use crate::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult};
use crate::ports::mcp::{McpCallRequest, McpClient, McpServerRegistry, McpToolDef};
use crate::ports::rag::RagStore;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Which upstream the Rig engine talks to.
///
/// The key lives only inside the `OpenRouter` variant. This type deliberately
/// has **no** `Debug` implementation: an engine target must never be logged or
/// formatted into a response, because doing so would leak the API key. Use
/// [`EngineTarget::kind`] for any diagnostic that does not need the secret.
#[derive(Clone)]
pub enum EngineTarget {
    Ollama { url: String },
    OpenRouter { api_key: String, base_url: String },
}

/// Non-secret discriminator for a target, safe to log and to put in health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineTargetKind {
    Ollama,
    OpenRouter,
}

impl EngineTarget {
    pub fn kind(&self) -> EngineTargetKind {
        match self {
            EngineTarget::Ollama { .. } => EngineTargetKind::Ollama,
            EngineTarget::OpenRouter { .. } => EngineTargetKind::OpenRouter,
        }
    }
}

/// Select the engine target from the single `PROVIDER` choice.
///
/// Ollama and OpenRouter both have a rig-core completion provider; OpenCode
/// does not, so it gets **no engine** (and an agent-scoped chat degrades with an
/// explicit warning rather than calling the wrong upstream).
pub fn engine_target_for(config: &Config) -> Option<EngineTarget> {
    match config.provider {
        ProviderKind::Ollama => Some(EngineTarget::Ollama {
            url: config.ollama_url.clone(),
        }),
        ProviderKind::OpenRouter => Some(EngineTarget::OpenRouter {
            api_key: config.openrouter_api_key.clone().unwrap_or_default(),
            base_url: config
                .openai_base_url
                .clone()
                .unwrap_or_else(|| OPENROUTER_BASE_URL.to_string()),
        }),
        ProviderKind::OpenCode => None,
    }
}

/// MCP tool already filtered by scope, re-checked on every invoke.
pub struct ScopedMcpTool {
    pub def: McpToolDef,
    pub client: Arc<dyn McpClient>,
    pub scope: AgentScope,
}

impl ScopedMcpTool {
    pub async fn invoke(&self, args: Value) -> Result<String, String> {
        if !self.scope.allows_tool(&self.def.server_id, &self.def.name) {
            return Err(format!(
                "tool '{}/{}' denied by agent scope",
                self.def.server_id, self.def.name
            ));
        }

        tracing::info!(
            server_id = %self.def.server_id,
            tool = %self.def.name,
            "scoped MCP tool invoke"
        );

        let result = self
            .client
            .call_tool(McpCallRequest::new(
                self.def.server_id.clone(),
                self.def.name.clone(),
                args,
            ))
            .await?;

        if result.is_error {
            return Err(result.content.to_string());
        }
        Ok(result.content.to_string())
    }
}

/// Engine implementation backed by a rig-core completion provider. The target
/// decides which provider that is; the Ollama-only original was hardwired to
/// `rig_core::providers::ollama`, which made an agent-scoped chat on another
/// provider call the wrong upstream and then silently fall back to a direct
/// chat (losing tools, RAG context and MCP).
pub struct RigAgentEngine {
    pub target: EngineTarget,
    pub mcp_registry: Arc<dyn McpServerRegistry>,
    pub rag: Option<Arc<dyn RagStore>>,
}

impl RigAgentEngine {
    /// Ergonomic Ollama constructor, kept for existing call sites.
    pub fn new(
        ollama_url: impl Into<String>,
        mcp_registry: Arc<dyn McpServerRegistry>,
        rag: Option<Arc<dyn RagStore>>,
    ) -> Self {
        Self::from_target(
            EngineTarget::Ollama {
                url: ollama_url.into(),
            },
            mcp_registry,
            rag,
        )
    }

    /// Build an engine targeting OpenRouter. `base_url` overrides the default
    /// endpoint for a compatible gateway; `None` uses rig-core's default.
    pub fn with_openrouter(
        api_key: impl Into<String>,
        base_url: Option<impl Into<String>>,
        mcp_registry: Arc<dyn McpServerRegistry>,
        rag: Option<Arc<dyn RagStore>>,
    ) -> Self {
        Self::from_target(
            EngineTarget::OpenRouter {
                api_key: api_key.into(),
                base_url: base_url
                    .map(Into::into)
                    .unwrap_or_else(|| OPENROUTER_BASE_URL.to_string()),
            },
            mcp_registry,
            rag,
        )
    }

    pub fn from_target(
        target: EngineTarget,
        mcp_registry: Arc<dyn McpServerRegistry>,
        rag: Option<Arc<dyn RagStore>>,
    ) -> Self {
        Self {
            target,
            mcp_registry,
            rag,
        }
    }

    /// Non-secret target discriminator, safe for logs and health.
    pub fn target_kind(&self) -> EngineTargetKind {
        self.target.kind()
    }

    /// Prepare scoped tools and optional RAG context for a run.
    pub async fn prepare(
        &self,
        req: &AgentRunRequest,
    ) -> Result<(Vec<ScopedMcpTool>, String), String> {
        let scope = &req.scope;
        let tool_defs = self.mcp_registry.discover_scoped_tools(scope).await?;

        let mut tools = Vec::with_capacity(tool_defs.len());
        for def in tool_defs {
            let client = self.mcp_registry.client_for(&def.server_id).await?;
            tools.push(ScopedMcpTool {
                def,
                client,
                scope: scope.clone(),
            });
        }

        let mut system = req.system_prompt.clone();
        if let Some(rag) = &self.rag {
            if !scope.rag_sources.is_empty() {
                let hits = rag
                    .query_scoped(scope, &req.user_message, 6)
                    .await
                    .unwrap_or_default();
                if !hits.is_empty() {
                    system.push_str("\n\n## Retrieved knowledge\n");
                    for (index, hit) in hits.iter().enumerate() {
                        system.push_str(&format!(
                            "\n### Chunk {} (source: {}, score: {:.3})\n{}\n",
                            index + 1,
                            hit.source_id,
                            hit.score,
                            hit.text
                        ));
                    }
                }
            }
        }

        Ok((tools, system))
    }

    /// Full run body (prepare + Rig prompt). Wrapped by timeout at the trait boundary.
    async fn run_inner(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        let (tools, system) = self.prepare(&req).await?;

        #[cfg(feature = "rig-engine")]
        {
            match &self.target {
                EngineTarget::Ollama { url } => {
                    builder::run_with_rig(url, req, system, tools).await
                }
                EngineTarget::OpenRouter { api_key, base_url } => {
                    builder::run_with_openrouter(
                        api_key,
                        Some(base_url.as_str()),
                        req,
                        system,
                        tools,
                    )
                    .await
                }
            }
        }

        #[cfg(not(feature = "rig-engine"))]
        {
            let _ = (system, tools);
            Err("feature `rig-engine` disabled".into())
        }
    }
}

#[async_trait]
impl AgentEngine for RigAgentEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        let timeout_secs = req.scope.timeout_secs.max(1);
        match tokio::time::timeout(Duration::from_secs(timeout_secs), self.run_inner(req)).await {
            Ok(result) => result,
            Err(_) => Err(format!("agent run timed out after {timeout_secs}s")),
        }
    }

    async fn run_stream(
        &self,
        req: AgentRunRequest,
    ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
        let (tx, rx) = mpsc::channel(32);
        // Clone the whole target, not just a URL: the spawned task must target
        // the same provider (with its key) as the engine it came from.
        let target = self.target.clone();
        let mcp_registry = self.mcp_registry.clone();
        let rag = self.rag.clone();
        let timeout_secs = req.scope.timeout_secs.max(1);

        tokio::spawn(async move {
            let engine = RigAgentEngine {
                target,
                mcp_registry,
                rag,
            };
            let result = match tokio::time::timeout(
                Duration::from_secs(timeout_secs),
                engine.run_inner(req),
            )
            .await
            {
                Ok(inner) => inner,
                Err(_) => Err(format!("agent run timed out after {timeout_secs}s")),
            };

            match result {
                Ok(res) => {
                    let _ = tx
                        .send(AgentRunEvent::Token {
                            text: res.text.clone(),
                        })
                        .await;
                    let _ = tx.send(AgentRunEvent::Completed { result: res }).await;
                }
                Err(msg) => {
                    let _ = tx.send(AgentRunEvent::Error { message: msg }).await;
                }
            }
        });

        Ok(rx)
    }
}
