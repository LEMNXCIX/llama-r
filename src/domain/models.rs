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

/// Per-response token accounting, when the upstream reports it.
///
/// `cost_usd` is computed from the per-million token prices attached to the
/// resolved model's capabilities. It is `None` unless **both** the token counts
/// above and both price figures are present — a missing price is never silently
/// replaced by a guess (today every catalog price is `None`, so a real response
/// carries counts but no fabricated cost).
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

impl Usage {
    /// Build usage from upstream counts and the model's per-million prices.
    /// Passing the prices separately keeps `domain` free of a dependency on the
    /// provider capability module — the caller reads them off `ModelCapabilities`.
    pub fn from_counts(
        input_tokens: u64,
        output_tokens: u64,
        cost_per_1m_input: Option<f64>,
        cost_per_1m_output: Option<f64>,
    ) -> Self {
        let cost_usd = match (cost_per_1m_input, cost_per_1m_output) {
            (Some(input_rate), Some(output_rate)) => Some(
                (input_tokens as f64 * input_rate + output_tokens as f64 * output_rate)
                    / 1_000_000.0,
            ),
            // One absent price is enough: never fabricate a partial cost.
            _ => None,
        };
        Self {
            input_tokens,
            output_tokens,
            cost_usd,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ChatResponse {
    pub model: String,
    pub created_at: String,
    pub message: ChatMessage,
    pub done: bool,
    /// Token counts reported by the upstream, when it reported any. Optional:
    /// an upstream with no usage in its response leaves this `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// The configured profile that actually answered, when a fallback was used.
    /// A **profile name**, never a key value: it exists so a degraded answer can
    /// be attributed to the upstream that produced it without disclosing a
    /// credential. `None` when the primary answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub served_by: Option<String>,
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
    /// — `route_resolution::peeked_answer` — feeds this field and the preview
    /// endpoint's twin, so the two surfaces describe the same answer the
    /// same way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probabilities_note: Option<String>,
    /// The routing keys that were offered, sorted (as built).
    pub candidates: Vec<String>,
}

/// A pending human decision attached to one SSE event.
///
/// Carries only non-secret identifiers: the approval id, the tool name and the
/// lowercase-hex SHA-256 of the exact arguments being approved. Raw arguments
/// are deliberately absent from the stream. `id` is a string rather than a
/// `Uuid` so the schema derives without an extra utoipa feature; clients see
/// the same JSON string either way.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ApprovalInfo {
    pub id: String,
    pub tool_name: String,
    pub args_hash: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ChatStreamEvent {
    pub model: String,
    pub created_at: String,
    pub message: ChatMessage,
    pub done: bool,
    /// Present only on an `ApprovalRequired` event. Skipped when absent so
    /// existing SSE clients see an unchanged payload on ordinary events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalInfo>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_computes_cost_from_both_prices() {
        let usage = Usage::from_counts(2_000_000, 1_000_000, Some(3.0), Some(6.0));
        assert_eq!(usage.input_tokens, 2_000_000);
        assert_eq!(usage.output_tokens, 1_000_000);
        assert_eq!(usage.cost_usd, Some(12.0), "2m*3 + 1m*6 = 12 USD");
    }

    #[test]
    fn usage_never_fabricates_a_cost_from_a_missing_price() {
        let no_input = Usage::from_counts(10, 10, None, Some(6.0));
        let no_output = Usage::from_counts(10, 10, Some(3.0), None);
        let neither = Usage::from_counts(10, 10, None, None);
        assert_eq!(no_input.cost_usd, None);
        assert_eq!(no_output.cost_usd, None);
        assert_eq!(neither.cost_usd, None);
        // Counts are still reported even when no price is known.
        assert_eq!(neither.input_tokens, 10);
        assert_eq!(neither.output_tokens, 10);
    }

    #[test]
    fn optional_usage_fields_are_absent_from_json_when_none() {
        let response = ChatResponse {
            model: "m".into(),
            created_at: "now".into(),
            message: ChatMessage {
                role: "assistant".into(),
                content: "hi".into(),
            },
            done: true,
            usage: None,
            served_by: None,
            debug_prompt: None,
            debug_route: None,
        };
        let json = serde_json::to_value(&response).unwrap();
        assert!(json.get("usage").is_none());
        assert!(json.get("served_by").is_none());
    }
}
