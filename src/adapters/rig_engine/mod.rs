//! Rig.rs agent engine adapter (feature-gated wiring documented in ARCHITECTURE_PLAN).

#[cfg(feature = "rig-engine")]
pub mod approval;
#[cfg(feature = "rig-engine")]
pub mod builder;
pub mod limits;
pub mod local_tools;
pub mod operations;
#[cfg(feature = "rig-engine")]
pub mod tools;

#[cfg(feature = "rig-engine")]
use crate::adapters::rig_engine::local_tools::LocalToolDeps;
use crate::adapters::rig_engine::local_tools::{LocalTool, LocalToolRegistry};
use crate::api::observability::AppObservability;
use crate::config::{Config, ProviderKind, OPENROUTER_BASE_URL};
use crate::domain::scope::AgentScope;
use crate::ports::auto::AutoDecider;
use crate::ports::delegation::Delegator;
use crate::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult};
use crate::ports::execution::ExecutionStore;
use crate::ports::mcp::{McpCallRequest, McpClient, McpServerRegistry, McpToolDef};
use crate::ports::rag::RagStore;
use crate::providers::capabilities::capabilities_for;
use crate::providers::resolver::ResolvedModel;
use crate::services::approval::ApprovalGate;
use crate::services::skill_manager::SkillManager;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Which upstream the Rig engine talks to.
///
/// The key lives only inside the key-bearing variants. This type deliberately
/// has **no** `Debug` implementation: an engine target must never be logged or
/// formatted into a response, because doing so would leak the API key. Use
/// [`EngineTarget::kind`] for any diagnostic that does not need the secret.
#[derive(Clone)]
pub enum EngineTarget {
    Ollama { url: String },
    OpenRouter { api_key: String, base_url: String },
    OpenAiCompatible { api_key: String, base_url: String },
    Anthropic { api_key: String, base_url: String },
    Gemini { api_key: String, base_url: String },
}

/// Non-secret discriminator for a target, safe to log and to put in health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineTargetKind {
    Ollama,
    OpenRouter,
    OpenAiCompatible,
    Anthropic,
    Gemini,
}

impl EngineTarget {
    pub fn kind(&self) -> EngineTargetKind {
        match self {
            EngineTarget::Ollama { .. } => EngineTargetKind::Ollama,
            EngineTarget::OpenRouter { .. } => EngineTargetKind::OpenRouter,
            EngineTarget::OpenAiCompatible { .. } => EngineTargetKind::OpenAiCompatible,
            EngineTarget::Anthropic { .. } => EngineTargetKind::Anthropic,
            EngineTarget::Gemini { .. } => EngineTargetKind::Gemini,
        }
    }
}

/// Read a key by its *variable name*; the value stays opaque and is never
/// trimmed, matching how `Config` and the provider profiles treat credentials.
/// The names are the ones declared by the built-in profiles in
/// `providers/registry.rs`, so a profile and the engine read the same secret.
fn api_key_from_env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// Turn a possibly-empty configured base URL into an optional override. The
/// Anthropic and Gemini targets carry an empty string to mean "use rig's
/// native endpoint", so it must reach the wrapper as `None`, not `Some("")`.
#[cfg(feature = "rig-engine")]
fn optional_base_url(url: &str) -> Option<&str> {
    (!url.is_empty()).then_some(url)
}

/// Select the engine target from the single `PROVIDER` choice at boot.
///
/// Every kind with a Rig completion client gets a target here, including the
/// three native clients added later (OpenAI-compatible, Anthropic, Gemini).
/// This is only the *starting* target: each request rebuilds it from its own
/// resolved provider in `run_inner`/`stream_inner`, so an agent may select a
/// different upstream than the one that booted. OpenCode is the sole kind with
/// no Rig client and therefore keeps `None`.
pub fn engine_target_for(config: &Config) -> Option<EngineTarget> {
    // The global provider *is* the resolved one for a boot-time target.
    let resolved = ResolvedModel {
        provider: config.provider,
        model: config.default_model.clone(),
        capabilities: capabilities_for(config.provider, &config.default_model),
    };
    engine_target_for_resolved(config, &resolved)
}

