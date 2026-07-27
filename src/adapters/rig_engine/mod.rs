//! Rig.rs agent engine adapter (feature-gated wiring documented in ARCHITECTURE_PLAN).
//!
//! This module provides a compile-ready bridge that enforces scope on every tool
//! call. Full `rig-core` integration is enabled once the dependency is added:
//!
//! ```toml
//! [features]
//! rig-engine = ["dep:rig-core"]
//! ```

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

/// Engine implementation. Until `rig-core` is added, `run` returns a clear error
/// after performing scoped tool discovery + optional RAG retrieve (side-effect free
/// prep that production code will reuse).
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

        // Future: construct rig agent with preamble=system, tools=tools, model=req.model
        // against Ollama at self.ollama_url, respecting scope.max_iterations / max_tool_calls.
        let _ = (system, &self.ollama_url, req.model.clone());

        Err(format!(
            "RigAgentEngine not fully wired (scoped tools prepared: {}). \
             Add rig-core dependency and complete adapter (ARCHITECTURE_PLAN §6.6 / Fase 3).",
            tools.len()
        ))
    }

    async fn run_stream(
        &self,
        _req: AgentRunRequest,
    ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
        Err("RigAgentEngine streaming not implemented yet".into())
    }
}
