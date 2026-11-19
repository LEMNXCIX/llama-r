//! Task 11: multi-provider eval smoke harness.
//!
//! This is a thin consumer of the declarative runner in `llama_r::evals`: it
//! loads the shipped suite, runs it with the CI options, prints the report and
//! gates on `all_passed(strict)`. All case execution, check application, table
//! rendering and redaction live in `src/evals/` and are tested there; nothing
//! is duplicated here.
//!
//! Every provider-class case is served from a local `FixtureServer`, so no live
//! key or network is involved. The `agent` case uses the real tool-loop runner
//! against a fake Ollama socket; the `judge` case has no router in these options
//! and therefore SKIPs (a skip never fails strict mode). A live smoke against
//! real keys is optional, labelled in the TOML, and deliberately not done here.

use llama_r::evals::{load_eval, run_suite, RunOptions};

#[tokio::test]
async fn runner_loads_cases_and_reports_all_case_provider_pairs() {
    let suite = load_eval();
    // Keep the six-provider wire coverage pinned: the runner exercises a real
    // adapter per kind, each with its own recorded response body.
    let provider_names: Vec<String> = suite
        .providers
        .iter()
        .map(|provider| provider.name.clone())
        .collect();
    assert_eq!(provider_names.len(), 6, "six provider profiles in the TOML");

    let options = RunOptions::default();
    let strict = options.strict();
    let report = run_suite(suite, options).await;
    report.print();

    let table = report.render_table();
    for name in &provider_names {
        assert!(table.contains(name), "report must name provider {name}");
    }
    // Dummy keys only: no credential material may reach the table. All four
    // dummy keys (sk-/ant-/oc-/gem-) share the "-eval-dummy" suffix.
    assert!(!table.contains("-eval-dummy"));
    assert!(
        report.all_passed(strict),
        "every strict row must pass (skips are allowed):\n{table}"
    );
}
