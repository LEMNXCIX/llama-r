//! Routing port: one decision, typed questions, probabilistic answers.
//!
//! Chat routing is a classification problem, not a lookup: which agent should
//! answer depends on what the message *is*, and the honest answer is a
//! distribution over candidates rather than a name. Liquid AI's d1 evaluates a
//! state against typed questions and returns probabilities per question, so this
//! port mirrors that shape for the two primitives routing needs — a `choice`
//! over labelled candidates and a `noul` confidence in a single interpretation.
//!
//! It parallels [`RagStore`](crate::ports::rag::RagStore) in that the core names
//! what it needs and never learns the wire format: the d1 adapter owns every
//! field d1 spells its own way, and a non-d1 decision model is a second
//! implementation behind the same trait rather than a branch in the core.
//!
//! Nothing here deserializes. `Answer` and `DecisionResponse` are `Serialize`
//! only because `debug_route` returns a decision to a human; the encoding that
//! `Question` and `Answer` travel *in* is the adapter's business, so a d1 tag
//! applied to these types would push that format back into the core.

use crate::error::AppError;
use async_trait::async_trait;
use serde::Serialize;
use std::collections::BTreeMap;

/// One thing worth asking d1, in one of its own shapes.
///
/// Questions travel outward only, so this type carries no serde attribute. d1's
/// encoding (`question` tags, `context`, `options`) is decided by the adapter
/// in Task 2 and must not become the core's vocabulary.
///
/// `Choice.criteria` is a map of label → description, and the returned `key`
/// is one of its keys. `Score.criteria` (unused here) is an *ordered list* whose
/// levels are indexed from 0 — not a map. Do not add `Score` with a map.
#[derive(Debug, Clone, PartialEq)]
pub enum Question {
    /// Pick one label out of `criteria`, scored against `instructions`.
    Choice {
        instructions: String,
        /// Label → what that label means. Ordered by key, so the same request
        /// produces the same question every time.
        criteria: BTreeMap<String, String>,
    },
    /// Ask how well `instructions` describes the state, as a probability.
    Noul { instructions: String },
}

/// Everything d1 is asked about one incoming message.
///
/// `state` is the message text as the adapter saw it. Routing builds these from
/// the request, so it never reaches into the adapter's payload shape.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionRequest {
    pub state: String,
    /// Question name → question. Names are the caller's handle on an answer:
    /// `answers` is keyed by exactly these.
    pub questions: BTreeMap<String, Question>,
}

/// d1's verdict, one entry per question asked.
///
/// The keys of `answers` are the `questions` keys of the request that produced
/// it. A missing answer is a bug in the adapter, not an empty probability — do
/// not read one as "no opinion"; there is no such thing here, which is what
/// `Noul` is for.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DecisionResponse {
    pub answers: BTreeMap<String, Answer>,
}

/// What one question answered.
///
/// Internally tagged so a serialized answer is self-describing: an HTTP debug
/// response can carry both kinds in one map without the reader having to know
/// which variant to expect before reading the value.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum Answer {
    /// The winning label, its confidence, and the full distribution.
    Choice {
        key: String,
        confidence: f32,
        /// Label → probability. Kept alongside `key` because a routing decision
        /// close to the threshold is worth logging, and a winner without its
        /// margin cannot be judged after the fact.
        probabilities: BTreeMap<String, f32>,
    },
    /// A probability, not a label: how well `instructions` fits.
    Noul { value: f32 },
}

/// One routing decision.
#[async_trait]
pub trait DecisionRouter: Send + Sync {
    /// Decide `req` against the configured model.
    ///
    /// Failures are the adapter's to classify: an unreachable or unparseable
    /// provider is [`AppError::Provider`] (a `502` at the edge), so a caller can
    /// tell "d1 said no" from "d1 did not answer" and fall back rather than
    /// treat the latter as a routing result.
    async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, AppError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::collections::BTreeMap;

    /// The port stores probabilities as `f32` (d1's resolution) and JSON numbers
    /// are read back as `f64`, so `0.91f32` arrives as `0.9100000262260437`.
    /// Comparing against the f64 literal `0.91` would fail on the representation,
    /// not the value, so every numeric assertion here goes through this epsilon.
    fn assert_prob(actual: &Value, expected: f64, path: &str) {
        let got = actual
            .as_f64()
            .unwrap_or_else(|| panic!("{path} is not a JSON number: {actual}"));
        assert!(
            (got - expected).abs() < 1e-6,
            "{path}: expected ~{expected}, got {got}"
        );
    }

    #[test]
    fn answers_serialize_with_their_answer_kind() {
        let mut answers = BTreeMap::new();
        answers.insert(
            "agent".to_string(),
            Answer::Choice {
                key: "fudi::ops".into(),
                confidence: 0.91,
                probabilities: BTreeMap::from([("fudi::ops".to_string(), 0.91)]),
            },
        );
        let response = DecisionResponse { answers };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["answers"]["agent"]["answer"], "choice");
        assert_eq!(json["answers"]["agent"]["key"], "fudi::ops");
        assert_prob(&json["answers"]["agent"]["confidence"], 0.91, "confidence");
        assert_prob(
            &json["answers"]["agent"]["probabilities"]["fudi::ops"],
            0.91,
            "probabilities[fudi::ops]",
        );
    }

    #[test]
    fn a_noul_is_a_probability_not_a_label() {
        let noul = Answer::Noul { value: 0.42 };
        let json = serde_json::to_value(&noul).unwrap();
        assert_eq!(json["answer"], "noul");
        assert_prob(&json["value"], 0.42, "value");
    }

    /// `answers` is one map, so a response can carry both kinds at once — which
    /// is the shape `debug_route` hands a human, and the reason `Answer` is
    /// internally tagged rather than externally tagged.
    #[test]
    fn both_answer_kinds_coexist_in_one_response() {
        let response = DecisionResponse {
            answers: BTreeMap::from([
                (
                    "agent".to_string(),
                    Answer::Choice {
                        key: "fudi::ops".into(),
                        confidence: 1.0 / 3.0,
                        probabilities: BTreeMap::from([("fudi::ops".to_string(), 0.25)]),
                    },
                ),
                ("confidence".to_string(), Answer::Noul { value: 1.0 / 3.0 }),
            ]),
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["answers"]["agent"]["answer"], "choice");
        assert_eq!(json["answers"]["agent"]["key"], "fudi::ops");
        assert_eq!(json["answers"]["confidence"]["answer"], "noul");
        assert_prob(&json["answers"]["agent"]["confidence"], 1.0 / 3.0, "choice");
        assert_prob(&json["answers"]["confidence"]["value"], 1.0 / 3.0, "noul");
    }

    /// The whole emitted document, so a later `#[serde]` change cannot silently
    /// alter the nesting the brief pins (`answers` → name → `answer`/`key`/…).
    #[test]
    fn the_emitted_shape_is_stable() {
        let response = DecisionResponse {
            answers: BTreeMap::from([(
                "agent".to_string(),
                Answer::Choice {
                    key: "fudi::ops".into(),
                    confidence: 1.0,
                    probabilities: BTreeMap::from([("fudi::ops".to_string(), 1.0)]),
                },
            )]),
        };
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"answers":{"agent":{"answer":"choice","key":"fudi::ops","confidence":1.0,"probabilities":{"fudi::ops":1.0}}}}"#
        );
    }
}
