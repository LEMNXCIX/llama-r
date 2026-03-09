use crate::context::analyzer::ContextEnricher;
use crate::context::store::ContextStore;
use crate::domain::models::{
    ChatMessage, ChatRequest, ChatResponse, JsonRpcRequest, JsonRpcResponse, ListModelsResponse,
};
use crate::optimizer::metrics::TokenMetrics;
use crate::optimizer::TokenOptimizer;
use crate::providers::LLMProvider;
use crate::services::agent_manager::AgentManager;
use crate::services::skill_manager::SkillManager;
use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, Sse},
        IntoResponse,
    },
    Json,
};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio_stream::StreamExt as _;

pub struct AppState {
    pub provider: Arc<dyn LLMProvider + Send + Sync>,
    pub agent_manager: Arc<AgentManager>,
    pub skill_manager: Arc<SkillManager>,
    pub context_store: Arc<ContextStore>,
    pub context_enricher: Arc<ContextEnricher>,
    pub metrics: Arc<TokenMetrics>,
    pub api_running: AtomicBool,
    pub grpc_running: AtomicBool,
    pub logs: Arc<Mutex<VecDeque<String>>>,
}

pub async fn list_models(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    match state.provider.list_models().await {
        Ok(models) => Ok(Json(ListModelsResponse { models })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_chat_core(
    state: &AppState,
    mut payload: ChatRequest,
) -> Result<ChatResponse, (StatusCode, String)> {
    if payload.messages.is_empty() {
        tracing::error!("Chat request failed: 'messages' array is empty.");
        return Err((
            StatusCode::BAD_REQUEST,
            "Missing required field: 'messages' cannot be empty.".to_string(),
        ));
    }

    let original_model = payload.model.clone();

    // 1. Resolve agent
    if let Some(agent) = state.agent_manager.get_agent(&payload.model) {
        let optimizer = TokenOptimizer::new(agent.config.optimize.clone());
        let mut optimized_messages = Vec::new();

        // Build enriched system prompt
        let sys_prompt_raw = state.context_enricher.build_system_prompt(&agent);
        let sys_prompt = optimizer.optimize(&sys_prompt_raw);
        state
            .metrics
            .record_optimization(sys_prompt_raw.len(), sys_prompt.len());
        optimized_messages.push(ChatMessage {
            role: "system".to_string(),
            content: sys_prompt,
        });

        // Optimize user messages
        for mut msg in payload.messages {
            let orig_len = msg.content.len();
            msg.content = optimizer.optimize(&msg.content);
            state
                .metrics
                .record_optimization(orig_len, msg.content.len());
            optimized_messages.push(msg);
        }

        payload.model = state.context_enricher.resolve_model(&agent);
        payload.messages = optimized_messages;
    }

    // 2. Call provider with Fallback logic
    match state.provider.chat(payload.clone()).await {
        Ok(response) => Ok(response),
        Err(e) => {
            // If the requested model (agent or direct) failed, try the default model
            let default_model = std::env::var("DEFAULT_MODEL").unwrap_or_default();
            if !default_model.is_empty()
                && original_model != default_model
                && payload.model != default_model
            {
                tracing::warn!(
                    "Model '{}' failed, falling back to '{}'. Error: {}",
                    original_model,
                    default_model,
                    e
                );
                payload.model = default_model;
                state.provider.chat(payload).await.map_err(|e2| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Fallback also failed: {}", e2),
                    )
                })
            } else {
                Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
            }
        }
    }
}

pub async fn chat(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(mut payload): Json<ChatRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    if payload.messages.is_empty() {
        tracing::error!("Chat request failed: 'messages' array is empty.");
        return Err((
            StatusCode::BAD_REQUEST,
            "Missing required field: 'messages' cannot be empty.".to_string(),
        ));
    }

    // 1. Resolve agent from X-Agent header (priority) or payload.model
    let agent_id = headers
        .get("x-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(&payload.model);

    if payload.stream {
        // Resolve agent for stream too
        if let Some(agent) = state.agent_manager.get_agent(agent_id) {
            tracing::info!("Routing streaming request to agent: {}", agent_id);
            payload.model = agent_id.to_string(); // Ensure we use the agent ID
            let optimizer = TokenOptimizer::new(agent.config.optimize.clone());
            let mut optimized_messages = Vec::new();

            // Build enriched system prompt via ContextEnricher
            let sys_prompt_raw = state.context_enricher.build_system_prompt(&agent);
            let sys_prompt = optimizer.optimize(&sys_prompt_raw);
            state
                .metrics
                .record_optimization(sys_prompt_raw.len(), sys_prompt.len());
            optimized_messages.push(ChatMessage {
                role: "system".to_string(),
                content: sys_prompt,
            });

            for mut msg in payload.messages {
                let orig_len = msg.content.len();
                msg.content = optimizer.optimize(&msg.content);
                state
                    .metrics
                    .record_optimization(orig_len, msg.content.len());
                optimized_messages.push(msg);
            }

            payload.model = state.context_enricher.resolve_model(&agent);
            payload.messages = optimized_messages;
        }

        let stream_res = state.provider.chat_stream(payload).await;
        match stream_res {
            Ok(stream) => {
                let sse_stream = stream.map(|res| match res {
                    Ok(event) => {
                        if let Ok(json_str) = serde_json::to_string(&event) {
                            Ok::<_, Infallible>(Event::default().data(json_str))
                        } else {
                            Ok::<_, Infallible>(Event::default().data("Serialization error"))
                        }
                    }
                    Err(e) => Ok::<_, Infallible>(Event::default().data(format!("Error: {}", e))),
                });

                Ok(Sse::new(sse_stream).into_response())
            }
            Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
        }
    } else {
        // Non-streaming: use agent if resolved
        if let Some(_agent) = state.agent_manager.get_agent(agent_id) {
            payload.model = agent_id.to_string();
        }

        match handle_chat_core(&state, payload).await {
            Ok(response) => Ok(Json(response).into_response()),
            Err(e) => Err(e),
        }
    }
}

/// OpenAI-compatible endpoint: POST /v1/chat/completions
/// Supports X-Agent header to route to a specific agent.
/// Compatible with LangChain, Vercel AI SDK, and any standard OpenAI client.
pub async fn openai_chat(
    state_extractor: State<Arc<AppState>>,
    axum::extract::RawQuery(_raw_query): axum::extract::RawQuery,
    headers: axum::http::HeaderMap,
    payload: Json<ChatRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // OpenAI endpoint also supports X-Agent header via the standard chat handler
    chat(state_extractor, headers, payload).await
}

pub async fn mcp_message(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    tracing::debug!(
        "MCP Request: method={}, id={:?}",
        payload.method,
        payload.id
    );

    match payload.method.as_str() {
        "initialize" => {
            let result = serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {
                        "listChanged": false
                    },
                    "resources": {
                        "subscribe": false,
                        "listChanged": true
                    },
                    "prompts": {
                        "listChanged": false
                    },
                    "skills": {
                        "listChanged": false
                    }
                },
                "serverInfo": {
                    "name": "llama-r-gateway",
                    "version": "0.1.0"
                }
            });

            if let Some(id) = payload.id {
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: Some(result),
                    error: None,
                };
                Json(resp).into_response()
            } else {
                StatusCode::BAD_REQUEST.into_response()
            }
        }
        "notifications/initialized" => {
            tracing::info!("MCP Handshake complete: notifications/initialized received");
            StatusCode::NO_CONTENT.into_response()
        }
        "tools/list" => {
            let agents = state.agent_manager.list_agents();
            let mut tools: Vec<serde_json::Value> = agents
                .iter()
                .filter(|a| !a.id.ends_with("_mcp")) // Filter out project-specific MCP agents
                .map(|a| {
                    let mut desc = format!("Agent: {}. ", a.config.name);
                    if let Some(ref project_id) = a.config.context_project {
                        desc.push_str(&format!("Provee contexto del proyecto '{}'. ", project_id));
                    }
                    if a.config.system_prompt.len() > 100 {
                        desc.push_str(&a.config.system_prompt[..100].replace("\n", " "));
                        desc.push_str("...");
                    } else {
                        desc.push_str(&a.config.system_prompt.replace("\n", " "));
                    }

                    serde_json::json!({
                        "name": a.id,
                        "description": desc,
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

            // Add Unified Project Query Tool
            tools.push(serde_json::json!({
                "name": "agent_query",
                "description": "Consulta un agente de Llama-R. Si se proporciona 'path', busca automáticamente el contexto del proyecto correspondiente.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "La pregunta o instrucción" },
                        "path": { "type": "string", "description": "Ruta absoluta del proyecto actual para autodetección de contexto" },
                        "agent_id": { "type": "string", "description": "ID del agente opcional (ej: 'rust_expert')" }
                    },
                    "required": ["query"]
                }
            }));

            tools.push(serde_json::json!({
                "name": "detect_project_context",
                "description": "Detecta si Llama-R tiene un contexto configurado para la ruta dada.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Ruta absoluta del proyecto" }
                    },
                    "required": ["path"]
                }
            }));

            // Add Skill Tools
            tools.push(serde_json::json!({
                "name": "list_installed_skills",
                "description": "Lista todas las skills de AI instaladas globalmente o en el proyecto.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            }));

            // Add Discovery & Management Tools
            tools.push(serde_json::json!({
                "name": "get_gateway_info",
                "description": "Obtiene información técnica sobre Llama-R (este gateway), arquitectura y reglas de integración.",
                "inputSchema": { "type": "object", "properties": {} }
            }));

            tools.push(serde_json::json!({
                "name": "get_api_spec",
                "description": "Retorna la especificación técnica de todos los endpoints disponibles (REST, OpenAI, gRPC, etc.).",
                "inputSchema": { "type": "object", "properties": {} }
            }));

            tools.push(serde_json::json!({
                "name": "list_contexts",
                "description": "Lista todos los contextos de proyecto generados en este gateway.",
                "inputSchema": { "type": "object", "properties": {} }
            }));

            tools.push(serde_json::json!({
                "name": "create_agent",
                "description": "Crea un nuevo agente de IA persistente en Llama-R.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "ID único (slug)" },
                        "name": { "type": "string", "description": "Nombre humano" },
                        "model": { "type": "string", "description": "Modelo a usar (opcional)" },
                        "system_prompt": { "type": "string", "description": "Prompt del sistema" },
                        "context_project": { "type": "string", "description": "ID del proyecto vinculado (opcional)" }
                    },
                    "required": ["id", "name", "system_prompt"]
                }
            }));

            tools.push(serde_json::json!({
                "name": "create_context",
                "description": "Genera un nuevo contexto AI para un proyecto mediante análisis automático.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project_id": { "type": "string", "description": "ID único para el proyecto" },
                        "project_path": { "type": "string", "description": "Ruta absoluta al código fuente" }
                    },
                    "required": ["project_id", "project_path"]
                }
            }));

            tools.push(serde_json::json!({
                "name": "get_skill_context",
                "description": "Obtiene el contenido completo (instrucciones) de una skill específica.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill_name": {
                            "type": "string",
                            "description": "El nombre/ID de la skill a consultar"
                        }
                    },
                    "required": ["skill_name"]
                }
            }));

            let result = serde_json::json!({
                "tools": tools
            });

            if let Some(id) = payload.id {
                Json(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: Some(result),
                    error: None,
                })
                .into_response()
            } else {
                StatusCode::NO_CONTENT.into_response()
            }
        }
        "resources/list" => {
            let contexts = state.context_store.list_contexts();
            let resources: Vec<serde_json::Value> = contexts.iter().map(|c| {
                serde_json::json!({
                    "uri": format!("mcp://projects/{}/context.md", c.project_id),
                    "name": format!("Contexto del proyecto: {}", c.project_id),
                    "description": format!("Documentación completa y reglas para {}", c.project_id),
                    "mimeType": "text/markdown"
                })
            }).collect();

            let result = serde_json::json!({
                "resources": resources
            });
            if let Some(id) = payload.id {
                Json(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: Some(result),
                    error: None,
                })
                .into_response()
            } else {
                StatusCode::NO_CONTENT.into_response()
            }
        }
        "resources/read" => {
            let uri = payload.params["uri"].as_str().unwrap_or("");
            let result = if uri.starts_with("mcp://projects/") {
                let project_id = uri.split("/").nth(2).unwrap_or("");
                let context = state.context_store.get_context_md(project_id);
                if !context.is_empty() {
                    serde_json::json!({
                        "contents": [
                            {
                                "uri": uri,
                                "mimeType": "text/markdown",
                                "text": context
                            }
                        ]
                    })
                } else {
                    serde_json::json!({ "error": "Context not found" })
                }
            } else {
                serde_json::json!({ "error": "Invalid URI" })
            };

            if let Some(id) = payload.id {
                Json(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: Some(result),
                    error: None,
                })
                .into_response()
            } else {
                StatusCode::NO_CONTENT.into_response()
            }
        }
        "prompts/list" => {
            let result = serde_json::json!({
                "prompts": [
                    {
                        "name": "technical_integration_guide",
                        "description": "Guía completa para integrar la IA de Llama-R en cualquier proyecto.",
                        "arguments": []
                    }
                ]
            });
            if let Some(id) = payload.id {
                Json(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: Some(result),
                    error: None,
                })
                .into_response()
            } else {
                StatusCode::NO_CONTENT.into_response()
            }
        }
        "prompts/get" => {
            let prompt_name = payload.params["name"].as_str().unwrap_or("");
            let result = if prompt_name == "technical_integration_guide" {
                let context = state.context_store.get_context_md("llama-r");
                let text = format!(
                    "Actúa como un arquitecto senior. Esta es la guía técnica de Llama-R:\n\n{}\n\nInstrucciones para otros agentes: Si necesitas integrar IA en un proyecto, usa los endpoints de /api/chat o /v1/chat/completions de este gateway.",
                    if context.is_empty() { "Llama-R: AI Context Hub & Gateway (Rust)." } else { &context }
                );
                serde_json::json!({
                    "description": "Technical Integration Guide",
                    "messages": [
                        {
                            "role": "user",
                            "content": {
                                "type": "text",
                                "text": text
                            }
                        }
                    ]
                })
            } else {
                serde_json::json!({ "error": "Prompt not found" })
            };

            if let Some(id) = payload.id {
                Json(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: Some(result),
                    error: None,
                })
                .into_response()
            } else {
                StatusCode::NO_CONTENT.into_response()
            }
        }
        "tools/call" => {
            let tool_name = payload.params["name"].as_str().unwrap_or("");

            if tool_name == "list_installed_skills" {
                let skills = state.skill_manager.list_skills();
                let result = serde_json::json!({
                    "content": [
                        {
                            "type": "text",
                            "text": format!("Skills instaladas:\n{}", skills.iter().map(|s| format!("- {}: {}", s.id, s.metadata.description)).collect::<Vec<_>>().join("\n"))
                        }
                    ]
                });
                return if let Some(id) = payload.id {
                    Json(JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(result),
                        error: None,
                    })
                    .into_response()
                } else {
                    StatusCode::NO_CONTENT.into_response()
                };
            }

            if tool_name == "get_gateway_info" {
                let context = state.context_store.get_context_md("llama-r");
                let result = serde_json::json!({
                    "content": [
                        {
                            "type": "text",
                            "text": if context.is_empty() { "Llama-R: High-Performance AI Gateway." } else { &context }
                        }
                    ]
                });
                return if let Some(id) = payload.id {
                    Json(JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(result),
                        error: None,
                    })
                    .into_response()
                } else {
                    StatusCode::NO_CONTENT.into_response()
                };
            }

            if tool_name == "get_api_spec" {
                let spec = serde_json::json!({
                    "rest": {
                        "chat": "POST /api/chat - SSE/JSON streaming",
                        "openai": "POST /v1/chat/completions - Compatible con header X-Agent",
                        "agents": "GET/POST /api/agents, GET/PUT/DELETE /api/agents/:id",
                        "contexts": "GET/POST /api/contexts, POST /api/contexts/:id/analyze"
                    },
                    "grpc": "LlamaGateway service en puerto 50051",
                    "mcp": "Endpoint JSON-RPC en /mcp",
                    "cli": "Comandos: init-agent, init-mcp, analyze, run"
                });
                let result = serde_json::json!({
                    "content": [{ "type": "text", "text": serde_json::to_string_pretty(&spec).unwrap() }]
                });
                return if let Some(id) = payload.id {
                    Json(JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(result),
                        error: None,
                    })
                    .into_response()
                } else {
                    StatusCode::NO_CONTENT.into_response()
                };
            }

            if tool_name == "list_contexts" {
                let contexts = state.context_store.list_contexts();
                let summary = contexts
                    .iter()
                    .map(|c| format!("- {}: {}", c.project_id, c.path))
                    .collect::<Vec<_>>()
                    .join("\n");
                let result =
                    serde_json::json!({ "content": [{ "type": "text", "text": summary }] });
                return if let Some(id) = payload.id {
                    Json(JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(result),
                        error: None,
                    })
                    .into_response()
                } else {
                    StatusCode::NO_CONTENT.into_response()
                };
            }

            if tool_name == "create_agent" {
                let args = &payload.params["arguments"];
                let agent_id = args["id"].as_str().unwrap_or("");
                let name = args["name"].as_str().unwrap_or("");
                let system_prompt = args["system_prompt"].as_str().unwrap_or("");

                if agent_id.is_empty() || name.is_empty() || system_prompt.is_empty() {
                    let err = "Missing required arguments: 'id', 'name', and 'system_prompt' are required.";
                    tracing::error!("create_agent failed: {}", err);
                    let result = serde_json::json!({ "content": [{ "type": "text", "text": err }], "isError": true });
                    return if let Some(id) = payload.id {
                        Json(JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id,
                            result: Some(result),
                            error: None,
                        })
                        .into_response()
                    } else {
                        StatusCode::NO_CONTENT.into_response()
                    };
                }

                let model = args["model"].as_str().unwrap_or("");
                let context_project = args["context_project"].as_str();

                // If the agent has a project context, put it in the project's agents folder
                let target_agent_id = args["agent_id"].as_str().unwrap_or(agent_id);
                let project_id = args["project_id"].as_str().unwrap_or("");

                let toml_content = if let Some(t) = args["toml"].as_str() {
                    t.to_string()
                } else {
                    format!(
                        "name = \"{}\"\nmodel = \"{}\"\nsystem_prompt = \"\"\"\n{}\n\"\"\"{}",
                        name,
                        model,
                        system_prompt,
                        context_project
                            .map(|p| format!("\ncontext_project = \"{}\"", p))
                            .unwrap_or_default()
                    )
                };

                let path = if !project_id.is_empty() {
                    let dir = crate::core::paths::get_project_agents_dir(project_id);
                    let _ = std::fs::create_dir_all(&dir);
                    dir.join(format!("{}.toml", target_agent_id))
                } else {
                    crate::core::paths::get_agents_dir().join(format!("{}.toml", target_agent_id))
                };

                let result = match std::fs::write(&path, toml_content) {
                    Ok(_) => {
                        let _ = state.agent_manager.load_agents();
                        serde_json::json!({ "content": [{ "type": "text", "text": format!("Agent '{}' created and loaded at {}.", target_agent_id, path.display()) }] })
                    }
                    Err(e) => {
                        serde_json::json!({ "content": [{ "type": "text", "text": e.to_string() }], "isError": true })
                    }
                };

                return if let Some(id) = payload.id {
                    Json(JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(result),
                        error: None,
                    })
                    .into_response()
                } else {
                    StatusCode::NO_CONTENT.into_response()
                };
            }

            if tool_name == "create_context" {
                let args = &payload.params["arguments"];
                let project_id = args["project_id"].as_str().unwrap_or("");
                let project_path = args["project_path"].as_str().unwrap_or("");

                if project_id.is_empty() || project_path.is_empty() {
                    let err =
                        "Missing required arguments: 'project_id' and 'project_path' are required.";
                    tracing::error!("create_context failed: {}", err);
                    let result = serde_json::json!({ "content": [{ "type": "text", "text": err }], "isError": true });
                    return if let Some(id) = payload.id {
                        Json(JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id,
                            result: Some(result),
                            error: None,
                        })
                        .into_response()
                    } else {
                        StatusCode::NO_CONTENT.into_response()
                    };
                }

                // Check if context already exists to prevent accidental regeneration
                if state.context_store.get_context(project_id).is_some() {
                    let err = format!(
                        "Context for '{}' already exists. Use re-analyze to refresh it.",
                        project_id
                    );
                    let result = serde_json::json!({ "content": [{ "type": "text", "text": err }], "isError": true });
                    return if let Some(id) = payload.id {
                        Json(JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id,
                            result: Some(result),
                            error: None,
                        })
                        .into_response()
                    } else {
                        StatusCode::NO_CONTENT.into_response()
                    };
                }

                let provider = state.provider.clone();
                let responder = move |prompt: String| {
                    let provider = provider.clone();
                    Box::pin(async move {
                        use crate::domain::models::ChatRequest;
                        let chat_req = ChatRequest {
                            model: std::env::var("DEFAULT_MODEL")
                                .unwrap_or_else(|_| "llama3".to_string()),
                            messages: vec![crate::domain::models::ChatMessage {
                                role: "user".to_string(),
                                content: prompt,
                            }],
                            stream: false,
                        };
                        provider
                            .chat(chat_req)
                            .await
                            .map(|r| r.message.content)
                            .map_err(|e| e.to_string())
                    })
                        as std::pin::Pin<
                            Box<dyn std::future::Future<Output = Result<String, String>> + Send>,
                        >
                };

                let analyzer =
                    crate::context::analyzer::ProjectAnalyzer::new(state.skill_manager.clone());
                let store = state.context_store.clone();
                let project_id_owned = project_id.to_string();

                match analyzer.analyze(project_id, project_path, responder).await {
                    Ok(ctx) => {
                        let _ = store.save_context(ctx);
                        let result = serde_json::json!({
                            "content": [{ "type": "text", "text": format!("Context for '{}' generated and saved.", project_id_owned) }]
                        });
                        return if let Some(id) = payload.id {
                            Json(JsonRpcResponse {
                                jsonrpc: "2.0".to_string(),
                                id,
                                result: Some(result),
                                error: None,
                            })
                            .into_response()
                        } else {
                            StatusCode::NO_CONTENT.into_response()
                        };
                    }
                    Err(e) => {
                        let result = serde_json::json!({ "content": [{ "type": "text", "text": e }], "isError": true });
                        return if let Some(id) = payload.id {
                            Json(JsonRpcResponse {
                                jsonrpc: "2.0".to_string(),
                                id,
                                result: Some(result),
                                error: None,
                            })
                            .into_response()
                        } else {
                            StatusCode::NO_CONTENT.into_response()
                        };
                    }
                }
            }

            if tool_name == "get_skill_context" {
                let skill_name = payload.params["arguments"]["skill_name"]
                    .as_str()
                    .unwrap_or("");
                let result = if let Some(skill) = state.skill_manager.get_skill(skill_name) {
                    serde_json::json!({
                        "content": [
                            {
                                "type": "text",
                                "text": skill.content
                            }
                        ]
                    })
                } else {
                    serde_json::json!({
                        "content": [
                            {
                                "type": "text",
                                "text": format!("Skill '{}' no encontrada.", skill_name)
                            }
                        ],
                        "isError": true
                    })
                };
                return if let Some(id) = payload.id {
                    Json(JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(result),
                        error: None,
                    })
                    .into_response()
                } else {
                    StatusCode::NO_CONTENT.into_response()
                };
            }

            if tool_name == "agent_query" {
                let args = &payload.params["arguments"];
                let query = args["query"].as_str().unwrap_or("");
                let path = args["path"].as_str().unwrap_or("");
                let mut agent_id = args["agent_id"].as_str().unwrap_or("").to_string();

                if query.is_empty() {
                    let err = "Missing required argument: 'query' is required.";
                    let result = serde_json::json!({ "content": [{ "type": "text", "text": err }], "isError": true });
                    return if let Some(id) = payload.id {
                        Json(JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id,
                            result: Some(result),
                            error: None,
                        })
                        .into_response()
                    } else {
                        StatusCode::NO_CONTENT.into_response()
                    };
                }

                // Auto-detect context by path if agent_id is not explicitly provided
                if agent_id.is_empty() && !path.is_empty() {
                    let contexts = state.context_store.list_contexts();
                    // Find the context whose path is a prefix of the provided path (longest match first)
                    let mut best_match: Option<crate::context::store::ProjectContext> = None;
                    for ctx in contexts {
                        if path.starts_with(&ctx.path) {
                            if best_match
                                .as_ref()
                                .map_or(true, |m| ctx.path.len() > m.path.len())
                            {
                                best_match = Some(ctx);
                            }
                        }
                    }

                    if let Some(ctx) = best_match {
                        agent_id = format!("{}_mcp", ctx.project_id);
                        tracing::info!("Auto-detected context: {} for path {}", agent_id, path);
                    }
                }

                if agent_id.is_empty() {
                    agent_id = "llama-r_mcp".to_string(); // Fallback if no path/match
                }

                let chat_req = crate::domain::models::ChatRequest {
                    model: agent_id,
                    messages: vec![crate::domain::models::ChatMessage {
                        role: "user".to_string(),
                        content: query.to_string(),
                    }],
                    stream: false,
                };

                match handle_chat_core(&state, chat_req).await {
                    Ok(resp) => {
                        let result = serde_json::json!({ "content": [{ "type": "text", "text": resp.message.content }] });
                        return if let Some(id) = payload.id {
                            Json(JsonRpcResponse {
                                jsonrpc: "2.0".to_string(),
                                id,
                                result: Some(result),
                                error: None,
                            })
                            .into_response()
                        } else {
                            StatusCode::NO_CONTENT.into_response()
                        };
                    }
                    Err((_code, msg)) => {
                        let result = serde_json::json!({ "content": [{ "type": "text", "text": msg }], "isError": true });
                        return if let Some(id) = payload.id {
                            Json(JsonRpcResponse {
                                jsonrpc: "2.0".to_string(),
                                id,
                                result: Some(result),
                                error: None,
                            })
                            .into_response()
                        } else {
                            StatusCode::NO_CONTENT.into_response()
                        };
                    }
                }
            }

            if tool_name == "detect_project_context" {
                let path = payload.params["arguments"]["path"].as_str().unwrap_or("");
                let contexts = state.context_store.list_contexts();
                let mut best_match: Option<String> = None;
                let mut match_len = 0;

                for ctx in contexts {
                    if path.starts_with(&ctx.path) && ctx.path.len() > match_len {
                        match_len = ctx.path.len();
                        best_match = Some(ctx.project_id.clone());
                    }
                }

                let result = if let Some(id) = best_match {
                    serde_json::json!({ "content": [{ "type": "text", "text": format!("Project context found: {}", id) }], "project_id": id })
                } else {
                    serde_json::json!({ "content": [{ "type": "text", "text": "No specialized context found for this path." }], "project_id": null })
                };

                return if let Some(id) = payload.id {
                    Json(JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(result),
                        error: None,
                    })
                    .into_response()
                } else {
                    StatusCode::NO_CONTENT.into_response()
                };
            }

            let query = payload.params["arguments"]["query"].as_str().unwrap_or("");

            tracing::info!("MCP Tool Call: tool={}, query={}", tool_name, query);

            let chat_req = ChatRequest {
                model: tool_name.to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: query.to_string(),
                }],
                stream: false,
            };

            match handle_chat_core(&state, chat_req).await {
                Ok(resp) => {
                    let result = serde_json::json!({
                        "content": [
                            {
                                "type": "text",
                                "text": resp.message.content
                            }
                        ]
                    });
                    if let Some(id) = payload.id {
                        Json(JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id,
                            result: Some(result),
                            error: None,
                        })
                        .into_response()
                    } else {
                        StatusCode::NO_CONTENT.into_response()
                    }
                }
                Err((_code, msg)) => {
                    if let Some(id) = payload.id {
                        let err_resp = JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id,
                            result: None,
                            error: Some(serde_json::json!({
                                "code": _code.as_u16() as i32,
                                "message": msg
                            })),
                        };
                        Json(err_resp).into_response()
                    } else {
                        StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    }
                }
            }
        }
        _ => {
            if let Some(id) = payload.id {
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: None,
                    error: Some(serde_json::json!({
                        "code": -32601,
                        "message": format!("Method not found: {}", payload.method)
                    })),
                };
                Json(resp).into_response()
            } else {
                StatusCode::NO_CONTENT.into_response()
            }
        }
    }
}




