//! Orchestrates a single agent turn: scope → (optional) history → engine → learn.

use crate::domain::scope::AgentScope;
use crate::error::AppError;
use crate::ports::engine::{AgentEngine, AgentRunRequest};
use crate::ports::history::ConversationStore;
use crate::ports::rag::RagStore;
use crate::services::agent_registry::AgentRegistry;
use std::sync::Arc;

pub struct AgentRuntime {
    pub registry: Arc<AgentRegistry>,
    pub engine: Arc<dyn AgentEngine>,
    pub history: Option<Arc<dyn ConversationStore>>,
    pub rag: Option<Arc<dyn RagStore>>,
}

pub struct RuntimeChatRequest {
    pub project_id: Option<String>,
    pub agent_id: Option<String>,
    pub conversation_id: Option<String>,
    pub user_message: String,
    pub model_override: Option<String>,
    /// Pre-built system prompt (from ContextEnricher). If empty, uses raw manifest prompt.
    pub system_prompt: Option<String>,
    pub default_model: String,
}

impl AgentRuntime {
    pub async fn chat(&self, req: RuntimeChatRequest) -> Result<String, AppError> {
        let registered = self
            .registry
            .resolve(req.project_id.as_deref(), req.agent_id.as_deref())
            .ok_or_else(|| AppError::NotFound("agent not found".into()))?;

        let scope: AgentScope = registered.scope.clone();
        let agent = registered.agent.clone();

        let span = tracing::info_span!(
            "agent.run",
            agent_id = %agent.qualified_id(),
            mcp_sources = scope.mcp_sources.len(),
            rag_sources = scope.rag_sources.len(),
        );
        let _guard = span.enter();

        if let Some(history) = &self.history {
            if agent.config.memory.persist_history {
                history
                    .append_user(req.conversation_id.as_deref(), &agent, &req.user_message)
                    .await
                    .map_err(AppError::Runtime)?;
            }
        }

        let system_prompt = req
            .system_prompt
            .unwrap_or_else(|| agent.config.system_prompt.clone());

        let model = req.model_override.unwrap_or_else(|| {
            if agent.config.model.is_empty() {
                req.default_model.clone()
            } else {
                agent.config.model.clone()
            }
        });

        let run_req = AgentRunRequest {
            agent: agent.clone(),
            scope: scope.clone(),
            model,
            system_prompt,
            user_message: req.user_message,
            history: vec![],
            conversation_id: req.conversation_id.clone(),
        };

        let result = self.engine.run(run_req).await.map_err(AppError::Runtime)?;

        if let Some(history) = &self.history {
            if agent.config.memory.persist_history {
                let _ = history
                    .append_assistant(req.conversation_id.as_deref(), &agent, &result.text)
                    .await;
                if agent.config.memory.index_summaries {
                    let _ = history
                        .maybe_summarize_and_index(&agent, &scope, self.rag.clone())
                        .await;
                }
            }
        }

        Ok(result.text)
    }
}
