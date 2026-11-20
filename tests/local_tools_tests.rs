//! Local-tool registry, scoped bridge and executors. Discovery, scope, budget
//! and the capability gate (Task 3) live alongside the `builtin:skill` and
//! `rag_query` executors (Task 4).

use async_trait::async_trait;
use llama_r::adapters::rig_engine::builder::build_tool_bridges;
use llama_r::adapters::rig_engine::limits::ToolCallBudget;
use llama_r::adapters::rig_engine::local_tools::{
    DelegateBounds, LocalToolBridge, LocalToolDeps, LocalToolKind, LocalToolRegistry,
    BUILTIN_DELEGATE_ID, BUILTIN_RAG_QUERY_ID, BUILTIN_SKILL_ID,
};
use llama_r::adapters::rig_engine::operations::OperationContext;
use llama_r::adapters::rig_engine::{EngineTarget, RigAgentEngine, ScopedTool};
use llama_r::config::{Config, ProviderKind};
use llama_r::domain::agent::{Agent, AgentConfig, RagWritePolicy};
use llama_r::domain::policy::{ApprovalPolicy, ApprovalRule, PermissionPolicy};
use llama_r::domain::scope::{AgentScope, ToolRef, ToolsAllow};
use llama_r::error::AppError;
use llama_r::ports::delegation::{DelegateRequest, DelegateResult, Delegator, ToolBudget};
use llama_r::ports::engine::{AgentRunEvent, AgentRunRequest};
use llama_r::ports::execution::ExecutionStore;
use llama_r::ports::mcp::{
    filter_tools_for_scope, McpCallRequest, McpCallResult, McpClient, McpServerRegistry, McpToolDef,
};
use llama_r::ports::rag::{RagChunk, RagReplaceBatch, RagStore, RagUpsert};
use llama_r::providers::capabilities::{capabilities_for, ModelCapabilities};
use llama_r::services::approval::ApprovalGate;
use llama_r::services::redaction::args_hash;
use llama_r::services::skill_manager::SkillManager;
use rig_core::tool::ToolDyn;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

/// Serialises the tests that move `LLAMA_R_DIR`, which is process-wide.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ---------------------------------------------------------------------------
// Fakes shared by the prepare/bridge tests
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeClient {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl McpClient for FakeClient {
    async fn list_tools(&self, _server_id: &str) -> Result<Vec<McpToolDef>, String> {
        Ok(Vec::new())
    }
    async fn call_tool(&self, _req: McpCallRequest) -> Result<McpCallResult, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(McpCallResult {
            content: Value::String("mcp ok".into()),
            is_error: false,
        })
    }
    async fn health(&self, _server_id: &str) -> Result<(), String> {
        Ok(())
    }
}

struct FakeRegistry {
    def: McpToolDef,
    discover_calls: Arc<AtomicUsize>,
    client_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl McpServerRegistry for FakeRegistry {
    async fn list_server_ids(&self) -> Vec<String> {
        vec![self.def.server_id.clone()]
    }
    async fn client_for(&self, _server_id: &str) -> Result<Arc<dyn McpClient>, String> {
        Ok(Arc::new(FakeClient {
            calls: self.client_calls.clone(),
        }))
    }
    async fn discover_scoped_tools(&self, scope: &AgentScope) -> Result<Vec<McpToolDef>, String> {
        self.discover_calls.fetch_add(1, Ordering::SeqCst);
        Ok(filter_tools_for_scope(vec![self.def.clone()], scope))
    }
}

fn mcp_def() -> McpToolDef {
    McpToolDef {
        server_id: "fake".into(),
        name: "do_thing".into(),
        description: "does a thing".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }
}

fn scope(local: ToolsAllow) -> AgentScope {
    AgentScope {
        agent_id: "reviewer".into(),
        project_id: Some("proj".into()),
        mcp_sources: HashSet::from(["fake".into()]),
        tools_allow: ToolsAllow::DenyAll,
        local_tools_allow: local,
        rag_sources: HashSet::new(),
        rag_write: RagWritePolicy::None,
        max_tool_calls: 8,
        max_iterations: 6,
        timeout_secs: 90,
        permission: llama_r::domain::policy::PermissionPolicy::default(),
    }
}

fn request(capabilities: ModelCapabilities, model: &str, local: ToolsAllow) -> AgentRunRequest {
    AgentRunRequest {
        agent: Agent {
            id: "reviewer".into(),
            project_id: Some("proj".into()),
            config: AgentConfig::default(),
        },
        scope: scope(local),
        model: model.to_string(),
        provider: ProviderKind::Ollama,
        capabilities,
        system_prompt: "base prompt".into(),
        user_message: "hola".into(),
        history: Vec::new(),
        conversation_id: None,
        execution_id: uuid::Uuid::new_v4(),
        shared_budget: None,
    }
}

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

fn engine(
    registry: FakeRegistry,
    local_tools: LocalToolRegistry,
) -> (RigAgentEngine, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let discover = registry.discover_calls.clone();
    let client_calls = registry.client_calls.clone();
    let engine = RigAgentEngine::from_target(
        EngineTarget::Ollama {
            url: "http://localhost:11434".into(),
        },
        Arc::new(registry),
        None,
        test_config(),
        Arc::new(SkillManager::new()),
        Arc::new(local_tools),
        Arc::new(ApprovalGate::default()),
    );
    (engine, discover, client_calls)
}

fn prepare_blocking(engine: &RigAgentEngine, req: &AgentRunRequest) -> Vec<ScopedTool> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async { engine.prepare(req).await.unwrap() }).0
}

