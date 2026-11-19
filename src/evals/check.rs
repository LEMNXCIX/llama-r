//! Declarative assertions applied to the evidence collected for an eval case.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ports::router::{Answer, DecisionRequest, DecisionRouter, Question};
use crate::services::redaction::redact_secrets;

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

/// What a runner collected for one case before applying its checks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvalEvidence {
    /// The decoded answer text (final tokens of a stream concatenated).
    pub text: String,
    /// Qualified tool names observed during an agent turn. Empty for
    /// provider-class cases; populated by the agent runner in a later task.
    pub tool_calls: Vec<String>,
}

/// Redact a detail string before it reaches a report.
///
/// Structured JSON is parsed and walked with [`redact_secrets`], so a secret
/// echoed back under a credential-looking field cannot reach the table. Plain
/// prose has no field names for redaction to inspect and is preserved verbatim
/// (the same rule `execution_api::redact_message` follows for user messages).
pub fn redact_detail(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(value @ (Value::Object(_) | Value::Array(_))) => {
            serde_json::to_string(&redact_secrets(&value)).unwrap_or_else(|_| text.to_string())
        }
        _ => text.to_string(),
    }
}

/// Parse evidence text as JSON when it is structured, so redaction can walk
/// credential-bearing fields rather than treating the whole text as prose.
fn evidence_value(evidence: &EvalEvidence) -> Value {
    serde_json::from_str(&evidence.text).unwrap_or_else(|_| Value::String(evidence.text.clone()))
}

/// Apply a check against collected evidence.
///
/// `judge` is handled here only for the no-router case (it skips, never
/// auto-passes). When a router is configured the runner calls [`apply_judge`]
/// instead, because a decision is asynchronous.
pub fn apply_check(check: &EvalCheck, evidence: &EvalEvidence) -> EvalCheckOutcome {
    match check {
        EvalCheck::Contains { text } => {
            if evidence.text.contains(text) {
                EvalCheckOutcome::Pass
            } else {
                EvalCheckOutcome::Fail {
                    detail: redact_detail(
                        &serde_json::json!({
                            "check": "contains",
                            "missing": text,
                            "in": evidence_value(evidence),
                        })
                        .to_string(),
                    ),
                }
            }
        }
        EvalCheck::NotContains { text } => {
            if !evidence.text.contains(text) {
                EvalCheckOutcome::Pass
            } else {
                EvalCheckOutcome::Fail {
                    detail: redact_detail(
                        &serde_json::json!({
                            "check": "not_contains",
                            "unexpected": text,
                            "in": evidence_value(evidence),
                        })
                        .to_string(),
                    ),
                }
            }
        }
        EvalCheck::CalledTool { name } => {
            if evidence.tool_calls.is_empty() {
                // Tool-call evidence comes from the agent runner; without it a
                // called_tool check is skipped, never silently passed.
                EvalCheckOutcome::Skipped {
                    reason: "no_tool_evidence".into(),
                }
            } else if evidence.tool_calls.iter().any(|call| call == name) {
                EvalCheckOutcome::Pass
            } else {
                EvalCheckOutcome::Fail {
                    detail: redact_detail(
                        &serde_json::json!({
                            "check": "called_tool",
                            "missing": name,
                            "calls": evidence.tool_calls,
                        })
                        .to_string(),
                    ),
                }
            }
        }
        EvalCheck::Judge { .. } => EvalCheckOutcome::Skipped {
            reason: "router_unavailable".into(),
        },
    }
}

/// Ask the decision router whether the evidence satisfies a `judge` question.
///
/// The question becomes a two-label `Choice` (`yes`/`no`) over the answer text.
/// A `yes` above `threshold` passes, a `no` above it fails, and anything else —
/// low confidence, an unexpected answer, a router error, or no answer at all —
/// is skipped with an explicit reason. There is no automatic pass: without a
/// decision model a judge check can only abstain.
pub async fn apply_judge(
    check: &EvalCheck,
    evidence: &EvalEvidence,
    router: &Arc<dyn DecisionRouter>,
    threshold: f32,
) -> EvalCheckOutcome {
    let EvalCheck::Judge { question } = check else {
        return EvalCheckOutcome::Skipped {
            reason: "not_a_judge_check".into(),
        };
    };

    let criteria = BTreeMap::from([
        (
            "yes".to_string(),
            "The answer satisfies the question".to_string(),
        ),
        (
            "no".to_string(),
            "The answer does not satisfy the question".to_string(),
        ),
    ]);
    let request = DecisionRequest {
        state: evidence.text.clone(),
        questions: BTreeMap::from([(
            "judge".to_string(),
            Question::Choice {
                instructions: question.clone(),
                criteria,
            },
        )]),
    };

    let response = match router.decide(request).await {
        Ok(response) => response,
        // The error text is deliberately dropped: it may name an endpoint or
        // carry credential material. The reason is the stable contract.
        Err(_) => {
            return EvalCheckOutcome::Skipped {
                reason: "router_unavailable".into(),
            }
        }
    };

    match response.answers.get("judge") {
        Some(Answer::Choice {
            key, confidence, ..
        }) if *confidence >= threshold => match key.as_str() {
            "yes" => EvalCheckOutcome::Pass,
            "no" => EvalCheckOutcome::Fail {
                detail: redact_detail(
                    &serde_json::json!({
                        "check": "judge",
                        "question": question,
                        "verdict": "no",
                        "evidence": evidence_value(evidence),
                    })
                    .to_string(),
                ),
            },
            other => EvalCheckOutcome::Skipped {
                reason: format!("unexpected_choice:{other}"),
            },
        },
        Some(Answer::Choice { .. }) => EvalCheckOutcome::Skipped {
            reason: "low_confidence".into(),
        },
        Some(Answer::Noul { .. }) => EvalCheckOutcome::Skipped {
            reason: "unexpected_answer_kind".into(),
        },
        None => EvalCheckOutcome::Skipped {
            reason: "router_unavailable".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_check_text_assertions() {
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
        assert_eq!(
            apply_check(
                &EvalCheck::NotContains {
                    text: "absent".into()
                },
                &evidence
            ),
            EvalCheckOutcome::Pass
        );
        assert!(matches!(
            apply_check(&EvalCheck::Contains { text: "x".into() }, &evidence),
            EvalCheckOutcome::Fail { .. }
        ));
    }

    #[test]
    fn called_tool_without_evidence_is_skipped() {
        assert_eq!(
            apply_check(
                &EvalCheck::CalledTool {
                    name: "server__tool".into()
                },
                &EvalEvidence::default()
            ),
            EvalCheckOutcome::Skipped {
                reason: "no_tool_evidence".into()
            }
        );
    }

    #[test]
    fn judge_without_a_router_is_never_a_pass() {
        assert_eq!(
            apply_check(
                &EvalCheck::Judge {
                    question: "ok?".into()
                },
                &EvalEvidence::default()
            ),
            EvalCheckOutcome::Skipped {
                reason: "router_unavailable".into()
            }
        );
    }
}
