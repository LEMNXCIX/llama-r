//! Delegation port: bounded subagent spawning.
//!
//! A delegating agent asks the injected [`Delegator`] to run a child turn. The
//! request carries every bound the tree must share — depth, deadline and the
//! single [`ToolBudget`] allowance — so an implementation cannot silently give
//! a child a fresh allowance or a deeper recursion budget. This module is pure
//! contract: the concrete runtime lives in a later task.

use crate::domain::agent::Agent;
use crate::ports::engine::AgentRunEvent;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

/// The recursion bounds a turn carries through a delegation tree.
///
/// `chain` holds the qualified ids of the turn's **ancestors**, oldest first —
/// it does not include the current agent. A child's request extends it with the
/// caller's qualified id, so cycle detection sees every ancestor at every level.
/// `depth` is the number of delegations taken so far (0 at the root), and
/// `effective_max_depth` is the tightest limit seen along the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationContext {
    pub chain: Vec<String>,
    pub depth: u32,
    pub effective_max_depth: u32,
    /// Execution id of the turn that delegated this one. `None` at the root,
    /// where there is no parent. Feeds the child's `TraceRecorded` payload so
    /// the tree can be reconstructed from the execution log alone.
    pub parent_execution_id: Option<Uuid>,
    /// Execution id of the root turn of the whole tree. `None` until the first
    /// delegation; the root's own trace uses its own id (see `trace_payload_for`).
    pub root_execution_id: Option<Uuid>,
}

impl DelegationContext {
    /// Root bounds, derived from the running agent's `[subagents]` policy.
    /// `None` means the agent does not delegate at all.
    pub fn root_for(agent: &Agent) -> Option<Self> {
        let policy = agent.config.subagents.as_ref()?;
        Some(Self {
            chain: Vec::new(),
            depth: 0,
            effective_max_depth: policy.effective_max_depth(),
            parent_execution_id: None,
            root_execution_id: None,
        })
    }
}

/// The shared tool-call allowance for a whole delegation tree.
///
/// Implemented for `tokio::sync::Mutex<ToolCallBudget>` so an
/// `Arc<Mutex<ToolCallBudget>>` coerces to `Arc<dyn ToolBudget>` and one
/// allowance covers the parent and every child.
#[async_trait]
pub trait ToolBudget: Send + Sync {
    async fn try_consume(&self) -> Result<(), String>;

    /// Successful consumptions so far. Additive with a default so a caller's
    /// fake budget stays valid; only used for the turn's `tool_calls`
    /// telemetry, never for the allowance decision itself.
    async fn used(&self) -> u32 {
        0
    }
}

/// One progressive event from a child turn, tagged with its origin.
#[derive(Debug, Clone)]
pub struct ChildEvent {
    pub event: AgentRunEvent,
    pub task_id: String,
    pub depth: u32,
    pub agent_qualified_id: String,
}

/// Forward a child event to the parent's stream. Synchronous by design: the
/// eventual adapter captures a `mpsc::UnboundedSender` and calls `send` inside
/// the closure, so no async closure is needed.
pub type EventForwarder = Arc<dyn Fn(ChildEvent) + Send + Sync>;

/// A validated request to run one child agent turn.
#[derive(Clone)]
pub struct DelegateRequest {
    /// Bare id of the agent to delegate to.
    pub agent_id: String,
    /// Bare id of the agent asking for the delegation.
    pub caller_agent_id: String,
    /// Project scope of the delegation. `None` means a global-only context.
    pub project_id: Option<String>,
    pub message: String,
    pub task_id: Option<String>,
    /// Qualified ids of every ancestor in the chain (cycle detection).
    pub chain: Vec<String>,
    pub depth: u32,
    /// Parent's validated maximum depth; the child must not exceed it.
    pub effective_max_depth: u32,
    pub deadline: Instant,
    pub budget: Arc<dyn ToolBudget>,
    /// Carries the child's `ApprovalRequired` events to the root's SSE. Built
    /// by the delegate tool from its own `LocalToolDeps.event_tx`. `None` on a
    /// non-streaming path, where the pending is discoverable via the gate.
    pub event_tx: Option<EventForwarder>,
    /// Execution id of the turn asking for this delegation. The child's trace
    /// uses it as `parent_execution_id`.
    pub parent_execution_id: Uuid,
    /// Execution id of the root of the delegation tree. The child's trace uses
    /// it as `root_execution_id`.
    pub root_execution_id: Uuid,
}

#[derive(Debug, Clone)]
pub struct DelegateResult {
    pub answer: String,
    pub task_id: String,
}

#[async_trait]
pub trait Delegator: Send + Sync {
    async fn delegate(&self, req: DelegateRequest) -> Result<DelegateResult, String>;
}
