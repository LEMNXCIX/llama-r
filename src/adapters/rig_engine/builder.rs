use crate::adapters::rig_engine::limits::ToolCallBudget;
use crate::adapters::rig_engine::tools::McpToolBridge;
use crate::adapters::rig_engine::ScopedMcpTool;
use crate::domain::models::ChatMessage;
use crate::ports::engine::{AgentRunRequest, AgentRunResult};
use rig_core::agent::Agent;
use rig_core::client::completion::CompletionClient;
use rig_core::client::Nothing;
use rig_core::completion::{CompletionModel, Message, Prompt};
use rig_core::providers::ollama;
use rig_core::providers::openrouter;
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

/// Provider-agnostic body of a single agent turn.
///
/// Everything here is generic over the completion client/model: history, tools,
/// the tool-call budget and the preamble are all defined by rig-core's
/// provider-independent traits. Only the concrete client is built by the two
/// thin wrappers below. Keeping this one copy is what stops the OpenRouter path
/// from silently losing tools, RAG context or MCP the way an Ollama-only
/// reimplementation would.
async fn run_agent<C, M>(
    client: &C,
    model_name: &str,
    req: AgentRunRequest,
    system: String,
    tools: Vec<ScopedMcpTool>,
) -> Result<AgentRunResult, String>
where
    C: CompletionClient<CompletionModel = M>,
    M: CompletionModel + 'static,
{
    let budget = Arc::new(Mutex::new(ToolCallBudget::new(req.scope.max_tool_calls)));
    let max_iterations = req.scope.max_iterations.max(1) as usize;
    let temperature = req.agent.config.temperature.map(|t| t as f64);

    let agent: Agent<M> = if tools.is_empty() {
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
        .map_err(|e| format!("agent chat failed: {}", e))?;

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

/// Run a single agent turn via Rig against Ollama. Caller is responsible for
/// the overall timeout.
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

    // Ollama is the historical default; an agent with no model keeps the
    // long-standing "llama3.2" fallback on this path only. Own the name so the
    // borrow does not outlive the move of `req` into the generic helper.
    let model_name = if req.model.is_empty() {
        "llama3.2".to_string()
    } else {
        req.model.clone()
    };

    run_agent(&client, &model_name, req, system, tools).await
}

/// Run a single agent turn via Rig against OpenRouter.
///
/// `base_url = None` uses rig-core's OpenRouter default; `Some` targets a
/// compatible gateway. The key is passed to the client as a bearer token and is
/// never placed in a URL, log or error string — any error text is scrubbed of
/// the key before it is returned.
pub async fn run_with_openrouter(
    api_key: &str,
    base_url: Option<&str>,
    req: AgentRunRequest,
    system: String,
    tools: Vec<ScopedMcpTool>,
) -> Result<AgentRunResult, String> {
    let client = match base_url {
        Some(url) => openrouter::Client::builder()
            .api_key(api_key.to_string())
            .base_url(url)
            .build(),
        None => openrouter::Client::new(api_key.to_string()),
    }
    // Deliberately discard the rig error detail: it could echo request data,
    // and a client-build failure needs no more than a stable description.
    .map_err(|_| "failed to create OpenRouter client".to_string())?;

    // Never borrow the Ollama-specific "llama3.2" fallback here. An empty model
    // on OpenRouter is an explicit, named error: a model is chosen by which
    // upstream/agent the request selected, not by an arbitrary hard-coded name.
    if req.model.is_empty() {
        return Err(
            "OpenRouter agent run requires a non-empty model; the agent and default model are both empty"
                .to_string(),
        );
    }
    let model_name = req.model.clone();

    let result = run_agent(&client, &model_name, req, system, tools).await;
    // Safety net: nothing from the upstream may carry the key back to a caller.
    match result {
        Err(message) => Err(message.replace(api_key, "[redacted]")),
        ok => ok,
    }
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

    /// The OpenRouter path must not borrow Ollama's "llama3.2" fallback when an
    /// agent carries no model, and the key must never surface in the error.
    /// Building the client is local; no network call is made before the model
    /// check, so this stays hermetic.
    #[tokio::test]
    async fn run_with_openrouter_rejects_an_empty_model_without_network() {
        use crate::domain::agent::{Agent, AgentConfig, RagWritePolicy};
        use crate::domain::scope::{AgentScope, ToolsAllow};
        use std::collections::HashSet;

        let req = AgentRunRequest {
            agent: Agent {
                id: "a".to_string(),
                project_id: None,
                config: AgentConfig::default(),
            },
            scope: AgentScope {
                agent_id: "a".to_string(),
                project_id: None,
                mcp_sources: HashSet::new(),
                tools_allow: ToolsAllow::DenyAll,
                rag_sources: HashSet::new(),
                rag_write: RagWritePolicy::None,
                max_tool_calls: 0,
                max_iterations: 1,
                timeout_secs: 1,
            },
            model: String::new(),
            system_prompt: "sys".to_string(),
            user_message: "hi".to_string(),
            history: Vec::new(),
            conversation_id: None,
        };

        let err = run_with_openrouter("sk-dummy-key", None, req, "sys".to_string(), Vec::new())
            .await
            .expect_err("an empty model must be rejected");
        assert!(err.to_ascii_lowercase().contains("model"), "got: {err}");
        assert!(
            !err.contains("llama3.2"),
            "must not borrow Ollama's fallback: {err}"
        );
        assert!(
            !err.contains("sk-dummy-key"),
            "the key must never appear in an error: {err}"
        );
    }
}
