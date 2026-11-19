//! Automatic-approval port: an optional, evidence-backed answer to "may this
//! tool call run without a human?".
//!
//! This is a port, not a policy decision: the core asks an injected decision
//! model and never decides for itself. [`AutoVerdict::Allow`] is the only answer
//! that may skip the human approval gate, and an implementation must only return
//! it on an affirmative, high-confidence response from that model. No router,
//! a router error, a refusal, or an unusable answer all keep the human in the
//! loop — automatic approval is never the default.
//!
//! The arguments are passed redacted and with their canonical hash already
//! computed, mirroring what the approval gate already has for human review. An
//! implementation should still apply redaction defensively before putting them
//! on the wire, so a mistake by a caller cannot leak a credential to the model.

use async_trait::async_trait;
use serde_json::Value;

/// The result of asking whether a tool call may run unattended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoVerdict {
    /// A decision model affirmatively approved this exact call. The only
    /// verdict that may bypass the human gate.
    Allow,
    /// A decision was reached, but it was not an approval (a refusal, low
    /// confidence, or an unusable answer). The human path still runs.
    Deny,
    /// No decision could be obtained (e.g. the router errored). The human path
    /// still runs, and this is counted separately from an explicit refusal.
    Unavailable,
}

/// Ask the configured decision model whether one tool call is safe to run
/// without human approval.
#[async_trait]
pub trait AutoDecider: Send + Sync {
    /// The configured default confidence threshold this decider was built with
    /// (for the router-backed adapter, `RouterConfig::min_confidence`). The gate
    /// uses it only when a tool's policy has no explicit `auto_threshold`; the
    /// per-call `threshold` passed to [`Self::should_allow`] is authoritative.
    fn min_confidence(&self) -> f32;

    /// `tool_name` is the qualified tool id, `redacted_args` is the call's
    /// arguments (already redacted by the caller, and re-redacted by the
    /// adapter), `args_hash_hex` is the shared canonical SHA-256 of the original
    /// arguments, and `threshold` is the per-call minimum confidence — the
    /// policy's `auto_threshold`, or the configured default.
    async fn should_allow(
        &self,
        tool_name: &str,
        redacted_args: &Value,
        args_hash_hex: &str,
        threshold: f32,
    ) -> AutoVerdict;
}