/// Select the engine target for one resolved request.
///
/// The provider comes from `resolved`, never from `config.provider`: an agent
/// may name a different upstream than the global one, and building a client
/// from the global choice is exactly the bug this path exists to prevent. The
/// resolved model travels in [`AgentRunRequest::model`], not in a transport
/// target, so it is not copied here.
///
/// Keys are read from the env vars named by the built-in provider profiles and
/// stay inside the returned target: no key in a URL, a `Debug` output or health.
pub fn engine_target_for_resolved(
    config: &Config,
    resolved: &ResolvedModel,
) -> Option<EngineTarget> {
    match resolved.provider {
        ProviderKind::Ollama => Some(EngineTarget::Ollama {
            url: config.ollama_url.clone(),
        }),
        ProviderKind::OpenRouter => Some(EngineTarget::OpenRouter {
            api_key: config.openrouter_api_key.clone().unwrap_or_default(),
            base_url: config
                .openai_base_url
                .clone()
                .unwrap_or_else(|| OPENROUTER_BASE_URL.to_string()),
        }),
        ProviderKind::OpenAiCompatible => Some(EngineTarget::OpenAiCompatible {
            api_key: api_key_from_env("OPENAI_API_KEY"),
            // An empty base URL means "use the client's own endpoint"; Task 5
            // passes it as `None` to the wrapper.
            base_url: config.openai_base_url.clone().unwrap_or_default(),
        }),
        ProviderKind::Anthropic => Some(EngineTarget::Anthropic {
            api_key: api_key_from_env("ANTHROPIC_API_KEY"),
            // An empty base URL uses rig's native Anthropic endpoint; a
            // configured OPENAI_BASE_URL points the client at a compatible
            // gateway instead (also what makes the wire testable locally).
            base_url: config.openai_base_url.clone().unwrap_or_default(),
        }),
        ProviderKind::Gemini => Some(EngineTarget::Gemini {
            api_key: api_key_from_env("GEMINI_API_KEY"),
            // Same compatible-gateway override as Anthropic; empty means rig's
            // native endpoint.
            base_url: config.openai_base_url.clone().unwrap_or_default(),
        }),
        ProviderKind::OpenCode => None,
    }
}

/// MCP tool already filtered by scope, re-checked on every invoke.
pub struct ScopedMcpTool {
    pub def: McpToolDef,
    pub client: Arc<dyn McpClient>,
    pub scope: AgentScope,
}

impl ScopedMcpTool {
    pub async fn invoke(&self, args: Value) -> Result<String, String> {
        if !self.scope.allows_tool(&self.def.server_id, &self.def.name) {
            return Err(format!(
                "tool '{}/{}' denied by agent scope",
                self.def.server_id, self.def.name
            ));
        }

        tracing::info!(
            server_id = %self.def.server_id,
            tool = %self.def.name,
            "scoped MCP tool invoke"
        );

        let result = self
            .client
            .call_tool(McpCallRequest::new(
                self.def.server_id.clone(),
                self.def.name.clone(),
                args,
            ))
            .await?;

        if result.is_error {
            return Err(result.content.to_string());
        }
        Ok(result.content.to_string())
    }
}

/// A tool that survived scope filtering, ready to be wrapped in a `ToolDyn`
/// bridge. Both variants carry only public, scope-safe data; the bridges are
/// built per run in `builder::build_agent` and share one `ToolCallBudget`.
///
/// The MCP variant is intentionally unboxed: the catalog shape is part of this
/// task's interface, and one scoped tool per run is short-lived, so a heap hop
/// would buy nothing here.
#[allow(
    clippy::large_enum_variant,
    reason = "interface pins Mcp(ScopedMcpTool); one short-lived tool per run, boxing buys nothing"
)]
pub enum ScopedTool {
    Mcp(ScopedMcpTool),
    Local(LocalTool),
}

