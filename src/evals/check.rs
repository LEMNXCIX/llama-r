//! Declarative assertions applied to the evidence collected for an eval case.

use serde::{Deserialize, Serialize};

/// One assertion from a case's `checks` array.
///
/// Tagged by `type`, so TOML looks like:
///
/// ```toml
/// [[cases.checks]]
/// type = "contains"
/// text = "hello"
/// ```
///
/// The accepted tags are `contains`, `not_contains`, `called_tool` and `judge`.
/// An unknown tag is rejected by serde with an error naming the bad value and
/// the four accepted ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvalCheck {
    /// The answer must contain `text`.
    Contains { text: String },
    /// The answer must not contain `text`.
    NotContains { text: String },
    /// The turn must have emitted a tool call with this qualified name
    /// (`server__tool` for MCP, bare id for a local tool).
    CalledTool { name: String },
    /// Ask the decision router `question`; no router means "skipped".
    Judge { question: String },
}

/// The result of applying one [`EvalCheck`].
///
/// `Skipped` is never a pass: `judge` without a router (or with too little
/// confidence) lands here with an explicit reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalCheckOutcome {
    Pass,
    Fail { detail: String },
    Skipped { reason: String },
}
