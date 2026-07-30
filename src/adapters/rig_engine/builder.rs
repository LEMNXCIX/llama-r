use crate::adapters::rig_engine::limits::ToolCallBudget;
use crate::adapters::rig_engine::tools::McpToolBridge;
use crate::adapters::rig_engine::ScopedMcpTool;
use crate::domain::models::ChatMessage;
use crate::ports::engine::{AgentRunRequest, AgentRunResult};
use rig_core::agent::Agent;
use rig_core::client::completion::CompletionClient;
use rig_core::client::Nothing;
use rig_core::completion::{Message, Prompt};
use rig_core::providers::ollama;
use rig_core::providers::ollama::CompletionModel;
use rig_core::tool::ToolDyn;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Convert llama-r chat history into Rig completion messages.
/// System messages are skipped (preamble already carries the system prompt).
pub fn to_rig_history(history: &[ChatMessage]) -> Vec<Message> {
    history
        .iter()
        .filter_map(|msg| match msg.role.as_str() {
            "system" => None,
            "assistant" => Some(Message::assistant(msg.content.clone())),
            // Treat tool / unknown roles as user-side context so they are not dropped.
            _ => Some(Message::user(msg.content.clone())),
        })
        .collect()
}

/// Run a single agent turn via Rig (Ollama). Caller is responsible for overall timeout.
pub async fn run_with_rig(
    ollama_url: &str,
    req: AgentRunRequest,
    system: String,
    tools: Vec<ScopedMcpTool>,
) -> Result<AgentRunResult, String> {
    let client = ollama::Client::builder()
        .api_key(Nothing)
        .base_url(ollama_url)
        .build()
        .map_err(|e| format!("failed to create Ollama client: {}", e))?;

    let model_name = if req.model.is_empty() {
        "llama3.2"
    } else {
        &req.model
    };

    let budget = Arc::new(Mutex::new(ToolCallBudget::new(req.scope.max_tool_calls)));
    let max_iterations = req.scope.max_iterations.max(1) as usize;
    let temperature = req.agent.config.temperature.map(|t| t as f64);

    let agent: Agent<CompletionModel> = if tools.is_empty() {
        let mut builder = client
            .agent(model_name)
            .preamble(&system)
            .default_max_turns(max_iterations);
        if let Some(temp) = temperature {
            builder = builder.temperature(temp);
        }
        builder.build()
    } else {
        let rig_tools: Vec<Box<dyn ToolDyn>> = tools
            .into_iter()
            .map(|t| {
                let bridge = McpToolBridge::new(t, budget.clone());
                Box::new(bridge) as Box<dyn ToolDyn>
            })
            .collect();
        let mut builder = client
            .agent(model_name)
            .preamble(&system)
            .tools(rig_tools)
            .default_max_turns(max_iterations);
        if let Some(temp) = temperature {
            builder = builder.temperature(temp);
        }
        builder.build()
    };

    let history = to_rig_history(&req.history);
    let text = agent
        .prompt(req.user_message.as_str())
        .history(history)
        .await
        .map_err(|e| format!("ollama chat failed: {}", e))?;

    let tool_calls = {
        let b = budget.lock().await;
        b.used()
    };
    // Approximate completed turns: one LLM turn plus one per tool call.
    let iterations = (1u32).saturating_add(tool_calls).min(max_iterations as u32);

    Ok(AgentRunResult {
        text,
        tool_calls,
        iterations,
        model: req.model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_rig_history_skips_system_and_maps_roles() {
        let history = vec![
            ChatMessage {
                role: "system".into(),
                content: "ignore".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            },
            ChatMessage {
                role: "assistant".into(),
                content: "hello".into(),
            },
        ];
        let msgs = to_rig_history(&history);
        assert_eq!(msgs.len(), 2);
    }
}
