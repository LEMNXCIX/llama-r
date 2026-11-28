//! Agent engine port (Rig-backed implementation lives under adapters).

use crate::config::ProviderKind;
use crate::domain::agent::Agent;
use crate::domain::models::ChatMessage;
use crate::domain::scope::AgentScope;
use crate::ports::delegation::{DelegationContext, ToolBudget};
use crate::providers::capabilities::ModelCapabilities;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

/// Hand-written `Debug`: every field is printable except the shared budget,
/// whose trait object deliberately has no `Debug`. Log its presence, never its
/// contents — an allowance is not a secret, but it is also not useful output.
impl std::fmt::Debug for AgentRunRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRunRequest")
            .field("agent", &self.agent)
            .field("scope", &self.scope)
            .field("model", &self.model)
            .field("provider", &self.provider)
            .field("capabilities", &self.capabilities)
            .field("system_prompt", &self.system_prompt)
            .field("user_message", &self.user_message)
            .field("history", &self.history)
            .field("conversation_id", &self.conversation_id)
            .field("execution_id", &self.execution_id)
            .field("max_iterations_override", &self.max_iterations_override)
            .field(
                "shared_budget",
                &self.shared_budget.as_ref().map(|_| "<shared budget>"),
            )
            .field("delegation_context", &self.delegation_context)
            .field(
                "tool_event_tx",
                &self.tool_event_tx.as_ref().map(|_| "<tool event sink>"),
            )
            .finish()
    }
}

#[derive(Clone)]
pub struct AgentRunRequest {
    pub agent: Agent,
    pub scope: AgentScope,
    pub model: String,
    /// The provider resolved for *this* agent, which may differ from the global
    /// `PROVIDER` the engine was built with. It travels per request so the engine
    /// can select the right client rather than reaching the wrong upstream.
    pub provider: ProviderKind,
    /// Capabilities of `(provider, model)`. Carried per request so a capability
    /// gate can read the same answer the resolver produced.
    pub capabilities: ModelCapabilities,
    pub system_prompt: String,
    pub user_message: String,
    pub history: Vec<ChatMessage>,
    pub conversation_id: Option<String>,
    /// The durable turn this run belongs to. Threads the Fase 7 execution id
    /// into the tool bridges so a retry of the same turn can replay a tool's
    /// stored result instead of repeating its side effect. Every caller must
    /// supply one; a fresh `Uuid::new_v4()` is correct for a one-shot turn that
    /// has no execution record (e.g. the TUI).
    pub execution_id: Uuid,
    /// Per-turn tool-loop budget override for a checkpoint-informed retry. The
    /// engine's limit is `unwrap_or(scope.max_iterations)`, so `None` keeps the
    /// agent manifest's budget byte for byte. Additive: no `AgentEngine` method
    /// signature changes, and no other caller has to opt in.
    pub max_iterations_override: Option<usize>,
    /// The one tool-call allowance shared by a whole delegation tree. `Some` on
    /// a child turn threaded from its parent's `DelegateRequest.budget`; `None`
    /// for an ordinary turn, where the engine mints a fresh allowance from
    /// `scope.max_tool_calls`. Additive by design: no existing engine signature
    /// changes, and a plain turn keeps its old behaviour byte for byte.
    pub shared_budget: Option<Arc<dyn ToolBudget>>,
    /// Recursion bounds for this turn, carried from a parent's delegation.
    /// `None` for an ordinary root turn, where the engine derives the bounds
    /// from the resolved agent's own manifest. Additive, like `shared_budget`.
    pub delegation_context: Option<DelegationContext>,
    /// Event channel for a child turn whose approvals must be forwarded to the
    /// root's SSE. `delegate_turn` creates it and drives a forwarder task; the
    /// non-streaming `run_inner` wires it into `LocalToolDeps.event_tx` so a
    /// child tool's `ApprovalRequired` is not lost. `None` on ordinary turns.
    pub approval_event_tx: Option<tokio::sync::mpsc::Sender<AgentRunEvent>>,
    /// Additive sink for the no-args tool-event log. Both tool bridges send one
    /// event per finished call; `AgentRuntime` owns the receiver and persists
    /// each event into the conversation that is already being written. The type
    /// deliberately has no arguments field, so a tool input can never reach it.
    pub tool_event_tx: Option<tokio::sync::mpsc::Sender<ToolEvent>>,
}

/// The only facts a tool call contributes to conversation history: its name and
/// whether it succeeded. Arguments are never carried here — a tool input may hold
/// an API key or a password, and the persisted `tool_events` row stores neither.
#[derive(Debug, Clone)]
pub struct ToolEvent {
    pub tool_name: String,
    pub success: bool,
}

#[derive(Debug, Clone)]
pub struct AgentRunResult {
    pub text: String,
    pub tool_calls: u32,
    pub iterations: u32,
    pub model: String,
}

/// Streaming / progressive events for SSE and TUI.
#[derive(Debug, Clone)]
pub enum AgentRunEvent {
    Token {
        text: String,
    },
    ToolCall {
        server_id: String,
        tool_name: String,
        arguments: Value,
    },
    ToolResult {
        server_id: String,
        tool_name: String,
        is_error: bool,
        preview: String,
    },
    Completed {
        result: AgentRunResult,
    },
    Error {
        message: String,
    },
    /// A tool needs a human decision before it may run. The caller awaits the
    /// oneshot registered in the approval gate; the stream keeps the request
    /// open while the human decides.
    ApprovalRequired {
        id: Uuid,
        tool_name: String,
        args: Value,
        /// Present on a child turn's approval re-emitted into the root's SSE:
        /// the effective task id from the `DelegateRequest`. `None` for a
        /// normal (non-delegated) approval.
        task_id: Option<String>,
        /// Present on a forwarded child approval: the delegation depth. `None`
        /// for a normal approval.
        depth: Option<u32>,
    },
}

#[async_trait]
pub trait AgentEngine: Send + Sync {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String>;

    async fn run_stream(
        &self,
        req: AgentRunRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<AgentRunEvent>, String>;
}
