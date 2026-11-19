//! Tasks 2–3: runner, report, strict mode, agent tool calls and judge checks.
//!
//! These tests exercise the runner end to end against the recorded fixture
//! server and against a fake Ollama socket that drives a real tool loop, plus
//! the pure check application and report rendering. No live key, network or
//! decision model is involved: judge checks skip when no router is injected,
//! and are driven by an injected fake `DecisionRouter` where a verdict matters.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use llama_r::error::AppError;
use llama_r::evals::{
    apply_check, apply_judge, load_eval, qualified_tool_name, redact_detail, run_suite, EvalCase,
    EvalCaseKind, EvalCheck, EvalCheckOutcome, EvalEvidence, EvalMode, EvalProvider, EvalReport,
    EvalRow, EvalSuite, FixtureServer, RunOptions,
};
use llama_r::ports::router::{Answer, DecisionRequest, DecisionResponse, DecisionRouter};

/// A minimal one-case suite with one provider, using the fixture answer derived
/// from `expect`.
fn suite_with(case: EvalCase) -> EvalSuite {
    EvalSuite {
        cases: vec![case],
        providers: vec![EvalProvider {
            name: "ollama".into(),
            kind: "ollama".into(),
        }],
        mode: EvalMode::Strict,
    }
}

fn plain_case(id: &str, expect: &str, checks: Vec<EvalCheck>) -> EvalCase {
    EvalCase {
        id: id.into(),
        kind: EvalCaseKind::Plain,
        prompt: "prompt".into(),
        expect: Some(expect.into()),
        checks,
        agent: None,
    }
}

fn outcome_of<'a>(report: &'a EvalReport, case: &str) -> &'a EvalCheckOutcome {
    &report
        .rows
        .iter()
        .find(|row| row.case == case)
        .unwrap_or_else(|| panic!("no row for case {case}"))
        .outcome
}

#[test]
fn contains_and_not_contains_pass_and_fail() {
    let evidence = EvalEvidence {
        text: "hello world".into(),
        tool_calls: Vec::new(),
    };

    assert_eq!(
        apply_check(
            &EvalCheck::Contains {
                text: "hello".into()
            },
            &evidence
        ),
        EvalCheckOutcome::Pass
    );
    assert!(matches!(
        apply_check(
            &EvalCheck::Contains {
                text: "nope".into()
            },
            &evidence
        ),
        EvalCheckOutcome::Fail { .. }
    ));
    assert_eq!(
        apply_check(
            &EvalCheck::NotContains {
                text: "nope".into()
            },
            &evidence
        ),
        EvalCheckOutcome::Pass
    );
    assert!(matches!(
        apply_check(
            &EvalCheck::NotContains {
                text: "hello".into()
            },
            &evidence
        ),
        EvalCheckOutcome::Fail { .. }
    ));
}

#[tokio::test]
async fn one_bad_case_fails_strict_but_not_lenient() {
    let suite = EvalSuite {
        cases: vec![
            plain_case(
                "good",
                "hello",
                vec![EvalCheck::Contains {
                    text: "hello".into(),
                }],
            ),
            plain_case(
                "bad",
                "hello",
                vec![EvalCheck::Contains {
                    text: "missing".into(),
                }],
            ),
        ],
        providers: vec![EvalProvider {
            name: "ollama".into(),
            kind: "ollama".into(),
        }],
        mode: EvalMode::Strict,
    };

    let report = run_suite(suite, RunOptions::default()).await;
    assert_eq!(report.rows.len(), 2);
    assert_eq!(outcome_of(&report, "good"), &EvalCheckOutcome::Pass);
    assert!(matches!(
        outcome_of(&report, "bad"),
        EvalCheckOutcome::Fail { .. }
    ));

    assert!(
        !report.all_passed(true),
        "strict mode must fail on a bad case"
    );
    assert!(
        report.all_passed(false),
        "lenient mode tolerates a bad case"
    );
}

