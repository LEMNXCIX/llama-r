use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// The routing decision that led to this response, when the router was
    /// consulted **and the request carried `X-Debug: true`** — the same gate
    /// as `debug_prompt`; without it the field is absent from the response
    /// entirely. The non-streaming surface only; `ChatStreamEvent` and the
    /// gRPC `ChatResponse` deliberately do not carry it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_route: Option<RoutedDebug>,
}

/// The routing decision, for the debug surface: what was offered, what the
/// model answered, and what dispatch did about it.
///
/// It lives here, next to the [`ChatResponse::debug_route`] field that carries
/// it, rather than beside the dispatch that produces it: `domain` is the layer
/// the API's types are declared in, and having `domain::models` reach back into
/// `api::chat_core` for a payload type inverted the dependency for exactly one
/// struct.
///
/// It reaches a client only when `X-Debug: true` asked for it — the same
/// gate that adds `debug_prompt`, and the reason the field can carry the
/// project's full roster and distribution without disclosing them to every
/// caller.
///
/// `outcome` is `"agent"` (a specialist won and was verified to exist) or
/// `"general"` (the request stayed with the project's general agent).
/// `reason` is one of
/// [`GeneralReason`](crate::services::agent_router::GeneralReason)'s
/// spellings — except `"stale_winner"`, which names the one rung the ladder
/// never sees: the winner was trusted, then removed by a hot reload before
/// it could be used. `winner`, `confidence` and `specialised` are `None`
/// when the router did not answer with a usable value, never `0.0` — an
/// absent value and a real zero are different reports.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct RoutedDebug {
    /// The project the request was routed *for*. A global winner keeps it:
    /// the empty project id lives in `ResolvedSelection`, not here.
    pub project_id: String,
    /// `"agent"` or `"general"` — where the request was dispatched.
    ///
    /// `String`, not `&'static str`: [`ChatResponse`] derives `Deserialize`,
    /// and a `&'static str` field would have forced `skip_deserializing`
    /// on it — a silent change to a public type's serde contract. The two
    /// spellings are constants either way; `String` just survives a
    /// round-trip.
    pub outcome: String,
    /// Why the general agent kept it, or `None` when a specialist won.
    pub reason: Option<String>,
    /// The label the model chose, when it answered the choice question.
    pub winner: Option<String>,
    pub confidence: Option<f32>,
    pub specialised: Option<f32>,
    /// The distribution over candidates as the model reported it, so a
    /// decision near the threshold can be judged after the fact rather
    /// than only by its winner. Kept whenever informative — a partial
    /// distribution is calibration input, so it is attached even when it
    /// names only some candidates. Empty only when the model reported
    /// none, or when a non-empty distribution contradicted its own winner;
    /// [`probabilities_note`](Self::probabilities_note) says which.
    pub probabilities: BTreeMap<String, f32>,
    /// Set whenever the distribution is not a plain account of the winner,
    /// naming which case: none reported, partial (the map is attached), or
    /// contradicting the winner — absent from its own distribution, or
    /// outscored by another candidate (the map is withheld). One extractor
    /// — `chat_core::peeked_answer` — feeds this field and the preview
    /// endpoint's twin, so the two surfaces describe the same answer the
    /// same way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probabilities_note: Option<String>,
    /// The routing keys that were offered, sorted (as built).
    pub candidates: Vec<String>,
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
