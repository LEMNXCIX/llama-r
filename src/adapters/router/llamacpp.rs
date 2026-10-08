//! llama.cpp adapter: Liquid AI's d1 over `POST /v1/systemone`.
//!
//! d1 is a decision model — it generates no tokens, so Ollama's generative
//! endpoints can never serve it; it runs on a separate `llama-server` process
//! instead. This module is the only place in the crate that knows that URL or
//! d1's response spelling: a [`DecisionRequest`] goes out through
//! `encode_question` and a [`DecisionResponse`] comes back through
//! `decode_response`, so a wire difference between the hosted API and
//! the local server costs this file and nothing above it.
//!
//! The response envelope has never been observed locally (see
//! `fixtures/README.md`), so decoding refuses rather than guesses:
//! `decode_response` turns every structural surprise into
//! [`AppError::Provider`], because an upstream caller reads that as "router
//! unavailable" and falls back to the project's general agent — a
//! partially-filled answer would route worse than no answer.

use crate::error::AppError;
use crate::ports::router::{Answer, DecisionRequest, DecisionResponse, DecisionRouter, Question};
use async_trait::async_trait;
use reqwest::Client;
use std::collections::BTreeMap;
use std::time::Duration;

/// d1 behind llama-server: one decision per incoming message.
pub struct LlamaCppRouter {
    base_url: String,
    /// The decision model llama-server was started with. The endpoint does not
    /// take it in the body (the server already has the model loaded), so it is
    /// carried to identify decisions in the log.
    model: String,
    http: Client,
}

impl LlamaCppRouter {
    /// `base_url` is llama-server's origin (e.g. `http://127.0.0.1:8080`);
    /// `timeout_ms` bounds one decision, so a wedged model cannot hold a chat
    /// request open past the caller's patience.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>, timeout_ms: u64) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            base_url: base_url.into(),
            model: model.into(),
            http,
        }
    }

    /// Join like `OllamaEmbeddings::endpoint`: a configured base URL with a
    /// trailing `/` must not produce `//v1/systemone`.
    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }
}

/// One question in d1's own request spelling: `{"type", "instructions",
/// "criteria"}` for a choice, `{"type", "instructions"}` for a noul.
///
/// Module-private on purpose: `Question` in the port carries no serde attribute,
/// so this function is the only place that decides how a question travels out.
/// A noul must not grow a `criteria` key — the documented noul has no such
/// field, and an empty object is not the same as an absent one.
fn encode_question(question: &Question) -> serde_json::Value {
    match question {
        Question::Choice {
            instructions,
            criteria,
        } => serde_json::json!({
            "type": "choice",
            "instructions": instructions,
            "criteria": criteria,
        }),
        Question::Noul { instructions } => serde_json::json!({
            "type": "noul",
            "instructions": instructions,
        }),
    }
}

/// Read a `/v1/systemone` body into a `DecisionResponse`, or explain why it is
/// not one.
///
/// The envelope has never been observed against a local `llama-server` (see
/// `fixtures/README.md`), so every structural surprise is an error rather than
/// a default: an upstream caller treats [`AppError::Provider`] as "router
/// unavailable" and falls back to the project's general agent, while a
/// partially-filled response would route worse than no answer. Nothing here
/// panics, and the answer map is only ever built whole.
fn decode_response(body: &serde_json::Value) -> Result<DecisionResponse, AppError> {
    let answers = body
        .get("answers")
        .and_then(|value| value.as_object())
        .ok_or_else(|| AppError::Provider("systemone response has no 'answers' object".into()))?;

    let mut decoded = BTreeMap::new();
    for (name, answer) in answers {
        decoded.insert(name.clone(), decode_answer(name, answer)?);
    }
    Ok(DecisionResponse { answers: decoded })
}