/// Engine implementation backed by a rig-core completion provider. The target
/// decides which provider that is; the Ollama-only original was hardwired to
/// `rig_core::providers::ollama`, which made an agent-scoped chat on another
/// provider call the wrong upstream and then silently fall back to a direct
/// chat (losing tools, RAG context and MCP).
pub struct RigAgentEngine {
    pub target: EngineTarget,
    pub mcp_registry: Arc<dyn McpServerRegistry>,
    pub rag: Option<Arc<dyn RagStore>>,
    /// Boot-time configuration, kept so each request can rebuild its target
    /// from the provider resolved *for that request* (`engine_target_for_resolved`).
    /// Holds credentials, so it is never logged, formatted or exposed through a
    /// `Debug` derive: use `target_kind()` for any non-secret diagnostic.
    pub config: Config,
    /// Loads a skill's `SKILL.md` when the model calls `llama_r__skill`.
    pub skill_manager: Arc<SkillManager>,
    /// The local-tool catalog `prepare` filters by scope. Built at boot from
    /// `tools/*.toml`, with the two built-ins always present.
    pub local_tools: Arc<LocalToolRegistry>,
    /// Human-approval gate. Owned by `AppState`, not by the hot-swapped stack:
    /// a TUI Apply rebuilds the engine but must not drop pending decisions.
    pub gate: Arc<ApprovalGate>,
    /// Optional automatic-approval decider. Owned by `AppState`, like the gate,
    /// so a settings Apply rebuilds the engine with the same `Auto` behaviour.
    /// `None` (the default for direct/test constructors) means every `Auto`
    /// tool keeps asking a human, as before Fase 6.
    pub auto_decider: Option<Arc<dyn AutoDecider>>,
    /// App-wide observability counters, shared with the gate via both tool
    /// bridges so `Auto` decisions are recorded in one place. Owned like the
    /// gate and decider, so a settings Apply keeps accumulating into the same
    /// counters. `None` for direct/test constructors: no counters to record.
    pub observability: Option<Arc<AppObservability>>,
    /// Durable execution store behind the tool-call idempotency guard. Built
    /// once at boot and shared, like the approval gate, so a settings Apply
    /// cannot lose the record of completed operations. A no-op store is the
    /// default: no `history` feature or a degraded boot simply replays nothing.
    pub execution_store: Arc<dyn ExecutionStore>,
    /// Bounded subagent delegator, injected from `AppState` through
    /// `CompanionHandles` so boot and a settings Apply share one handle. A
    /// `RuntimeDelegator` holds only a `Weak<AppState>`, so this cannot close
    /// the `AppState → engine → delegator → AppState` cycle. `None` for the
    /// many direct/test constructors, which then have no delegation tool.
    pub delegator: Option<Arc<dyn Delegator>>,
}

