//! Turns an agent's manifest and a request's history into one `ActiveContext`.
//!
//! The enricher owns the instruction sections; this service owns the ordering:
//! instructions first (never compacted), then an optional memory summary, with
//! the possibly-compacted history handed to the engine as a separate field.
//! Compactor errors are warnings, never chat errors.

use crate::context::active::{ActiveContext, DEFAULT_RESERVED_TOKENS};
use crate::context::analyzer::ContextEnricher;
use crate::domain::agent::Agent;
use crate::domain::models::ChatMessage;
use crate::error::AppError;
use crate::ports::compaction::{CompactRequest, ContextCompactor};
use crate::providers::capabilities::ModelCapabilities;
use std::sync::Arc;

/// The context plus the model the turn should run against. Task 4 replaces
/// `model` with a selected candidate; until then the current model passes
/// through unchanged.
#[derive(Debug, Clone)]
pub struct AssembledContext {
    pub context: ActiveContext,
    pub model: String,
}

pub struct ContextAssembler {
    enricher: Arc<ContextEnricher>,
    compactor: Arc<dyn ContextCompactor>,
}

impl ContextAssembler {
    pub fn new(enricher: Arc<ContextEnricher>, compactor: Arc<dyn ContextCompactor>) -> Self {
        Self {
            enricher,
            compactor,
        }
    }

    /// Assemble one turn. `capabilities` comes from the same resolution the
    /// engine uses, so the catalog's context window can be compared with the
    /// agent's own limit without a second lookup. `current_model` is the model
    /// the caller would have used before selection; this task returns it as-is.
    pub async fn assemble(
        &self,
        agent: &Agent,
        capabilities: &ModelCapabilities,
        history: Vec<ChatMessage>,
        current_model: String,
    ) -> Result<AssembledContext, AppError> {
        self.assemble_with_instructions(agent, capabilities, history, current_model, None)
            .await
    }

    /// Same assembly, but with a caller-supplied instruction block. The HTTP
    /// chat path passes a prompt it has already optimized and measured; the
    /// assembler must not rebuild it from the manifest, or those numbers would
    /// lie. A supplied prompt is still only the instruction section — the
    /// memory section, when there is one, is appended after it.
    pub async fn assemble_with_instructions(
        &self,
        agent: &Agent,
        capabilities: &ModelCapabilities,
        history: Vec<ChatMessage>,
        current_model: String,
        injected_instructions: Option<String>,
    ) -> Result<AssembledContext, AppError> {
        // The enricher is the one source of rule/skill/project/RAG sections. It
        // runs before the memory section is appended, so the instruction bytes
        // are identical whether or not the history was compacted.
        let instructions = match injected_instructions {
            Some(prompt) => prompt,
            None => self.enricher.build_system_prompt(agent).await,
        };

        let budget_tokens = budget(capabilities, agent);
        let compacted = if budget_tokens == 0 {
            // No known window on either side: keep the current behaviour and do
            // not even consult the compactor.
            None
        } else {
            let request = CompactRequest {
                system_prompt: instructions.clone(),
                history: history.clone(),
                budget_tokens,
                reserved_tokens: DEFAULT_RESERVED_TOKENS,
            };
            match self.compactor.compact(request).await {
                Ok(result) => Some(result),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "context compaction failed; using the original prompt and history"
                    );
                    None
                }
            }
        };

        let (history, memory) = match compacted {
            Some(result) => {
                let memory = result
                    .memory
                    .map(|summary| summary.trim().to_string())
                    .filter(|summary| !summary.is_empty());
                (result.history, memory)
            }
            None => (history, None),
        };

        let mut system_prompt = instructions;
        if let Some(memory) = &memory {
            system_prompt.push_str("\n## Conversation memory\n");
            system_prompt.push_str(memory);
        }

        Ok(AssembledContext {
            context: ActiveContext {
                system_prompt,
                memory,
                history,
                reserved_tokens: DEFAULT_RESERVED_TOKENS,
            },
            model: current_model,
        })
    }
}

/// `min(catalog, agent)`; a missing (or zero) side does not constrain the
/// budget, and both missing means no budget at all (zero).
fn budget(capabilities: &ModelCapabilities, agent: &Agent) -> u32 {
    let catalog = capabilities.max_context_tokens.filter(|value| *value > 0);
    let declared = u32::try_from(agent.config.max_context_tokens).unwrap_or(u32::MAX);
    let declared = (declared > 0).then_some(declared);
    match (catalog, declared) {
        (Some(catalog), Some(declared)) => catalog.min(declared),
        (Some(catalog), None) => catalog,
        (None, Some(declared)) => declared,
        (None, None) => 0,
    }
}
