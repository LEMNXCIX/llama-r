//! Agent engine port (Rig-backed implementation lives under adapters).

use crate::domain::agent::Agent;
use crate::domain::models::ChatMessage;
use crate::domain::scope::AgentScope;
use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct AgentRunRequest {
    pub agent: Agent,
    pub scope: AgentScope,
    pub model: String,
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
}

#[async_trait]
pub trait AgentEngine: Send + Sync {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String>;

    async fn run_stream(
        &self,
        req: AgentRunRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<AgentRunEvent>, String>;
}
