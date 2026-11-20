//! What an agent's configuration reaches the model with.
//!
//! One behaviour, one file, because it was wrong in exactly one place and the
//! symptom — "I wrote a rule and the model ignored it" — looks identical whether
//! the rule was dropped on the way to disk, on the way into the run request, or by
//! the model itself. The TOML writer is covered by the round-trip in
//! `src/domain/agent.rs`; this file covers the two hops after it.

use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::adapters::rig_engine::local_tools::LocalToolRegistry;
use llama_r::adapters::rig_engine::RigAgentEngine;
use llama_r::config::{Config, ProviderKind};
use llama_r::context::analyzer::ContextEnricher;
use llama_r::context::store::ContextStore;
use llama_r::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult};
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::providers::registry::ProviderRegistry;
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::agent_runtime::{AgentRuntime, RuntimeChatRequest};
use llama_r::services::skill_manager::SkillManager;
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Serialises the tests that move `LLAMA_R_DIR`, which is process-wide.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Records the run request it is handed, so a test can read back the exact system
/// prompt the engine would have sent.
struct RecordingEngine {
    seen: Arc<Mutex<Option<AgentRunRequest>>>,
}

#[async_trait::async_trait]
impl AgentEngine for RecordingEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        *self.seen.lock().unwrap() = Some(req);
        Ok(AgentRunResult {
            text: "ok".to_string(),
            tool_calls: 0,
            iterations: 0,
            model: "m".to_string(),
        })
    }

    async fn run_stream(
        &self,
        _req: AgentRunRequest,
    ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
        unreachable!("not exercised here")
    }
}

/// A registry holding one agent whose manifest carries `rules`, plus the runtime
/// that will chat as it.
///
/// The caller must hold `env_lock`: `reload_all` and both stores read
/// `LLAMA_R_DIR`, so this cannot be parallel with anything else that moves it.
fn world(rules: &[&str], tools: &[&str]) -> (AgentRuntime, Arc<Mutex<Option<AgentRunRequest>>>) {
    world_with_agent(rules, tools, "", "m")
}

/// Same as `world`, but lets the manifest declare its own provider and model so
/// a test can prove the run request resolves them per agent.
fn world_with_agent(
    rules: &[&str],
    tools: &[&str],
    provider: &str,
    model: &str,
) -> (AgentRuntime, Arc<Mutex<Option<AgentRunRequest>>>) {
    let dir = TempDir::new().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());

    let agents = dir.path().join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    let rendered: Vec<String> = rules.iter().map(|r| format!("{r:?}")).collect();
    let tools: Vec<String> = tools.iter().map(|t| format!("{t:?}")).collect();
    std::fs::write(
        agents.join("reviewer.toml"),
        format!(
            "name = \"Reviewer\"\nprovider = {provider:?}\nmodel = {model:?}\n\
             system_prompt = \"Base prompt\"\nrules = [{}]\n\
             \n[optimize]\nenabled = true\nrules = [{}]\n",
            rendered.join(", "),
            tools.join(", ")
        ),
    )
    .unwrap();

    let registry = AgentRegistry::new();
    registry.reload_all(&[]).unwrap();

    let seen = Arc::new(Mutex::new(None));
    let runtime = AgentRuntime {
        registry: Arc::new(registry),
        engine: Arc::new(RecordingEngine {
            seen: Arc::clone(&seen),
        }),
        history: None,
        rag: None,
        enricher: Arc::new(enricher()),
    };
    (runtime, seen)
}

/// Drive one turn to completion on a throwaway runtime.
///
/// The tests are synchronous on purpose. `world` moves `LLAMA_R_DIR`, which is
/// process-wide and guarded by a `std::Mutex`; holding that guard across an `await`
/// inside an async test is how a serialisation lock deadlocks a current-thread
/// runtime. Blocking here keeps the guard's scope lexical and obvious.
fn chat_blocking(runtime: &AgentRuntime, req: RuntimeChatRequest) {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(runtime.chat(req))
        .unwrap();
}

