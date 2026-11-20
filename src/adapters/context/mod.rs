//! Concrete `ContextCompactor` and `ModelSelector` adapters.

pub mod current_model_selector;
pub mod router_model_selector;
mod summarizing_compactor;

pub use current_model_selector::CurrentModelSelector;
pub use router_model_selector::RouterModelSelector;
pub use summarizing_compactor::SummarizingCompactor;

use crate::ports::compaction::{CompactRequest, CompactResult, ContextCompactor};
use async_trait::async_trait;

/// Compactor for callers with no budget pressure: history passes through
/// untouched and no memory is produced.
pub struct NoopCompactor;

#[async_trait]
impl ContextCompactor for NoopCompactor {
    async fn compact(&self, req: CompactRequest) -> Result<CompactResult, String> {
        Ok(CompactResult {
            memory: None,
            history: req.history,
        })
    }
}