// ---------------------------------------------------------------------------
// Executor wiring (Task 4)
// ---------------------------------------------------------------------------

/// Write a minimal but valid `SKILL.md`. `content` is the whole file, so a test
/// can assert the executor returned the skill verbatim.
fn write_skill(dir: &std::path::Path, id: &str, body: &str) -> String {
    let skill_dir = dir.join(id);
    std::fs::create_dir_all(&skill_dir).unwrap();
    let content = format!("---\nname: {id}\ndescription: skill {id}\n---\n\n{body}\n");
    std::fs::write(skill_dir.join("SKILL.md"), &content).unwrap();
    content
}

fn skill_deps(skills_root: &std::path::Path) -> LocalToolDeps {
    let manager = SkillManager::with_paths(vec![skills_root.to_path_buf()]);
    manager.scan_and_load();
    LocalToolDeps {
        skill_manager: Arc::new(manager),
        rag: None,
        gate: Arc::new(ApprovalGate::default()),
        auto_decider: None,
        observability: None,
        delegator: None,
        delegate_bounds: None,
        event_tx: None,
        operations: llama_r::adapters::rig_engine::operations::OperationContext::disabled(),
    }
}

fn empty_deps() -> LocalToolDeps {
    LocalToolDeps {
        skill_manager: Arc::new(SkillManager::with_paths(Vec::new())),
        rag: None,
        gate: Arc::new(ApprovalGate::default()),
        auto_decider: None,
        observability: None,
        delegator: None,
        delegate_bounds: None,
        event_tx: None,
        operations: llama_r::adapters::rig_engine::operations::OperationContext::disabled(),
    }
}

/// A `RagStore` that records every query and returns one chunk per source the
/// *scope* allows — the same isolation contract the real store enforces. It
/// deliberately holds data for a second, secret source that it never returns
/// unless the scope names it, so a test can prove the executor cannot bypass
/// scope even when the tool manifest carries a different `source_id`.
#[derive(Default)]
struct ScopingFakeRag {
    calls: Arc<AtomicUsize>,
    seen_sources: Arc<Mutex<Vec<String>>>,
    seen_top_k: Arc<Mutex<Option<usize>>>,
    seen_query: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl RagStore for ScopingFakeRag {
    async fn query_scoped(
        &self,
        scope: &AgentScope,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<RagChunk>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.seen_sources.lock().unwrap() = scope.rag_sources.iter().cloned().collect();
        *self.seen_top_k.lock().unwrap() = Some(top_k);
        *self.seen_query.lock().unwrap() = Some(query.to_string());

        // Data for two collections; only the ones the scope allows are returned.
        let all = [
            ("allowed", "content of the allowed source"),
            ("secret", "CONTENT OF THE SECRET SOURCE"),
        ];
        Ok(all
            .into_iter()
            .filter(|(source, _)| scope.rag_sources.contains(*source))
            .map(|(source, text)| RagChunk {
                id: format!("{source}#chunk-0"),
                source_id: source.to_string(),
                text: text.to_string(),
                score: 0.75,
                metadata: serde_json::json!({"origin": "fake"}),
            })
            .take(top_k)
            .collect())
    }

    async fn upsert_scoped(
        &self,
        _scope: &AgentScope,
        _docs: Vec<RagUpsert>,
    ) -> Result<usize, String> {
        Ok(0)
    }
    async fn replace_batch_scoped(
        &self,
        _scope: &AgentScope,
        _source_id: &str,
        _batch: RagReplaceBatch,
    ) -> Result<usize, String> {
        Ok(0)
    }
    async fn replace_scoped(
        &self,
        _scope: &AgentScope,
        _source_id: &str,
        _id_prefix: &str,
        _docs: Vec<RagUpsert>,
    ) -> Result<usize, String> {
        Ok(0)
    }
    async fn delete_document_scoped(
        &self,
        _scope: &AgentScope,
        _source_id: &str,
        _id_prefix: &str,
    ) -> Result<usize, String> {
        Ok(0)
    }
    async fn delete_collection_scoped(
        &self,
        _scope: &AgentScope,
        _source_id: &str,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn delete_collection(&self, _source_id: &str) -> Result<(), String> {
        Ok(())
    }
}

fn rag_deps(rag: Arc<ScopingFakeRag>) -> LocalToolDeps {
    LocalToolDeps {
        skill_manager: Arc::new(SkillManager::with_paths(Vec::new())),
        rag: Some(rag),
        gate: Arc::new(ApprovalGate::default()),
        auto_decider: None,
        observability: None,
        delegator: None,
        delegate_bounds: None,
        event_tx: None,
        operations: llama_r::adapters::rig_engine::operations::OperationContext::disabled(),
    }
}

fn builtin(id: &str) -> llama_r::adapters::rig_engine::local_tools::LocalTool {
    LocalToolRegistry::builtins()
        .tools()
        .iter()
        .find(|t| t.id == id)
        .unwrap()
        .clone()
}

// ---------------------------------------------------------------------------
// Step 1 tests
// ---------------------------------------------------------------------------

#[test]
fn registry_always_has_the_three_builtins() {
    let _guard = env_lock().lock().unwrap();
    let dir = TempDir::new().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());

    let registry = LocalToolRegistry::load().expect("an empty tools dir is valid");
    let tools = registry.tools();
    assert_eq!(tools.len(), 3, "exactly the three built-ins, no overlay");

    let skill = tools.iter().find(|t| t.id == "llama_r__skill").unwrap();
    assert!(matches!(skill.kind, LocalToolKind::BuiltinSkill));
    assert_eq!(
        skill.input_schema["properties"]["name"]["type"],
        Value::String("string".into())
    );

    let rag = tools.iter().find(|t| t.id == "llama_r__rag_query").unwrap();
    assert!(matches!(
        rag.kind,
        LocalToolKind::RagQuery { source_id: None }
    ));
    assert_eq!(
        rag.input_schema["properties"]["top_k"]["type"],
        Value::String("integer".into())
    );
}

#[test]
fn a_tools_toml_manifest_is_loaded_and_malformed_names_the_path() {
    let _guard = env_lock().lock().unwrap();
    let dir = TempDir::new().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());
    let tools_dir = dir.path().join("tools");
    std::fs::create_dir_all(&tools_dir).unwrap();

