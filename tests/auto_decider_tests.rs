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
use llama_r::api::observability::AppObservability;
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

// ---------------------------------------------------------------------------
// Task 2: the gate seam. All of these drive the real `approval::authorize`
// function; human-path cases resolve the pending from a background task so the
// call never blocks on the gate's 120 s default timeout.
// ---------------------------------------------------------------------------

use llama_r::adapters::rig_engine::approval::authorize;
use llama_r::domain::policy::{ApprovalPolicy, ApprovalRule, PermissionPolicy};
use llama_r::ports::engine::AgentRunEvent;
use llama_r::services::approval::{args_hash, ApprovalGate};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

fn auto_policy(threshold: Option<f32>) -> PermissionPolicy {
    PermissionPolicy {
        auto_threshold: threshold,
        approvals: vec![ApprovalRule {
            tool_pattern: "*".into(),
            policy: ApprovalPolicy::Auto,
        }],
        ..PermissionPolicy::default()
    }
}

fn policy_with(policy: ApprovalPolicy) -> PermissionPolicy {
    PermissionPolicy {
        connections_allow: Vec::new(),
        auto_threshold: None,
        approvals: vec![ApprovalRule {
            tool_pattern: "*".into(),
            policy,
        }],
    }
}

/// One observed `should_allow` call: tool, redacted args, hash, threshold.
type DeciderCall = (String, Value, String, f32);
type CallLog = Arc<Mutex<Vec<DeciderCall>>>;

/// A decider that records every call (tool, redacted args, hash, threshold) and
/// returns a fixed verdict. Used to prove *which* threshold the gate passed and
/// that a `Never`/`Always` tool never consults the decider at all.
struct RecordingDecider {
    verdict: AutoVerdict,
    min_confidence: f32,
    calls: CallLog,
}

#[async_trait]
impl AutoDecider for RecordingDecider {
    fn min_confidence(&self) -> f32 {
        self.min_confidence
    }

    async fn should_allow(
        &self,
        tool_name: &str,
        redacted_args: &Value,
        args_hash_hex: &str,
        threshold: f32,
    ) -> AutoVerdict {
        self.calls.lock().unwrap().push((
            tool_name.to_string(),
            redacted_args.clone(),
            args_hash_hex.to_string(),
            threshold,
        ));
        self.verdict
    }
}

fn recorder(verdict: AutoVerdict, min_confidence: f32) -> (RecordingDecider, CallLog) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    (
        RecordingDecider {
            verdict,
            min_confidence,
            calls: calls.clone(),
        },
        calls,
    )
}

/// Drive one gate call. A background task resolves any pending it sees through
/// the event channel, so a human-path call returns promptly and we can assert
/// that a pending really was registered.
async fn authorize_with_human(
    gate: &Arc<ApprovalGate>,
    permission: &PermissionPolicy,
    decider: Option<&dyn AutoDecider>,
    tool: &str,
    args: &Value,
) -> (Result<(), String>, Arc<AtomicBool>) {
    authorize_with_human_recording(gate, permission, decider, tool, args, None).await
}

/// Same driver, but threads an observability recorder through to `authorize`
/// so the task-4 counter tests can inspect what the gate recorded at the
/// decision point.
async fn authorize_with_human_recording(
    gate: &Arc<ApprovalGate>,
    permission: &PermissionPolicy,
    decider: Option<&dyn AutoDecider>,
    tool: &str,
    args: &Value,
    observability: Option<&AppObservability>,
) -> (Result<(), String>, Arc<AtomicBool>) {
    let (tx, mut rx) = mpsc::channel(4);
    let seen = Arc::new(AtomicBool::new(false));
    let gate2 = gate.clone();
    let seen2 = seen.clone();
    let expected = args.clone();
    let resolver = tokio::spawn(async move {
        if let Some(event) = rx.recv().await {
            match event {
                AgentRunEvent::ApprovalRequired { id, .. } => {
                    seen2.store(true, Ordering::SeqCst);
                    gate2.decide(id, true, args_hash(&expected)).unwrap();
                }
                _ => panic!("expected an ApprovalRequired event"),
            }
        }
    });
    let result = authorize(
        gate,
        Some(&tx),
        permission,
        tool,
        args,
        None,
        decider,
        observability,
    )
    .await;
    drop(tx);
    let _ = resolver.await;
    (result, seen)
}

