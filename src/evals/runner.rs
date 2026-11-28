//! Executes declarative eval cases and assembles the report.
//!
//! Provider-class cases (`plain`, `tool`, `stream`) run against the recorded
//! fixture server, one row per case × provider. Agent cases run a real agent
//! turn against a fake Ollama socket: the tool loop executes a tool, the runner
//! drains [`AgentRunEvent`](crate::ports::engine::AgentRunEvent)s into evidence,
//! and the same checks apply. Every per-case error is caught into a row so one
//! bad case cannot stop the suite.

use std::str::FromStr;
use std::sync::Arc;

use tokio_stream::StreamExt;

#[cfg(feature = "rig-engine")]
use crate::adapters::context::NoopCompactor;
#[cfg(feature = "rig-engine")]
use crate::adapters::mcp::StaticMcpRegistry;
#[cfg(feature = "rig-engine")]
use crate::adapters::rig_engine::RigAgentEngine;
#[cfg(feature = "rig-engine")]
use crate::config::Config;
use crate::config::ProviderKind;
#[cfg(feature = "rig-engine")]
use crate::context::analyzer::ContextEnricher;
#[cfg(feature = "rig-engine")]
use crate::context::store::ContextStore;
#[cfg(feature = "rig-engine")]
use crate::domain::agent::{Agent, AgentConfig};
use crate::domain::models::{ChatMessage, ChatRequest};
#[cfg(feature = "rig-engine")]
use crate::ports::engine::AgentRunEvent;
#[cfg(feature = "rig-engine")]
use crate::ports::mcp::McpServerRegistry;
use crate::ports::router::DecisionRouter;
#[cfg(feature = "rig-engine")]
use crate::providers::registry::ProviderRegistry;
#[cfg(feature = "rig-engine")]
use crate::services::agent_registry::AgentRegistry;
#[cfg(feature = "rig-engine")]
use crate::services::agent_runtime::{AgentRuntime, RuntimeChatRequest};
#[cfg(feature = "rig-engine")]
use crate::services::approval::ApprovalGate;
#[cfg(feature = "rig-engine")]
use crate::services::scope_builder::ScopeBuilder;
#[cfg(feature = "rig-engine")]
use crate::services::skill_manager::SkillManager;
#[cfg(feature = "rig-engine")]
use tempfile::TempDir;
#[cfg(feature = "rig-engine")]
use uuid::Uuid;

#[cfg(feature = "rig-engine")]
use super::agent_fixture::{AgentToolLoopServer, EVAL_AGENT_SKILL, EVAL_AGENT_TOOL};
#[cfg(feature = "rig-engine")]
use super::check::qualified_tool_name;
use super::check::{
    apply_check, apply_judge, redact_detail, EvalCheck, EvalCheckOutcome, EvalEvidence,
};
use super::fixture::{build_provider, recorded_body, FixtureServer};
use super::report::{EvalReport, EvalRow};
use super::{EvalCase, EvalCaseKind, EvalMode, EvalProvider, EvalSuite};

/// Confidence a judge answer must reach to be trusted, unless overridden.
pub const DEFAULT_JUDGE_THRESHOLD: f32 = 0.6;

/// Configuration for one [`run_suite`] call.
pub struct RunOptions {
    /// Optional decision model for `judge` checks. Without one they skip.
    pub router: Option<Arc<dyn DecisionRouter>>,
    /// Strict is the CI default; lenient tolerates failing rows.
    pub mode: EvalMode,
    /// Minimum confidence for a judge verdict to count.
    pub threshold: f32,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            router: None,
            mode: EvalMode::Strict,
            threshold: DEFAULT_JUDGE_THRESHOLD,
        }
    }
}

impl RunOptions {
    /// Whether the caller should treat the resulting report as a CI gate.
    pub fn strict(&self) -> bool {
        self.mode == EvalMode::Strict
    }
}