impl RigAgentEngine {
    /// Ergonomic Ollama constructor, kept for existing call sites.
    #[allow(
        clippy::too_many_arguments,
        reason = "companions are injected explicitly, as with `config` in the Fase-1 constructor change"
    )]
    pub fn new(
        ollama_url: impl Into<String>,
        mcp_registry: Arc<dyn McpServerRegistry>,
        rag: Option<Arc<dyn RagStore>>,
        config: Config,
        skill_manager: Arc<SkillManager>,
        local_tools: Arc<LocalToolRegistry>,
        gate: Arc<ApprovalGate>,
    ) -> Self {
        Self::from_target(
            EngineTarget::Ollama {
                url: ollama_url.into(),
            },
            mcp_registry,
            rag,
            config,
            skill_manager,
            local_tools,
            gate,
        )
    }

    /// Build an engine targeting OpenRouter. `base_url` overrides the default
    /// endpoint for a compatible gateway; `None` uses rig-core's default.
    #[allow(
        clippy::too_many_arguments,
        reason = "companions are injected explicitly, as with `config` in the Fase-1 constructor change"
    )]
    pub fn with_openrouter(
        api_key: impl Into<String>,
        base_url: Option<impl Into<String>>,
        mcp_registry: Arc<dyn McpServerRegistry>,
        rag: Option<Arc<dyn RagStore>>,
        config: Config,
        skill_manager: Arc<SkillManager>,
        local_tools: Arc<LocalToolRegistry>,
        gate: Arc<ApprovalGate>,
    ) -> Self {
        Self::from_target(
            EngineTarget::OpenRouter {
                api_key: api_key.into(),
                base_url: base_url
                    .map(Into::into)
                    .unwrap_or_else(|| OPENROUTER_BASE_URL.to_string()),
            },
            mcp_registry,
            rag,
            config,
            skill_manager,
            local_tools,
            gate,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "companions are injected explicitly, as with `config` in the Fase-1 constructor change"
    )]
    pub fn from_target(
        target: EngineTarget,
        mcp_registry: Arc<dyn McpServerRegistry>,
        rag: Option<Arc<dyn RagStore>>,
        config: Config,
        skill_manager: Arc<SkillManager>,
        local_tools: Arc<LocalToolRegistry>,
        gate: Arc<ApprovalGate>,
    ) -> Self {
        Self {
            target,
            mcp_registry,
            rag,
            config,
            skill_manager,
            local_tools,
            gate,
            auto_decider: None,
            observability: None,
            execution_store: Arc::new(crate::adapters::execution::NoOpExecutionStore),
            delegator: None,
        }
    }

    /// Install the boot-owned execution store. Kept as a chainable setter so
    /// the many existing engine constructors keep working with the no-op
    /// default; only the boot/Apply path needs durable operations.
    pub fn with_execution_store(mut self, store: Arc<dyn ExecutionStore>) -> Self {
        self.execution_store = store;
        self
    }

    /// Install the boot-owned `Auto` decider. A setter (rather than another
    /// constructor argument) keeps the many direct/test constructors unchanged:
    /// only the boot/Apply path supplies one, so an engine built in a test
    /// still routes every `Auto` tool to the human gate.
    pub fn with_auto_decider(mut self, decider: Option<Arc<dyn AutoDecider>>) -> Self {
        self.auto_decider = decider;
        self
    }

    /// Install the app-wide observability counters. A setter (like the decider's)
    /// keeps the many direct/test constructors unchanged; only the boot/Apply
    /// path supplies the `AppState` counters.
    pub fn with_observability(mut self, observability: Option<Arc<AppObservability>>) -> Self {
        self.observability = observability;
        self
    }

    /// Install the boot-owned delegator. A setter keeps every direct/test
    /// constructor unchanged: only the boot/Apply path supplies one.
    pub fn with_delegator(mut self, delegator: Option<Arc<dyn Delegator>>) -> Self {
        self.delegator = delegator;
        self
    }

    /// Non-secret target discriminator, safe for logs and health.
    pub fn target_kind(&self) -> EngineTargetKind {
        self.target.kind()
    }

    /// Prepare scoped tools and optional RAG context for a run.
    pub async fn prepare(
        &self,
        req: &AgentRunRequest,
    ) -> Result<(Vec<ScopedTool>, String), String> {
        let scope = &req.scope;

        // Tool discovery is gated on the capability of the model resolved for
        // *this* request. A model that cannot call tools (OpenCode, an unknown
        // or renamed model, …) must not be handed a tool it cannot use; skip
        // discovery entirely and say why, so the empty tool list is explainable
        // to an operator instead of looking like an empty MCP registry. The gate
        // sits above BOTH producers: a non-tool model sees no MCP and no local
        // tools, with the same warning.
        let mut tools: Vec<ScopedTool> = Vec::new();
        if req.capabilities.tool_calling {
            let tool_defs = self.mcp_registry.discover_scoped_tools(scope).await?;
            tools.reserve(tool_defs.len() + self.local_tools.tools().len());
            for def in tool_defs {
                let client = self.mcp_registry.client_for(&def.server_id).await?;
                tools.push(ScopedTool::Mcp(ScopedMcpTool {
                    def,
                    client,
                    scope: scope.clone(),
                }));
            }
            // Deny-all by default: a registered tool is not an offered tool.
            // The bridge re-checks this same predicate on every invocation.
            for local in self.local_tools.tools() {
                if scope.allows_local_tool(&local.id) {
                    tools.push(ScopedTool::Local(local.clone()));
                }
            }
        } else {
            tracing::warn!(
                agent = %req.agent.qualified_id(),
                model = %req.model,
                capability = "tool_calling",
                "skipping MCP tool discovery: the resolved model cannot call tools"
            );
        }

        let mut system = req.system_prompt.clone();
        if let Some(rag) = &self.rag {
            if !scope.rag_sources.is_empty() {
                let hits = rag
                    .query_scoped(scope, &req.user_message, 6)
                    .await
                    .unwrap_or_default();
                if !hits.is_empty() {
                    system.push_str("\n\n## Retrieved knowledge\n");
                    for (index, hit) in hits.iter().enumerate() {
                        system.push_str(&format!(
                            "\n### Chunk {} (source: {}, score: {:.3})\n{}\n",
                            index + 1,
                            hit.source_id,
                            hit.score,
                            hit.text
                        ));
                    }
                }
            }
        }

        Ok((tools, system))
    }

    /// Full run body (prepare + Rig prompt). Wrapped by timeout at the trait boundary.
    ///
    /// The client is chosen from the provider resolved for *this* request, not
    /// from `self.target` (the boot-time target). Using the boot target made an
    /// agent that names a different provider send its model to the wrong
    /// upstream; the engine is attached at boot regardless of `PROVIDER`, so the
    /// boot target is only a starting point, not the per-request answer.
    async fn run_inner(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        let (tools, system) = self.prepare(&req).await?;

        #[cfg(feature = "rig-engine")]
        {
            let target = self.target_for_request(&req)?;
            // The engine owns the executors' companions; each run clones them
            // into the bridges built for that request. The non-streaming path
            // has no SSE channel (`event_tx: None`), but the bridge still
            // registers the pending in the shared gate; the client discovers it
            // by polling GET /api/approvals and posts its decision there.
            let deps = LocalToolDeps {
                skill_manager: self.skill_manager.clone(),
                rag: self.rag.clone(),
                gate: self.gate.clone(),
                auto_decider: self.auto_decider.clone(),
                observability: self.observability.clone(),
                delegator: self.delegator.clone(),
                event_tx: None,
                operations: operations::OperationContext::new(
                    req.execution_id,
                    self.execution_store.clone(),
                ),
            };
            match &target {
                EngineTarget::Ollama { url } => {
                    builder::run_with_rig(url, req, system, tools, deps).await
                }
                EngineTarget::OpenRouter { api_key, base_url } => {
                    builder::run_with_openrouter(
                        api_key,
                        Some(base_url.as_str()),
                        req,
                        system,
                        tools,
                        deps,
                    )
                    .await
                }
                EngineTarget::OpenAiCompatible { api_key, base_url } => {
                    builder::run_with_openai_compatible(
                        api_key,
                        optional_base_url(base_url),
                        req,
                        system,
                        tools,
                        deps,
                    )
                    .await
                }
                EngineTarget::Anthropic { api_key, base_url } => {
                    builder::run_with_anthropic(
                        api_key,
                        optional_base_url(base_url),
                        req,
                        system,
                        tools,
                        deps,
                    )
                    .await
                }
                EngineTarget::Gemini { api_key, base_url } => {
                    builder::run_with_gemini(
                        api_key,
                        optional_base_url(base_url),
                        req,
                        system,
                        tools,
                        deps,
                    )
                    .await
                }
            }
        }

        #[cfg(not(feature = "rig-engine"))]
        {
            let _ = (system, tools);
            Err("feature `rig-engine` disabled".into())
        }
    }

    /// Build the engine target from the provider/capabilities resolved for one
    /// request. `None` means the resolved provider has no Rig completion client
    /// (OpenCode), which is a clear error rather than a silent boot-client call.
    #[cfg(feature = "rig-engine")]
    fn target_for_request(&self, req: &AgentRunRequest) -> Result<EngineTarget, String> {
        engine_target_for_resolved(
            &self.config,
            &ResolvedModel {
                provider: req.provider,
                model: req.model.clone(),
                capabilities: req.capabilities.clone(),
            },
        )
        .ok_or_else(|| {
            format!(
                "no Rig engine client for provider '{}': it has no completion client (OpenCode); an agent using it cannot run through the engine",
                req.provider
            )
        })
    }

    /// Streaming body: prepare scoped tools/context, then drive the real Rig
    /// streaming client for the resolved target. Wrapped by a timeout at the
    /// `AgentEngine` boundary. A target with no streaming model is refused with
    /// a named error — never a single full-text token pretending to be a
    /// stream.
    async fn stream_inner(
        &self,
        req: AgentRunRequest,
        tx: mpsc::Sender<AgentRunEvent>,
    ) -> Result<(), String> {
        let (tools, system) = self.prepare(&req).await?;

        #[cfg(feature = "rig-engine")]
        {
            let target = self.target_for_request(&req)?;
            // The caller's event channel is the one the SSE stream reads. A
            // bridge awaiting approval emits `ApprovalRequired` on it, so the
            // human sees the id/hash and can decide while the request stays open.
            let deps = LocalToolDeps {
                skill_manager: self.skill_manager.clone(),
                rag: self.rag.clone(),
                gate: self.gate.clone(),
                auto_decider: self.auto_decider.clone(),
                observability: self.observability.clone(),
                delegator: self.delegator.clone(),
                event_tx: Some(tx.clone()),
                operations: operations::OperationContext::new(
                    req.execution_id,
                    self.execution_store.clone(),
                ),
            };
            match &target {
                EngineTarget::Ollama { url } => {
                    builder::stream_with_rig(url, req, system, tools, &tx, deps).await
                }
                EngineTarget::OpenRouter { .. } => Err(builder::openrouter_streaming_unavailable()),
                EngineTarget::OpenAiCompatible { api_key, base_url } => {
                    builder::stream_with_openai_compatible(
                        api_key,
                        optional_base_url(base_url),
                        req,
                        system,
                        tools,
                        &tx,
                        deps,
                    )
                    .await
                }
                EngineTarget::Anthropic { api_key, base_url } => {
                    builder::stream_with_anthropic(
                        api_key,
                        optional_base_url(base_url),
                        req,
                        system,
                        tools,
                        &tx,
                        deps,
                    )
                    .await
                }
                EngineTarget::Gemini { api_key, base_url } => {
                    builder::stream_with_gemini(
                        api_key,
                        optional_base_url(base_url),
                        req,
                        system,
                        tools,
                        &tx,
                        deps,
                    )
                    .await
                }
            }
        }

        #[cfg(not(feature = "rig-engine"))]
        {
            let _ = (req, system, tools, tx);
            Err("feature `rig-engine` disabled".into())
        }
    }
}