#[tokio::test]
async fn auto_yes_high_confidence_executes_without_a_pending() {
    let (router, _seen) = fake_router(
        Reply::Choice {
            key: "yes",
            confidence: 0.95,
        },
        false,
    );
    let decider = RouterAutoDecider::new(router, 0.6);
    let gate = Arc::new(ApprovalGate::default());
    let (result, human_seen) = authorize_with_human(
        &gate,
        &auto_policy(None),
        Some(&decider),
        "db__drop",
        &json!({"table": "users"}),
    )
    .await;
    assert!(result.is_ok(), "an affirmative decision lets the tool run");
    assert!(
        !human_seen.load(Ordering::SeqCst),
        "no human approval expected"
    );
    assert!(
        gate.list_pending().is_empty(),
        "an auto-approved call must register no pending"
    );
}

#[tokio::test]
async fn auto_no_registers_a_pending() {
    let (router, _seen) = fake_router(
        Reply::Choice {
            key: "no",
            confidence: 0.95,
        },
        false,
    );
    let decider = RouterAutoDecider::new(router, 0.6);
    let gate = Arc::new(ApprovalGate::default());
    let (result, human_seen) = authorize_with_human(
        &gate,
        &auto_policy(None),
        Some(&decider),
        "db__drop",
        &json!({"table": "users"}),
    )
    .await;
    assert!(result.is_ok(), "the human approves, so the tool runs");
    assert!(
        human_seen.load(Ordering::SeqCst),
        "a refusal must fall back to the human gate"
    );
}

#[tokio::test]
async fn auto_low_confidence_registers_a_pending() {
    // 0.4 < 0.6 threshold: an affirmative label below the bar is a refusal.
    let (router, _seen) = fake_router(
        Reply::Choice {
            key: "yes",
            confidence: 0.4,
        },
        false,
    );
    let decider = RouterAutoDecider::new(router, 0.6);
    let gate = Arc::new(ApprovalGate::default());
    let (result, human_seen) = authorize_with_human(
        &gate,
        &auto_policy(None),
        Some(&decider),
        "db__drop",
        &json!({"table": "users"}),
    )
    .await;
    assert!(result.is_ok());
    assert!(
        human_seen.load(Ordering::SeqCst),
        "low confidence must fall back to the human gate"
    );
}

#[tokio::test]
async fn auto_router_error_registers_a_pending() {
    let (router, _seen) = fake_router(
        Reply::Choice {
            key: "yes",
            confidence: 0.99,
        },
        true, // router unreachable
    );
    let decider = RouterAutoDecider::new(router, 0.6);
    let gate = Arc::new(ApprovalGate::default());
    let (result, human_seen) = authorize_with_human(
        &gate,
        &auto_policy(None),
        Some(&decider),
        "db__drop",
        &json!({"table": "users"}),
    )
    .await;
    assert!(result.is_ok());
    assert!(
        human_seen.load(Ordering::SeqCst),
        "an unavailable router must fall back to the human gate"
    );
}

#[tokio::test]
async fn auto_without_a_decider_registers_a_pending() {
    let gate = Arc::new(ApprovalGate::default());
    let (result, human_seen) = authorize_with_human(
        &gate,
        &auto_policy(None),
        None,
        "db__drop",
        &json!({"table": "users"}),
    )
    .await;
    assert!(result.is_ok());
    assert!(
        human_seen.load(Ordering::SeqCst),
        "Auto with no decider is exactly Always"
    );
}

#[tokio::test]
async fn always_registers_a_pending_and_never_consults_the_decider() {
    let (decider, calls) = recorder(AutoVerdict::Allow, 0.6);
    let gate = Arc::new(ApprovalGate::default());
    let (result, human_seen) = authorize_with_human(
        &gate,
        &policy_with(ApprovalPolicy::Always),
        Some(&decider),
        "db__drop",
        &json!({"table": "users"}),
    )
    .await;
    assert!(result.is_ok());
    assert!(human_seen.load(Ordering::SeqCst));
    assert!(
        calls.lock().unwrap().is_empty(),
        "Always must not consult the auto decider"
    );
}

#[tokio::test]
async fn once_registers_a_pending_and_never_consults_the_decider() {
    // `Once` is unchanged by Fase 6: the gate only special-cases `Auto`.
    let (decider, calls) = recorder(AutoVerdict::Allow, 0.6);
    let gate = Arc::new(ApprovalGate::default());
    let (result, human_seen) = authorize_with_human(
        &gate,
        &policy_with(ApprovalPolicy::Once),
        Some(&decider),
        "db__drop",
        &json!({"table": "users"}),
    )
    .await;
    assert!(result.is_ok());
    assert!(human_seen.load(Ordering::SeqCst));
    assert!(
        calls.lock().unwrap().is_empty(),
        "Once must not consult the auto decider"
    );
}

