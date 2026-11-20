//! The fail-closed default [`ModelSelector`]: keep the current model.
//!
//! This is the selector installed when no decision router is configured, so a
//! deployment that never enabled routing sees exactly the model it saw before
//! this phase. It never calls a router and never invents a candidate.

use async_trait::async_trait;

use crate::ports::model_selection::{
    ModelSelection, ModelSelectionRequest, ModelSelector, SelectionReason,
};

pub struct CurrentModelSelector;

#[async_trait]
impl ModelSelector for CurrentModelSelector {
    async fn select(&self, req: ModelSelectionRequest) -> ModelSelection {
        ModelSelection {
            model: req.current_model,
            reason: SelectionReason::RouterUnavailable,
        }
    }
}
