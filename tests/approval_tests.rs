//! Task 3: the approval gate is a standalone runtime primitive. Bridge wiring
//! (blocking a tool call on a decision) belongs to Task 4, not here.

use llama_r::api::chat_core::engine_events_to_chat_stream;
use llama_r::domain::models::ChatStreamEvent;
use llama_r::ports::engine::{AgentRunEvent, AgentRunResult};
use llama_r::services::approval::{
    args_hash, args_hash_hex, ApprovalDecision, ApprovalError, ApprovalGate,
};
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use uuid::Uuid;

#[test]
fn request_returns_id_and_receiver() {
    let gate = ApprovalGate::default();
    let (id, mut rx) = gate.request("db__drop_table", &json!({"table": "users"}));
    let (other_id, _other_rx) = gate.request("db__drop_table", &json!({"table": "posts"}));

    assert_ne!(id, other_id, "each pending approval gets a fresh id");
    // The pending has not been decided, so the oneshot is still open.
    assert!(rx.try_recv().is_err());
}

#[test]
fn decide_approve_resolves_the_receiver() {
    let gate = ApprovalGate::default();
    let args = json!({"table": "users"});
    let (id, mut rx) = gate.request("db__drop_table", &args);

    gate.decide(id, true, args_hash(&args))
        .expect("approving an existing pending must resolve it");
    assert!(matches!(rx.try_recv(), Ok(ApprovalDecision::Approved)));

    // Atomic decide: the entry was taken under the write lock, so a second
    // decide cannot resolve the same oneshot (and cannot panic on double send).
    assert!(matches!(
        gate.decide(id, true, args_hash(&args)),
        Err(ApprovalError::NotFound(_))
    ));
}

#[test]
fn decide_deny_resolves_with_a_reason() {
    let gate = ApprovalGate::default();
    let args = json!({"amount": 100});
    let (id, mut rx) = gate.request("pay__send", &args);

    gate.decide(id, false, args_hash(&args)).unwrap();
    match rx.try_recv() {
        Ok(ApprovalDecision::Denied { reason }) => assert!(!reason.is_empty()),
        other => panic!("expected Denied with a reason, got {other:?}"),
    }
}

#[test]
fn decide_with_wrong_hash_rejects() {
    let gate = ApprovalGate::default();
    let args = json!({"amount": 100, "to": "alice"});
    let (id, mut rx) = gate.request("pay__send", &args);

    let tampered = json!({"amount": 200, "to": "alice"});
    assert!(matches!(
        gate.decide(id, true, args_hash(&tampered)),
        Err(ApprovalError::ArgsChanged)
    ));
    // A mismatch does not resolve the pending: the human can retry with the
    // correct hash, still bound to the original arguments.
    assert!(rx.try_recv().is_err());

    gate.decide(id, true, args_hash(&args))
        .expect("the correct hash resolves the original pending");
    assert!(matches!(rx.try_recv(), Ok(ApprovalDecision::Approved)));
}

#[test]
fn expired_pending_is_pruned_and_rejected() {
    let gate = ApprovalGate::new(Duration::from_millis(20));
    let args = json!({"table": "users"});
    let (id, mut rx) = gate.request("db__drop_table", &args);

    std::thread::sleep(Duration::from_millis(60));
    assert!(matches!(
        gate.decide(id, true, args_hash(&args)),
        Err(ApprovalError::Expired(_))
    ));
    // Rejecting an expired pending drops its sender, closing the receiver.
    assert!(rx.try_recv().is_err());

    // The reaper also runs on `request`: a stale pending is gone by the time
    // the next request arrives, while a live one is untouched.
    let gate = ApprovalGate::new(Duration::from_millis(20));
    let (stale_id, _stale_rx) = gate.request("old__tool", &json!({}));
    std::thread::sleep(Duration::from_millis(60));
    let (fresh_id, mut fresh_rx) = gate.request("new__tool", &json!({}));
    assert!(matches!(
        gate.decide(stale_id, true, args_hash(&json!({}))),
        Err(ApprovalError::NotFound(_))
    ));
    gate.decide(fresh_id, true, args_hash(&json!({})))
        .expect("the fresh pending survives the reaper");
    assert!(matches!(
        fresh_rx.try_recv(),
        Ok(ApprovalDecision::Approved)
    ));
}

#[test]
fn args_hash_is_deterministic_for_reordered_keys() {
    let a = json!({"b": 2, "a": 1, "nested": {"y": true, "x": "s"}});
    let b = json!({"nested": {"x": "s", "y": true}, "a": 1, "b": 2});
    assert_eq!(args_hash(&a), args_hash(&b));
    assert_eq!(args_hash_hex(&a), args_hash_hex(&b));

    let hex = args_hash_hex(&a);
    assert_eq!(hex.len(), 64, "SHA-256 is 32 bytes as lowercase hex");
    assert!(hex
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_ne!(args_hash(&a), args_hash(&json!({"a": 1})));
}

#[test]
fn approval_errors_map_to_the_expected_http_statuses() {
    use axum::http::StatusCode;
    use llama_r::error::AppError;

    let id = Uuid::new_v4();
    let missing: AppError = ApprovalError::NotFound(id).into();
    assert_eq!(missing.status_code(), StatusCode::NOT_FOUND);
    assert_eq!(missing.code(), "approval_not_found");

    let expired: AppError = ApprovalError::Expired(id).into();
    assert_eq!(expired.status_code(), StatusCode::GONE);
    assert_eq!(expired.code(), "approval_expired");

    let changed: AppError = ApprovalError::ArgsChanged.into();
    assert_eq!(changed.status_code(), StatusCode::BAD_REQUEST);
    assert_eq!(changed.code(), "approval_args_changed");
}

#[tokio::test]
async fn approval_required_event_maps_to_sse_without_breaking_other_events() {
    let id = Uuid::new_v4();
    let args = json!({"table": "users"});
    let expected_hash = args_hash_hex(&args);
    let (tx, rx) = mpsc::channel(8);

    tx.send(AgentRunEvent::Token {
        text: "working".to_string(),
    })
    .await
    .unwrap();
    tx.send(AgentRunEvent::ApprovalRequired {
        id,
        tool_name: "db__drop_table".to_string(),
        args: args.clone(),
    })
    .await
    .unwrap();
    tx.send(AgentRunEvent::Completed {
        result: AgentRunResult {
            text: String::new(),
            tool_calls: 0,
            iterations: 0,
            model: "m".to_string(),
        },
    })
    .await
    .unwrap();
    drop(tx);

    let mut stream = engine_events_to_chat_stream("m".to_string(), rx);
    let mut events: Vec<ChatStreamEvent> = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.expect("stream must not error"));
    }
    assert_eq!(events.len(), 3);

    let token = &events[0];
    assert!(token.approval.is_none());
    assert_eq!(token.message.content, "working");
    assert!(!token.done);
    let token_json = serde_json::to_value(token).unwrap();
    assert!(
        !token_json.as_object().unwrap().contains_key("approval"),
        "existing SSE clients must not see a new key on ordinary events"
    );

    let approval = &events[1];
    let info = approval
        .approval
        .as_ref()
        .expect("ApprovalRequired carries approval info");
    assert_eq!(info.id, id.to_string());
    assert_eq!(info.tool_name, "db__drop_table");
    assert_eq!(info.args_hash, expected_hash);
    assert!(
        !approval.done,
        "the request stays open while awaiting a human"
    );
    let approval_json = serde_json::to_value(approval).unwrap();
    assert!(approval_json["approval"].is_object());

    let done = &events[2];
    assert!(done.approval.is_none());
    assert!(done.done);
}
