//! Orchestrates a single agent turn: scope → (optional) history → engine → learn.

use crate::domain::models::ChatMessage;
use crate::domain::scope::AgentScope;
use crate::error::AppError;
use crate::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest};
use crate::ports::history::ConversationStore;
use crate::ports::rag::RagStore;
use crate::services::agent_registry::AgentRegistry;
use std::sync::Arc;
use tokio::sync::mpsc;

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
    /// Prior turns (user/assistant), excluding the injected system prompt and the current user message.
    pub history: Vec<ChatMessage>,
    pub model_override: Option<String>,
    /// Pre-built system prompt (from ContextEnricher). If empty, uses raw manifest prompt.
    pub system_prompt: Option<String>,
    pub default_model: String,
}

impl AgentRuntime {
    fn build_run_request(&self, req: RuntimeChatRequest) -> Result<AgentRunRequest, AppError> {
        let registered = self
            .registry
            .resolve(req.project_id.as_deref(), req.agent_id.as_deref())
            .ok_or_else(|| AppError::NotFound("agent not found".into()))?;

        let scope: AgentScope = registered.scope.clone();
        let agent = registered.agent.clone();

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

        Ok(AgentRunRequest {
            agent,
            scope,
            model,
            system_prompt,
            user_message: req.user_message,
            history: req.history,
            conversation_id: req.conversation_id,
        })
    }

    pub async fn chat(&self, req: RuntimeChatRequest) -> Result<String, AppError> {
        let conversation_id = req.conversation_id.clone();
        let user_message = req.user_message.clone();

        let registered = self
            .registry
            .resolve(req.project_id.as_deref(), req.agent_id.as_deref())
            .ok_or_else(|| AppError::NotFound("agent not found".into()))?;

        let agent = registered.agent.clone();
        let scope = registered.scope.clone();

        let mut resolved_conv_id = conversation_id.clone();

        if let Some(history) = &self.history {
            if agent.config.memory.persist_history {
                match history
                    .append_user(conversation_id.as_deref(), &agent, &user_message)
                    .await
                {
                    Ok(id) => {
                        resolved_conv_id = Some(id);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to persist user message");
                    }
                }
            }
        }

        let mut req_history = req.history.clone();
        if req_history.is_empty() {
            if let Some(history) = &self.history {
                if let Some(cid) = &resolved_conv_id {
                    if let Ok(msgs) = history.get_history(cid, 20).await {
                        if !msgs.is_empty() {
                            let count = msgs.len().saturating_sub(1);
                            req_history = msgs.into_iter().take(count).collect();
                        }
                    }
                }
            }
        }

        let mut updated_req = req;
        updated_req.conversation_id = resolved_conv_id;
        updated_req.history = req_history;

        let run_req = self.build_run_request(updated_req)?;

        let span = tracing::info_span!(
            "agent.run",
            agent_id = %agent.qualified_id(),
            mcp_sources = scope.mcp_sources.len(),
            rag_sources = scope.rag_sources.len(),
        );
        let _guard = span.enter();

        let result = self
            .engine
            .run(run_req.clone())
            .await
            .map_err(AppError::Runtime)?;

        if let Some(history) = &self.history {
            if agent.config.memory.persist_history {
                let _ = history
                    .append_assistant(run_req.conversation_id.as_deref(), &agent, &result.text)
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

    /// Stream agent events (tokens / completed / errors). Tool events are reserved for later.
    pub async fn chat_stream(
        &self,
        req: RuntimeChatRequest,
    ) -> Result<mpsc::Receiver<AgentRunEvent>, AppError> {
        let conversation_id = req.conversation_id.clone();
        let user_message = req.user_message.clone();

        let registered = self
            .registry
            .resolve(req.project_id.as_deref(), req.agent_id.as_deref())
            .ok_or_else(|| AppError::NotFound("agent not found".into()))?;

        let agent = registered.agent.clone();
        let scope = registered.scope.clone();

        let mut resolved_conv_id = conversation_id.clone();

        if let Some(history) = &self.history {
            if agent.config.memory.persist_history {
                match history
                    .append_user(conversation_id.as_deref(), &agent, &user_message)
                    .await
                {
                    Ok(id) => {
                        resolved_conv_id = Some(id);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to persist user message");
                    }
                }
            }
        }

        let mut req_history = req.history.clone();
        if req_history.is_empty() {
            if let Some(history) = &self.history {
                if let Some(cid) = &resolved_conv_id {
                    if let Ok(msgs) = history.get_history(cid, 20).await {
                        if !msgs.is_empty() {
                            let count = msgs.len().saturating_sub(1);
                            req_history = msgs.into_iter().take(count).collect();
                        }
                    }
                }
            }
        }

        let mut updated_req = req;
        updated_req.conversation_id = resolved_conv_id;
        updated_req.history = req_history;

        let run_req = self.build_run_request(updated_req)?;

        let span = tracing::info_span!(
            "agent.run_stream",
            agent_id = %agent.qualified_id(),
            mcp_sources = scope.mcp_sources.len(),
            rag_sources = scope.rag_sources.len(),
        );
        let _guard = span.enter();

        self.engine
            .run_stream(run_req)
            .await
            .map_err(AppError::Runtime)
    }
}
