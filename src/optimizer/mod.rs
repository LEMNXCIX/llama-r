pub mod ast;
pub mod metrics;
pub mod rules;

use crate::domain::agent::OptimizeConfig;

pub struct TokenOptimizer {
    config: OptimizeConfig,
}

impl TokenOptimizer {
    pub fn new(config: OptimizeConfig) -> Self {
        Self { config }
    }

    /// Optimizes the provided text using AST-based parsing if structured rules apply
    pub fn optimize(&self, input: &str) -> String {
        if !self.config.enabled {
            return input.to_string();
        }

        let mut output = input.to_string();

        for rule in &self.config.rules {
            output = rules::apply_rule(rule, &output);
        }

        output
    }
}
