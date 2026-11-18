//! Local-tool registry, scoped bridge and executors. Discovery, scope, budget
//! and the capability gate (Task 3) live alongside the `builtin:skill` and
//! `rag_query` executors (Task 4).

use async_trait::async_trait;
use llama_r::adapters::rig_engine::builder::build_tool_bridges;
use llama_r::adapters::rig_engine::limits::ToolCallBudget;
use llama_r::adapters::rig_engine::local_tools::{
    LocalToolBridge, LocalToolDeps, LocalToolKind, LocalToolRegistry, BUILTIN_RAG_QUERY_ID,
    BUILTIN_SKILL_ID,
};
use llama_r::adapters::rig_engine::{EngineTarget, RigAgentEngine, ScopedTool};
use llama_r::config::{Config, ProviderKind};
use llama_r::domain::agent::{Agent, AgentConfig, RagWritePolicy};
use llama_r::domain::scope::{AgentScope, ToolRef, ToolsAllow};
use llama_r::error::AppError;
use llama_r::ports::engine::AgentRunRequest;
use llama_r::ports::mcp::{
    filter_tools_for_scope, McpCallRequest, McpCallResult, McpClient, McpServerRegistry, McpToolDef,
};
use llama_r::ports::rag::{RagChunk, RagReplaceBatch, RagStore, RagUpsert};
use llama_r::providers::capabilities::{capabilities_for, ModelCapabilities};
use llama_r::services::skill_manager::SkillManager;
use rig_core::tool::ToolDyn;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;

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
    }
}

fn empty_deps() -> LocalToolDeps {
    LocalToolDeps {
        skill_manager: Arc::new(SkillManager::with_paths(Vec::new())),
        rag: None,
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
fn registry_always_has_the_two_builtins() {
    let _guard = env_lock().lock().unwrap();
    let dir = TempDir::new().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());

    let registry = LocalToolRegistry::load().expect("an empty tools dir is valid");
    let tools = registry.tools();
    assert_eq!(tools.len(), 2, "exactly the two built-ins, no overlay");

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
    assert_eq!(registry.tools().len(), 3);
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
        2,
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
        2,
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
        budget.clone(),
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
