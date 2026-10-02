use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ChatResponse {
    pub model: String,
    pub created_at: String,
    pub message: ChatMessage,
    pub done: bool,
    /// Optional: the full expanded prompt used for this request (only if requested via headers)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_prompt: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ChatStreamEvent {
    pub model: String,
    pub created_at: String,
    pub message: ChatMessage,
    pub done: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ModelInfo {
    pub name: String,
    pub modified_at: String,
    pub size: i64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ListModelsResponse {
    pub models: Vec<ModelInfo>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub tags: Option<Vec<String>>,
}

/// Where a skill came from, which decides precedence when ids collide.
///
/// Discovered by *how* it was found rather than by a path table: the loader
/// knows whether it was reading a project's own directory or a shared one.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillScope {
    /// Installed by another harness (Claude, Cursor, Windsurf, ...).
    Harness,
    /// Shared Llama-R skills, available to any project.
    LlamaR,
    /// Dedicated to one project; wins over the others.
    Project,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Skill {
    pub id: String,
    pub path: String,
    pub metadata: SkillMetadata,
    pub content: String,
    /// Provenance, used to resolve ids that exist in more than one scope.
    #[serde(default = "default_skill_scope")]
    pub scope: SkillScope,
}

/// Skills loaded before scopes existed are treated as shared, which keeps the
/// previous behaviour (a global scan could see them) instead of silently
/// demoting them to Harness and changing resolution.
fn default_skill_scope() -> SkillScope {
    SkillScope::LlamaR
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListSkillsResponse {
    pub skills: Vec<Skill>,
}
