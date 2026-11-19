//! Declarative evaluation cases and checks.
//!
//! This module owns only the data model for the eval runner: the TOML-shaped
//! types ([`EvalSuite`], [`EvalCase`], [`EvalCheck`]) and their desugaring rules.
//! Executing cases belongs to the runner (a later task), not here.

mod cases;
mod check;

pub use cases::{EvalCase, EvalCaseKind, EvalMode, EvalProvider, EvalSuite};
pub use check::{EvalCheck, EvalCheckOutcome};