/// How one case execution finished before its checks were applied.
enum Execution {
    /// Evidence to apply the case's checks against.
    Evidence(EvalEvidence),
    /// A recorded, expected condition with no answer to check (a stream that
    /// the provider cannot serve). Skipped, never passed or failed.
    Skip(String),
}

/// Run every case in the suite and return its report.
pub async fn run_suite(suite: EvalSuite, options: RunOptions) -> EvalReport {
    let mut rows = Vec::new();
    for case in &suite.cases {
        if case.kind == EvalCaseKind::Agent {
            rows.push(agent_row(case, &options).await);
            continue;
        }
        for provider in &suite.providers {
            let result = execute_provider_case(case, provider).await;
            rows.push(make_row(case, &provider.name, result, &options).await);
        }
    }
    EvalReport::new(rows)
}

fn row_kind(case: &EvalCase) -> String {
    format!("{:?}", case.kind).to_lowercase()
}

/// Run one agent case through a real tool loop and apply its checks.
async fn agent_row(case: &EvalCase, options: &RunOptions) -> EvalRow {
    let result = execute_agent_case(case).await;
    make_row(case, "agent", result, options).await
}

/// Build a real `AgentRuntime` against a fake Ollama socket and drive one turn.
///
/// The canned completion asks for a local tool, the engine executes it, and the
/// second completion returns the final text. `ToolCall` events become qualified
/// names in the evidence; a tool that fails to execute is a named error, not a
/// green row.
#[cfg(feature = "rig-engine")]
async fn execute_agent_case(case: &EvalCase) -> Result<Execution, String> {
    if case.checks.is_empty() {
        return Err(format!("case '{}' declares no checks", case.id));
    }

    let server = AgentToolLoopServer::start().await;

    // A real skill on disk so `llama_r__skill` executes instead of erroring.
    let dir = TempDir::new().map_err(|err| format!("case '{}': tempdir: {err}", case.id))?;
    let skills_root = dir.path().join("skills");
    let skill_dir = skills_root.join(EVAL_AGENT_SKILL);
    std::fs::create_dir_all(&skill_dir)
        .map_err(|err| format!("case '{}': skill dir: {err}", case.id))?;
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!(
            "---\nname: {EVAL_AGENT_SKILL}\ndescription: An eval fixture skill\n---\nEval fixture skill body"
        ),
    )
    .map_err(|err| format!("case '{}': skill file: {err}", case.id))?;
    let skill_manager = Arc::new(SkillManager::with_paths(vec![skills_root]));
    skill_manager.scan_and_load();

    // Build the agent in memory. Going through `reload_all` would mean writing a
    // manifest and moving the process-wide `LLAMA_R_DIR`, which races with every
    // other caller; `AgentRegistry::register` keeps the runner pure.
    let agent_id = case
        .agent
        .clone()
        .unwrap_or_else(|| "eval-agent".to_string());
    let agent = Agent {
        id: agent_id.clone(),
        project_id: None,
        config: AgentConfig {
            name: "Eval Agent".into(),
            provider: "ollama".into(),
            model: "llama3.2".into(),
            system_prompt: "You are an eval fixture agent.".into(),
            skills: vec![EVAL_AGENT_SKILL.into()],
            tools_local: vec![EVAL_AGENT_TOOL.into()],
            ..AgentConfig::default()
        },
    };
    let scope =
        ScopeBuilder::build(&agent).map_err(|err| format!("case '{}': scope: {err}", case.id))?;
    let registry = AgentRegistry::new();
    registry.register(agent, scope);

    let config = Config {
        port: 3000,
        provider: ProviderKind::Ollama,
        ollama_url: server.url(),
        default_model: "llama3.2".into(),
        embedding_model: "nomic-embed-text".into(),
        embedding_dimensions: 768,
        rag_enabled: false,
        history_retention_days: 90,
        openrouter_api_key: None,
        opencode_api_key: None,
        opencode_url: "http://127.0.0.1:4096".into(),
        openai_base_url: None,
        ..Config::default()
    };
    let engine = RigAgentEngine::new(
        server.url(),
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
        config,
        Arc::clone(&skill_manager),
        Arc::new(crate::adapters::rig_engine::local_tools::LocalToolRegistry::builtins()),
        Arc::new(ApprovalGate::default()),
    );
    let enricher = ContextEnricher::new(
        Arc::new(ContextStore::new()),
        Arc::new(SkillManager::new()),
        "llama3.2".into(),
        Arc::new(ProviderRegistry::default()),
        ProviderKind::Ollama,
    );
    let runtime = AgentRuntime {
        registry: Arc::new(registry),
        engine: Arc::new(engine),
        history: None,
        rag: None,
        enricher: Arc::new(enricher),
        compactor: Arc::new(NoopCompactor),
        selector: Arc::new(crate::adapters::context::CurrentModelSelector),
    };

    // A prebuilt prompt keeps the turn off the enricher's disk path; the tool is
    // offered by the scope, not by the words in the system prompt.
    let mut rx = runtime
        .chat_stream(RuntimeChatRequest {
            project_id: None,
            agent_id: Some(agent_id),
            conversation_id: None,
            user_message: case.prompt.clone(),
            history: Vec::new(),
            model_override: None,
            system_prompt: Some("You are an eval fixture agent.".into()),
            default_model: "llama3.2".into(),
            execution_id: Uuid::new_v4(),
            max_iterations_override: None,
            shared_budget: None,
            delegation_context: None,
            approval_event_tx: None,
            tool_event_tx: None,
        })
        .await
        .map_err(|err| format!("case '{}': {err}", case.id))?;

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut failed_tools = Vec::new();
    let mut stream_error = None;
    while let Some(event) = rx.events.recv().await {
        match event {
            AgentRunEvent::Token { text: delta } => text.push_str(&delta),
            AgentRunEvent::ToolCall {
                server_id,
                tool_name,
                ..
            } => tool_calls.push(qualified_tool_name(&server_id, &tool_name)),
            AgentRunEvent::ToolResult {
                server_id,
                tool_name,
                is_error,
                ..
            } => {
                if is_error {
                    failed_tools.push(qualified_tool_name(&server_id, &tool_name));
                }
            }
            // `Completed` carries the authoritative full final text.
            AgentRunEvent::Completed { result } => text = result.text,
            AgentRunEvent::Error { message } => stream_error = Some(message),
            _ => {}
        }
    }
    if let Some(message) = stream_error {
        return Err(format!("case '{}': {message}", case.id));
    }
    if !failed_tools.is_empty() {
        return Err(format!(
            "case '{}': tool(s) failed to execute: {}",
            case.id,
            failed_tools.join(", ")
        ));
    }
    // A `called_tool` check with no observed call must not become a green SKIP.
    // Provider cases have no tool evidence by design; an agent turn that was
    // supposed to call a tool and did not is a real failure.
    let expects_tool = case
        .checks
        .iter()
        .any(|check| matches!(check, EvalCheck::CalledTool { .. }));
    if expects_tool && tool_calls.is_empty() {
        return Err(format!(
            "case '{}': no tool call was observed in the agent turn",
            case.id
        ));
    }

    Ok(Execution::Evidence(EvalEvidence { text, tool_calls }))
}

