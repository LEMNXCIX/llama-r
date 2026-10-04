//! Orchestrates a single agent turn: scope → (optional) history → engine → learn.

use crate::context::analyzer::ContextEnricher;
use crate::domain::models::ChatMessage;
use crate::domain::scope::AgentScope;
use crate::error::AppError;
use crate::optimizer::TokenOptimizer;
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
    /// Builds the system prompt for a caller that did not bring one.
    ///
    /// Present because the alternative was a silent one: a request with no
    /// `system_prompt` used to fall back to the manifest's `system_prompt` and
    /// nothing else, which drops every section `ContextEnricher` adds — the agent's
    /// rules, its project context, its selected skills. The HTTP chat path passes an
    /// already-enriched prompt; the TUI passed `None`, so the same agent answered
    /// differently depending on which client asked, and a rule typed into the form
    /// simply never arrived.
    pub enricher: Arc<ContextEnricher>,
}

pub struct RuntimeChatRequest {
    pub project_id: Option<String>,
    pub agent_id: Option<String>,
    pub conversation_id: Option<String>,
    pub user_message: String,
    /// Prior turns (user/assistant), excluding the injected system prompt and the current user message.
    pub history: Vec<ChatMessage>,
    pub model_override: Option<String>,
    /// Pre-built system prompt. `None` means "build it from the agent's
    /// configuration", which `AgentRuntime` does with its `ContextEnricher` — not
    /// "use the manifest's `system_prompt` and drop the rest", which is what this
    /// used to do.
    pub system_prompt: Option<String>,
    pub default_model: String,
}

impl AgentRuntime {
    /// Resolve the agent and settle on the system prompt.
    ///
    /// A prompt the caller brought is used verbatim: `chat_core` has already run it
    /// through the optimiser and the metrics, so rebuilding it would do that work
    /// twice and the numbers would lie. A caller that brought none gets the enriched
    /// one, which is the only prompt that contains the agent's configuration.
    async fn build_run_request(
        &self,
        req: RuntimeChatRequest,
    ) -> Result<AgentRunRequest, AppError> {
        let registered = self
            .registry
            .resolve(req.project_id.as_deref(), req.agent_id.as_deref())
            .ok_or_else(|| AppError::NotFound("agent not found".into()))?;

        let scope: AgentScope = registered.scope.clone();
        let agent = registered.agent.clone();

        let system_prompt = match req.system_prompt {
            Some(prompt) => prompt,
            None => self.enricher.build_system_prompt(&agent).await,
        };
        // Applied here rather than by the caller, so every client that reaches an
        // engine gets the same prompt. `chat_core` used to do this itself and the
        // TUI did not, which meant the agent's configured optimizations applied to
        // HTTP requests and to nothing else — the same shape of disagreement as the
        // system prompt had.
        let optimizer = TokenOptimizer::new(agent.config.optimize.clone());
        let system_prompt = optimizer.optimize(&system_prompt);
        let user_message = optimizer.optimize(&req.user_message);

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
            user_message,
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

        let run_req = self.build_run_request(updated_req).await?;

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

        let run_req = self.build_run_request(updated_req).await?;

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