    std::fs::write(
        tools_dir.join("policies.toml"),
        "description = \"Search policies\"\ntype = \"rag_query\"\n\
         input_schema = { type = \"object\", properties = { query = { type = \"string\" } }, required = [\"query\"] }\n\
         source_id = \"politicas\"\n",
    )
    .unwrap();

    let registry = LocalToolRegistry::load().expect("a well-formed manifest loads");
    assert_eq!(registry.tools().len(), 4);
    let custom = registry
        .tools()
        .iter()
        .find(|t| t.id == "policies")
        .unwrap();
    assert_eq!(custom.description, "Search policies");
    assert!(
        matches!(&custom.kind, LocalToolKind::RagQuery { source_id: Some(id) } if id == "politicas"),
        "source_id comes from the manifest: {:?}",
        custom.kind
    );

    let bad = tools_dir.join("bad.toml");
    std::fs::write(&bad, "type = [not valid toml").unwrap();
    let err = LocalToolRegistry::load().expect_err("a malformed manifest is an error");
    let message = err.to_string();
    assert!(matches!(err, AppError::Config(_)));
    assert!(
        message.contains("bad.toml"),
        "the path must be named: {message}"
    );
    assert_eq!(
        LocalToolRegistry::builtins().tools().len(),
        3,
        "the non-fatal fallback is the built-ins only"
    );
}

#[test]
fn a_local_tool_outside_tools_local_is_not_offered() {
    let local = LocalToolRegistry::builtins();
    let registry = FakeRegistry {
        def: mcp_def(),
        discover_calls: Arc::new(AtomicUsize::new(0)),
        client_calls: Arc::new(AtomicUsize::new(0)),
    };
    let (engine, _, _) = engine(registry, local);
    let caps = capabilities_for(ProviderKind::Ollama, "llama3.2");
    assert!(caps.tool_calling);

    let denied = request(caps.clone(), "llama3.2", ToolsAllow::DenyAll);
    assert!(
        prepare_blocking(&engine, &denied).is_empty(),
        "deny-all means no local tool is offered"
    );

    let allow_one = ToolsAllow::Allowlist(HashSet::from([ToolRef::parse("llama_r__skill")]));
    let one = prepare_blocking(&engine, &request(caps.clone(), "llama3.2", allow_one));
    assert_eq!(one.len(), 1);
    match &one[0] {
        ScopedTool::Local(tool) => assert_eq!(tool.id, "llama_r__skill"),
        ScopedTool::Mcp(_) => panic!("an empty MCP scope must contribute no tools"),
    }

    let all = prepare_blocking(
        &engine,
        &request(caps, "llama3.2", ToolsAllow::AllFromSources),
    );
    assert_eq!(
        all.len(),
        3,
        "all built-ins when all local tools are allowed"
    );
}

#[test]
fn the_tool_calling_gate_skips_local_tools_too() {
    let registry = FakeRegistry {
        def: mcp_def(),
        discover_calls: Arc::new(AtomicUsize::new(0)),
        client_calls: Arc::new(AtomicUsize::new(0)),
    };
    let (engine, discover, _) = engine(registry, LocalToolRegistry::builtins());

    // An agent that allows every local tool still sees none on a model that
    // cannot call tools: the gate sits above both producers.
    let caps = capabilities_for(ProviderKind::OpenCode, "opencode-model");
    assert!(!caps.tool_calling);
    let tools = prepare_blocking(
        &engine,
        &request(caps, "opencode-model", ToolsAllow::AllFromSources),
    );
    assert!(
        tools.is_empty(),
        "local tools must respect the capability gate"
    );
    assert_eq!(
        discover.load(Ordering::SeqCst),
        0,
        "MCP discovery is skipped on the same gate"
    );
}

