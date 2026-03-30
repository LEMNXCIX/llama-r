use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentConfig {
    pub name: String,
    /// Model to use. If empty, falls back to the global DEFAULT_MODEL.
    #[serde(default)]
    pub model: String,
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
    /// Dynamic template variables. Use {{var_name}} in system_prompt.
    #[serde(default)]
    pub variables: HashMap<String, String>,
    #[serde(default = "default_context_budget")]
    pub max_context_tokens: usize,
    #[serde(default)]
    pub optimize: OptimizeConfig,
}

fn default_context_budget() -> usize {
    4096
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct OptimizeConfig {
    pub enabled: bool,
    #[serde(default)]
    pub rules: Vec<String>,
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
}
