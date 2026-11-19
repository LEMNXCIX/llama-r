//! Task 11: multi-provider eval smoke harness.
//!
//! Reads `evals/providers/smoke.eval.toml` and runs every case against every
//! configured provider, printing a pass/fail table. Fixture mode (the only mode
//! CI runs) binds a local `TcpListener` per case and serves a recorded response,
//! so no live key or network is involved. A live smoke against real keys is
//! optional, labelled in the TOML, and deliberately not performed here.
//!
//! The fixture server, TOML loader and per-adapter wire fixtures live in
//! `llama_r::evals` so the declarative runner and this smoke share one helper.

use llama_r::config::ProviderKind;
use llama_r::domain::models::{ChatMessage, ChatRequest};
use llama_r::evals::{
    build_provider, load_eval, recorded_body, EvalCase, EvalCaseKind, EvalSuite, FixtureServer,
};
use std::str::FromStr;
use tokio_stream::StreamExt;

fn kind_label(kind: EvalCaseKind) -> &'static str {
    match kind {
        EvalCaseKind::Plain => "plain",
        EvalCaseKind::Tool => "tool",
        EvalCaseKind::Stream => "stream",
        EvalCaseKind::Agent => "agent",
    }
}

#[derive(Debug)]
struct ResultRow {
    provider: String,
    case: String,
    kind: String,
    passed: bool,
    detail: String,
}

async fn run_case(case: &EvalCase, provider: &str, kind: ProviderKind) -> ResultRow {
    let expect = case.expect.as_deref().unwrap_or_default();
    let answer = format!("fixture answer: {expect}");
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

    let (passed, detail) = match case.kind {
        // Both plain and tool-prompt cases only check the returned answer text.
        // Direct chat has no tools path; tool invocation belongs to the Rig
        // engine and is out of scope for this fixture smoke.
        EvalCaseKind::Plain | EvalCaseKind::Tool => match client.chat(request).await {
            Ok(response) if response.message.content.contains(expect) => {
                (true, "matched expected answer".to_string())
            }
            Ok(response) => (
                false,
                format!("missing '{expect}' in: {}", response.message.content),
            ),
            Err(err) => (false, err.to_string()),
        },
        EvalCaseKind::Stream => match client.chat_stream(request).await {
            Ok(mut stream) => {
                let mut text = String::new();
                let mut error = None;
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(event) => text.push_str(&event.message.content),
                        Err(err) => {
                            error = Some(err.to_string());
                            break;
                        }
                    }
                }
                match error {
                    Some(err) => (false, err),
                    None if text.contains(expect) => (true, "matched streamed answer".to_string()),
                    None => (false, format!("missing '{expect}' in stream: {text}")),
                }
            }
            // Providers without streaming report Unavailable without connecting.
            // That is their recorded, expected behavior for a stream case.
            Err(err)
                if err
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("streaming is not supported") =>
            {
                (true, "streaming unavailable (expected)".to_string())
            }
            Err(err) => (false, err.to_string()),
        },
        EvalCaseKind::Agent => (
            false,
            "agent cases belong to the declarative runner".to_string(),
        ),
    };

    ResultRow {
        provider: provider.to_string(),
        case: case.id.clone(),
        kind: kind_label(case.kind).to_string(),
        passed,
        detail,
    }
}

async fn run_eval(eval: &EvalSuite) -> Vec<ResultRow> {
    let mut rows = Vec::new();
    for p in &eval.providers {
        let kind = ProviderKind::from_str(&p.kind).expect("valid provider kind in eval TOML");
        for case in &eval.cases {
            rows.push(run_case(case, &p.name, kind).await);
        }
    }
    rows
}

fn render_table(rows: &[ResultRow]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<20} {:<18} {:<8} {:<6} {}\n",
        "provider", "case", "kind", "result", "detail"
    ));
    for row in rows {
        let result = if row.passed { "PASS" } else { "FAIL" };
        out.push_str(&format!(
            "{:<20} {:<18} {:<8} {:<6} {}\n",
            row.provider, row.case, row.kind, result, row.detail
        ));
    }
    out
}

#[tokio::test]
async fn runner_loads_cases_and_reports_all_case_provider_pairs() {
    let eval: EvalSuite = load_eval();
    // Pinned expectations: the TOML ships 3 cases and 6 provider profiles.
    assert_eq!(eval.cases.len(), 3, "three smoke cases in the TOML");
    assert_eq!(eval.providers.len(), 6, "six provider profiles in the TOML");

    let rows = run_eval(&eval).await;
    assert_eq!(
        rows.len(),
        eval.cases.len() * eval.providers.len(),
        "one row per case x provider"
    );

    let table = render_table(&rows);
    println!("{table}");
    for case in &eval.cases {
        assert!(table.contains(&case.id), "table must name case {}", case.id);
    }
    for p in &eval.providers {
        assert!(
            table.contains(&p.name),
            "table must name provider {}",
            p.name
        );
    }
    assert_eq!(
        table.lines().count(),
        rows.len() + 1,
        "header + one line per row"
    );
    assert!(
        rows.iter().all(|row| row.passed),
        "every fixture-backed case should pass:\n{table}"
    );
    // Dummy keys only: no credential material may reach the table. All four
    // dummy keys (sk-/ant-/oc-/gem-) share the "-eval-dummy" suffix.
    assert!(!table.contains("-eval-dummy"));
}