#[tokio::test]
async fn local_bridge_rechecks_scope_and_shares_the_budget() {
    let tool = LocalToolRegistry::builtins()
        .tools()
        .iter()
        .find(|t| t.id == "llama_r__skill")
        .unwrap()
        .clone();
    let budget = Arc::new(AsyncMutex::new(ToolCallBudget::new(1)));

    let denied = LocalToolBridge::new(
        tool.clone(),
        scope(ToolsAllow::DenyAll),
        budget.clone(),
        empty_deps(),
    );
    let err = denied
        .invoke_json(serde_json::json!({"name": "x"}))
        .await
        .unwrap_err();
    assert!(err.contains("denied by agent scope"), "{err}");
    assert_eq!(
        budget.lock().await.used(),
        0,
        "denied calls are not charged"
    );

    let allowed = LocalToolBridge::new(
        tool.clone(),
        scope(ToolsAllow::AllFromSources),
        budget.clone(),
        empty_deps(),
    );
    assert_eq!(allowed.name(), "llama_r__skill");
    assert!(!allowed.description().is_empty());
    assert_eq!(allowed.parameters()["properties"]["name"]["type"], "string");

    // An unknown skill is a named refusal (the skill id appears), and it still
    // charges the budget because the charge happens before dispatch.
    let err = allowed
        .invoke_json(serde_json::json!({"name": "x"}))
        .await
        .unwrap_err();
    assert!(err.contains('x'), "unknown skill must be named: {err}");
    assert_eq!(budget.lock().await.used(), 1);
    let err = allowed
        .invoke_json(serde_json::json!({"name": "x"}))
        .await
        .unwrap_err();
    assert!(
        err.to_ascii_lowercase().contains("max_tool_calls")
            || err.to_ascii_lowercase().contains("exceeded"),
        "the second call hits the exhausted budget: {err}"
    );
}

