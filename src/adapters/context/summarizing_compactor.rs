//! Budget-aware compactor: summarize the oldest history, keep the newest.

use crate::domain::models::ChatMessage;
use crate::ports::compaction::{CompactRequest, CompactResult, ContextCompactor};
use crate::ports::history::{ExportMessage, Summarizer};
use async_trait::async_trait;
use std::sync::Arc;

/// Same rough estimate the prompt pruner uses in `src/context/analyzer.rs`.
/// Compaction picks a prefix to drop; it does not need an exact tokenizer.
const CHARS_PER_TOKEN: usize = 4;

pub struct SummarizingCompactor {
    summarizer: Arc<dyn Summarizer>,
}

impl SummarizingCompactor {
    pub fn new(summarizer: Arc<dyn Summarizer>) -> Self {
        Self { summarizer }
    }
}

fn estimate_tokens(message: &ChatMessage) -> usize {
    (message.role.len() + message.content.len()) / CHARS_PER_TOKEN
}

/// `ChatMessage` carries no id or timestamp, so hand the summarizer stable
/// positional ones in the original order — the same shape a `Summarizer`
/// receives from the conversation store's `recent_export_messages`.
fn to_export_messages(messages: &[ChatMessage]) -> Vec<ExportMessage> {
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| ExportMessage {
            id: format!("history-{index}"),
            role: message.role.clone(),
            content: message.content.clone(),
            created_at: index as i64,
        })
        .collect()
}

#[async_trait]
impl ContextCompactor for SummarizingCompactor {
    async fn compact(&self, req: CompactRequest) -> Result<CompactResult, String> {
        let CompactRequest {
            system_prompt,
            history,
            budget_tokens,
            reserved_tokens,
        } = req;

        // Zero means "no budget known": the assembler must not compact in
        // that case, so stay transparent if this is ever called anyway.
        if budget_tokens == 0 {
            return Ok(CompactResult {
                memory: None,
                history,
            });
        }

        let available = (budget_tokens as usize)
            .saturating_sub(reserved_tokens as usize)
            .saturating_sub(system_prompt.len() / CHARS_PER_TOKEN);

        // Walk from the newest message backwards and keep the largest suffix
        // that fits. The newest message is always kept, even if it alone is
        // over budget: dropping the whole history is not a summary.
        let mut kept_from = history.len();
        let mut kept_tokens = 0usize;
        for (index, message) in history.iter().enumerate().rev() {
            let tokens = estimate_tokens(message);
            if kept_from != history.len() && kept_tokens + tokens > available {
                break;
            }
            kept_tokens += tokens;
            kept_from = index;
        }

        if kept_from == 0 {
            return Ok(CompactResult {
                memory: None,
                history,
            });
        }

        let oldest = to_export_messages(&history[..kept_from]);
        let summary = self
            .summarizer
            .summarize(&oldest)
            .await
            .map_err(|e| format!("context compaction summarizer failed: {e}"))?;
        let memory = {
            let trimmed = summary.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };

        Ok(CompactResult {
            memory,
            history: history[kept_from..].to_vec(),
        })
    }
}
