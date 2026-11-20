//! Router-backed [`ModelSelector`]: one complexity `Choice` per request.
//!
//! The decision model is whatever [`DecisionRouter`] the caller injects — the
//! same one agent routing and `Auto` use, configured through `ROUTER_URL` /
//! `router.toml`. No model name is hardcoded here: the `Choice` criteria are
//! keyed by the two tier labels `simple` and `complex` and the model
//! ids appear only inside the descriptions the router reads. Both tier keys
//! are real criteria keys, so a conforming router (which returns one of the
//! keys it was given) can actually choose the complex tier.
//!
//! The verdict ladder is fail-closed, exactly like Fase 6a's `Auto`: a single
//! high-confidence `complex` answer is the only path to a different model.
//! An error, a `Noul`, a missing answer, a malformed confidence or a low
//! confidence all keep the current model with a named reason and a warning —
//! never a fabricated model. `relative_cost` only decides which candidate of
//! the chosen complexity tier wins, and in a tie the first declared one does.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::ports::model_selection::{
    ModelCandidate, ModelSelection, ModelSelectionRequest, ModelSelector, SelectionReason,
};
use crate::ports::router::{Answer, DecisionRequest, DecisionRouter, Question};

/// The question name is also the key the answer is read back under, so a
/// rename cannot leave the outgoing question and the lookup out of sync.
pub const QUESTION_KEY: &str = "complexity";

const INSTRUCTIONS: &str = "Classify this request by complexity. Answer `complex` when it needs \
     the most capable candidate, otherwise answer `simple` for the cheap one. Return one of the \
     criteria keys, never a model name of your own.";

pub struct RouterModelSelector {
    router: Arc<dyn DecisionRouter>,
    threshold: f32,
}

impl RouterModelSelector {
    pub fn new(router: Arc<dyn DecisionRouter>, threshold: f32) -> Self {
        Self { router, threshold }
    }

    /// Two real criteria keys — the tiers the router chooses between. Model
    /// ids (and their ranks) travel only as description text; they are never
    /// keys, so no decision model is hardcoded and a conforming router can
    /// answer `complex` and actually reach the complex tier.
    fn question(candidates: &[ModelCandidate]) -> Question {
        let options: Vec<String> = candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let hint = candidate.provider_hint.as_deref().unwrap_or("unspecified");
                format!(
                    "candidate {index}: model '{}', provider hint {hint}, relative_cost {}",
                    candidate.model, candidate.relative_cost
                )
            })
            .collect();
        let choices = options.join("; ");
        let criteria = BTreeMap::from([
            (
                "simple".to_string(),
                format!(
                    "Simple tier: use the lowest relative_cost candidate. Candidates: {choices}"
                ),
            ),
            (
                "complex".to_string(),
                format!(
                    "Complex tier: use the highest relative_cost candidate. Candidates: {choices}"
                ),
            ),
        ]);
        Question::Choice {
            instructions: INSTRUCTIONS.to_string(),
            criteria,
        }
    }

    /// `complex` picks the highest relative_cost; `simple` (or any other
    /// conforming key) the lowest. Both scan in declaration order with a strict
    /// comparison, so a tie always resolves to the first declared candidate of
    /// that tier — which is also why a missing cost table (all ranks 1) always
    /// picks the first.
    fn pick<'a>(candidates: &'a [ModelCandidate], key: &str) -> &'a ModelCandidate {
        let want_highest = key == "complex";
        candidates
            .iter()
            .reduce(|best, candidate| {
                let better = if want_highest {
                    candidate.relative_cost > best.relative_cost
                } else {
                    candidate.relative_cost < best.relative_cost
                };
                if better {
                    candidate
                } else {
                    best
                }
            })
            .expect("caller checked candidates are non-empty")
    }
}

#[async_trait]
impl ModelSelector for RouterModelSelector {
    async fn select(&self, req: ModelSelectionRequest) -> ModelSelection {
        let ModelSelectionRequest {
            candidates,
            prompt_hint,
            current_model,
        } = req;

        // No candidates means no answerable question: spend no decision and
        // report precisely why the current model survived.
        if candidates.is_empty() {
            return ModelSelection {
                model: current_model,
                reason: SelectionReason::NoCandidates,
            };
        }

        let request = DecisionRequest {
            state: prompt_hint,
            questions: BTreeMap::from([(QUESTION_KEY.to_string(), Self::question(&candidates))]),
        };

        let response = match self.router.decide(request).await {
            Ok(response) => response,
            // Drop the error text: it may name an endpoint or carry a key.
            Err(_) => {
                tracing::warn!("decision router failed; keeping the current model for selection");
                return ModelSelection {
                    model: current_model,
                    reason: SelectionReason::RouterUnavailable,
                };
            }
        };

        let (key, confidence) = match response.answers.get(QUESTION_KEY) {
            Some(Answer::Choice {
                key, confidence, ..
            }) if confidence.is_finite() => (key.clone(), *confidence),
            Some(Answer::Choice { .. }) => {
                tracing::warn!(
                    "decision router returned a non-finite confidence; \
                     keeping the current model for selection"
                );
                return ModelSelection {
                    model: current_model,
                    reason: SelectionReason::RouterUnavailable,
                };
            }
            // A `Noul`, a wrong answer kind or an absent answer are all the
            // same defect: the router did not answer the question we asked.
            _ => {
                tracing::warn!(
                    "decision router did not answer the complexity choice; \
                     keeping the current model for selection"
                );
                return ModelSelection {
                    model: current_model,
                    reason: SelectionReason::RouterUnavailable,
                };
            }
        };

        if confidence < self.threshold {
            tracing::warn!(
                confidence,
                threshold = self.threshold,
                "low-confidence complexity choice; keeping the current model"
            );
            return ModelSelection {
                model: current_model,
                reason: SelectionReason::LowConfidence,
            };
        }

        ModelSelection {
            model: Self::pick(&candidates, &key).model.clone(),
            reason: SelectionReason::Chosen,
        }
    }
}