#[tokio::test]
async fn build_agent_attaches_both_bridge_kinds_with_one_budget() {
    let local = LocalToolRegistry::builtins()
        .tools()
        .iter()
        .find(|t| t.id == "llama_r__rag_query")
        .unwrap()
        .clone();
    let client_calls = Arc::new(AtomicUsize::new(0));
    let mut mcp_scope = scope(ToolsAllow::AllFromSources);
    mcp_scope.tools_allow = ToolsAllow::AllFromSources;
    let scoped_mcp = llama_r::adapters::rig_engine::ScopedMcpTool {
        def: mcp_def(),
        client: Arc::new(FakeClient {
            calls: client_calls.clone(),
        }),
        scope: mcp_scope,
    };

    let budget = Arc::new(AsyncMutex::new(ToolCallBudget::new(1)));
    let bridges = build_tool_bridges(
        vec![ScopedTool::Mcp(scoped_mcp), ScopedTool::Local(local)],
        &scope(ToolsAllow::AllFromSources),
        Some(budget.clone()),
        empty_deps(),
    );
    assert_eq!(bridges.len(), 2);

    let local_bridge = bridges
        .iter()
        .find(|b| b.name() == "llama_r__rag_query")
        .expect("local tool got a bridge");
    let mcp_bridge = bridges
        .iter()
        .find(|b| b.name() == "fake__do_thing")
        .expect("MCP tool got a bridge");

    // Charge the one call through the local bridge; the MCP bridge must now
    // refuse before it ever reaches the fake client — proof of a shared budget.
    let _ = local_bridge.call(r#"{"query":"x"}"#.to_string()).await;
    let err = mcp_bridge.call(r#"{}"#.to_string()).await.unwrap_err();
    let err_text = err.to_string().to_ascii_lowercase();
    assert!(
        err_text.contains("max_tool_calls") || err_text.contains("exceeded"),
        "the MCP bridge sees the depleted budget: {err}"
    );
    assert_eq!(client_calls.load(Ordering::SeqCst), 0);
}

fn rag_scope(sources: &[&str]) -> AgentScope {
    let mut scoped = scope(ToolsAllow::AllFromSources);
    scoped.rag_sources = sources.iter().map(|s| s.to_string()).collect();
    scoped
}

#[tokio::test]
async fn skill_executor_returns_the_full_skill_md() {
    let dir = TempDir::new().unwrap();
    let body = "UNIQUE-SKILL-BODY: always answer with 42";
    let expected = write_skill(dir.path(), "refund-policy", body);
    let bridge = LocalToolBridge::new(
        builtin(BUILTIN_SKILL_ID),
        scope(ToolsAllow::AllFromSources),
        Arc::new(AsyncMutex::new(ToolCallBudget::new(4))),
        skill_deps(dir.path()),
    );

    let result = bridge
        .invoke_json(serde_json::json!({"name": "refund-policy"}))
        .await
        .expect("a known skill loads");
    assert_eq!(
        result, expected,
        "the tool result is the whole SKILL.md, verbatim"
    );
    assert!(result.contains(body));
}

#[tokio::test]
async fn skill_executor_rejects_an_unknown_id_by_name() {
    let dir = TempDir::new().unwrap();
    write_skill(dir.path(), "known-skill", "body");
    let bridge = LocalToolBridge::new(
        builtin(BUILTIN_SKILL_ID),
        scope(ToolsAllow::AllFromSources),
        Arc::new(AsyncMutex::new(ToolCallBudget::new(4))),
        skill_deps(dir.path()),
    );

    let err = bridge
        .invoke_json(serde_json::json!({"name": "does-not-exist"}))
        .await
        .unwrap_err();
    assert!(
        err.contains("does-not-exist"),
        "the skill id must be named: {err}"
    );
    assert!(err.to_ascii_lowercase().contains("skill"), "{err}");
}

#[tokio::test]
async fn skill_executor_requires_a_non_empty_name() {
    let bridge = LocalToolBridge::new(
        builtin(BUILTIN_SKILL_ID),
        scope(ToolsAllow::AllFromSources),
        Arc::new(AsyncMutex::new(ToolCallBudget::new(4))),
        empty_deps(),
    );
    let err = bridge.invoke_json(serde_json::json!({})).await.unwrap_err();
    assert!(err.to_ascii_lowercase().contains("name"), "{err}");
}

#[tokio::test]
async fn rag_query_returns_only_scoped_chunks() {
    let fake = Arc::new(ScopingFakeRag::default());
    // A manifest-supplied source_id is informational: it pins "secret", but the
    // authoritative scope only allows "allowed", so only that source is returned.
    let mut tool = builtin(BUILTIN_RAG_QUERY_ID);
    tool.kind = LocalToolKind::RagQuery {
        source_id: Some("secret".into()),
    };
    let bridge = LocalToolBridge::new(
        tool,
        rag_scope(&["allowed"]),
        Arc::new(AsyncMutex::new(ToolCallBudget::new(4))),
        rag_deps(fake.clone()),
    );

    let result = bridge
        .invoke_json(serde_json::json!({"query": "policy", "top_k": 5}))
        .await
        .expect("the scoped query runs");
    let chunks: Value = serde_json::from_str(&result).expect("the tool result is JSON");
    let array = chunks.as_array().expect("chunks are a JSON array");
    assert_eq!(array.len(), 1, "only the scope-allowed source: {result}");
    assert_eq!(array[0]["source_id"], "allowed");
    assert_eq!(array[0]["text"], "content of the allowed source");
    assert!(
        !result.contains("SECRET"),
        "the secret source must not leak: {result}"
    );

    assert_eq!(
        *fake.seen_sources.lock().unwrap(),
        vec!["allowed".to_string()]
    );
    assert_eq!(fake.seen_query.lock().unwrap().as_deref(), Some("policy"));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rag_query_with_empty_scope_errors_without_querying() {
    let fake = Arc::new(ScopingFakeRag::default());
    let bridge = LocalToolBridge::new(
        builtin(BUILTIN_RAG_QUERY_ID),
        scope(ToolsAllow::AllFromSources), // rag_sources is empty
        Arc::new(AsyncMutex::new(ToolCallBudget::new(4))),
        rag_deps(fake.clone()),
    );

    let err = bridge
        .invoke_json(serde_json::json!({"query": "x"}))
        .await
        .unwrap_err();
    assert!(err.contains("no rag_sources"), "{err}");
    assert_eq!(
        fake.calls.load(Ordering::SeqCst),
        0,
        "an empty scope must never reach the store"
    );
}

#[tokio::test]
async fn rag_query_without_a_store_errors_by_name() {
    let bridge = LocalToolBridge::new(
        builtin(BUILTIN_RAG_QUERY_ID),
        rag_scope(&["allowed"]),
        Arc::new(AsyncMutex::new(ToolCallBudget::new(4))),
        empty_deps(), // rag: None
    );

    let err = bridge
        .invoke_json(serde_json::json!({"query": "x"}))
        .await
        .unwrap_err();
    assert!(err.to_ascii_lowercase().contains("rag"), "{err}");
}

#[tokio::test]
async fn rag_query_caps_top_k_and_defaults_it() {
    let fake = Arc::new(ScopingFakeRag::default());
    let bridge = LocalToolBridge::new(
        builtin(BUILTIN_RAG_QUERY_ID),
        rag_scope(&["allowed"]),
        Arc::new(AsyncMutex::new(ToolCallBudget::new(8))),
        rag_deps(fake.clone()),
    );

    bridge
        .invoke_json(serde_json::json!({"query": "q", "top_k": 100}))
        .await
        .unwrap();
    assert_eq!(*fake.seen_top_k.lock().unwrap(), Some(6), "cap at 6");

    bridge
        .invoke_json(serde_json::json!({"query": "q", "top_k": 2}))
        .await
        .unwrap();
    assert_eq!(
        *fake.seen_top_k.lock().unwrap(),
        Some(2),
        "smaller top_k honoured"
    );

    bridge
        .invoke_json(serde_json::json!({"query": "q"}))
        .await
        .unwrap();
    assert_eq!(
        *fake.seen_top_k.lock().unwrap(),
        Some(6),
        "missing top_k defaults to 6"
    );
}

#[tokio::test]
async fn n_plus_one_calls_exhaust_the_budget() {
    let dir = TempDir::new().unwrap();
    write_skill(dir.path(), "known-skill", "body");
    let budget = Arc::new(AsyncMutex::new(ToolCallBudget::new(2)));
    let bridge = LocalToolBridge::new(
        builtin(BUILTIN_SKILL_ID),
        scope(ToolsAllow::AllFromSources),
        budget,
        skill_deps(dir.path()),
    );

    for i in 0..2 {
        bridge
            .invoke_json(serde_json::json!({"name": "known-skill"}))
            .await
            .unwrap_or_else(|err| panic!("call {i} should succeed: {err}"));
    }
    let err = bridge
        .invoke_json(serde_json::json!({"name": "known-skill"}))
        .await
        .unwrap_err();
    let text = err.to_ascii_lowercase();
    assert!(
        text.contains("max_tool_calls") || text.contains("exceeded"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Task 4: the scoped `llama_r__delegate` tool.
// ---------------------------------------------------------------------------

/// A delegator that records every request so a test can inspect the bounds the
/// bridge put on it, and returns a fixed answer.
#[derive(Default, Clone)]
struct RecordedDelegations(Arc<Mutex<Vec<DelegateRequest>>>);

struct FakeDelegator {
    seen: RecordedDelegations,
    answer: String,
}

#[async_trait]
impl Delegator for FakeDelegator {
    async fn delegate(&self, req: DelegateRequest) -> Result<DelegateResult, String> {
        self.seen.0.lock().unwrap().push(req.clone());
        Ok(DelegateResult {
            answer: self.answer.clone(),
            task_id: req
                .task_id
                .clone()
                .unwrap_or_else(|| "generated-task".into()),
        })
    }
}

fn fake_delegator(answer: &str) -> (Arc<dyn Delegator>, RecordedDelegations) {
    let seen = RecordedDelegations::default();
    let delegator = Arc::new(FakeDelegator {
        seen: seen.clone(),
        answer: answer.into(),
    });
    (delegator, seen)
}

fn parent_bounds() -> DelegateBounds {
    DelegateBounds {
        target_agent_id: Some("child".into()),
        chain: vec!["proj/parent".into()],
        depth: 0,
        effective_max_depth: 2,
    }
}

fn delegate_scope(local: ToolsAllow) -> AgentScope {
    let mut scoped = scope(local);
    scoped.agent_id = "parent".into();
    scoped
}

fn delegate_deps(delegator: Arc<dyn Delegator>, bounds: Option<DelegateBounds>) -> LocalToolDeps {
    let mut deps = empty_deps();
    deps.delegator = Some(delegator);
    deps.delegate_bounds = bounds;
    deps
}

fn tool_budget(max: u32) -> Arc<dyn ToolBudget> {
    Arc::new(AsyncMutex::new(ToolCallBudget::new(max)))
}

#[test]
fn delegate_is_a_typed_builtin_tool() {
    let _guard = env_lock().lock().unwrap();
    let dir = TempDir::new().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());

    let registry = LocalToolRegistry::load().expect("an empty tools dir is valid");
    assert_eq!(registry.tools().len(), 3, "three built-ins, no overlay");
    let tool = registry
        .tools()
        .iter()
        .find(|t| t.id == BUILTIN_DELEGATE_ID)
        .expect("the delegate tool is always registered");
    assert!(matches!(tool.kind, LocalToolKind::Delegate));
    assert_eq!(tool.input_schema["type"], "object");
    assert_eq!(
        tool.input_schema["properties"]["message"]["type"],
        Value::String("string".into())
    );
    assert_eq!(
        tool.input_schema["properties"]["task_id"]["type"],
        Value::String("string".into())
    );
    assert_eq!(
        tool.input_schema["required"][0],
        Value::String("message".into())
    );
    assert_eq!(
        tool.input_schema["required"].as_array().unwrap().len(),
        1,
        "only message is required; task_id is optional"
    );
}

#[test]
fn delegate_tool_is_offered_only_when_the_scope_allows_it() {
    let registry = FakeRegistry {
        def: mcp_def(),
        discover_calls: Arc::new(AtomicUsize::new(0)),
        client_calls: Arc::new(AtomicUsize::new(0)),
    };
    let (engine, _, _) = engine(registry, LocalToolRegistry::builtins());
    let caps = capabilities_for(ProviderKind::Ollama, "llama3.2");

    let denied = prepare_blocking(
        &engine,
        &request(caps.clone(), "llama3.2", ToolsAllow::DenyAll),
    );
    assert!(
        !denied
            .iter()
            .any(|t| matches!(t, ScopedTool::Local(l) if l.id == BUILTIN_DELEGATE_ID)),
        "deny-all must not offer the delegate tool"
    );

    let one = prepare_blocking(
        &engine,
        &request(
            caps.clone(),
            "llama3.2",
            ToolsAllow::Allowlist(HashSet::from([ToolRef::parse(BUILTIN_DELEGATE_ID)])),
        ),
    );
    assert_eq!(one.len(), 1, "only the allowlisted delegate tool");
    assert!(matches!(&one[0], ScopedTool::Local(l) if l.id == BUILTIN_DELEGATE_ID));

    let all = prepare_blocking(
        &engine,
        &request(caps, "llama3.2", ToolsAllow::AllFromSources),
    );
    assert_eq!(all.len(), 3, "all three built-ins when all are allowed");
}

#[tokio::test]
async fn delegate_call_reaches_the_delegator_with_bounds_and_one_budget_unit() {
    let (delegator, seen) = fake_delegator("CHILD_ANSWER");
    let budget = tool_budget(2);
    let bridge = LocalToolBridge::new(
        builtin(BUILTIN_DELEGATE_ID),
        delegate_scope(ToolsAllow::AllFromSources),
        budget.clone(),
        delegate_deps(delegator, Some(parent_bounds())),
    );

    let result = bridge
        .invoke_json(serde_json::json!({"message": "do it", "task_id": "task-9"}))
        .await
        .expect("the delegate call runs");
    let parsed: Value = serde_json::from_str(&result).expect("the tool result is JSON");
    assert_eq!(parsed["answer"], "CHILD_ANSWER");
    assert_eq!(parsed["task_id"], "task-9");

    {
        let guard = seen.0.lock().unwrap();
        assert_eq!(guard.len(), 1, "the delegator was called exactly once");
        let req = &guard[0];
        assert_eq!(req.caller_agent_id, "parent");
        assert_eq!(req.project_id.as_deref(), Some("proj"));
        assert_eq!(req.agent_id, "child", "target comes from the allowlist");
        assert_eq!(req.message, "do it");
        assert_eq!(req.task_id.as_deref(), Some("task-9"));
        assert_eq!(req.chain, vec!["proj/parent".to_string()]);
        assert_eq!(req.depth, 1, "the tool sends the caller's depth + 1");
        assert_eq!(req.effective_max_depth, 2);
        assert!(req.deadline > std::time::Instant::now());
    }

    assert_eq!(budget.used().await, 1, "exactly one allowance was spent");
}

#[tokio::test]
async fn delegate_denial_spends_no_budget_and_never_calls_the_delegator() {
    let gate = Arc::new(ApprovalGate::default());
    let mut scoped = delegate_scope(ToolsAllow::AllFromSources);
    scoped.permission = PermissionPolicy {
        connections_allow: Vec::new(),
        approvals: vec![ApprovalRule {
            tool_pattern: BUILTIN_DELEGATE_ID.into(),
            policy: ApprovalPolicy::Always,
        }],
        auto_threshold: None,
    };
    let (delegator, seen) = fake_delegator("SHOULD_NOT_RUN");
    let budget = tool_budget(2);
    let mut deps = delegate_deps(delegator, Some(parent_bounds()));
    deps.gate = gate.clone();
    let bridge = LocalToolBridge::new(builtin(BUILTIN_DELEGATE_ID), scoped, budget.clone(), deps);

    let args = serde_json::json!({"message": "do it"});
    let expected = args.clone();
    let decider = tokio::spawn(async move {
        loop {
            let pending = gate.list_pending();
            if let Some(p) = pending.first() {
                assert_eq!(p.tool_name, BUILTIN_DELEGATE_ID);
                gate.decide(p.id, false, args_hash(&expected)).unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });

    let err = bridge.invoke_json(args).await.unwrap_err();
    assert!(err.contains("approval denied"), "{err}");
    decider.await.unwrap();
    assert_eq!(budget.used().await, 0, "a denial spends no allowance");
    assert!(
        seen.0.lock().unwrap().is_empty(),
        "a denial never calls the delegator"
    );
}

/// Minimal in-memory execution store: only the operation cache is exercised by
/// the replay test, every other method is an inert default.
struct OpStore {
    operations: Mutex<std::collections::HashMap<String, Value>>,
}

#[async_trait]
impl ExecutionStore for OpStore {
    async fn create(
        &self,
        _execution: &llama_r::domain::execution::AgentExecution,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn append_event(
        &self,
        _execution_id: Uuid,
        _event: &llama_r::domain::execution::ExecutionEvent,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn set_status(
        &self,
        _execution_id: Uuid,
        _status: llama_r::domain::execution::ExecutionStatus,
        _error: Option<&str>,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn get(
        &self,
        _execution_id: Uuid,
    ) -> Result<Option<llama_r::domain::execution::AgentExecution>, String> {
        Ok(None)
    }
    async fn list_interrupted(
        &self,
    ) -> Result<Vec<llama_r::domain::execution::AgentExecution>, String> {
        Ok(Vec::new())
    }
    async fn list(
        &self,
        _limit: usize,
    ) -> Result<Vec<llama_r::domain::execution::AgentExecution>, String> {
        Ok(Vec::new())
    }
    async fn get_events(
        &self,
        _execution_id: Uuid,
    ) -> Result<Vec<llama_r::domain::execution::ExecutionEvent>, String> {
        Ok(Vec::new())
    }
    async fn record_operation(&self, id: &str, result: &Value) -> Result<(), String> {
        self.operations
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert_with(|| result.clone());
        Ok(())
    }
    async fn get_operation(&self, id: &str) -> Result<Option<Value>, String> {
        Ok(self.operations.lock().unwrap().get(id).cloned())
    }
    async fn save_pending_approval(
        &self,
        _approval: &llama_r::ports::execution::PendingApprovalRecord,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn load_pending_approvals(
        &self,
    ) -> Result<Vec<llama_r::ports::execution::PendingApprovalRecord>, String> {
        Ok(Vec::new())
    }
    async fn decide_pending(
        &self,
        _id: Uuid,
        _decision: llama_r::ports::execution::ApprovalDecisionRecord,
        _decided_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn delegate_result_is_replayed_for_the_same_operation_context() {
    let (delegator, seen) = fake_delegator("CHILD_ANSWER");
    let store: Arc<dyn ExecutionStore> = Arc::new(OpStore {
        operations: Mutex::new(std::collections::HashMap::new()),
    });
    let mut deps = delegate_deps(delegator, Some(parent_bounds()));
    deps.operations = OperationContext::new(Uuid::new_v4(), store);
    let budget = tool_budget(4);
    let bridge = LocalToolBridge::new(
        builtin(BUILTIN_DELEGATE_ID),
        delegate_scope(ToolsAllow::AllFromSources),
        budget.clone(),
        deps,
    );

    let args = serde_json::json!({"message": "same", "task_id": "t"});
    let first = bridge.invoke_json(args.clone()).await.unwrap();
    let second = bridge.invoke_json(args).await.unwrap();
    assert_eq!(first, second, "a retry replays the stored result");
    assert_eq!(
        seen.0.lock().unwrap().len(),
        1,
        "the delegator ran once, not twice"
    );
    assert_eq!(
        budget.used().await,
        2,
        "the pipeline charges before the idempotency guard replays"
    );
}

#[tokio::test]
async fn delegate_emits_tool_call_and_tool_result_events() {
    let (delegator, _) = fake_delegator("CHILD_ANSWER");
    let (tx, mut rx) = mpsc::channel(8);
    let mut deps = delegate_deps(delegator, Some(parent_bounds()));
    deps.event_tx = Some(tx);
    let bridge = LocalToolBridge::new(
        builtin(BUILTIN_DELEGATE_ID),
        delegate_scope(ToolsAllow::AllFromSources),
        tool_budget(2),
        deps,
    );

    bridge
        .invoke_json(serde_json::json!({"message": "do it"}))
        .await
        .expect("the delegate call runs");
    let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(events.len(), 2, "a ToolCall and a ToolResult");
    assert!(
        matches!(&events[0], AgentRunEvent::ToolCall { tool_name, .. } if tool_name == BUILTIN_DELEGATE_ID),
        "first event is the ToolCall: {events:?}"
    );
    assert!(
        matches!(&events[1], AgentRunEvent::ToolResult { tool_name, is_error: false, .. } if tool_name == BUILTIN_DELEGATE_ID),
        "second event is a successful ToolResult: {events:?}"
    );
}

#[tokio::test]
async fn delegate_bad_args_and_missing_companions_are_named_errors() {
    let (delegator, _) = fake_delegator("CHILD_ANSWER");
    let bridge = LocalToolBridge::new(
        builtin(BUILTIN_DELEGATE_ID),
        delegate_scope(ToolsAllow::AllFromSources),
        tool_budget(16),
        delegate_deps(delegator.clone(), Some(parent_bounds())),
    );
    for (args, needle) in [
        (serde_json::json!({}), "message"),
        (serde_json::json!({"message": 42}), "message"),
        (
            serde_json::json!({"message": "ok", "task_id": 7}),
            "task_id",
        ),
    ] {
        let err = bridge.invoke_json(args.clone()).await.unwrap_err();
        assert!(
            err.to_ascii_lowercase().contains(needle),
            "the error names '{needle}': {err}"
        );
    }

    let no_delegator = LocalToolBridge::new(
        builtin(BUILTIN_DELEGATE_ID),
        delegate_scope(ToolsAllow::AllFromSources),
        tool_budget(4),
        empty_deps(),
    );
    let err = no_delegator
        .invoke_json(serde_json::json!({"message": "ok"}))
        .await
        .unwrap_err();
    assert!(err.contains("delegator"), "{err}");

    let no_policy = LocalToolBridge::new(
        builtin(BUILTIN_DELEGATE_ID),
        delegate_scope(ToolsAllow::AllFromSources),
        tool_budget(4),
        delegate_deps(delegator.clone(), None),
    );
    let err = no_policy
        .invoke_json(serde_json::json!({"message": "ok"}))
        .await
        .unwrap_err();
    assert!(err.contains("subagents"), "{err}");

    let no_target = LocalToolBridge::new(
        builtin(BUILTIN_DELEGATE_ID),
        delegate_scope(ToolsAllow::AllFromSources),
        tool_budget(4),
        delegate_deps(
            delegator,
            Some(DelegateBounds {
                target_agent_id: None,
                ..parent_bounds()
            }),
        ),
    );
    let err = no_target
        .invoke_json(serde_json::json!({"message": "ok"}))
        .await
        .unwrap_err();
    assert!(err.contains("allowed"), "{err}");
}