/// Agent cases without the `rig-engine` feature cannot run a tool loop. This is
/// a named failure rather than a fabricated pass; no agent cases ship in the v1
/// suite, so a light build stays green.
#[cfg(not(feature = "rig-engine"))]
async fn execute_agent_case(case: &EvalCase) -> Result<Execution, String> {
    Err(format!(
        "case '{}': agent cases require the rig-engine feature",
        case.id
    ))
}

/// Run one provider-class case against its own one-shot fixture server.
async fn execute_provider_case(
    case: &EvalCase,
    provider: &EvalProvider,
) -> Result<Execution, String> {
    if case.checks.is_empty() {
        return Err(format!("case '{}' declares no checks", case.id));
    }
    let kind = ProviderKind::from_str(&provider.kind)
        .map_err(|err| format!("case '{}': {err}", case.id))?;

    let answer = format!(
        "fixture answer: {}",
        case.expect.clone().unwrap_or_default()
    );
    let server = FixtureServer::start(recorded_body(kind, &answer)).await;
    let client = build_provider(kind, &server.url());
    let request = ChatRequest {
        model: String::new(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: case.prompt.clone(),
        }],
        stream: case.kind == EvalCaseKind::Stream,
    };

    let text = match case.kind {
        EvalCaseKind::Plain | EvalCaseKind::Tool => {
            let response = client
                .chat(request)
                .await
                .map_err(|err| format!("case '{}': {err}", case.id))?;
            response.message.content
        }
        EvalCaseKind::Stream => {
            let mut stream = match client.chat_stream(request).await {
                Ok(stream) => stream,
                // Providers without streaming report Unavailable without
                // connecting; that is their recorded, expected behavior.
                Err(err)
                    if err
                        .to_string()
                        .to_ascii_lowercase()
                        .contains("streaming is not supported") =>
                {
                    return Ok(Execution::Skip("streaming_unavailable".into()));
                }
                Err(err) => return Err(format!("case '{}': {err}", case.id)),
            };
            let mut text = String::new();
            while let Some(event) = stream.next().await {
                match event {
                    Ok(event) => text.push_str(&event.message.content),
                    Err(err) => return Err(format!("case '{}': {err}", case.id)),
                }
            }
            text
        }
        EvalCaseKind::Agent => unreachable!("agent cases are dispatched before this point"),
    };

    Ok(Execution::Evidence(EvalEvidence {
        text,
        tool_calls: Vec::new(),
    }))
}