/// One named answer, or the reason it is not one.
///
/// The kind is decided by which value field is present — `choice` or `noul` —
/// because those are the field names the decode test pins, and field presence
/// works whether or not the envelope carries the optional `type` string. When
/// `type` *is* present it must agree with the body: an envelope that
/// contradicts itself is refused, not guessed at.
fn decode_answer(name: &str, answer: &serde_json::Value) -> Result<Answer, AppError> {
    let object = answer.as_object().ok_or_else(|| {
        AppError::Provider(format!(
            "systemone answer '{name}' is not an object: {answer}"
        ))
    })?;

    let is_choice = match (object.contains_key("choice"), object.contains_key("noul")) {
        (true, false) => true,
        (false, true) => false,
        _ => {
            return Err(AppError::Provider(format!(
                "systemone answer '{name}' has no recognisable kind: \
                 expected exactly one of 'choice' or 'noul'"
            )))
        }
    };

    let kind = if is_choice { "choice" } else { "noul" };
    if let Some(declared) = object.get("type").and_then(|value| value.as_str()) {
        if declared != kind {
            return Err(AppError::Provider(format!(
                "systemone answer '{name}' declares type '{declared}' but its body is '{kind}'"
            )));
        }
    }

    if is_choice {
        let key = object
            .get("choice")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                AppError::Provider(format!("systemone answer '{name}' has no string 'choice'"))
            })?
            .to_string();
        let confidence = answer_number(object, name, "confidence")? as f32;
        let probabilities = object
            .get("probabilities")
            .and_then(|value| value.as_object())
            .ok_or_else(|| {
                AppError::Provider(format!(
                    "systemone answer '{name}' has no 'probabilities' object"
                ))
            })?
            .iter()
            .map(|(label, probability)| {
                probability
                    .as_f64()
                    .map(|value| (label.clone(), value as f32))
                    .ok_or_else(|| {
                        AppError::Provider(format!(
                            "systemone answer '{name}' has a non-numeric probability for '{label}'"
                        ))
                    })
            })
            .collect::<Result<BTreeMap<String, f32>, AppError>>()?;
        Ok(Answer::Choice {
            key,
            confidence,
            probabilities,
        })
    } else {
        let value = answer_number(object, name, "noul")? as f32;
        Ok(Answer::Noul { value })
    }
}

/// Read a required probability field, treating "missing" and "not a number"
/// as the same structural surprise: the caller cannot use either.
fn answer_number(
    object: &serde_json::Map<String, serde_json::Value>,
    name: &str,
    field: &str,
) -> Result<f64, AppError> {
    object
        .get(field)
        .and_then(|value| value.as_f64())
        .ok_or_else(|| {
            AppError::Provider(format!(
                "systemone answer '{name}' has no numeric '{field}'"
            ))
        })
}

