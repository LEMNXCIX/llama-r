//! Declarative in-process tools the model may invoke, mirroring the MCP side.
//!
//! The registry is deliberately small: it holds typed definitions (id, JSON
//! Schema, executor kind) and knows nothing about Rig or about how an executor
//! runs. The `ToolDyn` bridge is where the Rig-specific surface lives, and it
//! repeats the two disciplines `McpToolBridge` already enforces — a fresh scope
//! check and a charged `ToolCallBudget` on every call.
//!
//! Executor bodies land with the skill/rag wiring in the next piece. Until then
//! an otherwise authorised call returns a named "executor not wired" error: it
//! is a refusal, never a silent no-op that looks like a successful tool call.

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
    /// Query the RAG collections the scope may read. `source_id` pins one
    /// collection; `None` means "the scope's first allowed rag_source".
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
    use crate::adapters::rig_engine::limits::ToolCallBudget;
    use crate::domain::scope::AgentScope;
    use rig_core::tool::{ToolDyn, ToolError};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// One `ToolDyn` per registered local tool. Holds the definition, the scope
    /// of the request that offered it and the budget shared with MCP bridges.
    pub struct LocalToolBridge {
        pub tool: LocalTool,
        pub scope: AgentScope,
        pub budget: Arc<Mutex<ToolCallBudget>>,
    }

    impl LocalToolBridge {
        pub fn new(tool: LocalTool, scope: AgentScope, budget: Arc<Mutex<ToolCallBudget>>) -> Self {
            Self {
                tool,
                scope,
                budget,
            }
        }

        pub async fn invoke_json(&self, _args: Value) -> Result<String, String> {
            // Defense in depth, exactly as on the MCP bridge: re-check the
            // scope before charging the budget or dispatching anything.
            if !self.scope.allows_local_tool(&self.tool.id) {
                return Err(format!(
                    "local tool '{}' denied by agent scope",
                    self.tool.id
                ));
            }

            {
                let mut b = self.budget.lock().await;
                b.try_consume()?;
            }

            // Task 4 replaces this with the skill/rag executors. A named error
            // is deliberate: a silent Ok("") would look like a real result.
            Err(format!(
                "local tool '{}' executor is not wired yet",
                self.tool.id
            ))
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
pub use bridge::LocalToolBridge;

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
