//! Compaction of conversation history under a token budget.
//!
//! History is the only thing a compactor may change. Instructions, skills,
//! rules and RAG sections live in `system_prompt` and are never truncated:
//! the caller builds them once and re-injects them byte-for-byte after
//! compaction. This port stays deliberately small — the caller decides
//! whether there is budget pressure, the compactor decides how much of the
//! oldest history to drop and what memory to hand back.

use crate::domain::models::ChatMessage;
use async_trait::async_trait;

/// Everything a compactor needs to decide which history to keep.
#[derive(Debug, Clone)]
pub struct CompactRequest {
    pub system_prompt: String,
    pub history: Vec<ChatMessage>,
    pub budget_tokens: u32,
    pub reserved_tokens: u32,
}

/// The compacted history plus an optional summary of what was dropped.
#[derive(Debug, Clone)]
pub struct CompactResult {
    pub memory: Option<String>,
    pub history: Vec<ChatMessage>,
}

#[async_trait]
pub trait ContextCompactor: Send + Sync {
    /// Returns a named `Err` on failure; best-effort degradation is the
    /// caller's decision, not something hidden inside the port.
    async fn compact(&self, req: CompactRequest) -> Result<CompactResult, String>;
}