#[tokio::test]
async fn a_malformed_case_is_reported_and_does_not_stop_the_suite() {
    // `broken` declares no checks and no expect, so it cannot be executed. It
    // must surface as a FAIL row carrying its id, and `good` must still run.
    let suite = EvalSuite {
        cases: vec![
            EvalCase {
                id: "broken".into(),
                kind: EvalCaseKind::Plain,
                prompt: "prompt".into(),
                expect: None,
                checks: Vec::new(),
                agent: None,
            },
            plain_case(
                "good",
                "hello",
                vec![EvalCheck::Contains {
                    text: "hello".into(),
                }],
            ),
        ],
        providers: vec![EvalProvider {
            name: "ollama".into(),
            kind: "ollama".into(),
        }],
        mode: EvalMode::Strict,
    };

    let report = run_suite(suite, RunOptions::default()).await;
    assert_eq!(report.rows.len(), 2, "one row per case x provider");
    match outcome_of(&report, "broken") {
        EvalCheckOutcome::Fail { detail } => {
            assert!(
                detail.contains("no checks"),
                "error must explain the malformed case: {detail}"
            );
        }
        other => panic!("malformed case must be a FAIL row, got {other:?}"),
    }
    assert_eq!(outcome_of(&report, "good"), &EvalCheckOutcome::Pass);
}

#[tokio::test]
async fn judge_without_router_is_skipped_with_reason() {
    let suite = suite_with(EvalCase {
        id: "judged".into(),
        kind: EvalCaseKind::Plain,
        prompt: "prompt".into(),
        expect: Some("hello".into()),
        checks: vec![EvalCheck::Judge {
            question: "Is the answer polite?".into(),
        }],
        agent: None,
    });

    let report = run_suite(suite, RunOptions::default()).await;
    assert_eq!(
        outcome_of(&report, "judged"),
        &EvalCheckOutcome::Skipped {
            reason: "router_unavailable".into()
        }
    );
    // A skipped judge never fails, in either mode.
    assert!(report.all_passed(true));
}

