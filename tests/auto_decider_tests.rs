//! Task 1: `AutoDecider` port and its router-backed adapter.
//!
//! The only automatic-approval path is one affirmative, high-confidence
//! decision from the injected `DecisionRouter`. These tests pin every verdict
//! branch, the question shape, the per-call threshold, and — critically — that
//! a secret in the arguments never reaches the decision model while its
//! canonical hash does. No live decision model is involved: a fake router
//! captures the `DecisionRequest` so the outgoing state can be inspected.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use llama_r::adapters::auto::RouterAutoDecider;
use llama_r::error::AppError;
use llama_r::ports::auto::{AutoDecider, AutoVerdict};
use llama_r::ports::router::{Answer, DecisionRequest, DecisionResponse, DecisionRouter, Question};
use llama_r::services::redaction::{args_hash_hex, REDACTED_MARKER};
use serde_json::{json, Value};

/// What the fake router will answer, in the shapes the real port allows.
enum Reply {
    Choice {
        key: &'static str,
        confidence: f32,
    },
    Noul {
        value: f32,
    },
    /// A successful call with no answer for the question we asked.
    Missing,
}

/// A `DecisionRouter` that returns a fixed reply (or an error) and records the
/// exact request it received. Mirrors the fake in `tests/evals_tests.rs`, with
/// the request capture this task needs to assert redaction.
struct FakeRouter {
    reply: Reply,
    fail: bool,
    seen: Arc<Mutex<Option<DecisionRequest>>>,
}

#[async_trait]
impl DecisionRouter for FakeRouter {
    async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, AppError> {
        *self.seen.lock().unwrap() = Some(req);
        if self.fail {
            return Err(AppError::Provider("fake router unreachable".into()));
        }
        let answer = match &self.reply {
            Reply::Choice { key, confidence } => Answer::Choice {
                key: (*key).to_string(),
                confidence: *confidence,
                probabilities: BTreeMap::from([((*key).to_string(), *confidence)]),
            },
            Reply::Noul { value } => Answer::Noul { value: *value },
            Reply::Missing => {
                return Ok(DecisionResponse {
                    answers: BTreeMap::new(),
                })
            }
        };
        Ok(DecisionResponse {
            answers: BTreeMap::from([("auto".to_string(), answer)]),
        })
    }
}

/// Build a fake router plus the handle used to inspect the request it got.
fn fake_router(
    reply: Reply,
    fail: bool,
) -> (Arc<dyn DecisionRouter>, Arc<Mutex<Option<DecisionRequest>>>) {
    let seen = Arc::new(Mutex::new(None));
    let router = FakeRouter {
        reply,
        fail,
        seen: seen.clone(),
    };
    (Arc::new(router), seen)
}

/// Drive one decision with a fixed argument set and return the verdict plus the
/// captured request handle. `min_confidence` is the constructor default;
/// `threshold` is the per-call value the gate would pass from policy.
async fn decide(
    reply: Reply,
    fail: bool,
    min_confidence: f32,
    threshold: f32,
    args: &Value,
) -> (AutoVerdict, Arc<Mutex<Option<DecisionRequest>>>) {
    let (router, seen) = fake_router(reply, fail);
    let decider = RouterAutoDecider::new(router, min_confidence);
    let hash = args_hash_hex(args);
    let verdict = decider
        .should_allow("db.query", args, &hash, threshold)
        .await;
    (verdict, seen)
}

#[tokio::test]
async fn affirmative_high_confidence_allows() {
    let (verdict, _) = decide(
        Reply::Choice {
            key: "yes",
            confidence: 0.9,
        },
        false,
        0.6,
        0.6,
        &json!({"table": "users"}),
    )
    .await;
    assert_eq!(verdict, AutoVerdict::Allow);
}

#[tokio::test]
async fn confident_no_denies() {
    let (verdict, _) = decide(
        Reply::Choice {
            key: "no",
            confidence: 0.95,
        },
        false,
        0.6,
        0.6,
        &json!({"table": "users"}),
    )
    .await;
    assert_eq!(verdict, AutoVerdict::Deny);
}

#[tokio::test]
async fn affirmative_below_threshold_denies() {
    let (verdict, _) = decide(
        Reply::Choice {
            key: "yes",
            confidence: 0.55,
        },
        false,
        0.6,
        0.6,
        &json!({"table": "users"}),
    )
    .await;
    assert_eq!(verdict, AutoVerdict::Deny);
}

