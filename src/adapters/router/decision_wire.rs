//! Shared wire codec for the hosted decision adapters.
//!
//! OpenRouter (`POST /api/alpha/decisions`) and OpenCode
//! (`POST /api/experimental/generate/decisions`) both expose the Liquid AI d1
//! decisions shape: typed questions in, `answers.<name>` out. This module owns
//! that one shape so the two adapters differ only in origin and auth — a wire
//! change costs this file, not the two callers.
//!
//! Decoding is deliberately strict. A `refusal`, an unknown/extra answer, or a
//! question that came back unanswered is an [`AppError::Provider`], which the
//! routing ladder reads as `RouterUnavailable` and degrades to the project's
//! general agent. Nothing here ever invents a probability: a partial or
//! guessed answer would route worse than no answer at all.

use crate::error::AppError;
use crate::ports::router::{Answer, DecisionRequest, DecisionResponse, Question};
use std::collections::{BTreeMap, BTreeSet};

/// One question in the upstream's request spelling: `type`, `instructions`,
/// and `criteria` for a choice; `type` and `instructions` for a noul. A noul
/// must not gain an empty `criteria` object — that is a different question.
pub(super) fn encode_question(question: &Question) -> serde_json::Value {
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

/// The whole request body: `model`, `state`, and the named `questions` object.
pub(super) fn encode_body(model: &str, req: &DecisionRequest) -> serde_json::Value {
    let questions: serde_json::Map<String, serde_json::Value> = req
        .questions
        .iter()
        .map(|(name, question)| (name.clone(), encode_question(question)))
        .collect();
    serde_json::json!({ "model": model, "state": req.state, "questions": questions })
}

/// Read a decisions response into a [`DecisionResponse`], or explain why it is
/// not one.
///
/// `expected` is the set of question names from the request. Every one of them
/// must be answered, and no other name may appear: an extra answer is as much
/// a surprise as a missing one. OpenCode historically wraps experimental
/// bodies in `{"data": {...}}` (its chat endpoint does), so `answers` is
/// accepted at the top level or under `data`, never both.
pub(super) fn decode_response(
    body: &serde_json::Value,
    expected: &BTreeSet<String>,
) -> Result<DecisionResponse, AppError> {
    let answers = body
        .get("answers")
        .and_then(|value| value.as_object())
        .or_else(|| {
            body.get("data")
                .and_then(|data| data.get("answers"))
                .and_then(|value| value.as_object())
        })
        .ok_or_else(|| AppError::Provider("decisions response has no 'answers' object".into()))?;

    let mut decoded = BTreeMap::new();
    for (name, answer) in answers {
        if !expected.contains(name) {
            return Err(AppError::Provider(format!(
                "decisions response carries unexpected answer '{name}'"
            )));
        }
        decoded.insert(name.clone(), decode_answer(name, answer)?);
    }

    for name in expected {
        if !decoded.contains_key(name) {
            return Err(AppError::Provider(format!(
                "decisions response has no answer for question '{name}'"
            )));
        }
    }

    Ok(DecisionResponse { answers: decoded })
}

/// One named answer, or the reason it is not one.
///
/// `type` is required — it is the only way to tell a `refusal` or a `score`
/// (both unsupported here) from a usable answer. The value fields follow the
/// d1 envelope: `choice`/`confidence`/`probabilities` and `noul`. The port's
/// serialized field names (`key`, `value`) are accepted as aliases so a
/// compatible upstream echoing the debug shape still decodes.
fn decode_answer(name: &str, answer: &serde_json::Value) -> Result<Answer, AppError> {
    let object = answer.as_object().ok_or_else(|| {
        AppError::Provider(format!(
            "decisions answer '{name}' is not an object: {answer}"
        ))
    })?;

    let kind = object
        .get("type")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            AppError::Provider(format!("decisions answer '{name}' has no 'type' field"))
        })?;

    match kind {
        "refusal" => Err(AppError::Provider(format!(
            "decisions answer '{name}' was refused"
        ))),
        "score" => Err(AppError::Provider(format!(
            "decisions answer '{name}' is an unsupported 'score' answer"
        ))),
        "choice" => {
            let key = object
                .get("choice")
                .or_else(|| object.get("key"))
                .and_then(|value| value.as_str())
                .ok_or_else(|| {
                    AppError::Provider(format!("decisions answer '{name}' has no string 'choice'"))
                })?
                .to_string();
            let confidence = answer_number(object, name, "confidence")? as f32;
            let probabilities = object
                .get("probabilities")
                .and_then(|value| value.as_object())
                .ok_or_else(|| {
                    AppError::Provider(format!(
                        "decisions answer '{name}' has no 'probabilities' object"
                    ))
                })?
                .iter()
                .map(|(label, probability)| {
                    probability
                        .as_f64()
                        .map(|value| (label.clone(), value as f32))
                        .ok_or_else(|| {
                            AppError::Provider(format!(
                                "decisions answer '{name}' has a non-numeric probability for '{label}'"
                            ))
                        })
                })
                .collect::<Result<BTreeMap<String, f32>, AppError>>()?;
            Ok(Answer::Choice {
                key,
                confidence,
                probabilities,
            })
        }
        "noul" => {
            let value = object
                .get("noul")
                .or_else(|| object.get("value"))
                .and_then(|value| value.as_f64())
                .ok_or_else(|| {
                    AppError::Provider(format!("decisions answer '{name}' has no numeric 'noul'"))
                })?;
            Ok(Answer::Noul {
                value: value as f32,
            })
        }
        other => Err(AppError::Provider(format!(
            "decisions answer '{name}' has unknown type '{other}'"
        ))),
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
                "decisions answer '{name}' has no numeric '{field}'"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected() -> BTreeSet<String> {
        BTreeSet::from(["agent".to_string(), "specialized".to_string()])
    }

    fn reply() -> serde_json::Value {
        serde_json::json!({
            "answers": {
                "agent": {
                    "type": "choice", "choice": "ops", "confidence": 0.83,
                    "probabilities": { "ops": 0.92, "general": 0.08 }
                },
                "specialized": { "type": "noul", "noul": 0.44 }
            }
        })
    }

    #[test]
    fn encode_choice_and_noul_keep_their_distinct_shapes() {
        let choice = encode_question(&Question::Choice {
            instructions: "Which agent?".into(),
            criteria: BTreeMap::from([("ops".to_string(), "Pedidos".to_string())]),
        });
        assert_eq!(choice["type"], "choice");
        assert_eq!(choice["criteria"]["ops"], "Pedidos");

        let noul = encode_question(&Question::Noul {
            instructions: "Specialised?".into(),
        });
        assert_eq!(noul["type"], "noul");
        assert!(noul.get("criteria").is_none());
    }

    #[test]
    fn decode_reads_choice_and_noul() {
        let decoded = decode_response(&reply(), &expected()).unwrap();
        assert_eq!(decoded.answers.len(), 2);
        match &decoded.answers["agent"] {
            Answer::Choice { key, .. } => assert_eq!(key, "ops"),
            other => panic!("agent must be a Choice, got {other:?}"),
        }
        match &decoded.answers["specialized"] {
            Answer::Noul { value } => assert!((*value - 0.44).abs() < 1e-6, "{value}"),
            other => panic!("specialized must be a Noul, got {other:?}"),
        }
    }

    #[test]
    fn decode_accepts_a_data_wrapped_envelope() {
        let wrapped = serde_json::json!({ "data": reply() });
        assert!(decode_response(&wrapped, &expected()).is_ok());
    }

    #[test]
    fn decode_refuses_every_structural_surprise() {
        let cases = [
            serde_json::json!({ "something": "else" }),
            serde_json::json!({ "answers": "yes" }),
            serde_json::json!({ "answers": {
                "agent": { "type": "refusal", "refusal": "no" },
                "specialized": { "type": "noul", "noul": 0.4 }
            } }),
            serde_json::json!({ "answers": {
                "agent": { "type": "choice", "choice": "ops", "confidence": 0.8,
                           "probabilities": { "ops": 1.0 } }
            } }),
            serde_json::json!({ "answers": {
                "agent": { "type": "score", "score": 2.0 },
                "specialized": { "type": "noul", "noul": 0.4 }
            } }),
            serde_json::json!({ "answers": {
                "agent": { "type": "weird" },
                "specialized": { "type": "noul", "noul": 0.4 }
            } }),
            serde_json::json!({ "answers": {
                "agent": { "type": "choice", "choice": "ops", "confidence": 0.8,
                           "probabilities": { "ops": 1.0 } },
                "specialized": { "type": "noul", "noul": 0.4 },
                "extra": { "type": "noul", "noul": 0.4 }
            } }),
        ];
        for body in cases {
            match decode_response(&body, &expected()) {
                Err(AppError::Provider(message)) => assert!(!message.is_empty()),
                other => panic!("expected AppError::Provider, got {other:?}"),
            }
        }
    }
}
