//! Conversation history and continuous-learning hooks.

use crate::domain::agent::Agent;
use crate::domain::models::ChatMessage;
use crate::domain::scope::AgentScope;
use crate::ports::rag::RagStore;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

/// Turns a slice of exported messages into a short summary.
///
/// The empty string is a valid, deliberate answer meaning "no summary": a
/// no-op summarizer returns `Ok(String::new())` rather than an error, so the
/// caller can treat "nothing to learn" as a normal outcome. An `Err` means the
/// summarizer itself failed and is logged as a best-effort warning by callers.
#[async_trait]
pub trait Summarizer: Send + Sync {
    async fn summarize(&self, messages: &[ExportMessage]) -> Result<String, String>;

    /// Stable, secret-free label for the concrete strategy.
    ///
    /// Production strategies override this with `ollama`, `chat-completions`,
    /// `opencode` or `noop`; test fakes inherit the `custom` default. It is a
    /// label only — no URL, credential or argument ever reaches it — so boot
    /// code and selection tests can identify a strategy without downcasting.
    fn strategy_name(&self) -> &'static str {
        "custom"
    }

    /// Whether this is the deliberately quiet strategy (no URL/key available).
    ///
    /// Defaults to `false`; only the real `NoopSummarizer` overrides it. Boot
    /// uses this to refuse pairing a no-op summarizer with the real
    /// `SummarizingCompactor`, which would silently drop old turns.
    fn is_noop(&self) -> bool {
        false
    }
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

    /// Summarize and index a conversation's latest turns.
    ///
    /// `Some(id)` targets exactly that conversation (the turn that triggered
    /// the summary); `None` keeps the legacy "latest conversation for this
    /// agent" behaviour so older callers and adapters remain valid.
    async fn maybe_summarize_and_index(
        &self,
        conversation_id: Option<&str>,
        agent: &Agent,
        scope: &AgentScope,
        rag: Option<Arc<dyn RagStore>>,
    ) -> Result<Option<ConversationSummary>, String>;

    /// Record that a tool call finished, without the arguments.
    ///
    /// Only the tool name and its success flag are observable: no args are
    /// passed, so no secret can reach the persisted tool-event log.
    async fn append_tool_event(
        &self,
        conversation_id: &str,
        tool_name: &str,
        success: bool,
    ) -> Result<(), String>;

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

    /// Purge messages/conversations older than `retention_days`.
    ///
    /// `per_agent_days` overrides the global retention for the listed
    /// `agent_qualified_id`s; conversations not listed (or when the map is
    /// `None`) use the global value.
    async fn purge_old_conversations(
        &self,
        retention_days: u32,
        per_agent_days: Option<&HashMap<String, u32>>,
    ) -> Result<u64, String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Fixed-text summarizer that records the messages it was handed.
    struct FakeSummarizer {
        text: String,
        captured: Arc<Mutex<Vec<Vec<ExportMessage>>>>,
    }

    impl FakeSummarizer {
        fn new(text: &str) -> Self {
            Self {
                text: text.to_string(),
                captured: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn captured(&self) -> Vec<Vec<ExportMessage>> {
            self.captured.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Summarizer for FakeSummarizer {
        async fn summarize(&self, messages: &[ExportMessage]) -> Result<String, String> {
            self.captured.lock().unwrap().push(messages.to_vec());
            Ok(self.text.clone())
        }
    }

    /// Deliberately quiet summarizer: an empty string means "no summary".
    struct NoopSummarizer;

    #[async_trait]
    impl Summarizer for NoopSummarizer {
        async fn summarize(&self, _messages: &[ExportMessage]) -> Result<String, String> {
            Ok(String::new())
        }
    }

    fn sample_messages() -> Vec<ExportMessage> {
        vec![
            ExportMessage {
                id: "m1".into(),
                role: "user".into(),
                content: "hello".into(),
                created_at: 1,
            },
            ExportMessage {
                id: "m2".into(),
                role: "assistant".into(),
                content: "hi".into(),
                created_at: 2,
            },
        ]
    }

    #[tokio::test]
    async fn fake_summarizer_returns_fixed_text_and_captures_messages() {
        let summarizer = FakeSummarizer::new("a concise summary");
        let messages = sample_messages();

        let out = summarizer.summarize(&messages).await.expect("summarize");

        assert_eq!(out, "a concise summary");

        let captured = summarizer.captured();
        assert_eq!(captured.len(), 1, "summarize must be called exactly once");
        let as_tuples: Vec<_> = captured[0]
            .iter()
            .map(|m| {
                (
                    m.id.as_str(),
                    m.role.as_str(),
                    m.content.as_str(),
                    m.created_at,
                )
            })
            .collect();
        assert_eq!(
            as_tuples,
            vec![("m1", "user", "hello", 1), ("m2", "assistant", "hi", 2)],
            "the summarizer must receive the exact messages it was handed"
        );
    }

    #[tokio::test]
    async fn noop_summarizer_returns_empty_string() {
        let summarizer = NoopSummarizer;

        let out = summarizer
            .summarize(&sample_messages())
            .await
            .expect("summarize");

        assert_eq!(out, "", "no summary means an empty string, not an error");
    }

    #[test]
    fn summarizer_can_be_held_as_a_trait_object() {
        let summarizer: Arc<dyn Summarizer> = Arc::new(NoopSummarizer);
        let _ = summarizer;
    }
}
