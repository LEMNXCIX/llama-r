use crate::api::chat_core::{execute_chat, AgentSelection};
use crate::api::handlers::AppState;
use crate::context::analyzer::ProjectAnalyzer;
use crate::domain::models::{ChatMessage, ChatRequest, JsonRpcRequest, JsonRpcResponse};
use crate::error::AppError;
use crate::services::validation::{canonicalize_project_path, validate_identifier};
use axum::{http::StatusCode, response::IntoResponse, Json};
use std::sync::Arc;

pub async fn handle_message(state: Arc<AppState>, payload: JsonRpcRequest) -> axum::response::Response {
    tracing::debug!(method = %payload.method, id = ?payload.id, "MCP request received");

    let response = match payload.method.as_str() {
        "initialize" => handle_initialize(&payload),
        "notifications/initialized" => Ok(StatusCode::NO_CONTENT.into_response()),
        "tools/list" => handle_tools_list(&state, &payload),
        "resources/list" => handle_resources_list(&state, &payload),
        "resources/read" => handle_resources_read(&state, &payload),
        "prompts/list" => handle_prompts_list(&payload),
        "prompts/get" => handle_prompt_get(&payload),
        "tools/call" => handle_tool_call(&state, &payload).await,
        _ => Ok(method_not_found(&payload)),
    };

    match response {
        Ok(resp) => resp,
        Err(error) => {
            state.observability.record_mcp_error();
            to_mcp_error_response(&payload, error)
        }
    }
}

fn handle_initialize(payload: &JsonRpcRequest) -> Result<axum::response::Response, AppError> {
    let result = serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "subscribe": false, "listChanged": true },
            "prompts": { "listChanged": false },
            "skills": { "listChanged": false }
        },
        "serverInfo": {
            "name": "llama-r-gateway",
            "version": "0.1.0"
        }
    });
    jsonrpc_result(payload, result)
}

fn handle_tools_list(
    state: &AppState,
    payload: &JsonRpcRequest,
) -> Result<axum::response::Response, AppError> {
    let mut tools: Vec<serde_json::Value> = state
        .agent_manager
        .list_agents()
        .into_iter()
        .filter(|agent| !agent.id.ends_with("_mcp"))
        .map(|agent| {
            let mut description = format!("Agent: {}. ", agent.config.name);
            if let Some(project_id) = &agent.config.context_project {
                description.push_str(&format!("Provee contexto del proyecto '{}'. ", project_id));
            }
            let summary = agent.config.system_prompt.replace('\n', " ");
            if summary.len() > 100 {
                description.push_str(&summary[..100]);
                description.push_str("...");
            } else {
                description.push_str(&summary);
            }
            serde_json::json!({
                "name": agent.id,
                "description": description,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Tu pregunta o instrucción para el agente"
                        }
                    },
                    "required": ["query"]
                }
            })
        })
        .collect();

    tools.extend([
        serde_json::json!({
            "name": "agent_query",
            "description": "Consulta un agente de Llama-R. Si se proporciona 'path', busca automáticamente el contexto del proyecto correspondiente.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "La pregunta o instrucción" },
                    "path": { "type": "string", "description": "Ruta absoluta del proyecto actual para autodetección de contexto" },
                    "agent_id": { "type": "string", "description": "ID del agente opcional" }
                },
                "required": ["query"]
            }
        }),
        serde_json::json!({
            "name": "detect_project_context",
            "description": "Detecta si Llama-R tiene un contexto configurado para la ruta dada.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Ruta absoluta del proyecto" }
                },
                "required": ["path"]
            }
        }),
        serde_json::json!({
            "name": "list_installed_skills",
            "description": "Lista todas las skills de AI instaladas globalmente o en el proyecto.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
        serde_json::json!({
            "name": "get_gateway_info",
            "description": "Obtiene información técnica sobre Llama-R.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        serde_json::json!({
            "name": "get_api_spec",
            "description": "Retorna la especificación técnica de los endpoints disponibles.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        serde_json::json!({
            "name": "list_contexts",
            "description": "Lista todos los contextos de proyecto generados en este gateway.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        serde_json::json!({
            "name": "create_agent",
            "description": "Crea un nuevo agente de IA persistente en Llama-R.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "name": { "type": "string" },
                    "model": { "type": "string" },
                    "system_prompt": { "type": "string" },
                    "context_project": { "type": "string" },
                    "rules": { "type": "array", "items": { "type": "string" } },
                    "skills": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["id", "name", "system_prompt"]
            }
        }),
        serde_json::json!({
            "name": "create_context",
            "description": "Genera un nuevo contexto AI para un proyecto mediante análisis automático.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_id": { "type": "string" },
                    "project_path": { "type": "string" }
                },
                "required": ["project_id", "project_path"]
            }
        }),
        serde_json::json!({
            "name": "get_skill_context",
            "description": "Obtiene el contenido completo de una skill específica.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "skill_name": { "type": "string" }
                },
                "required": ["skill_name"]
            }
        }),
    ]);

    jsonrpc_result(payload, serde_json::json!({ "tools": tools }))
}

