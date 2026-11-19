//! The result of running an eval suite, and its human-readable table.
//!
//! One row is one case × provider pair (agent cases have a single row). A row
//! carries the aggregated outcome of all the case's checks: a failure wins over
//! a skip, which wins over a pass. Details are redacted again at render time so
//! a hand-built or legacy row cannot leak a secret through the table.

use super::check::{redact_detail, EvalCheckOutcome};

/// One result row in an [`EvalReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalRow {
    pub case: String,
    pub provider: String,
    pub kind: String,
    pub outcome: EvalCheckOutcome,
}

impl EvalRow {
    /// A one-word result label for the table.
    pub fn result(&self) -> &'static str {
        match self.outcome {
            EvalCheckOutcome::Pass => "PASS",
            EvalCheckOutcome::Fail { .. } => "FAIL",
            EvalCheckOutcome::Skipped { .. } => "SKIP",
        }
    }

    /// The detail or skip reason, redacted for display.
    pub fn display_detail(&self) -> String {
        match &self.outcome {
            EvalCheckOutcome::Pass => "-".into(),
            EvalCheckOutcome::Fail { detail } => redact_detail(detail),
            EvalCheckOutcome::Skipped { reason } => format!("skipped: {reason}"),
        }
    }
}

/// All rows produced by one [`super::run_suite`] call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvalReport {
    pub rows: Vec<EvalRow>,
}

impl EvalReport {
    pub fn new(rows: Vec<EvalRow>) -> Self {
        Self { rows }
    }

    /// Whether the suite counts as green.
    ///
    /// Strict mode fails on any FAIL row. Skipped rows never fail in either
    /// mode: an abstaining judge is not a failing judge. Lenient mode tolerates
    /// failures (they are informational), which is what makes one bad case
    /// fail strict but not lenient.
    pub fn all_passed(&self, strict: bool) -> bool {
        if strict {
            !self
                .rows
                .iter()
                .any(|row| matches!(row.outcome, EvalCheckOutcome::Fail { .. }))
        } else {
            true
        }
    }

    /// A fixed-width table naming cases, providers, kinds, results and details.
    pub fn render_table(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{:<20} {:<18} {:<8} {:<6} {}\n",
            "provider", "case", "kind", "result", "detail"
        ));
        for row in &self.rows {
            out.push_str(&format!(
                "{:<20} {:<18} {:<8} {:<6} {}\n",
                row.provider,
                row.case,
                row.kind,
                row.result(),
                row.display_detail()
            ));
        }
        out
    }

    /// Print the table to stdout (used by the smoke test and the CLI).
    pub fn print(&self) {
        print!("{}", self.render_table());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(outcome: EvalCheckOutcome) -> EvalRow {
        EvalRow {
            case: "c".into(),
            provider: "p".into(),
            kind: "plain".into(),
            outcome,
        }
    }

    #[test]
    fn strict_fails_on_a_failure_lenient_does_not() {
        let report = EvalReport::new(vec![
            row(EvalCheckOutcome::Pass),
            row(EvalCheckOutcome::Fail {
                detail: "bad".into(),
            }),
        ]);
        assert!(!report.all_passed(true));
        assert!(report.all_passed(false));
    }

    #[test]
    fn skipped_never_fails_either_mode() {
        let report = EvalReport::new(vec![row(EvalCheckOutcome::Skipped {
            reason: "router_unavailable".into(),
        })]);
        assert!(report.all_passed(true));
        assert!(report.all_passed(false));
    }

    #[test]
    fn table_names_every_part_and_redacts_details() {
        let report = EvalReport::new(vec![EvalRow {
            case: "case-1".into(),
            provider: "ollama".into(),
            kind: "stream".into(),
            outcome: EvalCheckOutcome::Fail {
                detail: r#"{"api_key":"sk-secret"}"#.into(),
            },
        }]);
        let table = report.render_table();
        assert!(table.contains("case-1"));
        assert!(table.contains("ollama"));
        assert!(table.contains("stream"));
        assert!(table.contains("FAIL"));
        assert!(!table.contains("sk-secret"));
        assert!(table.contains("[REDACTED]"));
    }
}