#[tokio::test]
async fn never_tool_never_consults_the_decider() {
    let (decider, calls) = recorder(AutoVerdict::Allow, 0.6);
    let gate = Arc::new(ApprovalGate::default());
    let (result, human_seen) = authorize_with_human(
        &gate,
        &policy_with(ApprovalPolicy::Never),
        Some(&decider),
        "db__read",
        &json!({"table": "users"}),
    )
    .await;
    assert!(result.is_ok());
    assert!(
        !human_seen.load(Ordering::SeqCst),
        "Never needs no approval"
    );
    assert!(
        gate.list_pending().is_empty(),
        "a Never tool must not register a pending"
    );
    assert!(
        calls.lock().unwrap().is_empty(),
        "a Never tool must not consult the decider"
    );
}

#[tokio::test]
async fn policy_auto_threshold_override_reaches_the_decider() {
    let (decider, calls) = recorder(AutoVerdict::Allow, 0.42);
    let gate = ApprovalGate::default();
    authorize(
        &gate,
        None,
        &auto_policy(Some(0.85)),
        "db__drop",
        &json!({"table": "users"}),
        None,
        Some(&decider),
        None,
    )
    .await
    .expect("an Allow verdict executes the tool");
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].3, 0.85, "the policy override is authoritative");
    assert!(
        gate.list_pending().is_empty(),
        "an auto-approved call registers no pending"
    );
}

/// A shared buffer usable as a `tracing` writer and read back as text, so the
/// fail-closed warning can be asserted rather than assumed.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn out_of_range_auto_threshold_falls_back_fail_closed() {
    use tracing::instrument::WithSubscriber;

    // 0.0 would make every "yes" pass; 1.5 is above the (0.0, 1.0] range that
    // RouterConfig::min_confidence also enforces. Both must be ignored.
    for bad in [0.0_f32, 1.5] {
        let (decider, calls) = recorder(AutoVerdict::Allow, 0.42);
        let args = json!({"table": "users", "api_key": "SUPER-SECRET-VALUE"});
        let gate = ApprovalGate::default();
        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .without_time()
            .with_target(false)
            .finish();

        authorize(
            &gate,
            None,
            &auto_policy(Some(bad)),
            "db__drop",
            &args,
            None,
            Some(&decider),
            None,
        )
        .with_subscriber(subscriber)
        .await
        .expect("an Allow verdict executes the tool");

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].3, 0.42,
            "out-of-range override {bad} must fall back to the configured default"
        );
        drop(calls);

        let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            log.contains("auto_threshold"),
            "warning must name the offending field: {log:?}"
        );
        assert!(
            log.contains(&format!("{bad:?}")),
            "warning must name the offending value: {log:?}"
        );
        assert!(
            !log.contains("SUPER-SECRET-VALUE"),
            "warning leaked a secret: {log:?}"
        );
    }
}

#[tokio::test]
async fn no_override_falls_back_to_the_decider_min_confidence() {
    let (decider, calls) = recorder(AutoVerdict::Allow, 0.42);
    let gate = ApprovalGate::default();
    authorize(
        &gate,
        None,
        &auto_policy(None),
        "db__drop",
        &json!({"table": "users"}),
        None,
        Some(&decider),
        None,
    )
    .await
    .expect("an Allow verdict executes the tool");
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].3, 0.42,
        "the configured decider default is the fallback threshold"
    );
}

#[tokio::test]
async fn gate_redacts_secrets_before_calling_the_decider() {
    let args = json!({"table": "users", "api_key": "SUPER-SECRET-VALUE"});
    let (decider, calls) = recorder(AutoVerdict::Allow, 0.6);
    let gate = ApprovalGate::default();
    authorize(
        &gate,
        None,
        &auto_policy(None),
        "db__drop",
        &args,
        None,
        Some(&decider),
        None,
    )
    .await
    .expect("an Allow verdict executes the tool");
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "db__drop");
    assert_eq!(calls[0].2, args_hash_hex(&args), "canonical hash is passed");
    assert!(!calls[0].1.to_string().contains("SUPER-SECRET-VALUE"));
    assert!(calls[0].1.to_string().contains(REDACTED_MARKER));
}

