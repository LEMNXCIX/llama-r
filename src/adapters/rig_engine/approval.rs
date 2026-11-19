//! The approval seam shared by the MCP and local `ToolDyn` bridges.
//!
//! A bridge is deep inside rig-core's tool loop (`agent.prompt()` /
//! `agent.stream_prompt()`), so it cannot reach the caller directly. It carries
//! the run's [`ApprovalGate`] and an optional engine event sender: `Some` on the
//! streaming path, where the `ApprovalRequired` event reaches the SSE caller,
//! and `None` on the non-streaming path. Both paths register a pending in the
//! gate and block on its oneshot; the channel only decides *how the human
//! discovers* the pending. With a channel the SSE event carries the id; without
//! one the client polls `GET /api/approvals` and posts a decision to
//! `POST /api/approvals/:id`. Neither path executes the tool before a decision.
//!
//! Two invariants are enforced here, not in either bridge, so MCP and local
//! tools cannot drift apart:
//! - the budget is charged by the *caller* only after this function returns
//!   `Ok`, i.e. after approval — a denial, expiry or hash mismatch never spends
//!   an allowance;
//! - the decision is bound to the exact arguments shown in the event by the
//!   gate's SHA-256 hash; a changed hash is rejected by [`ApprovalGate::decide`].
//!
//! A dropped request leaves its pending entry in the gate until the gate's own
//! reaper collects it (Fase 7 seeds a background reaper); the bridge's own
//! timeout keeps that from hanging the tool call.