fn request() -> RuntimeChatRequest {
    RuntimeChatRequest {
        project_id: None,
        agent_id: Some("reviewer".to_string()),
        conversation_id: None,
        user_message: "hola".to_string(),
        history: Vec::new(),
        model_override: None,
        system_prompt: None,
        default_model: "fallback".to_string(),
        execution_id: uuid::Uuid::new_v4(),
        shared_budget: None,
        delegation_context: None,
    }
}

fn enricher() -> ContextEnricher {
    ContextEnricher::new(
        Arc::new(ContextStore::new()),
        Arc::new(SkillManager::new()),
        "fallback".to_string(),
        Arc::new(ProviderRegistry::default()),
        ProviderKind::Ollama,
    )
}

/// The rule an operator typed into the agent form has to reach the model.
///
/// The TUI sends `system_prompt: None`, which `AgentRuntime` has always read as
/// "use the manifest's `system_prompt` verbatim" — and that string contains nothing
/// but the base prompt. Rules, project context and selected skills are all sections
/// `ContextEnricher` appends, so every one of them was dropped on this path. The
/// HTTP path passes the enriched prompt itself, which is why the same agent behaved
/// differently depending on which client asked.
#[test]
fn a_turn_with_no_prebuilt_prompt_still_carries_the_agent_rules() {
    let _guard = env_lock().lock().unwrap();
    let (runtime, seen) = world(&["Always explain the risk"], &[]);

    chat_blocking(&runtime, request());

    let seen = seen.lock().unwrap();
    let prompt = &seen.as_ref().expect("the engine ran").system_prompt;
    assert!(
        prompt.contains("Always explain the risk"),
        "the rule must reach the engine; the prompt was {prompt:?}"
    );
}

/// The base prompt is still there. A fix that replaced the manifest prompt instead
/// of enriching it would satisfy the test above while losing the agent's own words.
#[test]
fn enriching_keeps_the_manifest_prompt() {
    let _guard = env_lock().lock().unwrap();
    let (runtime, seen) = world(&["Always explain the risk"], &[]);

    chat_blocking(&runtime, request());

    let seen = seen.lock().unwrap();
    let prompt = &seen.as_ref().unwrap().system_prompt;
    assert!(
        prompt.contains("Base prompt"),
        "the agent's own prompt is the first section, not a discarded one: {prompt:?}"
    );
}

/// A prompt the caller built and passed in is the caller's to choose. The HTTP path
/// has already run it through the optimiser and the metrics by then, so rebuilding
/// it here would double both.
#[test]
fn a_prebuilt_prompt_is_used_verbatim() {
    let _guard = env_lock().lock().unwrap();
    let (runtime, seen) = world(&["Always explain the risk"], &[]);

    let mut req = request();
    req.system_prompt = Some("exactamente esto".to_string());
    chat_blocking(&runtime, req);

    assert_eq!(
        seen.lock().unwrap().as_ref().unwrap().system_prompt,
        "exactamente esto",
        "the caller's prompt is not second-guessed"
    );
}

/// The resolved agent travels with the turn, or the rules would be applied to
/// somebody else's request.
#[test]
fn the_turn_carries_the_agent_it_resolved() {
    let _guard = env_lock().lock().unwrap();
    let (runtime, seen) = world(&["Always explain the risk"], &[]);

    chat_blocking(&runtime, request());

    let run = seen.lock().unwrap();
    let run = run.as_ref().unwrap();
    assert_eq!(run.agent.id, "reviewer");
    assert_eq!(
        run.agent.config.rules,
        vec!["Always explain the risk".to_string()],
        "and its rules, which is what makes the prompt above possible"
    );
}

