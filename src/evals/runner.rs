//! Executes declarative eval cases and assembles the report.
//!
//! Provider-class cases (`plain`, `tool`, `stream`) run against the recorded
//! fixture server, one row per case × provider. Agent cases will be wired to a
//! real tool loop in a later task; until then they produce a named FAIL row
//! rather than a false pass. Every per-case error is caught into a row so one
//! bad case cannot stop the suite.

use std::str::FromStr;
use std::sync::Arc;

use tokio_stream::StreamExt;

use crate::config::ProviderKind;
use crate::domain::models::{ChatMessage, ChatRequest};
use crate::ports::router::DecisionRouter;

use super::check::{
    apply_check, apply_judge, redact_detail, EvalCheck, EvalCheckOutcome, EvalEvidence,
};
use super::fixture::{build_provider, recorded_body, FixtureServer};
use super::report::{EvalReport, EvalRow};
use super::{EvalCase, EvalCaseKind, EvalMode, EvalProvider, EvalSuite};

/// Confidence a judge answer must reach to be trusted, unless overridden.
pub const DEFAULT_JUDGE_THRESHOLD: f32 = 0.6;

/// Configuration for one [`run_suite`] call.
pub struct RunOptions {
    /// Optional decision model for `judge` checks. Without one they skip.
    pub router: Option<Arc<dyn DecisionRouter>>,
    /// Strict is the CI default; lenient tolerates failing rows.
    pub mode: EvalMode,
    /// Minimum confidence for a judge verdict to count.
    pub threshold: f32,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            router: None,
            mode: EvalMode::Strict,
            threshold: DEFAULT_JUDGE_THRESHOLD,
        }
    }
}

impl RunOptions {
    /// Whether the caller should treat the resulting report as a CI gate.
    pub fn strict(&self) -> bool {
        self.mode == EvalMode::Strict
    }
}

/// How one case execution finished before its checks were applied.
enum Execution {
    /// Evidence to apply the case's checks against.
    Evidence(EvalEvidence),
    /// A recorded, expected condition with no answer to check (a stream that
    /// the provider cannot serve). Skipped, never passed or failed.
    Skip(String),
}

/// Run every case in the suite and return its report.
pub async fn run_suite(suite: EvalSuite, options: RunOptions) -> EvalReport {
    let mut rows = Vec::new();
    for case in &suite.cases {
        if case.kind == EvalCaseKind::Agent {
            rows.push(agent_row(case));
            continue;
        }
        for provider in &suite.providers {
            let result = execute_provider_case(case, provider).await;
            rows.push(make_row(case, &provider.name, result, &options).await);
        }
    }
    EvalReport::new(rows)
}

fn row_kind(case: &EvalCase) -> String {
    format!("{:?}", case.kind).to_lowercase()
}

/// An agent case has no tool-loop runner in this task: fail loudly, with its
/// id, instead of inventing evidence. A later task replaces this row with the
/// real agent turn.
fn agent_row(case: &EvalCase) -> EvalRow {
    EvalRow {
        case: case.id.clone(),
        provider: "agent".into(),
        kind: row_kind(case),
        outcome: EvalCheckOutcome::Fail {
            detail: redact_detail(
                &serde_json::json!({
                    "error": "agent case execution is not available yet",
                    "case": case.id,
                })
                .to_string(),
            ),
        },
    }
}

/// Run one provider-class case against its own one-shot fixture server.
async fn execute_provider_case(
    case: &EvalCase,
    provider: &EvalProvider,
) -> Result<Execution, String> {
    if case.checks.is_empty() {
        return Err(format!("case '{}' declares no checks", case.id));
    }
    let kind = ProviderKind::from_str(&provider.kind)
        .map_err(|err| format!("case '{}': {err}", case.id))?;

    let answer = format!(
        "fixture answer: {}",
        case.expect.clone().unwrap_or_default()
    );
    let server = FixtureServer::start(recorded_body(kind, &answer)).await;
    let client = build_provider(kind, &server.url());
    let request = ChatRequest {
        model: String::new(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: case.prompt.clone(),
        }],
        stream: case.kind == EvalCaseKind::Stream,
    };

    let text = match case.kind {
        EvalCaseKind::Plain | EvalCaseKind::Tool => {
            let response = client
                .chat(request)
                .await
                .map_err(|err| format!("case '{}': {err}", case.id))?;
            response.message.content
        }
        EvalCaseKind::Stream => {
            let mut stream = match client.chat_stream(request).await {
                Ok(stream) => stream,
                // Providers without streaming report Unavailable without
                // connecting; that is their recorded, expected behavior.
                Err(err)
                    if err
                        .to_string()
                        .to_ascii_lowercase()
                        .contains("streaming is not supported") =>
                {
                    return Ok(Execution::Skip("streaming_unavailable".into()));
                }
                Err(err) => return Err(format!("case '{}': {err}", case.id)),
            };
            let mut text = String::new();
            while let Some(event) = stream.next().await {
                match event {
                    Ok(event) => text.push_str(&event.message.content),
                    Err(err) => return Err(format!("case '{}': {err}", case.id)),
                }
            }
            text
        }
        EvalCaseKind::Agent => unreachable!("agent cases are dispatched before this point"),
    };

    Ok(Execution::Evidence(EvalEvidence {
        text,
        tool_calls: Vec::new(),
    }))
}

/// Turn a case execution into one report row, applying all of the case's
/// checks. A failure wins over a skip, which wins over a pass.
async fn make_row(
    case: &EvalCase,
    provider: &str,
    result: Result<Execution, String>,
    options: &RunOptions,
) -> EvalRow {
    let base = |outcome| EvalRow {
        case: case.id.clone(),
        provider: provider.to_string(),
        kind: row_kind(case),
        outcome,
    };

    match result {
        Err(err) => base(EvalCheckOutcome::Fail {
            detail: redact_detail(&err),
        }),
        Ok(Execution::Skip(reason)) => base(EvalCheckOutcome::Skipped { reason }),
        Ok(Execution::Evidence(evidence)) => {
            let mut failures = Vec::new();
            let mut skips = Vec::new();
            for check in &case.checks {
                let outcome = match check {
                    EvalCheck::Judge { .. } if options.router.is_some() => {
                        let router = options.router.as_ref().expect("router checked above");
                        apply_judge(check, &evidence, router, options.threshold).await
                    }
                    _ => apply_check(check, &evidence),
                };
                match outcome {
                    EvalCheckOutcome::Pass => {}
                    EvalCheckOutcome::Fail { detail } => failures.push(detail),
                    EvalCheckOutcome::Skipped { reason } => skips.push(reason),
                }
            }
            let outcome = if !failures.is_empty() {
                EvalCheckOutcome::Fail {
                    detail: redact_detail(&failures.join("; ")),
                }
            } else if !skips.is_empty() {
                EvalCheckOutcome::Skipped {
                    reason: skips.join("; "),
                }
            } else {
                EvalCheckOutcome::Pass
            };
            base(outcome)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_are_strict_with_a_0_6_threshold_and_no_router() {
        let options = RunOptions::default();
        assert!(options.strict());
        assert!((options.threshold - 0.6).abs() < f32::EPSILON);
        assert!(options.router.is_none());
    }

    #[tokio::test]
    async fn an_empty_suite_reports_no_rows() {
        let report = run_suite(EvalSuite::default(), RunOptions::default()).await;
        assert!(report.rows.is_empty());
        assert!(report.all_passed(true));
    }
}