// ---------------------------------------------------------------------------
// Task 4: observability counters recorded at the gate decision point. The
// recorder is threaded into the gate itself (not wrapped around the decider),
// so the no-decider case is counted too and every verdict lands exactly once.
// ---------------------------------------------------------------------------

fn auto_counts(obs: &AppObservability) -> (u64, u64, u64) {
    let snap = obs.snapshot();
    (snap.auto_approved, snap.auto_denied, snap.auto_unavailable)
}

#[tokio::test]
async fn gate_records_auto_approved_for_an_allow_verdict() {
    let (decider, calls) = recorder(AutoVerdict::Allow, 0.6);
    let gate = Arc::new(ApprovalGate::default());
    let obs = AppObservability::new();
    let (result, human_seen) = authorize_with_human_recording(
        &gate,
        &auto_policy(None),
        Some(&decider),
        "db__drop",
        &json!({"table": "users"}),
        Some(&obs),
    )
    .await;
    assert!(result.is_ok(), "an Allow verdict lets the tool run");
    assert!(!human_seen.load(Ordering::SeqCst), "no human was involved");
    assert_eq!(calls.lock().unwrap().len(), 1, "exactly one decision");
    assert_eq!(auto_counts(&obs), (1, 0, 0));
}

#[tokio::test]
async fn gate_records_auto_denied_for_a_deny_verdict() {
    let (decider, calls) = recorder(AutoVerdict::Deny, 0.6);
    let gate = Arc::new(ApprovalGate::default());
    let obs = AppObservability::new();
    let (result, human_seen) = authorize_with_human_recording(
        &gate,
        &auto_policy(None),
        Some(&decider),
        "db__drop",
        &json!({"table": "users"}),
        Some(&obs),
    )
    .await;
    assert!(result.is_ok(), "the human approves after the refusal");
    assert!(human_seen.load(Ordering::SeqCst));
    assert_eq!(calls.lock().unwrap().len(), 1, "exactly one decision");
    assert_eq!(auto_counts(&obs), (0, 1, 0));
}

#[tokio::test]
async fn gate_records_auto_unavailable_for_an_unavailable_verdict() {
    let (decider, calls) = recorder(AutoVerdict::Unavailable, 0.6);
    let gate = Arc::new(ApprovalGate::default());
    let obs = AppObservability::new();
    let (result, human_seen) = authorize_with_human_recording(
        &gate,
        &auto_policy(None),
        Some(&decider),
        "db__drop",
        &json!({"table": "users"}),
        Some(&obs),
    )
    .await;
    assert!(result.is_ok());
    assert!(human_seen.load(Ordering::SeqCst));
    assert_eq!(calls.lock().unwrap().len(), 1, "exactly one decision");
    assert_eq!(auto_counts(&obs), (0, 0, 1));
}

#[tokio::test]
async fn gate_records_auto_unavailable_when_no_decider_is_installed() {
    let gate = Arc::new(ApprovalGate::default());
    let obs = AppObservability::new();
    let (result, human_seen) = authorize_with_human_recording(
        &gate,
        &auto_policy(None),
        None,
        "db__drop",
        &json!({"table": "users"}),
        Some(&obs),
    )
    .await;
    assert!(result.is_ok());
    assert!(
        human_seen.load(Ordering::SeqCst),
        "Auto with no decider = Always"
    );
    assert_eq!(auto_counts(&obs), (0, 0, 1));
}

#[tokio::test]
async fn never_always_and_once_leave_auto_counters_at_zero() {
    // None of these policies is an automatic decision, even when a decider is
    // installed: the pure engine resolves them before the gate's `Auto` branch.
    for policy in [
        ApprovalPolicy::Never,
        ApprovalPolicy::Always,
        ApprovalPolicy::Once,
    ] {
        let (decider, calls) = recorder(AutoVerdict::Allow, 0.6);
        let gate = Arc::new(ApprovalGate::default());
        let obs = AppObservability::new();
        let (result, _) = authorize_with_human_recording(
            &gate,
            &policy_with(policy),
            Some(&decider),
            "db__read",
            &json!({"table": "users"}),
            Some(&obs),
        )
        .await;
        assert!(result.is_ok());
        assert!(
            calls.lock().unwrap().is_empty(),
            "{policy:?} must not consult the decider"
        );
        assert_eq!(
            auto_counts(&obs),
            (0, 0, 0),
            "{policy:?} must not touch the auto counters"
        );
    }
}