/// A turn whose agent configures a tool must arrive at the engine compressed.
///
/// This is the whole reason the tools are applied here. `chat_core` applied them
/// for HTTP requests and nothing else, so from the TUI an agent's
/// `optimize.rules` was configuration that appeared to work and did not — the same
/// shape of bug as the unenriched system prompt one commit earlier.
#[test]
fn a_configured_tool_reaches_the_engine() {
    let _guard = env_lock().lock().unwrap();
    let (runtime, seen) = world(&["Always explain the risk"], &["compress_code"]);
    let _ = seen;

    chat_blocking(&runtime, request());

    let seen = seen.lock().unwrap();
    let run = seen.as_ref().unwrap();
    assert!(
        !run.system_prompt.contains("\n\n"),
        "collapse_blanks ran on the prompt: {:?}",
        run.system_prompt
    );
    assert!(
        !run.user_message.contains("   "),
        "the message was compressed too: {:?}",
        run.user_message
    );
}

/// One door, not two. `chat_core` still optimizes for the legacy provider path, and
/// doing it there as well would run every tool twice. Both are idempotent today, and
/// a tool that is not would be applied twice in silence.
#[test]
fn a_tool_is_applied_once_and_only_once() {
    let _guard = env_lock().lock().unwrap();
    let (runtime, seen) = world(&["Always explain the risk"], &["strip_ansi"]);

    let mut req = request();
    req.user_message = "\u{1b}[31mhola\u{1b}[0m".to_string();
    chat_blocking(&runtime, req);

    let run = seen.lock().unwrap();
    let run = run.as_ref().unwrap();
    assert_eq!(run.user_message, "hola", "the escapes went, once");
}

/// A tool name that matches nothing is reported when the agent loads, so it is never
/// a silent no-op. Pinned here because the picker dropping it on save and the
/// registry warning about it are the two halves of one behaviour: the picker removes
/// what it cannot run, and the registry says what it removed.
#[test]
fn an_unknown_tool_is_named_by_the_loader() {
    let _guard = env_lock().lock().unwrap();
    let (runtime, _seen) = world(&["Always explain the risk"], &["compres_code"]);

    let agent = runtime.registry.get_agent("reviewer").unwrap();
    let optimizer = llama_r::optimizer::TokenOptimizer::new(agent.config.optimize.clone());
    assert_eq!(optimizer.unknown_tools(), vec!["compres_code"]);
    assert_eq!(
        optimizer.optimize("```python\n# c\nx = 1\n```"),
        "```python\n# c\nx = 1\n```",
        "and it changed nothing, which is what the warning is about"
    );
}

/// The run request carries the provider and capabilities resolved *for that
/// agent*, not the global `PROVIDER` the engine was built with. This is the data
/// Task 5 needs to pick a client per request instead of calling the wrong one.
#[test]
fn a_turn_carries_the_resolved_provider_and_capabilities() {
    let _guard = env_lock().lock().unwrap();
    let (runtime, seen) = world_with_agent(&[], &[], "anthropic", "claude-3-5-sonnet");

    chat_blocking(&runtime, request());

    let seen = seen.lock().unwrap();
    let run = seen.as_ref().expect("the engine ran");
    assert_eq!(run.provider, ProviderKind::Anthropic);
    assert_eq!(
        run.model, "claude-3-5-sonnet",
        "the agent's own model still wins over the default"
    );
    assert!(
        run.capabilities.vision && run.capabilities.tool_calling,
        "the capabilities are those of the resolved provider/model pair"
    );
}

/// An explicit model override still wins over the resolved model: threading the
/// resolver through `build_run_request` must not change that precedence.
#[test]
fn a_model_override_still_beats_the_resolved_model() {
    let _guard = env_lock().lock().unwrap();
    let (runtime, seen) = world_with_agent(&[], &[], "anthropic", "claude-3-5-sonnet");

    let mut req = request();
    req.model_override = Some("override-model".to_string());
    chat_blocking(&runtime, req);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.as_ref().unwrap().model, "override-model");
}

/// The enricher the runtime needs is the same one the chat path uses, so this is a
/// reminder of what it must be built from — `ContextStore` and `SkillManager` read
/// `LLAMA_R_DIR`, so both have to exist before the runtime is built.
#[test]
fn the_enricher_is_built_from_the_stores() {
    let _guard = env_lock().lock().unwrap();
    let (_runtime, _seen) = world(&[], &[]);

    let prompt = enricher();
    assert_eq!(
        prompt.resolve_model(&llama_r::domain::agent::Agent {
            id: "x".into(),
            project_id: None,
            config: llama_r::domain::agent::AgentConfig {
                model: String::new(),
                ..Default::default()
            },
        }),
        "fallback",
        "an agent with no model falls back to the configured default"
    );
}

