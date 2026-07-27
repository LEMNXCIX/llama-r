//! Rig.rs agent engine adapter (feature-gated wiring documented in ARCHITECTURE_PLAN).

#[cfg(feature = "rig-engine")]
pub mod builder;
pub mod limits;
#[cfg(feature = "rig-engine")]
pub mod tools;

use crate::domain::scope::AgentScope;
use crate::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult};
use crate::ports::mcp::{McpCallRequest, McpClient, McpServerRegistry, McpToolDef};
use crate::ports::rag::RagStore;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;

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

/// Engine implementation backed by rig-core Ollama provider.
pub struct RigAgentEngine {
    pub ollama_url: String,
    pub mcp_registry: Arc<dyn McpServerRegistry>,
    pub rag: Option<Arc<dyn RagStore>>,
}

impl RigAgentEngine {
    pub fn new(
        ollama_url: impl Into<String>,
        mcp_registry: Arc<dyn McpServerRegistry>,
        rag: Option<Arc<dyn RagStore>>,
    ) -> Self {
        Self {
            ollama_url: ollama_url.into(),
            mcp_registry,
            rag,
        }
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
}

#[async_trait]
impl AgentEngine for RigAgentEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        let (tools, system) = self.prepare(&req).await?;

        #[cfg(feature = "rig-engine")]
        {
            return builder::run_with_rig(&self.ollama_url, &self.mcp_registry, req, system, tools)
                .await;
        }

        #[cfg(not(feature = "rig-engine"))]
        {
            let _ = (system, tools);
            Err("feature `rig-engine` disabled".into())
        }
    }

    async fn run_stream(
        &self,
        req: AgentRunRequest,
    ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
        let (tools, system) = self.prepare(&req).await?;

        #[cfg(feature = "rig-engine")]
        {
            let (tx, rx) = mpsc::channel(32);
            let ollama_url = self.ollama_url.clone();
            let mcp_registry = self.mcp_registry.clone();

            tokio::spawn(async move {
                let result =
                    builder::run_with_rig(&ollama_url, &mcp_registry, req, system, tools).await;
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

        #[cfg(not(feature = "rig-engine"))]
        {
            let _ = (tools, system);
            Err("feature `rig-engine` disabled".into())
        }
    }
}
