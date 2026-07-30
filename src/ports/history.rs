//! Conversation history and continuous-learning hooks.

use crate::domain::agent::Agent;
use crate::domain::models::ChatMessage;
use crate::domain::scope::AgentScope;
use crate::ports::rag::RagStore;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub conversation_id: String,
    pub agent_qualified_id: String,
    pub summary: String,
    pub turn_count: u32,
}

/// Summary of a single conversation (for listing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationRecord {
    pub id: String,
    pub agent_qualified_id: String,
    pub project_id: Option<String>,
    pub agent_id: String,
    pub turn_count: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Full export of a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationExport {
    pub record: ConversationRecord,
    pub messages: Vec<ExportMessage>,
    pub summaries: Vec<ExportSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportMessage {
    pub id: String,
    pub role: String,
    pub content: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSummary {
    pub id: String,
    pub turn_range_start: u32,
    pub turn_range_end: u32,
    pub summary_text: String,
    pub indexed_in_rag: bool,
    pub created_at: i64,
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

    /// Returns the N most recent messages for a conversation, ordered ASC.
    async fn get_history(
        &self,
        conversation_id: &str,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, String>;

    /// Lists conversations for an agent (latest first), paginated.
    async fn list_conversations(
        &self,
        agent_qualified_id: &str,
        page: usize,
        page_size: usize,
    ) -> Result<Vec<ConversationRecord>, String>;

    /// Gets a single conversation record by id.
    async fn get_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationRecord>, String>;

    /// Deletes a conversation and all its messages (cascade).
    async fn delete_conversation(&self, conversation_id: &str) -> Result<(), String>;

    /// Export: full messages + summaries for a conversation.
    async fn export_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<ConversationExport, String>;

    /// Purge messages/conversations older than retention_days.
    async fn purge_old_conversations(&self, retention_days: u32) -> Result<u64, String>;
}