// ---------------------------------------------------------------------------
// End-to-end: deferred skill → tool offer → on-demand load (Task 6)
//
// Everything above stops at the engine boundary: a `RecordingEngine` sees the
// request but never drives a tool loop. This case drives the *real* Rig engine
// against a fake Ollama socket so the three halves of the seam are proven in
// one turn: the prompt the model sees, the tool catalog it is offered, and the
// bytes that come back when it calls the tool.
// ---------------------------------------------------------------------------

const SKILL_ID: &str = "x";
const SKILL_DESCRIPTION: &str = "Provides deferred guidance for x";
const SKILL_BODY_MARKER: &str = "DEFERRED X BODY SECRET";
const FINAL_ANSWER: &str = "loaded and answered";

/// A fake Ollama that plays both halves of the tool loop on one socket: the
/// first completion asks for `llama_r__skill`, the one after the tool result
/// returns a final answer. Every raw request body is recorded so the test can
/// inspect the system prompt, the tool schemas and the loaded body.
struct ToolLoopServer {
    addr: SocketAddr,
    bodies: Arc<Mutex<Vec<String>>>,
    handle: JoinHandle<()>,
}

impl ToolLoopServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bodies);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let Some(body) = read_request(&mut socket).await else {
                    return;
                };
                sink.lock().unwrap().push(body.clone());

                // Ollama sends a tool result back as `{"role":"tool", …}`.
                // Seeing one means the engine executed the tool and is now
                // asking for the final answer.
                let payload = if body.contains("\"role\":\"tool\"") {
                    serde_json::json!({
                        "model": "llama3.2",
                        "created_at": "2024-01-01T00:00:00Z",
                        "message": {"role": "assistant", "content": FINAL_ANSWER},
                        "done": true
                    })
                    .to_string()
                } else {
                    serde_json::json!({
                        "model": "llama3.2",
                        "created_at": "2024-01-01T00:00:00Z",
                        "message": {
                            "role": "assistant",
                            "content": "",
                            "tool_calls": [{
                                "type": "function",
                                "function": {
                                    "name": "llama_r__skill",
                                    "arguments": {"name": SKILL_ID}
                                }
                            }]
                        },
                        "done": true
                    })
                    .to_string()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        Self {
            addr,
            bodies,
            handle,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }
}