use crate::domain::policy::{
    ApprovalDecision as PolicyDecision, ApprovalPolicy, PermissionEngine, PermissionPolicy,
};
use crate::ports::auto::{AutoDecider, AutoVerdict};
use crate::ports::engine::AgentRunEvent;
use crate::services::approval::{ApprovalDecision as GateDecision, ApprovalGate};
use crate::services::redaction::{args_hash_hex, redact_secrets};
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Authorize one tool call. `Ok(())` means "execute" (the caller then charges
/// the shared budget and dispatches); `Err` is a named refusal carrying the tool
/// name and the reason.
///
/// The pure permission engine returns `Approve` for `Once`, `Always` and `Auto`
/// alike. This gate is the only place `Auto` can skip the human: when the matched
/// rule is `Auto` and `auto_decider` is present, an affirmative, high-confidence
/// verdict returns `Ok(())` **without registering a pending**. Every other
/// outcome (a refusal, a low-confidence answer, an unavailable router, no
/// decider, or any `Once`/`Always` rule) takes the unchanged human path below.
/// The budget is never charged here — the caller does that only after `Ok`.
pub async fn authorize(
    gate: &ApprovalGate,
    event_tx: Option<&mpsc::Sender<AgentRunEvent>>,
    permission: &PermissionPolicy,
    tool_name: &str,
    args: &Value,
    execution_id: Option<Uuid>,
    auto_decider: Option<&dyn AutoDecider>,
) -> Result<(), String> {
    match PermissionEngine::authorize(permission, tool_name) {
        PolicyDecision::Allow => Ok(()),
        PolicyDecision::Approve => {
            // `Auto` is the only policy that may skip the human, and only with a
            // positive answer from the injected decision model. The per-call
            // threshold is authoritative: an explicit policy override wins, else
            // the decider's configured default. Args are redacted before they
            // leave this process even though the adapter redacts again.
            if permission.approval_for(tool_name) == ApprovalPolicy::Auto {
                if let Some(decider) = auto_decider {
                    let fallback = decider.min_confidence();
                    // Fail closed: an out-of-range override (e.g. `0.0`, which
                    // would make every "yes" auto-approve) is ignored so it can
                    // never lower the bar below the configured default. Same
                    // `(0.0, 1.0]` range RouterConfig::min_confidence validates.
                    // The warning names only the number, never any argument.
                    let threshold = match permission.auto_threshold {
                        Some(value) if value.is_finite() && value > 0.0 && value <= 1.0 => value,
                        Some(value) => {
                            tracing::warn!(
                                ?value,
                                tool = %tool_name,
                                "ignoring out-of-range auto_threshold (expected (0.0, 1.0]); using the configured default"
                            );
                            fallback
                        }
                        None => fallback,
                    };
                    let verdict = decider
                        .should_allow(
                            tool_name,
                            &redact_secrets(args),
                            &args_hash_hex(args),
                            threshold,
                        )
                        .await;
                    if verdict == AutoVerdict::Allow {
                        return Ok(());
                    }
                    // Deny / Unavailable: fall through to the human gate. Never
                    // execute a tool the model refused or could not judge.
                }
            }

            // Register first, then (if there is a channel) announce. Registering
            // before the announcement means a non-streaming client can discover
            // the pending via GET /api/approvals even though no SSE event exists
            // for it. The gate stores the raw args for that list endpoint; the
            // SSE event carries only non-secret id/name/hash.
            let (id, rx) = gate.request_for(execution_id, tool_name, args);
            if let Some(tx) = event_tx {
                tx.send(AgentRunEvent::ApprovalRequired {
                    id,
                    tool_name: tool_name.to_string(),
                    args: args.clone(),
                })
                .await
                .map_err(|_| {
                    format!(
                        "tool '{tool_name}' was not executed: the approval event channel is closed"
                    )
                })?;
            }

            match tokio::time::timeout(gate.timeout(), rx).await {
                Ok(Ok(GateDecision::Approved)) => Ok(()),
                Ok(Ok(GateDecision::Denied { reason })) => Err(format!(
                    "tool '{tool_name}' was not executed: approval denied ({reason})"
                )),
                // The pending was reaped or its sender dropped (client gone):
                // a named refusal, never a silent empty success.
                Ok(Err(_)) => Err(format!(
                    "tool '{tool_name}' was not executed: the approval request was dropped or expired"
                )),
                Err(_) => Err(format!(
                    "tool '{tool_name}' was not executed: approval timed out after {:?}",
                    gate.timeout()
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::policy::{ApprovalPolicy, ApprovalRule};
    use crate::services::approval::args_hash;
    use std::sync::Arc;
    use std::time::Duration;

    fn always() -> PermissionPolicy {
        PermissionPolicy {
            connections_allow: Vec::new(),
            approvals: vec![ApprovalRule {
                tool_pattern: "*".into(),
                policy: ApprovalPolicy::Always,
            }],
            auto_threshold: None,
        }
    }

    #[tokio::test]
    async fn no_event_channel_registers_a_discoverable_pending() {
        // The non-streaming path has no SSE event carrying the id, so the human
        // discovers the pending through the gate's list (surfaced by
        // GET /api/approvals) and decides it there. The tool still waits.
        let gate = Arc::new(ApprovalGate::default());
        let args = serde_json::json!({"table": "users"});
        let decide_gate = gate.clone();
        let expected = args.clone();
        let decider = tokio::spawn(async move {
            let pending = loop {
                let list = decide_gate.list_pending();
                if let Some(p) = list.first() {
                    break p.clone();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            assert_eq!(pending.tool_name, "db__drop");
            assert_eq!(pending.args, expected);
            decide_gate
                .decide(pending.id, true, args_hash(&expected))
                .unwrap();
        });

        authorize(&gate, None, &always(), "db__drop", &args, None, None)
            .await
            .expect("approval through the list/decide path lets the tool run");
        decider.await.unwrap();
    }

    #[tokio::test]
    async fn never_policy_returns_without_touching_the_gate() {
        let gate = ApprovalGate::default();
        let (tx, mut rx) = mpsc::channel(4);
        authorize(
            &gate,
            Some(&tx),
            &PermissionPolicy::default(),
            "db__drop",
            &serde_json::json!({}),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err(), "no event for a Never-policy tool");
    }
}