/// Apply every check to one piece of evidence. A failure wins over a skip,
/// which wins over a pass. Shared by the provider and agent row builders.
async fn apply_checks(
    case: &EvalCase,
    evidence: &EvalEvidence,
    options: &RunOptions,
) -> EvalCheckOutcome {
    let mut failures = Vec::new();
    let mut skips = Vec::new();
    for check in &case.checks {
        let outcome = match check {
            EvalCheck::Judge { .. } if options.router.is_some() => {
                let router = options.router.as_ref().expect("router checked above");
                apply_judge(check, evidence, router, options.threshold).await
            }
            _ => apply_check(check, evidence),
        };
        match outcome {
            EvalCheckOutcome::Pass => {}
            EvalCheckOutcome::Fail { detail } => failures.push(detail),
            EvalCheckOutcome::Skipped { reason } => skips.push(reason),
        }
    }
    if !failures.is_empty() {
        EvalCheckOutcome::Fail {
            detail: redact_detail(&failures.join("; ")),
        }
    } else if !skips.is_empty() {
        EvalCheckOutcome::Skipped {
            reason: skips.join("; "),
        }
    } else {
        EvalCheckOutcome::Pass
    }
}

/// Turn a case execution into one report row, applying all of the case's checks.
async fn make_row(
    case: &EvalCase,
    provider: &str,
    result: Result<Execution, String>,
    options: &RunOptions,
) -> EvalRow {
    let base = |outcome| EvalRow {
        case: case.id.clone(),
        provider: provider.to_string(),
        kind: row_kind(case),
        outcome,
    };

    match result {
        Err(err) => base(EvalCheckOutcome::Fail {
            detail: redact_detail(&err),
        }),
        Ok(Execution::Skip(reason)) => base(EvalCheckOutcome::Skipped { reason }),
        Ok(Execution::Evidence(evidence)) => base(apply_checks(case, &evidence, options).await),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_are_strict_with_a_0_6_threshold_and_no_router() {
        let options = RunOptions::default();
        assert!(options.strict());
        assert!((options.threshold - 0.6).abs() < f32::EPSILON);
        assert!(options.router.is_none());
    }

    #[tokio::test]
    async fn an_empty_suite_reports_no_rows() {
        let report = run_suite(EvalSuite::default(), RunOptions::default()).await;
        assert!(report.rows.is_empty());
        assert!(report.all_passed(true));
    }
}
