//! Declarative in-process tools the model may invoke, mirroring the MCP side.
//!
//! The registry is deliberately small: it holds typed definitions (id, JSON
//! Schema, executor kind) and knows nothing about Rig or about how an executor
//! runs. The `ToolDyn` bridge is where the Rig-specific surface lives, and it
//! repeats the two disciplines `McpToolBridge` already enforces — a fresh scope
//! check and a charged `ToolCallBudget` on every call.
//!
//! Two executors live behind the bridge: `BuiltinSkill` loads a `SKILL.md` from
//! the `SkillManager`, and `RagQuery` runs a scope-bounded RAG query. Both hold
//! the same two disciplines as MCP: the scope is re-checked and the shared
//! `ToolCallBudget` is charged before any executor runs. A missing skill, an
//! empty RAG scope or a missing store is a named refusal, never a silent no-op
//! that looks like a successful tool call.

use crate::error::AppError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::Path;

/// The two ids are stable API: agent manifests name them in `tools_local`.
pub const BUILTIN_SKILL_ID: &str = "llama_r__skill";
pub const BUILTIN_RAG_QUERY_ID: &str = "llama_r__rag_query";

/// One tool the model can call. `id` is the name in the Rig tool catalog and the
/// key `AgentScope::allows_local_tool` checks; it is the file stem for a
/// manifest-defined tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalTool {
    pub id: String,
    pub description: String,
    pub input_schema: Value,
    pub kind: LocalToolKind,
}

/// Which in-process executor a tool dispatches to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalToolKind {
    /// Load a skill's `SKILL.md` on demand (`SkillManager::get_skill`).
    BuiltinSkill,
    /// Query the RAG collections the scope may read.
    ///
    /// `source_id` is informational from a `tools/*.toml` manifest: it is kept
    /// for display and future use, but the query is always scoped via the
    /// request's [`AgentScope`], which is the authoritative isolation boundary.
    /// A manifest can never widen a query beyond `scope.rag_sources`.
    RagQuery { source_id: Option<String> },
}

/// Built-ins plus any `tools/*.toml` overlay found under the base directory.
#[derive(Debug, Clone)]
pub struct LocalToolRegistry {
    tools: Vec<LocalTool>,
}

impl Default for LocalToolRegistry {
    fn default() -> Self {
        Self::builtins()
    }
}

