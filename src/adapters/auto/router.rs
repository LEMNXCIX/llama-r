//! Router-backed [`AutoDecider`]: one safety `Choice` per tool call.
//!
//! The decision model sees only the tool name, the canonical argument hash and
//! the redacted arguments — never an unredacted credential. The bridge to d1 is
//! the same [`DecisionRouter`] agent routing uses, so no new wire format is
//! introduced here; the core's typed question/answer shapes travel outward.
//!
//! The verdict ladder is deliberately one-way: a single affirmative `yes`
//! above the threshold is the only path to [`AutoVerdict::Allow`]. Everything
//! else refuses or degrades, and both keep the human gate in place.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::ports::auto::{AutoDecider, AutoVerdict};
use crate::ports::router::{Answer, DecisionRequest, DecisionRouter, Question};
use crate::services::redaction::redact_secrets;

/// The question name is also the key the answer is looked up under. Using one
/// constant for both means a rename cannot leave the request and lookup out of
/// sync (which would silently turn every verdict into a denial).
const QUESTION_KEY: &str = "auto";

const INSTRUCTIONS: &str = "Is this tool call safe to execute automatically? Answer yes only \
when the tool name and its arguments are clearly benign and carry no suspicious or harmful \
intent. Answer no when you are unsure, when the arguments look malicious, or when a human \
should review the call first.";

const CRITERIA_YES: &str = "The tool call is clearly safe and may be approved without a human";
const CRITERIA_NO: &str = "The tool call is unsafe, uncertain, or needs human review";

/// An [`AutoDecider`] that asks the injected decision router for a `yes`/`no`
/// safety choice over one tool call.
pub struct RouterAutoDecider {
    router: Arc<dyn DecisionRouter>,
    min_confidence: f32,
}

impl RouterAutoDecider {
    /// `min_confidence` is the default threshold (the router config's
    /// `MIN_CONFIDENCE`). It is retained here so a caller can read it as the
    /// fallback for a policy without an explicit `auto_threshold`; the
    /// authoritative value for one decision is the `threshold` passed to
    /// [`AutoDecider::should_allow`].
    pub fn new(router: Arc<dyn DecisionRouter>, min_confidence: f32) -> Self {
        Self {
            router,
            min_confidence,
        }
    }

    /// The configured default threshold, exposed for the gate to use when a
    /// policy does not override it.
    pub fn min_confidence(&self) -> f32 {
        self.min_confidence
    }

    fn safety_question() -> Question {
        Question::Choice {
            instructions: INSTRUCTIONS.to_string(),
            criteria: BTreeMap::from([
                ("yes".to_string(), CRITERIA_YES.to_string()),
                ("no".to_string(), CRITERIA_NO.to_string()),
            ]),
        }
    }
}

#[async_trait]
impl AutoDecider for RouterAutoDecider {
    async fn should_allow(
        &self,
        tool_name: &str,
        redacted_args: &Value,
        args_hash_hex: &str,
        threshold: f32,
    ) -> AutoVerdict {
        // Redact again even though the caller is expected to have done it: the
        // operation is idempotent (the marker contains no secret fragment) and
        // a caller mistake must not put a credential on the wire.
        let redacted = redact_secrets(redacted_args);
        let args_json =
            serde_json::to_string(&redacted).expect("a serde_json::Value always serializes");
        let state = format!("tool: {tool_name}\nargs_hash: {args_hash_hex}\nargs: {args_json}");

        let request = DecisionRequest {
            state,
            questions: BTreeMap::from([(QUESTION_KEY.to_string(), Self::safety_question())]),
        };

        let response = match self.router.decide(request).await {
            Ok(response) => response,
            // Drop the error text: it may name an endpoint or carry credentials.
            Err(_) => return AutoVerdict::Unavailable,
        };

        match response.answers.get(QUESTION_KEY) {
            Some(Answer::Choice {
                key, confidence, ..
            }) if key == "yes" && *confidence >= threshold => AutoVerdict::Allow,
            // A refusal, low confidence, an unexpected key, a `Noul`, or an
            // absent answer are all refusals — never an automatic approval.
            _ => AutoVerdict::Deny,
        }
    }
}
