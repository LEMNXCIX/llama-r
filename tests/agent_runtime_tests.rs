//! What an agent's configuration reaches the model with.
//!
//! One behaviour, one file, because it was wrong in exactly one place and the
//! symptom — "I wrote a rule and the model ignored it" — looks identical whether
//! the rule was dropped on the way to disk, on the way into the run request, or by
//! the model itself. The TOML writer is covered by the round-trip in
//! `src/domain/agent.rs`; this file covers the two hops after it.

use llama_r::context::analyzer::ContextEnricher;
use llama_r::context::store::ContextStore;
use llama_r::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult};
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::agent_runtime::{AgentRuntime, RuntimeChatRequest};
use llama_r::services::skill_manager::SkillManager;
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;
use tokio::sync::mpsc;

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
    let dir = TempDir::new().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());

    let agents = dir.path().join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    let rendered: Vec<String> = rules.iter().map(|r| format!("{r:?}")).collect();
    let tools: Vec<String> = tools.iter().map(|t| format!("{t:?}")).collect();
    std::fs::write(
        agents.join("reviewer.toml"),
        format!(
            "name = \"Reviewer\"\nmodel = \"m\"\nsystem_prompt = \"Base prompt\"\nrules = [{}]\n\
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
    }
}

fn enricher() -> ContextEnricher {
    ContextEnricher::new(
        Arc::new(ContextStore::new()),
        Arc::new(SkillManager::new()),
        "fallback".to_string(),
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
