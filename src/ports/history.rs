//! Conversation history and continuous-learning hooks.

use crate::domain::agent::Agent;
use crate::domain::scope::AgentScope;
use crate::ports::rag::RagStore;
use async_trait::async_trait;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ConversationSummary {
    pub conversation_id: String,
    pub agent_qualified_id: String,
    pub summary: String,
    pub turn_count: u32,
}

#[async_trait]
pub trait ConversationStore: Send + Sync {
    async fn append_user(
        &self,
        conversation_id: Option<&str>,
        agent: &Agent,
        content: &str,
    ) -> Result<String, String>;

    async fn append_assistant(
        &self,
        conversation_id: Option<&str>,
        agent: &Agent,
        content: &str,
    ) -> Result<(), String>;

    async fn maybe_summarize_and_index(
        &self,
        agent: &Agent,
        scope: &AgentScope,
        rag: Option<Arc<dyn RagStore>>,
    ) -> Result<Option<ConversationSummary>, String>;
}
