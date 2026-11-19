//! Declarative evaluation cases, checks, runner and reports.
//!
//! The data model lives in [`cases`] and [`check`]; [`runner`] executes cases
//! (using the recorded [`fixture`] for provider-class ones and a fake Ollama
//! socket for agent ones when `rig-engine` is on) and [`report`] renders the
//! result. Only the agent fixture is feature-gated; everything else uses
//! always-on dependencies, so `--no-default-features` stays clean.

#[cfg(feature = "rig-engine")]
mod agent_fixture;
mod cases;
mod check;
mod fixture;
mod report;
mod runner;

#[cfg(feature = "rig-engine")]
pub use agent_fixture::{EVAL_AGENT_ANSWER, EVAL_AGENT_SKILL, EVAL_AGENT_TOOL};
pub use cases::{EvalCase, EvalCaseKind, EvalMode, EvalProvider, EvalSuite};
pub use check::{
    apply_check, apply_judge, qualified_tool_name, redact_detail, EvalCheck, EvalCheckOutcome,
    EvalEvidence,
};
pub use fixture::{build_provider, load_eval, recorded_body, FixtureServer, EVAL_MODEL};
pub use report::{EvalReport, EvalRow};
pub use runner::{run_suite, RunOptions, DEFAULT_JUDGE_THRESHOLD};

/// The shipped v1 smoke suite, shared by the runner and the smoke test.
const EVAL_TOML: &str = include_str!("../../evals/providers/smoke.eval.toml");
