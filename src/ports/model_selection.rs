//! Model selection by prompt complexity.
//!
//! This port is deliberately additive and small: a caller that already knows
//! which model to use needs nothing from it, and a caller with candidates asks
//! one question of an injected [`DecisionRouter`](crate::ports::router::DecisionRouter)
//! — the same configurable decision model agent routing and `Auto` use. No
//! model name is hardcoded here or in the adapters; the router is the only
//! source of a complexity verdict, and its failure is never turned into a
//! fabricated model choice.
//!
//! `relative_cost` is an ordinal rank, not a price: it may only say which
//! candidate is the cheaper/simpler one, never how much anything costs, and it
//! is never written to `ModelCapabilities` or `cost_usd`.

use async_trait::async_trait;

/// One model the selector may choose. `provider_hint` is a description only —
/// the manifest/engine still resolves the real upstream. `relative_cost` is a
/// rank (lower = simpler), not a monetary value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCandidate {
    pub model: String,
    pub provider_hint: Option<String>,
    pub relative_cost: u32,
}

/// What the selector needs to decide. `prompt_hint` becomes the state shown to
/// the decision router; `current_model` is the fail-closed answer for every
/// path that cannot produce a trustworthy choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelectionRequest {
    pub candidates: Vec<ModelCandidate>,
    pub prompt_hint: String,
    pub current_model: String,
}

/// Why the selector returned the model it did. Only `Chosen` names a candidate;
/// every other variant means "keep the current model".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionReason {
    Chosen,
    NoCandidates,
    RouterUnavailable,
    LowConfidence,
}

/// The result of one selection. `reason` makes a degradation observable
/// without inventing a model the caller could not have chosen itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelection {
    pub model: String,
    pub reason: SelectionReason,
}

#[async_trait]
pub trait ModelSelector: Send + Sync {
    /// Selects a model for this request. Implementors must return a model on
    /// every path: selection is best-effort and never fails a chat.
    async fn select(&self, req: ModelSelectionRequest) -> ModelSelection;
}
