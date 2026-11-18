//! The approval seam shared by the MCP and local `ToolDyn` bridges.
//!
//! A bridge is deep inside rig-core's tool loop (`agent.prompt()` /
//! `agent.stream_prompt()`), so it cannot reach the caller directly. It carries
//! the run's [`ApprovalGate`] and an optional engine event sender: `Some` on the
//! streaming path, where the `ApprovalRequired` event reaches the SSE caller,
//! and `None` on the non-streaming path, which has no channel to deliver a
//! decision to a human and therefore refuses an approval-gated tool with a
//! named error rather than faking an event nobody can see.
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
    ApprovalDecision as PolicyDecision, PermissionEngine, PermissionPolicy,
};
use crate::ports::engine::AgentRunEvent;
use crate::services::approval::{ApprovalDecision as GateDecision, ApprovalGate};
use serde_json::Value;
use tokio::sync::mpsc;

/// Authorize one tool call. `Ok(())` means "execute" (the caller then charges
/// the shared budget and dispatches); `Err` is a named refusal carrying the tool
/// name and the reason.
pub async fn authorize(
    gate: &ApprovalGate,
    event_tx: Option<&mpsc::Sender<AgentRunEvent>>,
    permission: &PermissionPolicy,
    tool_name: &str,
    args: &Value,
) -> Result<(), String> {
    match PermissionEngine::authorize(permission, tool_name) {
        PolicyDecision::Allow => Ok(()),
        PolicyDecision::Approve => {
            let tx = event_tx.ok_or_else(|| {
                format!(
                    "tool '{tool_name}' requires human approval, but this agent run has no approval event channel; \
                     HITL approval requires the streaming (SSE) path"
                )
            })?;

            let (id, rx) = gate.request(tool_name, args);
            // The full args travel to the caller in the event so a human can see
            // what they are approving; the gate stores only their one-way hash.
            tx.send(AgentRunEvent::ApprovalRequired {
                id,
                tool_name: tool_name.to_string(),
                args: args.clone(),
            })
            .await
            .map_err(|_| {
                format!("tool '{tool_name}' was not executed: the approval event channel is closed")
            })?;

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

    fn always() -> PermissionPolicy {
        PermissionPolicy {
            connections_allow: Vec::new(),
            approvals: vec![ApprovalRule {
                tool_pattern: "*".into(),
                policy: ApprovalPolicy::Always,
            }],
        }
    }

    #[tokio::test]
    async fn no_event_channel_is_a_named_refusal_not_a_hang() {
        let gate = ApprovalGate::default();
        let err = authorize(&gate, None, &always(), "db__drop", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("requires human approval"), "{err}");
        assert!(err.contains("streaming"), "{err}");
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
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err(), "no event for a Never-policy tool");
    }
}
