//! Task 7: a model that cannot call tools must not be handed MCP tools.
//!
//! The capability catalog says which `(provider, model)` pairs can actually call
//! tools. `RigAgentEngine::prepare` is where tools are discovered from MCP, so it
//! is the one place that can refuse to attach them. These tests drive the real
//! `prepare` with a fake registry that records whether discovery even happened,
//! and capture the warning an operator sees when tools vanish.

use async_trait::async_trait;
use llama_r::adapters::rig_engine::RigAgentEngine;
use llama_r::config::{Config, ProviderKind};
use llama_r::domain::agent::{Agent, AgentConfig, RagWritePolicy};
use llama_r::domain::scope::{AgentScope, ToolsAllow};
use llama_r::ports::engine::AgentRunRequest;
use llama_r::ports::mcp::{
    filter_tools_for_scope, McpCallRequest, McpCallResult, McpClient, McpServerRegistry, McpToolDef,
};
use llama_r::providers::capabilities::{capabilities_for, ModelCapabilities};
use std::collections::HashSet;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A client that is never actually invoked; `prepare` only needs one to exist.
struct FakeClient;

#[async_trait]
impl McpClient for FakeClient {
    async fn list_tools(&self, _server_id: &str) -> Result<Vec<McpToolDef>, String> {
        Ok(Vec::new())
    }
    async fn call_tool(&self, _req: McpCallRequest) -> Result<McpCallResult, String> {
        unreachable!("prepare never invokes a tool")
    }
    async fn health(&self, _server_id: &str) -> Result<(), String> {
        Ok(())
    }
}

/// A registry that would hand `prepare` one in-scope tool, and counts how many
/// times discovery was attempted. The counter is what proves tools were skipped
/// rather than discovered and then dropped.
struct FakeRegistry {
    tool: McpToolDef,
    discover_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl McpServerRegistry for FakeRegistry {
    async fn list_server_ids(&self) -> Vec<String> {
        vec![self.tool.server_id.clone()]
    }

    async fn client_for(&self, _server_id: &str) -> Result<Arc<dyn McpClient>, String> {
        Ok(Arc::new(FakeClient))
    }

    async fn discover_scoped_tools(&self, scope: &AgentScope) -> Result<Vec<McpToolDef>, String> {
        self.discover_calls.fetch_add(1, Ordering::SeqCst);
        Ok(filter_tools_for_scope(vec![self.tool.clone()], scope))
    }
}

fn tool_def() -> McpToolDef {
    McpToolDef {
        server_id: "fake".into(),
        name: "do_thing".into(),
        description: "does a thing".into(),
        input_schema: serde_json::Value::Null,
    }
}

fn scope() -> AgentScope {
    AgentScope {
        agent_id: "reviewer".into(),
        project_id: Some("proj".into()),
        mcp_sources: HashSet::from(["fake".into()]),
        tools_allow: ToolsAllow::AllFromSources,
        local_tools_allow: ToolsAllow::DenyAll,
        rag_sources: HashSet::new(),
        rag_write: RagWritePolicy::None,
        max_tool_calls: 8,
        max_iterations: 6,
        timeout_secs: 90,
    }
}

fn request(capabilities: ModelCapabilities, model: &str) -> AgentRunRequest {
    AgentRunRequest {
        agent: Agent {
            id: "reviewer".into(),
            project_id: Some("proj".into()),
            config: AgentConfig::default(),
        },
        scope: scope(),
        model: model.to_string(),
        provider: ProviderKind::Ollama,
        capabilities,
        system_prompt: "base prompt".into(),
        user_message: "hola".into(),
        history: Vec::new(),
        conversation_id: None,
    }
}

/// A minimal config for building an engine in tests. Holds no keys.
fn test_config() -> Config {
    Config {
        port: 3000,
        provider: ProviderKind::Ollama,
        ollama_url: "http://localhost:11434".to_string(),
        default_model: "llama3.2".to_string(),
        embedding_model: "nomic-embed-text".to_string(),
        embedding_dimensions: 768,
        rag_enabled: true,
        history_retention_days: 90,
        openrouter_api_key: None,
        opencode_api_key: None,
        opencode_url: "http://127.0.0.1:4096".to_string(),
        openai_base_url: None,
    }
}

/// Build an engine with a fake registry and return it together with the counter
/// that records whether tool discovery was attempted.
fn engine() -> (RigAgentEngine, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let registry = FakeRegistry {
        tool: tool_def(),
        discover_calls: Arc::clone(&calls),
    };
    let engine = RigAgentEngine::new(
        "http://localhost:11434",
        Arc::new(registry),
        None,
        test_config(),
    );
    (engine, calls)
}

fn prepare_blocking(engine: &RigAgentEngine, req: &AgentRunRequest) -> (Vec<String>, String) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (tools, system) = rt.block_on(async { engine.prepare(req).await.unwrap() });
    (tools.into_iter().map(|t| t.def.name).collect(), system)
}

/// The central behaviour: a model whose catalog says `tool_calling: false`
/// gets no tools at all, even though the registry would have supplied one.
#[test]
fn a_model_without_tool_calling_loses_its_tools() {
    let (engine, calls) = engine();
    let caps = capabilities_for(ProviderKind::OpenCode, "opencode-model");
    assert!(!caps.tool_calling, "fixture: OpenCode cannot call tools");
    let req = request(caps, "opencode-model");

    let (tools, system) = prepare_blocking(&engine, &req);

    assert!(
        tools.is_empty(),
        "no tools may reach a non-tool model: {tools:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "discovery must be skipped, not run and filtered away"
    );
    assert_eq!(
        system, "base prompt",
        "the system prompt is still built on this path"
    );
}

/// The change is a gate, not a removal: a capable model still gets its tools,
/// through the exact same discovery path as before.
#[test]
fn a_tool_calling_model_keeps_its_tools() {
    let (engine, calls) = engine();
    let caps = capabilities_for(ProviderKind::Ollama, "llama3.2");
    assert!(caps.tool_calling, "fixture: Ollama can call tools");
    let req = request(caps, "llama3.2");

    let (tools, system) = prepare_blocking(&engine, &req);

    assert_eq!(tools, vec!["do_thing".to_string()]);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the capable model still discovers tools exactly once"
    );
    assert_eq!(system, "base prompt");
}

/// A shared buffer that can be used as a tracing writer and read back as text.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The warning has to name the agent (qualified id), the model and the missing
/// capability, so an operator can see *why* the tools disappeared. Captured
/// from the real `prepare` call, not from a hand-built message.
#[test]
fn the_warning_names_agent_model_and_capability() {
    let (engine, _calls) = engine();
    let caps = capabilities_for(ProviderKind::OpenCode, "opencode-model");
    let req = request(caps, "opencode-model");

    let buf = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .without_time()
        .with_target(false)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        prepare_blocking(&engine, &req);
    });

    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("proj/reviewer"),
        "agent missing from warning: {log:?}"
    );
    assert!(
        log.contains("opencode-model"),
        "model missing from warning: {log:?}"
    );
    assert!(
        log.contains("tool_calling"),
        "capability missing from warning: {log:?}"
    );
}