#[test]
fn report_table_contains_no_secret() {
    const SECRET: &str = "sk-planted-secret-123";
    let report = EvalReport::new(vec![EvalRow {
        case: "leaky".into(),
        provider: "ollama".into(),
        kind: "plain".into(),
        outcome: EvalCheckOutcome::Fail {
            // A structured detail is exactly what redact_secrets can inspect.
            detail: format!(r#"{{"api_key":"{SECRET}","note":"echo"}}"#),
        },
    }]);

    let table = report.render_table();
    assert!(!table.contains(SECRET), "table leaked the secret:\n{table}");
    assert!(table.contains("[REDACTED]"));
    assert!(table.contains("leaky"), "table must name the case");
    assert!(table.contains("ollama"), "table must name the provider");
    assert!(table.contains("FAIL"), "table must name the result");
}

#[test]
fn apply_check_redacts_structured_evidence() {
    const SECRET: &str = "ant-planted-secret-456";
    let evidence = EvalEvidence {
        text: format!(r#"{{"api_key":"{SECRET}","answer":"hello"}}"#),
        tool_calls: Vec::new(),
    };
    let outcome = apply_check(
        &EvalCheck::Contains {
            text: "absent".into(),
        },
        &evidence,
    );
    match outcome {
        EvalCheckOutcome::Fail { detail } => {
            assert!(
                !detail.contains(SECRET),
                "detail leaked the secret: {detail}"
            );
            assert!(detail.contains("[REDACTED]"));
        }
        other => panic!("expected a FAIL, got {other:?}"),
    }
    // The standalone helper behaves the same way on a structured detail.
    let redacted = redact_detail(&format!(r#"{{"token":"{SECRET}"}}"#));
    assert!(!redacted.contains(SECRET));
}

#[tokio::test]
async fn the_v1_smoke_suite_runs_green_through_the_runner() {
    // The exact scenario Task 4 will use: six providers × three cases, with no
    // router injected. Judge checks would skip; the v1 file has none, so every
    // row is a pass or an expected streaming-unavailable skip.
    let report = run_suite(load_eval(), RunOptions::default()).await;
    assert_eq!(report.rows.len(), 18, "3 cases × 6 providers");
    assert!(report.all_passed(true), "v1 fixture suite must stay green");
    let table = report.render_table();
    assert!(!table.contains("-eval-dummy"), "no dummy key may leak");
    for case in load_eval().cases {
        assert!(table.contains(&case.id), "table names case {}", case.id);
    }
}

#[test]
fn load_eval_parses_the_v1_smoke_toml() {
    let suite = load_eval();
    assert_eq!(suite.cases.len(), 3, "three smoke cases in the TOML");
    assert_eq!(
        suite.providers.len(),
        6,
        "six provider profiles in the TOML"
    );
    assert_eq!(suite.mode, EvalMode::Strict);
    // expect sugar was folded into a Contains check at load time.
    assert!(matches!(
        suite.cases[0].checks.as_slice(),
        [EvalCheck::Contains { .. }]
    ));
}

#[tokio::test]
async fn fixture_server_serves_a_recorded_body() {
    let server = FixtureServer::start(serde_json::json!({"answer": "hello"})).await;
    let url = server.url();
    let body = reqwest::get(&url)
        .await
        .expect("fixture responds")
        .text()
        .await
        .expect("body is readable");
    assert_eq!(body.trim(), r#"{"answer":"hello"}"#);
}

#[test]
fn table_with_no_router_option_is_skipped_not_failed_for_judge_checks() {
    // The judge check alone (not the whole suite) is skipped when there is no
    // router, even when the surrounding text checks pass.
    let evidence = EvalEvidence {
        text: "hello".into(),
        tool_calls: Vec::new(),
    };
    let outcome = apply_check(
        &EvalCheck::Judge {
            question: "Is this polite?".into(),
        },
        &evidence,
    );
    assert_eq!(
        outcome,
        EvalCheckOutcome::Skipped {
            reason: "router_unavailable".into()
        }
    );
}

// ---------------------------------------------------------------------------
// Task 3: a fake decision router (no live d1 process) and agent tool-loop cases
// ---------------------------------------------------------------------------

/// A `DecisionRouter` that always answers the `judge` question with a fixed
/// `yes`/`no` and confidence, or fails. This is the fake the spec asks for to
/// cover the non-skipped branches of `apply_judge` without a live d1 server.
struct FakeRouter {
    verdict: &'static str,
    confidence: f32,
    fail: bool,
}

impl FakeRouter {
    fn yes(confidence: f32) -> Arc<dyn DecisionRouter> {
        Arc::new(Self {
            verdict: "yes",
            confidence,
            fail: false,
        })
    }

    fn no(confidence: f32) -> Arc<dyn DecisionRouter> {
        Arc::new(Self {
            verdict: "no",
            confidence,
            fail: false,
        })
    }
}

#[async_trait]
impl DecisionRouter for FakeRouter {
    async fn decide(&self, _req: DecisionRequest) -> Result<DecisionResponse, AppError> {
        if self.fail {
            return Err(AppError::Provider("fake router unreachable".into()));
        }
        Ok(DecisionResponse {
            answers: BTreeMap::from([(
                "judge".to_string(),
                Answer::Choice {
                    key: self.verdict.to_string(),
                    confidence: self.confidence,
                    probabilities: BTreeMap::from([(self.verdict.to_string(), self.confidence)]),
                },
            )]),
        })
    }
}

fn judged_case(id: &str) -> EvalCase {
    EvalCase {
        id: id.into(),
        kind: EvalCaseKind::Plain,
        prompt: "Is this answer useful?".into(),
        expect: Some("useful".into()),
        checks: vec![EvalCheck::Judge {
            question: "Is the answer useful?".into(),
        }],
        agent: None,
    }
}

#[tokio::test]
async fn judge_with_fake_router_yes_passes_no_fails() {
    let suite = suite_with(judged_case("yes-case"));
    let options = RunOptions {
        router: Some(FakeRouter::yes(0.95)),
        ..RunOptions::default()
    };
    assert_eq!(
        outcome_of(&run_suite(suite, options).await, "yes-case"),
        &EvalCheckOutcome::Pass
    );

    let suite = suite_with(judged_case("no-case"));
    let options = RunOptions {
        router: Some(FakeRouter::no(0.95)),
        ..RunOptions::default()
    };
    let report = run_suite(suite, options).await;
    match outcome_of(&report, "no-case") {
        EvalCheckOutcome::Fail { detail } => {
            assert!(detail.contains("\"verdict\":\"no\""), "detail: {detail}");
        }
        other => panic!("a confident 'no' must FAIL, got {other:?}"),
    }
    assert!(
        !report.all_passed(true),
        "a failed judge fails the strict suite"
    );
}

#[tokio::test]
async fn judge_low_confidence_is_skipped_not_failed() {
    let evidence = EvalEvidence {
        text: "an answer".into(),
        tool_calls: Vec::new(),
    };
    let check = EvalCheck::Judge {
        question: "Good enough?".into(),
    };

    // A confident verdict below the threshold abstains with the stable reason.
    let router = FakeRouter::no(0.3);
    assert_eq!(
        apply_judge(&check, &evidence, &router, 0.6).await,
        EvalCheckOutcome::Skipped {
            reason: "low_confidence".into()
        }
    );
    // A router that errors also abstains (error text is deliberately dropped).
    let broken: Arc<dyn DecisionRouter> = Arc::new(FakeRouter {
        verdict: "yes",
        confidence: 0.99,
        fail: true,
    });
    assert_eq!(
        apply_judge(&check, &evidence, &broken, 0.6).await,
        EvalCheckOutcome::Skipped {
            reason: "router_unavailable".into()
        }
    );
    // A custom threshold is honoured: 0.75 clears 0.6 but not 0.8.
    let router = FakeRouter::yes(0.75);
    assert_eq!(
        apply_judge(&check, &evidence, &router, 0.8).await,
        EvalCheckOutcome::Skipped {
            reason: "low_confidence".into()
        }
    );
}

// ---- agent cases: the fake-socket tool loop (rig-engine only) --------------

#[cfg(feature = "rig-engine")]
fn agent_case(id: &str, prompt: &str, checks: Vec<EvalCheck>) -> EvalCase {
    EvalCase {
        id: id.into(),
        kind: EvalCaseKind::Agent,
        prompt: prompt.into(),
        expect: None,
        checks,
        agent: Some("eval-agent".into()),
    }
}

#[cfg(feature = "rig-engine")]
fn agent_suite(cases: Vec<EvalCase>) -> EvalSuite {
    EvalSuite {
        cases,
        providers: Vec::new(),
        mode: EvalMode::Strict,
    }
}

#[cfg(feature = "rig-engine")]
use llama_r::evals::{EVAL_AGENT_ANSWER as AGENT_ANSWER, EVAL_AGENT_TOOL as AGENT_TOOL};

/// The runner's qualification must be byte-identical to the engine's MCP naming
/// scheme; local tools keep their bare id (an empty `server_id`).
#[cfg(feature = "rig-engine")]
#[test]
fn called_tool_uses_the_same_qualified_name_as_the_engine() {
    use llama_r::adapters::rig_engine::tools::rig_tool_name;
    assert_eq!(
        qualified_tool_name("my-mcp", "list_orders"),
        rig_tool_name("my-mcp", "list_orders")
    );
    assert_eq!(qualified_tool_name("", "llama_r__skill"), "llama_r__skill");
}

#[cfg(feature = "rig-engine")]
#[tokio::test]
async fn called_tool_detects_a_real_agent_tool_call() {
    let report = run_suite(
        agent_suite(vec![
            agent_case(
                "right-tool",
                "use the tool",
                vec![
                    EvalCheck::CalledTool {
                        name: AGENT_TOOL.into(),
                    },
                    EvalCheck::Contains {
                        text: AGENT_ANSWER.into(),
                    },
                ],
            ),
            agent_case(
                "wrong-tool",
                "use the tool",
                vec![EvalCheck::CalledTool {
                    name: "server__not_called".into(),
                }],
            ),
        ]),
        RunOptions::default(),
    )
    .await;

    assert_eq!(report.rows.len(), 2, "one row per agent case");
    assert_eq!(
        outcome_of(&report, "right-tool"),
        &EvalCheckOutcome::Pass,
        "the real tool call must be observed and qualified: {}",
        report.render_table()
    );
    match outcome_of(&report, "wrong-tool") {
        EvalCheckOutcome::Fail { detail } => {
            assert!(
                detail.contains(AGENT_TOOL),
                "detail should name the tool seen: {detail}"
            );
            assert!(
                detail.contains("called_tool"),
                "detail should name the check: {detail}"
            );
        }
        other => panic!("a wrong tool name must FAIL, got {other:?}"),
    }
    assert!(!report.all_passed(true));
}

#[cfg(feature = "rig-engine")]
#[tokio::test]
async fn a_tool_that_fails_to_execute_never_yields_a_green_row() {
    // `llama_r__skill` gets a skill id that does not exist. The tool result is
    // an error, so the row must FAIL with the tool named in a redacted detail —
    // even though the model goes on to produce a final answer.
    let prompt = r#"{"name":"skill-that-does-not-exist"}"#;
    let report = run_suite(
        agent_suite(vec![agent_case(
            "failing-tool",
            prompt,
            vec![EvalCheck::CalledTool {
                name: AGENT_TOOL.into(),
            }],
        )]),
        RunOptions::default(),
    )
    .await;

    match outcome_of(&report, "failing-tool") {
        EvalCheckOutcome::Fail { detail } => {
            assert!(detail.contains("failed to execute"), "detail: {detail}");
            assert!(
                detail.contains(AGENT_TOOL),
                "detail must name the tool: {detail}"
            );
        }
        other => panic!("a failed tool must FAIL the row, got {other:?}"),
    }
}

#[cfg(feature = "rig-engine")]
#[tokio::test]
async fn a_called_tool_case_with_no_observed_call_fails_not_skips() {
    // The fixture answers straight away when asked to skip the tool. A
    // `called_tool` case must then FAIL, not become a green SKIP row.
    let report = run_suite(
        agent_suite(vec![agent_case(
            "no-call",
            r#"{"skip_tool":true}"#,
            vec![EvalCheck::CalledTool {
                name: AGENT_TOOL.into(),
            }],
        )]),
        RunOptions::default(),
    )
    .await;

    match outcome_of(&report, "no-call") {
        EvalCheckOutcome::Fail { detail } => {
            assert!(
                detail.contains("no tool call was observed"),
                "detail: {detail}"
            );
        }
        other => panic!("an unobserved call must FAIL, got {other:?}"),
    }
    assert!(!report.all_passed(true));
}

#[cfg(feature = "rig-engine")]
#[tokio::test]
async fn an_agent_case_with_only_text_checks_works() {
    // No `called_tool`: the same evidence still drives plain text assertions.
    let report = run_suite(
        agent_suite(vec![agent_case(
            "text-only",
            "hello agent",
            vec![
                EvalCheck::Contains {
                    text: AGENT_ANSWER.into(),
                },
                EvalCheck::NotContains {
                    text: "never-said".into(),
                },
            ],
        )]),
        RunOptions::default(),
    )
    .await;
    assert_eq!(outcome_of(&report, "text-only"), &EvalCheckOutcome::Pass);
}

#[cfg(feature = "rig-engine")]
#[tokio::test]
async fn a_secret_in_a_tool_arg_never_reaches_the_report() {
    const SECRET: &str = "sk-eval-tool-secret-42";
    // The prompt becomes the tool's arguments on the fake socket; a credential
    // field there is echoed into the answer, so it would naturally flow into a
    // failing check's detail. Redaction must strip it at every layer.
    let prompt = format!(r#"{{"name":"eval-tool","api_key":"{SECRET}"}}"#);
    let report = run_suite(
        agent_suite(vec![agent_case(
            "leaky-tool",
            &prompt,
            // A deliberately failing check forces a detail that embeds the
            // answer, so the credential echoed from the tool arg is exercised.
            vec![EvalCheck::Contains {
                text: "force-a-failure".into(),
            }],
        )]),
        RunOptions::default(),
    )
    .await;

    assert!(matches!(
        outcome_of(&report, "leaky-tool"),
        EvalCheckOutcome::Fail { .. }
    ));
    let table = report.render_table();
    assert!(
        !table.contains(SECRET),
        "the report leaked the secret:\n{table}"
    );
    assert!(
        table.contains("[REDACTED]"),
        "the report should show redaction"
    );
}
