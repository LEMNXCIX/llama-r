//! Task 2: runner, report and strict mode for declarative evals.
//!
//! These tests exercise the runner end to end against the recorded fixture
//! server, plus the pure check application and report rendering. No live key,
//! network or decision model is involved: judge checks are expected to skip
//! when no router is injected.

use llama_r::evals::{
    apply_check, load_eval, redact_detail, run_suite, EvalCase, EvalCaseKind, EvalCheck,
    EvalCheckOutcome, EvalEvidence, EvalMode, EvalProvider, EvalReport, EvalRow, EvalSuite,
    FixtureServer, RunOptions,
};

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