impl LocalToolRegistry {
    /// The two tools that always exist. They are *registered*, not offered:
    /// `prepare` still filters them by `scope.allows_local_tool`.
    pub fn builtins() -> Self {
        Self {
            tools: vec![
                LocalTool {
                    id: BUILTIN_SKILL_ID.to_string(),
                    description: "Load the full content of a skill by name, on demand.".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": { "name": { "type": "string" } },
                        "required": ["name"]
                    }),
                    kind: LocalToolKind::BuiltinSkill,
                },
                LocalTool {
                    id: BUILTIN_RAG_QUERY_ID.to_string(),
                    description: "Search the agent's allowed RAG sources.".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" },
                            "top_k": { "type": "integer" }
                        },
                        "required": ["query"]
                    }),
                    kind: LocalToolKind::RagQuery { source_id: None },
                },
            ],
        }
    }

    /// Built-ins first, then overlay each well-formed manifest in `tools/`.
    ///
    /// A missing directory is fine (most deployments write no manifests). A
    /// malformed file is an [`AppError::Config`] naming its path; the caller in
    /// `build_companions` logs it and falls back to `builtins()`, the same
    /// non-fatal pattern used for `router.toml` and `providers.toml`.
    pub fn load() -> Result<Self, AppError> {
        let mut registry = Self::builtins();
        let dir = crate::core::paths::get_base_dir().join("tools");
        if !dir.exists() {
            return Ok(registry);
        }
        let entries = fs::read_dir(&dir).map_err(|err| {
            AppError::Config(format!("{} could not be read: {}", dir.display(), err))
        })?;

        // Deterministic order: `read_dir` has no guaranteed order, and a
        // collision on an id must not depend on filesystem luck.
        let mut paths: Vec<_> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .collect();
        paths.sort();

        for path in paths {
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let manifest = Self::read_manifest(&path)?;
            let id = manifest
                .id
                .clone()
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(|| {
                    path.file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or_default()
                        .to_string()
                });
            if id.is_empty() {
                return Err(AppError::Config(format!(
                    "{}: a local tool needs a non-empty id",
                    path.display()
                )));
            }
            let kind = Self::kind_from_manifest(&path, &manifest)?;
            let tool = LocalTool {
                id,
                description: manifest.description.unwrap_or_default(),
                input_schema: manifest
                    .input_schema
                    .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}})),
                kind,
            };
            // A manifest may deliberately override a built-in (same id).
            if let Some(existing) = registry.tools.iter_mut().find(|t| t.id == tool.id) {
                *existing = tool;
            } else {
                registry.tools.push(tool);
            }
        }
        Ok(registry)
    }

    fn read_manifest(path: &Path) -> Result<LocalToolManifest, AppError> {
        let raw = fs::read_to_string(path).map_err(|err| {
            AppError::Config(format!("{} could not be read: {}", path.display(), err))
        })?;
        toml::from_str(&raw).map_err(|err| {
            AppError::Config(format!("{} is not valid TOML: {}", path.display(), err))
        })
    }

    fn kind_from_manifest(
        path: &Path,
        manifest: &LocalToolManifest,
    ) -> Result<LocalToolKind, AppError> {
        match manifest.tool_type.as_str() {
            "builtin:skill" | "skill" => Ok(LocalToolKind::BuiltinSkill),
            "rag_query" => Ok(LocalToolKind::RagQuery {
                source_id: manifest
                    .source_id
                    .clone()
                    .filter(|id| !id.trim().is_empty()),
            }),
            other => Err(AppError::Config(format!(
                "{}: unknown tool type '{}' (expected 'builtin:skill' or 'rag_query')",
                path.display(),
                other
            ))),
        }
    }

    /// Every registered tool, built-ins included. Callers must still apply the
    /// scope check before offering one to a model.
    pub fn tools(&self) -> &[LocalTool] {
        &self.tools
    }
}

/// The shape of `tools/*.toml`. Every key but `type` is optional so a small
/// manifest stays small; `input_schema` is a serde_json value deserialised
/// straight from the TOML table.
#[derive(Debug, Deserialize)]
struct LocalToolManifest {
    #[serde(rename = "type")]
    tool_type: String,
    id: Option<String>,
    description: Option<String>,
    input_schema: Option<Value>,
    source_id: Option<String>,
}

