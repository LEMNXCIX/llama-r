//! Declarative permission policy and its pure evaluation engine.
//!
//! A [`PermissionPolicy`] is compiled from the agent TOML into `AgentScope`
//! (see `crate::services::scope_builder`). The engine performs no I/O: given a
//! policy and a qualified tool name it returns only a decision, so it can be
//! tested without a runtime, a provider, or a gate.

use serde::{Deserialize, Serialize};

/// When a tool requires human approval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Never ask: the tool executes without approval. This is the default, so
    /// an agent with no `[[policy.approvals]]` rules behaves as before.
    #[default]
    Never,
    /// Ask on first use only. Suppressing later approvals is a gate concern;
    /// the pure engine treats `Once` like `Always` because both block the
    /// first call.
    Once,
    /// Ask before every call.
    Always,
    /// Ask via an automatic decision model. There is no decision model in this
    /// phase (Fase 6), so `Auto` is exactly `Always` today — documented here
    /// so nobody assumes a silent approval.
    Auto,
}

/// One ordered approval rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRule {
    /// Simple glob over the qualified tool name: `server__tool` for an MCP
    /// tool, or the bare id for a local tool. `*` matches any run of
    /// characters; everything else is literal. The whole string must match.
    #[serde(default)]
    pub tool_pattern: String,
    #[serde(default)]
    pub policy: ApprovalPolicy,
}

/// Permission policy carried from the agent manifest into `AgentScope`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPolicy {
    /// Connection ids whose tools the agent may see. Kept here as declarative
    /// data; tool visibility itself is still enforced by
    /// `AgentScope::allows_tool`, so a v1 manifest with an empty value keeps
    /// the behavior it already had.
    #[serde(default)]
    pub connections_allow: Vec<String>,
    /// Ordered approval rules. The first pattern that matches wins.
    #[serde(default)]
    pub approvals: Vec<ApprovalRule>,
}

impl PermissionPolicy {
    /// The policy of the first matching rule, or `Never` when none matches.
    pub fn approval_for(&self, tool_name: &str) -> ApprovalPolicy {
        self.approvals
            .iter()
            .find(|rule| glob_matches(&rule.tool_pattern, tool_name))
            .map(|rule| rule.policy)
            .unwrap_or(ApprovalPolicy::Never)
    }
}

/// Pure decision returned by [`PermissionEngine::authorize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Execute without human approval.
    Allow,
    /// Pause for human approval before executing.
    Approve,
}

/// Stateless evaluator of a [`PermissionPolicy`] against a tool name.
pub struct PermissionEngine;

impl PermissionEngine {
    /// Decide whether `tool_name` may run without approval under `policy`.
    ///
    /// Pure: no state, no side effects, no I/O. The first matching glob rule
    /// wins; with no match (or no rules at all) the default `Never` allows.
    pub fn authorize(policy: &PermissionPolicy, tool_name: &str) -> ApprovalDecision {
        match policy.approval_for(tool_name) {
            ApprovalPolicy::Never => ApprovalDecision::Allow,
            // `Once`, `Always` and `Auto` all require approval before a call.
            // `Once`'s "first time only" and `Auto`'s future decision model
            // belong to the gate (Fase 3 Task 4 / Fase 6), not to this pure
            // function.
            ApprovalPolicy::Once | ApprovalPolicy::Always | ApprovalPolicy::Auto => {
                ApprovalDecision::Approve
            }
        }
    }
}

/// Minimal glob matcher: `*` matches zero or more characters, every other
/// character is literal. Case-sensitive, whole-string, with no `?`, `[]`,
/// escaping or path separators. Patterns come from a trusted agent manifest,
/// never from a model output or a request header.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let mut p = 0usize;
    let mut t = 0usize;
    let mut star: Option<usize> = None;
    let mut after_star = 0usize;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == text[t] || pattern[p] == '*') {
            if pattern[p] == '*' {
                star = Some(p);
                after_star = t;
                p += 1;
            } else {
                p += 1;
                t += 1;
            }
        } else if let Some(s) = star {
            // Retry with the star consuming one more character.
            p = s + 1;
            after_star += 1;
            t = after_star;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_handles_empty_and_trailing_stars() {
        assert!(glob_matches("", ""));
        assert!(!glob_matches("", "x"));
        assert!(glob_matches("*", ""));
        assert!(glob_matches("a*", "a"));
        assert!(glob_matches("a*", "abc"));
        assert!(!glob_matches("a*", "ba"));
    }
}