fn handle_resources_list(
    state: &AppState,
    payload: &JsonRpcRequest,
) -> Result<axum::response::Response, AppError> {
    let resources: Vec<serde_json::Value> = state
        .context_store
        .list_contexts()
        .iter()
        .map(|context| {
            serde_json::json!({
                "uri": format!("mcp://projects/{}/context.md", context.project_id),
                "name": format!("Contexto del proyecto: {}", context.project_id),
                "description": format!("Documentación completa y reglas para {}", context.project_id),
                "mimeType": "text/markdown"
            })
        })
        .collect();
    jsonrpc_result(payload, serde_json::json!({ "resources": resources }))
}

fn handle_resources_read(
    state: &AppState,
    payload: &JsonRpcRequest,
) -> Result<axum::response::Response, AppError> {
    let uri = payload.params["uri"].as_str().unwrap_or("");
    let result = if uri.starts_with("mcp://projects/") {
        let project_id = uri.split('/').nth(2).unwrap_or("");
        let context = state.context_store.get_context_md(project_id);
        if context.is_empty() {
            serde_json::json!({ "error": "Context not found" })
        } else {
            serde_json::json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/markdown",
                    "text": context
                }]
            })
        }
    } else {
        serde_json::json!({ "error": "Invalid URI" })
    };
    jsonrpc_result(payload, result)
}

fn handle_prompts_list(payload: &JsonRpcRequest) -> Result<axum::response::Response, AppError> {
    jsonrpc_result(
        payload,
        serde_json::json!({
            "prompts": [{
                "name": "technical_integration_guide",
                "description": "Guía completa para integrar la IA de Llama-R en cualquier proyecto.",
                "arguments": []
            }]
        }),
    )
}

fn handle_prompt_get(payload: &JsonRpcRequest) -> Result<axum::response::Response, AppError> {
    jsonrpc_result(
        payload,
        serde_json::json!({
            "description": "Guía técnica de integración de Llama-R.",
            "messages": [{
                "role": "user",
                "content": {
                    "type": "text",
                    "text": "Usa /api/chat o /v1/chat/completions para integrar Llama-R, o MCP en /api/mcp para herramientas y recursos."
                }
            }]
        }),
    )
}

async fn handle_tool_call(
    state: &Arc<AppState>,
    payload: &JsonRpcRequest,
) -> Result<axum::response::Response, AppError> {
    let tool_name = payload.params["name"].as_str().unwrap_or("");
    let args = &payload.params["arguments"];

    match tool_name {
        "get_gateway_info" => {
            let context = state.context_store.get_context_md("llama-r");
            let text = if context.is_empty() {
                "Llama-R: High-Performance AI Gateway.".to_string()
            } else {
                context
            };
            tool_text_result(payload, text)
        }
        "get_api_spec" => {
            let spec = serde_json::json!({
                "rest": {
                    "chat": "POST /api/chat - SSE/JSON streaming",
                    "openai": "POST /v1/chat/completions - Compatible con header X-Agent",
                    "agents": "GET/POST /api/agents, GET/PUT/DELETE /api/agents/:id",
                    "contexts": "GET/POST /api/contexts, POST /api/contexts/:id/analyze"
                },
                "grpc": "LlamaGateway service en puerto 50051",
                "mcp": "Endpoint JSON-RPC en /api/mcp",
                "cli": "Comandos: init-agent, init-mcp, analyze, reanalyze, run"
            });
            let text = serde_json::to_string_pretty(&spec)?;
            tool_text_result(payload, text)
        }
        "list_contexts" => {
            let summary = state
                .context_store
                .list_contexts()
                .iter()
                .map(|context| format!("- {}: {}", context.project_id, context.path))
                .collect::<Vec<_>>()
                .join("\n");
            tool_text_result(payload, summary)
        }
        "create_agent" => create_agent_tool(state, payload, args).await,
        "create_context" => create_context_tool(state, payload, args).await,
        "get_skill_context" => {
            let skill_name = args["skill_name"].as_str().unwrap_or("");
            match state.skill_manager.get_skill(skill_name) {
                Some(skill) => tool_text_result(payload, skill.content),
                None => tool_error_result(payload, format!("Skill '{}' no encontrada.", skill_name)),
            }
        }
        "agent_query" => agent_query_tool(state, payload, args).await,
        "detect_project_context" => detect_project_context_tool(state, payload, args),
        "list_installed_skills" => {
            let summary = state
                .skill_manager
                .list_skills()
                .iter()
                .map(|skill| format!("- {}: {}", skill.id, skill.metadata.description))
                .collect::<Vec<_>>()
                .join("\n");
            tool_text_result(payload, summary)
        }
        other => {
            let query = args["query"].as_str().unwrap_or("");
            if query.is_empty() {
                tool_error_result(payload, "Missing required argument: 'query' is required.".to_string())
            } else {
                let response = execute_chat(
                    state,
                    ChatRequest {
                        model: other.to_string(),
                        messages: vec![ChatMessage {
                            role: "user".to_string(),
                            content: query.to_string(),
                        }],
                        stream: false,
                    },
                    AgentSelection::default(),
                )
                .await?;
                tool_text_result(payload, response.message.content)
            }
        }
    }
}

