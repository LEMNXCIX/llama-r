//! Agent domain model and TOML manifest schema (v1 + v2 scope fields).
//!
//! New scope fields are optional with **safe defaults** (deny-all tools, no RAG).
//! Existing agent TOML files continue to deserialize without changes.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentConfig {
    pub name: String,
    /// Human-readable description for UIs and MCP tool listings.
    #[serde(default)]
    pub description: String,
    /// Model to use. If empty, falls back to the global DEFAULT_MODEL.
    #[serde(default)]
    pub model: String,
    /// Provider hint for multi-provider routing (`ollama` today).
    #[serde(default = "default_provider")]
    pub provider: String,
    pub system_prompt: String,
    /// Optional: link to a project context for richer prompts.
    #[serde(default)]
    pub context_project: Option<String>,
    /// Additional context files to inject (paths relative to the Llama-R root).
    #[serde(default)]
    pub context_files: Vec<String>,
    /// Agent-specific operating rules injected into the final system prompt.
    #[serde(default)]
    pub rules: Vec<String>,
    /// Agent-specific skills to load from the shared skill registry.
    #[serde(default)]
    pub skills: Vec<String>,
    /// System-managed skills selected from project analysis.
    #[serde(default)]
    pub auto_skills: Vec<String>,
    /// Local (in-process) tools this agent may call. Empty = deny-all;
    /// `["*"]` = all local tools; otherwise an explicit allowlist by id.
    #[serde(default)]
    pub tools_local: Vec<String>,
    /// Subset of `skills`/`auto_skills` announced by name and description only.
    /// The body is loaded on demand via the `llama_r__skill` local tool.
    #[serde(default)]
    pub deferred_skills: Vec<String>,
    /// Dynamic template variables. Use {{var_name}} in system_prompt.
    #[serde(default)]
    pub variables: HashMap<String, String>,
    #[serde(default = "default_context_budget")]
    pub max_context_tokens: usize,
    #[serde(default)]
    pub optimize: OptimizeConfig,

    // --- Scope isolation (manifest v2) ---
    /// MCP server ids this agent may discover tools from.
    #[serde(default)]
    pub mcp_sources: Vec<String>,
    /// Tool allowlist. Empty = deny-all. `["*"]` = all tools from `mcp_sources`.
    #[serde(default)]
    pub tools_override: Vec<String>,
    /// Knowledge bases this agent may query (never implicit global access).
    #[serde(default)]
    pub rag_sources: Vec<String>,
    /// Write policy for RAG upserts.
    #[serde(default)]
    pub rag_write: RagWritePolicy,
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub temperature: Option<f32>,

    /// Compatibility stub: old manifests may carry a `[subagents]` table, but
    /// the runtime never read it. Accept and ignore it so those files still load.
    ///
    /// `pub` so test fixtures that build `AgentConfig` with a struct literal
    /// (including integration crates) can leave it at `None`. It is never read
    /// outside this module and never serialised.
    #[serde(default, skip_serializing)]
    pub subagents: Option<toml::Table>,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub observability: AgentObservabilityConfig,
    /// Curated routing text for the agent router. `None` (the default) means
    /// the router derives the text from the rest of the manifest.
    #[serde(default)]
    pub routing: Option<RoutingConfig>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            model: String::new(),
            provider: default_provider(),
            system_prompt: String::new(),
            context_project: None,
            context_files: Vec::new(),
            rules: Vec::new(),
            skills: Vec::new(),
            auto_skills: Vec::new(),
            tools_local: Vec::new(),
            deferred_skills: Vec::new(),
            variables: HashMap::new(),
            max_context_tokens: default_context_budget(),
            optimize: OptimizeConfig::default(),
            mcp_sources: Vec::new(),
            tools_override: Vec::new(),
            rag_sources: Vec::new(),
            rag_write: RagWritePolicy::default(),
            max_tool_calls: default_max_tool_calls(),
            max_iterations: default_max_iterations(),
            timeout_secs: default_timeout_secs(),
            temperature: None,
            subagents: None,
            memory: MemoryConfig::default(),
            observability: AgentObservabilityConfig::default(),
            routing: None,
        }
    }
}

fn default_provider() -> String {
    "ollama".to_string()
}

fn default_context_budget() -> usize {
    4096
}

fn default_max_tool_calls() -> u32 {
    8
}

fn default_max_iterations() -> u32 {
    6
}

fn default_timeout_secs() -> u64 {
    90
}

fn default_true() -> bool {
    true
}

fn default_summarize_every() -> u32 {
    10
}

fn default_retention_days() -> u32 {
    90
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, ToSchema)]
pub struct OptimizeConfig {
    pub enabled: bool,
    #[serde(default)]
    pub rules: Vec<String>,
}

