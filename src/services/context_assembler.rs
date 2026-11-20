//! Turns an agent's manifest and a request's history into one `ActiveContext`.
//!
//! The enricher owns the instruction sections; this service owns the ordering:
//! instructions first (never compacted), then an optional memory summary, with
//! the possibly-compacted history handed to the engine as a separate field.
//! Compactor errors are warnings, never chat errors.

use crate::adapters::context::CurrentModelSelector;
use crate::context::active::{ActiveContext, DEFAULT_RESERVED_TOKENS};
use crate::context::analyzer::ContextEnricher;
use crate::domain::agent::Agent;
use crate::domain::models::ChatMessage;
use crate::error::AppError;
use crate::ports::compaction::{CompactRequest, ContextCompactor};
use crate::ports::model_selection::{
    ModelCandidate, ModelSelectionRequest, ModelSelector, SelectionReason,
};
use crate::providers::capabilities::ModelCapabilities;
use std::collections::HashMap;
use std::sync::Arc;

/// The context plus the model the turn should run against. `model` is either
/// the caller's current model (no candidates) or the selector's choice.
#[derive(Debug, Clone)]
pub struct AssembledContext {
    pub context: ActiveContext,
    pub model: String,
}

pub struct ContextAssembler {
    enricher: Arc<ContextEnricher>,
    compactor: Arc<dyn ContextCompactor>,
    /// Best-effort selector. The default `CurrentModelSelector` keeps the
    /// current model on every path, so an agent without candidates — and a
    /// deployment without a decision router — see no change.
    selector: Arc<dyn ModelSelector>,
    /// Ordinal ranks from the optional `models.toml`. A candidate absent from
    /// the table ranks 1; this is never a price and never reaches `cost_usd`.
    relative_costs: HashMap<String, u32>,
    /// The current turn's user message, supplied by the caller. `history` holds
    /// only *prior* turns, so without this the router would classify the previous
    /// request instead of the one being answered.
    prompt_hint: Option<String>,
}

impl ContextAssembler {
    pub fn new(enricher: Arc<ContextEnricher>, compactor: Arc<dyn ContextCompactor>) -> Self {
        Self {
            enricher,
            compactor,
            selector: Arc::new(CurrentModelSelector),
            relative_costs: HashMap::new(),
            prompt_hint: None,
        }
    }

    /// Install a complexity selector and the relative-cost table it ranks with.
    /// Shared by boot and a settings Apply through the same companion handles.
    pub fn with_selector(
        mut self,
        selector: Arc<dyn ModelSelector>,
        relative_costs: HashMap<String, u32>,
    ) -> Self {
        self.selector = selector;
        self.relative_costs = relative_costs;
        self
    }

    /// Supply the current turn's prompt for complexity classification. Purely
    /// selection input: it never touches the assembled instruction bytes.
    pub fn with_prompt_hint(mut self, prompt_hint: String) -> Self {
        self.prompt_hint = Some(prompt_hint);
        self
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

        let model = self
            .select_model(agent, &history, &system_prompt, current_model)
            .await;

        Ok(AssembledContext {
            context: ActiveContext {
                system_prompt,
                memory,
                history,
                reserved_tokens: DEFAULT_RESERVED_TOKENS,
            },
            model,
        })
    }

    /// Pick the model for this turn. With no declared candidates the current
    /// model passes straight through and no selector is consulted. Otherwise
    /// one complexity question is spent; only a `Chosen` answer naming a
    /// declared candidate is honoured — every degradation keeps the current
    /// model, with a warning, so a selector can never invent a model id.
    async fn select_model(
        &self,
        agent: &Agent,
        history: &[ChatMessage],
        instructions: &str,
        current_model: String,
    ) -> String {
        if agent.config.model_candidates.is_empty() {
            return current_model;
        }

        let candidates: Vec<ModelCandidate> = agent
            .config
            .model_candidates
            .iter()
            .map(|id| ModelCandidate {
                model: id.clone(),
                provider_hint: None,
                relative_cost: self.relative_costs.get(id).copied().unwrap_or(1),
            })
            .collect();

        // The current user message is the complexity signal. Callers that did
        // not supply one fall back to the last history turn, then the instruction
        // block — history alone would classify the *previous* request.
        let prompt_hint = self
            .prompt_hint
            .clone()
            .filter(|hint| !hint.trim().is_empty())
            .or_else(|| {
                history
                    .iter()
                    .rev()
                    .find(|message| message.role == "user")
                    .map(|message| message.content.clone())
                    .filter(|content| !content.trim().is_empty())
            })
            .unwrap_or_else(|| instructions.to_string());

        let selection = self
            .selector
            .select(ModelSelectionRequest {
                candidates,
                prompt_hint,
                current_model: current_model.clone(),
            })
            .await;

        if selection.reason == SelectionReason::Chosen
            && agent.config.model_candidates.contains(&selection.model)
        {
            return selection.model;
        }

        if selection.reason != SelectionReason::NoCandidates {
            tracing::warn!(
                reason = ?selection.reason,
                model = %selection.model,
                "model selection did not yield a declared candidate; keeping the current model"
            );
        }
        current_model
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
