//! Agent engine port (Rig-backed implementation lives under adapters).

use crate::config::ProviderKind;
use crate::domain::agent::Agent;
use crate::domain::models::ChatMessage;
use crate::domain::scope::AgentScope;
use crate::providers::capabilities::ModelCapabilities;
use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone)]
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