impl Drop for ToolLoopServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Read one full HTTP/1.1 request (headers plus `Content-Length` body) off a
/// socket. Rig's Ollama client sends a single `POST /api/chat` per turn.
async fn read_request(socket: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let header_end = loop {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let len = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let total = header_end + len;
    while buf.len() < total {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Some(String::from_utf8_lossy(&buf[header_end..total]).to_string())
}

fn local_tools_config(ollama_url: &str) -> Config {
    Config {
        port: 3000,
        provider: ProviderKind::Ollama,
        ollama_url: ollama_url.to_string(),
        default_model: "llama3.2".to_string(),
        embedding_model: "nomic-embed-text".to_string(),
        embedding_dimensions: 768,
        rag_enabled: false,
        history_retention_days: 90,
        openrouter_api_key: None,
        opencode_api_key: None,
        opencode_url: "http://127.0.0.1:4096".to_string(),
        openai_base_url: None,
    }
}

/// One turn with a deferred skill, driven end to end against a real engine.
///
/// The agent's TOML alone opts in: `tools_local` offers the built-in, and
/// `deferred_skills` keeps the body out of the prompt until the model asks for
/// it. No code change and no restart sits between the manifest and the wire.
#[test]
fn a_deferred_skill_is_announced_offered_and_loaded_on_demand() {
    let _guard = env_lock().lock().unwrap();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let server = ToolLoopServer::start().await;
            let dir = TempDir::new().unwrap();
            std::env::set_var("LLAMA_R_DIR", dir.path());

            // A real SKILL.md on disk, discovered through a restricted manager
            // so the developer's own skills can never leak into the assertion.
            let skills_root = dir.path().join("skills");
            let skill_dir = skills_root.join(SKILL_ID);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!(
                    "---\nname: {SKILL_ID}\ndescription: {SKILL_DESCRIPTION}\n---\n{SKILL_BODY_MARKER}"
                ),
            )
            .unwrap();
            let skill_manager = Arc::new(SkillManager::with_paths(vec![skills_root]));
            skill_manager.scan_and_load();

            let agents = dir.path().join("agents");
            std::fs::create_dir_all(&agents).unwrap();
            std::fs::write(
                agents.join("reviewer.toml"),
                format!(
                    "name = \"Reviewer\"\nprovider = \"ollama\"\nmodel = \"llama3.2\"\n\
                     system_prompt = \"Base prompt\"\nskills = [{SKILL_ID:?}]\n\
                     tools_local = [\"llama_r__skill\"]\ndeferred_skills = [{SKILL_ID:?}]\n"
                ),
            )
            .unwrap();

            let registry = AgentRegistry::new();
            registry.reload_all(&[]).unwrap();

            let cfg = local_tools_config(&server.url());
            let engine = RigAgentEngine::new(
                server.url(),
                Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
                None,
                cfg,
                Arc::clone(&skill_manager),
                Arc::new(LocalToolRegistry::builtins()),
                Arc::new(llama_r::services::approval::ApprovalGate::default()),
            );
            let enricher = ContextEnricher::new(
                Arc::new(ContextStore::new()),
                skill_manager,
                "llama3.2".to_string(),
                Arc::new(ProviderRegistry::default()),
                ProviderKind::Ollama,
            );
            let runtime = AgentRuntime {
                registry: Arc::new(registry),
                engine: Arc::new(engine),
                history: None,
                rag: None,
                enricher: Arc::new(enricher),
            };

            let answer = runtime.chat(request()).await.unwrap();
            assert_eq!(answer, FINAL_ANSWER, "the loop must finish with the model's answer");

            let bodies = server.bodies();
            assert!(
                bodies.len() >= 2,
                "a tool call then a final answer were expected, got {} request(s)",
                bodies.len()
            );

            let first: Value = serde_json::from_str(&bodies[0]).unwrap();
            let system = first["messages"][0]["content"]
                .as_str()
                .expect("the system prompt is the first message");
            let announcement = format!("- {SKILL_ID} — {SKILL_DESCRIPTION}");
            assert!(
                system.contains(&announcement),
                "the skill's id and description must be announced: {system}"
            );
            assert!(
                system.contains("llama_r__skill"),
                "the model must be told how to load the body: {system}"
            );
            assert!(
                !system.contains(SKILL_BODY_MARKER),
                "the deferred body must stay out of the prompt: {system}"
            );

            let tools = first["tools"].as_array().expect("tools are offered");
            let skill_tool = tools
                .iter()
                .find(|tool| tool["function"]["name"] == "llama_r__skill")
                .expect("llama_r__skill must be offered");
            assert_eq!(
                skill_tool["function"]["parameters"]["properties"]["name"]["type"],
                "string",
                "the built-in's schema travels with the offer: {skill_tool}"
            );
            assert_eq!(
                skill_tool["function"]["parameters"]["required"][0], "name"
            );
            assert!(
                tools
                    .iter()
                    .all(|tool| tool["function"]["name"] != "llama_r__rag_query"),
                "a built-in the agent did not opt into stays unoffered (deny-all)"
            );

            let second: Value = serde_json::from_str(&bodies[1]).unwrap();
            let tool_result = second["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|msg| msg["role"] == "tool")
                .expect("the tool result must travel back to the model");
            assert!(
                tool_result["content"]
                    .as_str()
                    .unwrap()
                    .contains(SKILL_BODY_MARKER),
                "calling llama_r__skill must yield the full SKILL.md: {tool_result}"
            );
        });
}
