//! Orchestrates a single agent turn: scope → (optional) history → engine → learn.

use crate::context::analyzer::ContextEnricher;
use crate::domain::models::ChatMessage;
use crate::domain::scope::AgentScope;
use crate::error::AppError;
use crate::optimizer::TokenOptimizer;
use crate::ports::delegation::{DelegationContext, ToolBudget};
use crate::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest};
use crate::ports::history::ConversationStore;
use crate::ports::rag::RagStore;
use crate::services::agent_registry::AgentRegistry;
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

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
    /// The durable turn id to stamp on the engine request. Created by whichever
    /// caller resolves the agent (`chat_core`) so the execution record, its
    /// events and the tool-operation ids all share one identity.
    pub execution_id: Uuid,
    /// The one tool allowance shared by a whole delegation tree, carried from a
    /// child turn's `DelegateRequest.budget`. `None` for an ordinary turn.
    pub shared_budget: Option<Arc<dyn ToolBudget>>,
    /// The delegation bounds carried from a parent turn; `None` at the root.
    pub delegation_context: Option<DelegationContext>,
    /// Event channel for a child turn whose approvals must be forwarded to the
    /// root's SSE. `None` for an ordinary or streaming-root turn.
    pub approval_event_tx: Option<tokio::sync::mpsc::Sender<crate::ports::engine::AgentRunEvent>>,
}

/// The answer plus the conversation the turn was persisted under, if any.
///
/// `conversation_id` is the id `append_user` returned — the one that actually
/// has rows in the store — never a freshly minted uuid. It is `None` when no
/// history was written (no agent, no store, or `persist_history = false`), so a
/// caller never advertises a conversation a client cannot resume.
pub struct RuntimeChatResult {
    pub text: String,
    pub conversation_id: Option<String>,
}

/// The event stream plus the conversation the turn was persisted under. The
/// non-streaming surface carries the same id on a `ChatResponse` DTO field; the
/// SSE surface has no body field before the stream starts, so the id rides on a
/// response header built by the HTTP handler.
pub struct RuntimeStreamResult {
    pub events: mpsc::Receiver<AgentRunEvent>,
    pub conversation_id: Option<String>,
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

        // Resolve the provider/capabilities for this agent from the same source
        // the system prompt used. `model_override` above keeps its precedence:
        // the resolved model id is transport metadata here, not the name the
        // engine will actually ask for.
        let resolved = self.enricher.resolve(&agent);

        Ok(AgentRunRequest {
            agent,
            scope,
            model,
            provider: resolved.provider,
            capabilities: resolved.capabilities,
            system_prompt,
            user_message,
            history: req.history,
            conversation_id: req.conversation_id,
            execution_id: req.execution_id,
            shared_budget: req.shared_budget,
            delegation_context: req.delegation_context,
            approval_event_tx: req.approval_event_tx,
        })
    }

    pub async fn chat(&self, req: RuntimeChatRequest) -> Result<RuntimeChatResult, AppError> {
        let conversation_id = req.conversation_id.clone();
        let user_message = req.user_message.clone();

        let registered = self
            .registry
            .resolve(req.project_id.as_deref(), req.agent_id.as_deref())
            .ok_or_else(|| AppError::NotFound("agent not found".into()))?;

        let agent = registered.agent.clone();
        let scope = registered.scope.clone();

        let mut resolved_conv_id = conversation_id.clone();
        // Echo only an id that was actually persisted. A supplied id with
        // `persist_history = false` (or a failed append) must not be advertised:
        // there is no conversation for the client to resume.
        let mut persisted_conv_id: Option<String> = None;

        if let Some(history) = &self.history {
            if agent.config.memory.persist_history {
                match history
                    .append_user(conversation_id.as_deref(), &agent, &user_message)
                    .await
                {
                    Ok(id) => {
                        persisted_conv_id = Some(id.clone());
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
                    // `None` selects the legacy latest-conversation path.
                    let _ = history
                        .maybe_summarize_and_index(None, &agent, &scope, self.rag.clone())
                        .await;
                }
            }
        }

        Ok(RuntimeChatResult {
            text: result.text,
            conversation_id: persisted_conv_id,
        })
    }

    /// Stream agent events (tokens / completed / errors). Tool events are reserved for later.
    pub async fn chat_stream(
        &self,
        req: RuntimeChatRequest,
    ) -> Result<RuntimeStreamResult, AppError> {
        let conversation_id = req.conversation_id.clone();
        let user_message = req.user_message.clone();

        let registered = self
            .registry
            .resolve(req.project_id.as_deref(), req.agent_id.as_deref())
            .ok_or_else(|| AppError::NotFound("agent not found".into()))?;

        let agent = registered.agent.clone();
        let scope = registered.scope.clone();

        let mut resolved_conv_id = conversation_id.clone();
        let mut persisted_conv_id: Option<String> = None;

        if let Some(history) = &self.history {
            if agent.config.memory.persist_history {
                match history
                    .append_user(conversation_id.as_deref(), &agent, &user_message)
                    .await
                {
                    Ok(id) => {
                        persisted_conv_id = Some(id.clone());
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

        let events = self
            .engine
            .run_stream(run_req)
            .await
            .map_err(AppError::Runtime)?;
        Ok(RuntimeStreamResult {
            events,
            conversation_id: persisted_conv_id,
        })
    }
}
