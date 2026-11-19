//! Declarative evaluation cases, checks, runner and reports.
//!
//! The data model lives in [`cases`] and [`check`]; [`runner`] executes cases
//! (using the recorded [`fixture`] for provider-class ones) and [`report`]
//! renders the result. Everything here is compiled unconditionally: it uses
//! only always-on dependencies, so `--no-default-features` stays clean.

mod cases;
mod check;
mod fixture;
mod report;
mod runner;

pub use cases::{EvalCase, EvalCaseKind, EvalMode, EvalProvider, EvalSuite};
pub use check::{
    apply_check, apply_judge, redact_detail, EvalCheck, EvalCheckOutcome, EvalEvidence,
};
pub use fixture::{build_provider, load_eval, recorded_body, FixtureServer, EVAL_MODEL};
pub use report::{EvalReport, EvalRow};
pub use runner::{run_suite, RunOptions, DEFAULT_JUDGE_THRESHOLD};

/// The shipped v1 smoke suite, shared by the runner and the smoke test.
const EVAL_TOML: &str = include_str!("../../evals/providers/smoke.eval.toml");