/// Controls where an agent may write RAG documents.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RagWritePolicy {
    /// No writes allowed (default).
    #[default]
    None,
    /// Only the agent's own memory collection.
    OwnMemoryOnly,
    /// Any collection listed in `rag_sources`.
    Listed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub persist_history: bool,
    #[serde(default = "default_summarize_every")]
    pub summarize_every_n_turns: u32,
    #[serde(default)]
    pub index_summaries: bool,
    #[serde(default)]
    pub summary_collection: Option<String>,
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            persist_history: true,
            summarize_every_n_turns: default_summarize_every(),
            index_summaries: false,
            summary_collection: None,
            retention_days: default_retention_days(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentObservabilityConfig {
    #[serde(default = "default_true")]
    pub trace: bool,
    /// Never enable in production for agents that handle secrets/PII.
    #[serde(default)]
    pub log_tool_args: bool,
    #[serde(default)]
    pub log_tool_results: bool,
}

impl Default for AgentObservabilityConfig {
    fn default() -> Self {
        Self {
            trace: true,
            log_tool_args: false,
            log_tool_results: false,
        }
    }
}

/// Curated routing text. `hint` replaces the derived description when present.
#[derive(Debug, Serialize, Deserialize, Clone, Default, utoipa::ToSchema)]
pub struct RoutingConfig {
    #[serde(default)]
    pub hint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Agent {
    pub id: String, // Usually the filename without .toml
    pub project_id: Option<String>,
    pub config: AgentConfig,
}

impl Agent {
    pub fn qualified_id(&self) -> String {
        match &self.project_id {
            Some(project_id) => format!("{}/{}", project_id, self.id),
            None => self.id.clone(),
        }
    }

    /// Canonical own-memory RAG source id for this agent.
    pub fn memory_source_id(&self) -> String {
        match &self.project_id {
            Some(project_id) => format!("agent:{project_id}/{}/memory", self.id),
            None => format!("agent:{}/memory", self.id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_toml_deserializes_with_safe_scope_defaults() {
        let raw = r#"
name = "Nutricionista"
model = "llama3.2"
system_prompt = "Eres un nutricionista."
rules = ["Prioriza evidencia."]
"#;
        let cfg: AgentConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.name, "Nutricionista");
        assert!(cfg.mcp_sources.is_empty());
        assert!(cfg.tools_override.is_empty());
        assert!(cfg.rag_sources.is_empty());
        assert_eq!(cfg.rag_write, RagWritePolicy::None);
        assert_eq!(cfg.max_tool_calls, 8);
        assert_eq!(cfg.provider, "ollama");
        assert!(cfg.memory.persist_history);
    }

    #[test]
    fn v2_scope_fields_roundtrip() {
        let raw = r#"
name = "Fudi Ops"
system_prompt = "Ops assistant"
mcp_sources = ["fudi"]
tools_override = ["fudi/list_orders", "*"]
rag_sources = ["fudi/policies", "agent:fudi_ops/memory"]
rag_write = "own_memory_only"
max_tool_calls = 12
timeout_secs = 120

[memory]
index_summaries = true
summary_collection = "agent:fudi_ops/memory"
"#;
        let cfg: AgentConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.mcp_sources, vec!["fudi"]);
        assert_eq!(cfg.tools_override.len(), 2);
        assert_eq!(cfg.rag_write, RagWritePolicy::OwnMemoryOnly);
        assert_eq!(cfg.max_tool_calls, 12);
        assert!(cfg.memory.index_summaries);
    }

    #[test]
    fn a_manifest_with_subagents_still_deserialises() {
        let toml = "name = \"Demo\"\nsystem_prompt = \"hi\"\n\
                    [subagents]\nallowed = [\"one\"]\nallow_global = true\nmax_depth = 2\n";
        let config: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.name, "Demo");
        assert_eq!(config.system_prompt, "hi");
    }

    #[test]
    fn the_subagents_stub_never_serialises() {
        let config = AgentConfig::default();
        let text = toml::to_string(&config).unwrap();
        assert!(
            !text.contains("subagents"),
            "dead config must not leak back out: {text}"
        );
    }

    #[test]
    fn a_manifest_without_the_new_fields_loads() {
        // A v1 manifest must keep loading unchanged: both new fields are
        // `#[serde(default)]` and therefore default to empty allowlists/sets.
        let raw = r#"
name = "Legacy"
system_prompt = "v1 agent"
skills = ["legacy-skill"]
rules = ["Be brief."]
"#;
        let cfg: AgentConfig = toml::from_str(raw).unwrap();
        assert!(cfg.tools_local.is_empty());
        assert!(cfg.deferred_skills.is_empty());
    }

    #[test]
    fn tools_local_and_deferred_skills_round_trip() {
        let raw = r#"
name = "V2 Agent"
system_prompt = "v2 agent"
skills = ["reviewer-skill", "legacy-skill"]
tools_local = ["llama_r__skill", "llama_r__rag_query"]
deferred_skills = ["reviewer-skill"]
"#;
        let cfg: AgentConfig = toml::from_str(raw).unwrap();
        assert_eq!(
            cfg.tools_local,
            vec!["llama_r__skill", "llama_r__rag_query"]
        );
        assert_eq!(cfg.deferred_skills, vec!["reviewer-skill"]);

        let encoded = toml::to_string(&cfg).unwrap();
        let reparsed: AgentConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(reparsed.tools_local, cfg.tools_local);
        assert_eq!(reparsed.deferred_skills, cfg.deferred_skills);
    }
}