/// Extra room above the approval gate's own deadline when a request carries
/// approval rules. The gate's `tokio::time::timeout` in the bridge must fire
/// first so an undecided approval becomes a *named tool error* that rig feeds
/// back into the tool loop. If the engine-level timeout fired instead it would
/// drop the bridge's pending oneshot (a late decision would be a no-op) and, on
/// the non-streaming path, hand chat_core an `Err` that triggers its legacy
/// direct-chat fallback — a gated tool silently bypassed.
const APPROVAL_TIMEOUT_MARGIN: Duration = Duration::from_secs(5);

/// Effective whole-run timeout at the `AgentEngine` boundary.
///
/// Without approval rules the scope deadline is unchanged. With them the run
/// may legitimately block on a human for up to the gate's deadline, so the
/// engine timeout is the larger of the scope deadline and `gate + margin`,
/// guaranteeing the bridge's named refusal wins before this outer deadline.
fn effective_run_timeout(scope: &AgentScope, gate_timeout: Duration) -> Duration {
    let scope_timeout = Duration::from_secs(scope.timeout_secs.max(1));
    if scope.permission.approvals.is_empty() {
        scope_timeout
    } else {
        scope_timeout.max(gate_timeout + APPROVAL_TIMEOUT_MARGIN)
    }
}

