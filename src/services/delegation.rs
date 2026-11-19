//! Pure, redaction-free validation for bounded subagent delegation.
//!
//! Every rejection is a named error string, so a caller (or test) can tell a
//! depth breach from a cycle from a policy denial without parsing prose. This
//! function performs no I/O beyond the in-memory registry lookups and emits no
//! prompts or arguments — it only decides whether a child may be spawned.

use crate::domain::agent::Agent;
use crate::ports::delegation::DelegateRequest;
use crate::services::agent_registry::AgentRegistry;

/// Resolve and authorize a delegation target.
///
/// The caller's `[subagents]` policy supplies the allowlist and the
/// `allow_global` switch; the request carries the already-validated depth and
/// the ancestor chain. A target must be a same-project agent, or a global agent
/// when the policy opts in. Cross-project targets are never reachable.
pub fn validate_delegation(
    registry: &AgentRegistry,
    req: &DelegateRequest,
) -> Result<Agent, String> {
    // Depth is a hard bound carried from the caller; never trust a child to
    // lower it for itself.
    if req.depth > req.effective_max_depth {
        return Err(format!(
            "delegation depth limit exceeded: depth {} > max {}",
            req.depth, req.effective_max_depth
        ));
    }

    // Find the delegating agent so we can read its policy.
    let caller = registry
        .resolve_agent(req.project_id.as_deref(), Some(&req.caller_agent_id))
        .or_else(|| registry.get_agent(&req.caller_agent_id))
        .ok_or_else(|| {
            format!(
                "delegation caller agent '{}' not found",
                req.caller_agent_id
            )
        })?;
    let policy = caller.config.subagents.as_ref().ok_or_else(|| {
        format!(
            "delegation caller agent '{}' has no [subagents] policy",
            req.caller_agent_id
        )
    })?;

    // Allowlist is deny-all by default.
    if !policy.allowed.iter().any(|id| id == &req.agent_id) {
        return Err(format!(
            "delegation to agent '{}' is not in caller's allowlist",
            req.agent_id
        ));
    }

    // Resolve the target inside the same project first.
    let target = match req.project_id.as_deref() {
        Some(project) => match registry.get_project_agent(project, &req.agent_id) {
            Some(agent) => agent,
            None => {
                // Not in this project: a global agent is allowed only when the
                // policy opts in; an agent in another project never is.
                if let Some(global) = registry.get_agent(&req.agent_id) {
                    if !policy.allow_global {
                        return Err(format!(
                            "delegation to global agent '{}' requires subagents.allow_global",
                            req.agent_id
                        ));
                    }
                    global
                } else {
                    let elsewhere = registry
                        .list_agents()
                        .into_iter()
                        .find(|agent| agent.id == req.agent_id);
                    if let Some(other) = elsewhere {
                        return Err(format!(
                            "delegation to agent '{}' in project '{}' is not allowed (cross-project)",
                            req.agent_id,
                            other.project_id.unwrap_or_default()
                        ));
                    }
                    return Err(format!(
                        "delegation target agent '{}' not found",
                        req.agent_id
                    ));
                }
            }
        },
        None => registry
            .get_agent(&req.agent_id)
            .ok_or_else(|| format!("delegation target agent '{}' not found", req.agent_id))?,
    };

    // Cycle detection uses the qualified id so a same-named agent in a
    // different project cannot trip a false positive.
    let qualified = target.qualified_id();
    if req.chain.iter().any(|id| id == &qualified) {
        return Err(format!(
            "delegation cycle detected: agent '{}' already in chain",
            qualified
        ));
    }

    Ok(target)
}