#[tokio::test]
async fn noul_and_unusable_answers_deny() {
    // A `Noul` is a probability, not a label — it is never an approval.
    let (noul, _) = decide(
        Reply::Noul { value: 0.99 },
        false,
        0.6,
        0.6,
        &json!({"table": "users"}),
    )
    .await;
    assert_eq!(noul, AutoVerdict::Deny);

    // An unexpected key is not the one affirmative label we accept.
    let (unexpected, _) = decide(
        Reply::Choice {
            key: "maybe",
            confidence: 0.99,
        },
        false,
        0.6,
        0.6,
        &json!({"table": "users"}),
    )
    .await;
    assert_eq!(unexpected, AutoVerdict::Deny);

    // A successful call with no answer for our question is not an approval.
    let (missing, _) = decide(Reply::Missing, false, 0.6, 0.6, &json!({"table": "users"})).await;
    assert_eq!(missing, AutoVerdict::Deny);
}

#[tokio::test]
async fn router_error_is_unavailable() {
    let (verdict, _) = decide(
        Reply::Choice {
            key: "yes",
            confidence: 0.99,
        },
        true,
        0.6,
        0.6,
        &json!({"table": "users"}),
    )
    .await;
    assert_eq!(verdict, AutoVerdict::Unavailable);
}

#[tokio::test]
async fn secret_is_redacted_but_hash_is_sent() {
    let args = json!({
        "table": "users",
        "api_key": "SUPER-SECRET-VALUE",
        "nested": {"token": "ANOTHER-SECRET"}
    });
    let hash = args_hash_hex(&args);

    let (verdict, seen) = decide(
        Reply::Choice {
            key: "yes",
            confidence: 0.9,
        },
        false,
        0.6,
        0.6,
        &args,
    )
    .await;
    assert_eq!(verdict, AutoVerdict::Allow);

    let req = seen.lock().unwrap().clone().expect("router was called");
    assert!(
        req.state.contains("db.query"),
        "tool name must reach the model"
    );
    assert!(
        req.state.contains(&hash),
        "canonical hash must reach the model"
    );
    assert!(
        req.state.contains(REDACTED_MARKER),
        "redacted args must be visible in the state"
    );
    assert!(!req.state.contains("SUPER-SECRET-VALUE"), "secret leaked");
    assert!(
        !req.state.contains("ANOTHER-SECRET"),
        "nested secret leaked"
    );
}

#[tokio::test]
async fn per_call_threshold_overrides_the_constructor_default() {
    let args = json!({"table": "users"});
    let reply = || Reply::Choice {
        key: "yes",
        confidence: 0.75,
    };

    // Same decider, same confidence: the per-call threshold decides.
    let (low, _) = decide(reply(), false, 0.6, 0.6, &args).await;
    assert_eq!(low, AutoVerdict::Allow);

    let (high, _) = decide(reply(), false, 0.6, 0.8, &args).await;
    assert_eq!(high, AutoVerdict::Deny);
}

#[tokio::test]
async fn constructor_retains_its_configured_default() {
    let (router, _) = fake_router(
        Reply::Choice {
            key: "yes",
            confidence: 0.9,
        },
        false,
    );
    let decider = RouterAutoDecider::new(router, 0.72);
    assert_eq!(decider.min_confidence(), 0.72);
}

#[tokio::test]
async fn question_is_a_yes_no_choice_over_the_redacted_state() {
    let (_, seen) = decide(
        Reply::Choice {
            key: "yes",
            confidence: 0.9,
        },
        false,
        0.6,
        0.6,
        &json!({"table": "users", "password": "hunter2"}),
    )
    .await;
    let req = seen.lock().unwrap().clone().expect("router was called");
    assert_eq!(req.questions.len(), 1);
    let question = req.questions.get("auto").expect("auto question asked");
    match question {
        Question::Choice {
            instructions,
            criteria,
        } => {
            assert!(
                !instructions.trim().is_empty(),
                "safety instructions required"
            );
            assert!(criteria.contains_key("yes"));
            assert!(criteria.contains_key("no"));
        }
        other => panic!("expected a Choice question, got {other:?}"),
    }
    assert!(!req.state.contains("hunter2"), "password leaked into state");
}
