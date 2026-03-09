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
    /// Dynamic template variables. Use {{var_name}} in system_prompt.
    #[serde(default)]
    pub variables: HashMap<String, String>,
    #[serde(default)]
    pub optimize: OptimizeConfig,
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
    pub config: AgentConfig,
}