#[async_trait]
impl DecisionRouter for LlamaCppRouter {
    /// One forward pass on d1: `state` plus typed questions in, calibrated
    /// answers out.
    ///
    /// Connection, status, parse and shape failures all become
    /// [`AppError::Provider`] (a `502` at the edge) with the same
    /// `embeddings`-style phrasing, so a caller can tell "d1 said no" from
    /// "d1 did not answer" and fall back rather than treat silence as a
    /// routing result.
    async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, AppError> {
        tracing::debug!(
            model = %self.model,
            questions = req.questions.len(),
            "systemone decision request"
        );

        let questions: serde_json::Map<String, serde_json::Value> = req
            .questions
            .iter()
            .map(|(name, question)| (name.clone(), encode_question(question)))
            .collect();
        let body = serde_json::json!({ "state": req.state, "questions": questions });

        let response = self
            .http
            .post(self.endpoint("/v1/systemone"))
            .json(&body)
            .send()
            .await
            .map_err(|err| AppError::Provider(format!("systemone request failed: {err}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(AppError::Provider(format!(
                "systemone HTTP {status}: {text}"
            )));
        }

        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|err| AppError::Provider(format!("systemone JSON parse failed: {err}")))?;

        decode_response(&value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `f64` JSON numbers come back as `f32` in the port (`0.87f32` widens to
    /// `0.8700000047683716f64`), so numeric assertions compare with an
    /// epsilon. Shape assertions stay exact.
    fn assert_prob(actual: f32, expected: f64, path: &str) {
        assert!(
            (actual as f64 - expected).abs() < 1e-6,
            "{path}: expected ~{expected}, got {actual}"
        );
    }

    #[test]
    fn encode_choice_uses_the_documented_wire_shape() {
        let q = Question::Choice {
            instructions: "Which agent?".into(),
            criteria: BTreeMap::from([("fudi::ops".to_string(), "Pedidos".to_string())]),
        };
        let json = encode_question(&q);
        assert_eq!(json["type"], "choice");
        assert_eq!(json["instructions"], "Which agent?");
        assert_eq!(json["criteria"]["fudi::ops"], "Pedidos");
    }

    #[test]
    fn encode_noul_has_no_criteria_field() {
        let json = encode_question(&Question::Noul {
            instructions: "Specialised?".into(),
        });
        assert_eq!(json["type"], "noul");
        assert_eq!(json["instructions"], "Specialised?");
        assert!(json.get("criteria").is_none());
    }

    #[test]
    fn decode_reads_the_fixture_into_answers() {
        let body: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/systemone_response.json")).unwrap();

        // The fixture itself spells the documented field names, independent of
        // what decode does with them.
        assert_eq!(body["answers"]["agent"]["type"], "choice");
        assert_eq!(body["answers"]["agent"]["choice"], "fudi::ops");
        assert!(body["answers"]["agent"].get("confidence").is_some());
        assert!(body["answers"]["agent"].get("probabilities").is_some());
        assert_eq!(body["answers"]["specialized"]["type"], "noul");
        assert!(body["answers"]["specialized"].get("noul").is_some());

        let decoded = decode_response(&body).unwrap();
        assert_eq!(decoded.answers.len(), 2);

        // The Choice carries `key`, `confidence`, `probabilities`.
        match &decoded.answers["agent"] {
            Answer::Choice {
                key,
                confidence,
                probabilities,
            } => {
                assert_eq!(key, "fudi::ops");
                assert_prob(*confidence, 0.82, "agent.confidence");
                assert_eq!(probabilities.len(), 2);
                assert!(probabilities.contains_key("fudi::ops"));
                assert!(probabilities.contains_key("fudi::fudi"));
                assert_prob(
                    probabilities["fudi::ops"],
                    0.87,
                    "agent.probabilities[fudi::ops]",
                );
                assert_prob(
                    probabilities["fudi::fudi"],
                    0.13,
                    "agent.probabilities[fudi::fudi]",
                );
            }
            other => panic!("agent must decode as a Choice, got {other:?}"),
        }

        // The Noul is a float, not a label.
        match &decoded.answers["specialized"] {
            Answer::Noul { value } => assert_prob(*value, 0.96, "specialized.noul"),
            other => panic!("specialized must decode as a Noul, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_an_unexpected_envelope_instead_of_panicking() {
        let body = serde_json::json!({ "something": "else" });
        assert!(decode_response(&body).is_err());
    }

    /// Every structural surprise must become an `AppError::Provider` the
    /// caller can fall back on — never a panic, never a half-filled
    /// `DecisionResponse`.
    #[test]
    fn decode_rejects_structural_surprises_instead_of_partial_answers() {
        let cases = [
            (
                "answers is not an object",
                serde_json::json!({ "answers": "yes" }),
            ),
            (
                "answer is not an object",
                serde_json::json!({ "answers": { "agent": 42 } }),
            ),
            (
                "unknown answer kind",
                serde_json::json!({ "answers": { "agent": { "type": "score", "score": 2.0 } } }),
            ),
            (
                "kind with neither value field",
                serde_json::json!({ "answers": { "agent": { "type": "choice", "confidence": 1.0 } } }),
            ),
            (
                "type contradicts the body",
                serde_json::json!({ "answers": { "agent": {
                    "type": "noul",
                    "choice": "fudi::ops",
                    "confidence": 1.0,
                    "probabilities": { "fudi::ops": 1.0 },
                } } }),
            ),
            (
                "missing confidence",
                serde_json::json!({ "answers": { "agent": {
                    "type": "choice",
                    "choice": "fudi::ops",
                    "probabilities": { "fudi::ops": 1.0 },
                } } }),
            ),
            (
                "missing probabilities",
                serde_json::json!({ "answers": { "agent": {
                    "type": "choice",
                    "choice": "fudi::ops",
                    "confidence": 1.0,
                } } }),
            ),
            (
                "probability is not a number",
                serde_json::json!({ "answers": { "agent": {
                    "type": "choice",
                    "choice": "fudi::ops",
                    "confidence": 1.0,
                    "probabilities": { "fudi::ops": "1.0" },
                } } }),
            ),
            (
                "noul is not a number",
                serde_json::json!({ "answers": { "agent": { "type": "noul", "noul": "yes" } } }),
            ),
        ];

        for (label, body) in cases {
            match decode_response(&body) {
                Err(AppError::Provider(message)) => {
                    assert!(!message.is_empty(), "{label}: provider error is empty")
                }
                other => panic!("{label}: expected AppError::Provider, got {other:?}"),
            }
        }
    }
}
