use crate::adapters::rig_engine::limits::{RunDeadline, ToolCallBudget};
use crate::adapters::rig_engine::tools::McpToolBridge;
use crate::adapters::rig_engine::ScopedMcpTool;
use crate::ports::engine::{AgentRunRequest, AgentRunResult};
use crate::ports::mcp::McpServerRegistry;
use rig_core::agent::Agent;
use rig_core::client::completion::CompletionClient;
use rig_core::client::Nothing;
use rig_core::completion::Prompt;
use rig_core::providers::ollama;
use rig_core::providers::ollama::CompletionModel;
use rig_core::tool::ToolDyn;
use std::sync::Arc;
use tokio::sync::Mutex;

pub async fn run_with_rig(
    ollama_url: &str,
    _mcp_registry: &Arc<dyn McpServerRegistry>,
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

    let deadline = RunDeadline::new(req.scope.timeout_secs);
    let timeout = deadline.remaining();

    let result = tokio::time::timeout(timeout, agent.prompt(&req.user_message)).await;

    match result {
        Ok(Ok(text)) => Ok(AgentRunResult {
            text,
            tool_calls: {
                let b = budget.lock().await;
                b.used()
            },
            iterations: max_iterations as u32,
            model: req.model,
        }),
        Ok(Err(e)) => Err(format!("ollama chat failed: {}", e)),
        Err(_) => Err(format!(
            "agent run timed out after {}s",
            req.scope.timeout_secs
        )),
    }
}
