//! The context an engine actually sees for one turn.
//!
//! `ActiveContext` is assembled once per turn. Its instruction sections
//! (system prompt, rules, skills, project context and RAG configuration) are
//! built by `ContextEnricher`. The compactor sees them only as context for its
//! budget math and cannot change them — `CompactResult` has no prompt field, so
//! only `history` may be compacted and the assembler re-injects the instructions
//! verbatim. A compactor's summary arrives as `memory` and is
//! rendered into the prompt by the assembler *after* the instructions, so the
//! instruction bytes are identical whether or not compaction happened.

use crate::domain::models::ChatMessage;

/// Space kept out of the compaction budget for tool schemas and retrieved RAG
/// documents. It is a conservative allowance, not a measured token count; the
/// compactor subtracts it from the available history budget before deciding
/// which oldest turns to summarize.
pub const DEFAULT_RESERVED_TOKENS: u32 = 1_024;

#[derive(Debug, Clone)]
pub struct ActiveContext {
    /// Instruction sections plus an optional `## Conversation memory` suffix.
    pub system_prompt: String,
    /// Summary of the turns the compactor dropped, when it produced one.
    pub memory: Option<String>,
    /// History handed to the engine, possibly compacted to fit the budget.
    pub history: Vec<ChatMessage>,
    /// Tokens reserved for tools and RAG on this turn.
    pub reserved_tokens: u32,
}