async fn create_agent_tool(
    state: &Arc<AppState>,
    payload: &JsonRpcRequest,
    args: &serde_json::Value,
) -> Result<axum::response::Response, AppError> {
    let agent_id = args["id"].as_str().unwrap_or("");
    let name = args["name"].as_str().unwrap_or("");
    let system_prompt = args["system_prompt"].as_str().unwrap_or("");
    if agent_id.is_empty() || name.is_empty() || system_prompt.is_empty() {
        return tool_error_result(
            payload,
            "Missing required arguments: 'id', 'name', and 'system_prompt' are required.".to_string(),
        );
    }
    validate_identifier(agent_id, "agent_id")?;
    let model = args["model"].as_str().unwrap_or("");
    let context_project = args["context_project"].as_str();
    let project_id = args["project_id"].as_str().unwrap_or("");
    let rules = args["rules"]
        .as_array()
        .map(|items| items.iter().filter_map(|item| item.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    let skills = args["skills"]
        .as_array()
        .map(|items| items.iter().filter_map(|item| item.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    let toml_content = if let Some(raw) = args["toml"].as_str() {
        raw.to_string()
    } else {
        let context_project_line = context_project
            .map(|project| format!("\ncontext_project = \"{}\"", project))
            .unwrap_or_default();
        let rules_line = if rules.is_empty() {
            String::new()
        } else {
            format!(
                "\nrules = [{}]",
                rules
                    .iter()
                    .map(|rule| format!("\"{}\"", rule.replace('"', "\\\"")))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let skills_line = if skills.is_empty() {
            String::new()
        } else {
            format!(
                "\nskills = [{}]",
                skills
                    .iter()
                    .map(|skill| format!("\"{}\"", skill))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        format!(
            "name = \"{}\"\nmodel = \"{}\"\nsystem_prompt = \"\"\"\n{}\n\"\"\"{}{}{}",
            name,
            model,
            system_prompt,
            context_project_line,
            rules_line,
            skills_line
        )
    };

    let path = if project_id.is_empty() {
        crate::core::paths::get_agents_dir().join(format!("{}.toml", agent_id))
    } else {
        let dir = crate::core::paths::get_project_agents_dir(project_id);
        std::fs::create_dir_all(&dir)?;
        dir.join(format!("{}.toml", agent_id))
    };
    std::fs::write(&path, toml_content)?;
    if let Err(error) = state.agent_manager.load_agents() {
        tracing::warn!(error = %error, "Failed to reload agents after MCP create_agent");
    }
    tool_text_result(payload, format!("Agent '{}' created and loaded at {}.", agent_id, path.display()))
}

async fn create_context_tool(
    state: &Arc<AppState>,
    payload: &JsonRpcRequest,
    args: &serde_json::Value,
) -> Result<axum::response::Response, AppError> {
    let project_id = args["project_id"].as_str().unwrap_or("");
    let project_path = args["project_path"].as_str().unwrap_or("");
    if project_id.is_empty() || project_path.is_empty() {
        return tool_error_result(
            payload,
            "Missing required arguments: 'project_id' and 'project_path' are required.".to_string(),
        );
    }
    validate_identifier(project_id, "project_id")?;
    if state.context_store.get_context(project_id).is_some() {
        return tool_error_result(
            payload,
            format!("Context for '{}' already exists. Use re-analyze to refresh it.", project_id),
        );
    }
    let canonical_path = canonicalize_project_path(project_path)?;
    let provider = state.provider.clone();
    let responder = move |prompt: String| {
        let provider = provider.clone();
        Box::pin(async move {
            provider
                .chat(ChatRequest {
                    model: std::env::var("DEFAULT_MODEL").unwrap_or_else(|_| "llama3".to_string()),
                    messages: vec![ChatMessage {
                        role: "user".to_string(),
                        content: prompt,
                    }],
                    stream: false,
                })
                .await
                .map(|response| response.message.content)
                .map_err(|error| error.to_string())
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
    };
    let analyzer = ProjectAnalyzer::new(state.skill_manager.clone());
    let context = analyzer
        .analyze(project_id, &canonical_path.to_string_lossy(), responder)
        .await
        .map_err(AppError::Runtime)?;
    state.context_store.save_context(context)?;
    tool_text_result(payload, format!("Context for '{}' generated and saved.", project_id))
}

async fn agent_query_tool(
    state: &Arc<AppState>,
    payload: &JsonRpcRequest,
    args: &serde_json::Value,
) -> Result<axum::response::Response, AppError> {
    let query = args["query"].as_str().unwrap_or("");
    if query.is_empty() {
        return tool_error_result(payload, "Missing required argument: 'query' is required.".to_string());
    }
    let mut agent_id = args["agent_id"].as_str().unwrap_or("").to_string();
    let path = args["path"].as_str().unwrap_or("");
    if agent_id.is_empty() && !path.is_empty() {
        agent_id = detect_best_project_context(state, path)
            .map(|project_id| format!("{}_mcp", project_id))
            .unwrap_or_default();
    }
    if agent_id.is_empty() {
        agent_id = "llama-r_mcp".to_string();
    }
    let response = execute_chat(
        state,
        ChatRequest {
            model: agent_id.clone(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: query.to_string(),
            }],
            stream: false,
        },
        AgentSelection { project_id: None, agent_id: Some(&agent_id) },
    )
    .await?;
    tool_text_result(payload, response.message.content)
}

fn detect_project_context_tool(
    state: &AppState,
    payload: &JsonRpcRequest,
    args: &serde_json::Value,
) -> Result<axum::response::Response, AppError> {
    let path = args["path"].as_str().unwrap_or("");
    let result = if let Some(project_id) = detect_best_project_context(state, path) {
        serde_json::json!({
            "content": [{ "type": "text", "text": format!("Project context found: {}", project_id) }],
            "project_id": project_id,
        })
    } else {
        serde_json::json!({
            "content": [{ "type": "text", "text": "No specialized context found for this path." }],
            "project_id": null,
        })
    };
    jsonrpc_result(payload, result)
}

fn detect_best_project_context(state: &AppState, path: &str) -> Option<String> {
    let mut best_match: Option<String> = None;
    let mut match_len = 0;
    for context in state.context_store.list_contexts() {
        if path.starts_with(&context.path) && context.path.len() > match_len {
            match_len = context.path.len();
            best_match = Some(context.project_id);
        }
    }
    best_match
}

fn jsonrpc_result(
    payload: &JsonRpcRequest,
    result: serde_json::Value,
) -> Result<axum::response::Response, AppError> {
    if let Some(id) = &payload.id {
        Ok(Json(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            result: Some(result),
            error: None,
        })
        .into_response())
    } else {
        Ok(StatusCode::NO_CONTENT.into_response())
    }
}

fn tool_text_result(
    payload: &JsonRpcRequest,
    text: String,
) -> Result<axum::response::Response, AppError> {
    jsonrpc_result(
        payload,
        serde_json::json!({
            "content": [{ "type": "text", "text": text }]
        }),
    )
}

fn tool_error_result(
    payload: &JsonRpcRequest,
    message: String,
) -> Result<axum::response::Response, AppError> {
    jsonrpc_result(
        payload,
        serde_json::json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true
        }),
    )
}

fn method_not_found(payload: &JsonRpcRequest) -> axum::response::Response {
    if let Some(id) = &payload.id {
        Json(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            result: None,
            error: Some(serde_json::json!({
                "code": -32601,
                "message": format!("Method not found: {}", payload.method)
            })),
        })
        .into_response()
    } else {
        StatusCode::NO_CONTENT.into_response()
    }
}

fn to_mcp_error_response(payload: &JsonRpcRequest, error: AppError) -> axum::response::Response {
    if let Some(id) = &payload.id {
        Json(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            result: None,
            error: Some(serde_json::json!({
                "code": error.status_code().as_u16() as i32,
                "message": error.to_string()
            })),
        })
        .into_response()
    } else {
        error.into_response()
    }
}