#[async_trait]
impl AgentEngine for RigAgentEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        let timeout = effective_run_timeout(&req.scope, self.gate.timeout());
        match tokio::time::timeout(timeout, self.run_inner(req)).await {
            Ok(result) => result,
            Err(_) => Err(format!("agent run timed out after {}s", timeout.as_secs())),
        }
    }

    async fn run_stream(
        &self,
        req: AgentRunRequest,
    ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
        let (tx, rx) = mpsc::channel(32);
        // Clone the boot target and config: the spawned task rebuilds the real
        // target from the request's resolved provider. The config is cloned too
        // so that per-request target selection sees the same credentials.
        let target = self.target.clone();
        let config = self.config.clone();
        let mcp_registry = self.mcp_registry.clone();
        let rag = self.rag.clone();
        let skill_manager = self.skill_manager.clone();
        let local_tools = self.local_tools.clone();
        // The gate is shared, not cloned into a new one: pending approvals from
        // other requests must survive and must be decidable while this stream
        // is open. AppState owns the single instance.
        let gate = self.gate.clone();
        let auto_decider = self.auto_decider.clone();
        let observability = self.observability.clone();
        let execution_store = self.execution_store.clone();
        let delegator = self.delegator.clone();
        let timeout = effective_run_timeout(&req.scope, gate.timeout());

        tokio::spawn(async move {
            let engine = RigAgentEngine {
                target,
                mcp_registry,
                rag,
                config,
                skill_manager,
                local_tools,
                gate,
                auto_decider,
                observability,
                execution_store,
                delegator,
            };
            // The timeout covers the whole stream lifecycle, not just a single
            // completion, and stays at the trait boundary as before. With
            // approval rules it outlives the gate deadline so the bridge's
            // named refusal fires (and rig continues the turn) before this
            // outer deadline can drop the pending oneshot.
            let stream = engine.stream_inner(req, tx.clone());
            match tokio::time::timeout(timeout, stream).await {
                Ok(Ok(())) => {}
                Ok(Err(msg)) => {
                    let _ = tx.send(AgentRunEvent::Error { message: msg }).await;
                }
                Err(_) => {
                    let _ = tx
                        .send(AgentRunEvent::Error {
                            message: format!("agent stream timed out after {}s", timeout.as_secs()),
                        })
                        .await;
                }
            }
        });

        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::RagWritePolicy;
    use crate::domain::policy::{ApprovalPolicy, ApprovalRule, PermissionPolicy};
    use std::collections::HashSet;

    fn scope(timeout_secs: u64, permission: PermissionPolicy) -> AgentScope {
        AgentScope {
            agent_id: "a".into(),
            project_id: None,
            mcp_sources: HashSet::new(),
            tools_allow: crate::domain::scope::ToolsAllow::DenyAll,
            local_tools_allow: crate::domain::scope::ToolsAllow::DenyAll,
            rag_sources: HashSet::new(),
            rag_write: RagWritePolicy::None,
            max_tool_calls: 1,
            max_iterations: 2,
            timeout_secs,
            permission,
        }
    }

    fn approvals() -> PermissionPolicy {
        PermissionPolicy {
            connections_allow: Vec::new(),
            approvals: vec![ApprovalRule {
                tool_pattern: "*".into(),
                policy: ApprovalPolicy::Always,
            }],
            auto_threshold: None,
        }
    }

    #[test]
    fn effective_timeout_without_approvals_is_the_scope_deadline() {
        // No approval rules: the engine deadline must stay at the short scope
        // value even though the gate's own timeout is much longer.
        let gate = Duration::from_secs(120);
        let scope = scope(1, PermissionPolicy::default());
        assert_eq!(effective_run_timeout(&scope, gate), Duration::from_secs(1));
    }

    #[test]
    fn effective_timeout_with_approvals_outlives_the_gate_deadline() {
        // With approval rules the outer deadline must exceed the gate's 2s by
        // the margin, so the bridge's named refusal wins before this timeout.
        let gate = Duration::from_secs(2);
        let scope = scope(1, approvals());
        let effective = effective_run_timeout(&scope, gate);
        assert!(effective > gate, "gate must fire first: {effective:?}");
        assert_eq!(effective, gate + APPROVAL_TIMEOUT_MARGIN);
    }

    #[test]
    fn effective_timeout_with_approvals_never_shrinks_a_longer_scope_deadline() {
        // The real 90s scope vs 120s gate case takes the gate branch.
        let gate = Duration::from_secs(120);
        let short = scope(90, approvals());
        assert_eq!(
            effective_run_timeout(&short, gate),
            gate + APPROVAL_TIMEOUT_MARGIN
        );
        // A scope deadline already longer than gate+margin is kept as-is.
        let long = scope(200, approvals());
        assert_eq!(effective_run_timeout(&long, gate), Duration::from_secs(200));
    }

    #[test]
    fn effective_timeout_clamps_zero_scope_to_one_second() {
        let gate = Duration::from_secs(2);
        let scope = scope(0, PermissionPolicy::default());
        assert_eq!(effective_run_timeout(&scope, gate), Duration::from_secs(1));
    }
}