#[cfg(feature = "rig-engine")]
mod bridge {
    use super::*;
    use crate::adapters::rig_engine::approval;
    use crate::adapters::rig_engine::limits::ToolCallBudget;
    use crate::adapters::rig_engine::operations::{self, OperationContext};
    use crate::domain::scope::AgentScope;
    use crate::ports::engine::AgentRunEvent;
    use crate::ports::rag::RagStore;
    use crate::services::approval::ApprovalGate;
    use crate::services::skill_manager::SkillManager;
    use rig_core::tool::{ToolDyn, ToolError};
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex};

    /// Upper bound — and the default — for `rag_query`'s `top_k`. The model must
    /// not be able to ask for an unbounded slice of the knowledge base through
    /// a tool call; `prepare` uses the same ceiling for injected context.
    const RAG_QUERY_TOP_K_CAP: usize = 6;

    /// The in-process companions the executors need, cloned from the engine into
    /// every bridge built for a run. `SkillManager` loads a skill's `SKILL.md`;
    /// `RagStore` answers scope-bounded queries (`None` when RAG is disabled).
    #[derive(Clone)]
    pub struct LocalToolDeps {
        pub skill_manager: Arc<SkillManager>,
        pub rag: Option<Arc<dyn RagStore>>,
        /// The run's approval gate, owned by `AppState` so a settings Apply
        /// cannot drop a pending decision. Also read by the MCP bridge via
        /// `build_tool_bridges`, so both bridge kinds share one gate.
        pub gate: Arc<ApprovalGate>,
        /// `Some` on the streaming path (the `ApprovalRequired` event reaches
        /// the SSE caller); `None` on the non-streaming path.
        pub event_tx: Option<mpsc::Sender<AgentRunEvent>>,
        /// The turn's durable operation context, shared with the MCP bridge.
        /// Carries the `execution_id` the operation id is scoped to.
        pub operations: OperationContext,
    }

    /// One `ToolDyn` per registered local tool. Holds the definition, the scope
    /// of the request that offered it, the budget shared with MCP bridges and
    /// the executor dependencies.
    pub struct LocalToolBridge {
        pub tool: LocalTool,
        pub scope: AgentScope,
        pub budget: Arc<Mutex<ToolCallBudget>>,
        pub deps: LocalToolDeps,
        /// Cloned from `deps` so the gate is an explicit part of the bridge,
        /// mirroring `McpToolBridge`.
        pub gate: Arc<ApprovalGate>,
        pub event_tx: Option<mpsc::Sender<AgentRunEvent>>,
        /// The same idempotency guard the MCP bridge uses. `Some` for every
        /// bridge built by the engine; a directly built test bridge can leave
        /// it out and execute without a store.
        pub operations: Option<OperationContext>,
    }

    impl LocalToolBridge {
        pub fn new(
            tool: LocalTool,
            scope: AgentScope,
            budget: Arc<Mutex<ToolCallBudget>>,
            deps: LocalToolDeps,
        ) -> Self {
            let gate = deps.gate.clone();
            let event_tx = deps.event_tx.clone();
            let operations = Some(deps.operations.clone());
            Self {
                tool,
                scope,
                budget,
                deps,
                gate,
                event_tx,
                operations,
            }
        }

        pub async fn invoke_json(&self, args: Value) -> Result<String, String> {
            // Defense in depth, exactly as on the MCP bridge: re-check the
            // scope before charging the budget or dispatching anything.
            if !self.scope.allows_local_tool(&self.tool.id) {
                return Err(format!(
                    "local tool '{}' denied by agent scope",
                    self.tool.id
                ));
            }

            // Authorize (possibly blocking on a human) before charging the
            // shared budget. A denial/expiry/args-change spends nothing. A
            // local tool's bare id is the name approval rules match on.
            approval::authorize(
                &self.gate,
                self.event_tx.as_ref(),
                &self.scope.permission,
                &self.tool.id,
                &args,
                self.operations.as_ref().map(|ctx| ctx.execution_id),
            )
            .await?;

            {
                let mut b = self.budget.lock().await;
                b.try_consume()?;
            }

            // The durable guard wraps only the actual dispatch: scope, approval
            // and the shared budget are still enforced above it on every call.
            // A replay returns the stored result without touching the executor.
            let tool_id = self.tool.id.clone();
            let dispatch_args = args.clone();
            operations::run_idempotent(self.operations.as_ref(), &tool_id, &args, async {
                match &self.tool.kind {
                    LocalToolKind::BuiltinSkill => {
                        Self::exec_skill(&self.deps, &tool_id, &dispatch_args)
                    }
                    LocalToolKind::RagQuery { source_id } => {
                        // `source_id` is informational; scope stays authoritative.
                        self.exec_rag_query(source_id.as_deref(), &dispatch_args)
                            .await
                    }
                }
            })
            .await
        }

        /// Load a declared skill's full `SKILL.md` on demand. An unknown or
        /// empty id is a named error carrying the id — never a panic or `""`.
        fn exec_skill(deps: &LocalToolDeps, tool_id: &str, args: &Value) -> Result<String, String> {
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| {
                    format!("local tool '{tool_id}' requires a non-empty string 'name' argument")
                })?;
            match deps.skill_manager.get_skill(name) {
                Some(skill) => Ok(skill.content),
                None => Err(format!(
                    "local tool '{tool_id}': unknown skill '{name}' — check the agent's declared skills"
                )),
            }
        }

        /// Run a RAG query bounded by the request scope. The scope check here is
        /// deliberate even though `prepare` already filtered with it: it is the
        /// isolation boundary, and `RagStore::query_scoped` enforces the same
        /// contract. With no allowed sources (or no store) we refuse without
        /// touching the store at all.
        async fn exec_rag_query(
            &self,
            source_id: Option<&str>,
            args: &Value,
        ) -> Result<String, String> {
            let tool_id = &self.tool.id;
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .filter(|query| !query.trim().is_empty())
                .ok_or_else(|| {
                    format!("local tool '{tool_id}' requires a non-empty string 'query' argument")
                })?;

            if self.scope.rag_sources.is_empty() {
                return Err(format!(
                    "local tool '{tool_id}': the agent scope has no rag_sources; refusing to query (source_id '{}' is informational)",
                    source_id.unwrap_or("<none>")
                ));
            }

            let top_k = args
                .get("top_k")
                .and_then(Value::as_u64)
                .unwrap_or(RAG_QUERY_TOP_K_CAP as u64)
                .clamp(1, RAG_QUERY_TOP_K_CAP as u64) as usize;

            let rag = self.deps.rag.as_ref().ok_or_else(|| {
                format!("local tool '{tool_id}': no RAG store is configured (RAG is disabled)")
            })?;
            let chunks = rag.query_scoped(&self.scope, query, top_k).await?;

            // Project to JSON explicitly: `RagChunk` is a domain port type
            // without a `Serialize` derive, and this shape keeps the full chunk
            // (including metadata) in the tool result without changing the port.
            let view: Vec<Value> = chunks
                .into_iter()
                .map(|chunk| {
                    serde_json::json!({
                        "id": chunk.id,
                        "source_id": chunk.source_id,
                        "text": chunk.text,
                        "score": chunk.score,
                        "metadata": chunk.metadata,
                    })
                })
                .collect();
            serde_json::to_string(&view).map_err(|err| {
                format!("local tool '{tool_id}': failed to serialize RAG results: {err}")
            })
        }
    }

    impl ToolDyn for LocalToolBridge {
        fn name(&self) -> String {
            self.tool.id.clone()
        }

        fn description(&self) -> String {
            if self.tool.description.is_empty() {
                format!("Local tool {}", self.tool.id)
            } else {
                self.tool.description.clone()
            }
        }

        fn parameters(&self) -> Value {
            if self.tool.input_schema.is_object() {
                self.tool.input_schema.clone()
            } else {
                serde_json::json!({"type": "object", "properties": {}})
            }
        }

        fn call<'a>(
            &'a self,
            args: String,
        ) -> rig_core::wasm_compat::WasmBoxedFuture<'a, Result<String, ToolError>> {
            Box::pin(async move {
                let args_val: Value = serde_json::from_str(&args).map_err(ToolError::JsonError)?;
                self.invoke_json(args_val)
                    .await
                    .map_err(|e| ToolError::ToolCallError(Box::new(std::io::Error::other(e))))
            })
        }
    }
}

#[cfg(feature = "rig-engine")]
pub use bridge::{LocalToolBridge, LocalToolDeps};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_schemas_are_typed_objects() {
        let registry = LocalToolRegistry::builtins();
        assert_eq!(registry.tools().len(), 2);
        for tool in registry.tools() {
            assert_eq!(tool.input_schema["type"], Value::String("object".into()));
            assert!(tool.input_schema["properties"].is_object());
        }
    }
}
