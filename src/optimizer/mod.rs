pub mod metrics;
pub mod tools;

use crate::domain::agent::OptimizeConfig;
use crate::optimizer::tools::unknown;

pub struct TokenOptimizer {
    config: OptimizeConfig,
}

impl TokenOptimizer {
    pub fn new(config: OptimizeConfig) -> Self {
        Self { config }
    }

    /// Apply the agent's tools to `input`, in the order they are configured.
    ///
    /// A no-op when the optimizer is off, and a no-op for an empty list — which is
    /// what every agent written by the TUI used to carry, with `enabled = true`
    /// beside it, because the form wrote that flag whether or not anything was
    /// selected.
    ///
    /// Order matters and is the configuration's order: `dedent_code` after
    /// `compress_code` drops fewer lines than before it, and nobody should have to
    /// know that.
    pub fn optimize(&self, input: &str) -> String {
        if !self.config.enabled {
            return input.to_string();
        }

        let mut output = input.to_string();
        for tool in &self.config.rules {
            output = tools::apply(tool, &output);
        }
        output
    }

    /// The configured names that name no tool.
    ///
    /// Separate from `optimize` on purpose: an unrecognised name has to keep
    /// working — a chat turn cannot fail because of a config typo — so the warning
    /// belongs to whoever loads the agent, not to the hot path.
    pub fn unknown_tools(&self) -> Vec<&str> {
        unknown(&self.config.rules)
    }
}
