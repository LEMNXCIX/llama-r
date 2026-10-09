//! Admin/debug routing endpoint (`POST /api/route`): the decision, without
//! the chat.
//!
//! Calibrating `MIN_CONFIDENCE` against live traffic is guesswork — the only
//! way to learn what a threshold *would* do is to run the decision and look
//! at it. This endpoint runs exactly the decision a chat would run (same
//! [`build_candidates`], same [`build_questions`], same [`route`] ladder,
//! same state-text budget) and returns the result **without executing the
//! chat**, so a preview costs one decision-model forward pass instead of a
//! full agent turn.
//!
//! Gated by the `X-Debug: true` header, checked **before** the body is
//! parsed — the structure of [`crate::api::rag_api`]'s gate, with this
//! endpoint's own name in the message: an unauthorized caller always gets
//! 403 (never a 422 describing the schema), and a large body cannot be used
//! to probe the router. A router that is switched off returns
//! [`AppError::Unavailable`], which maps to `501` — a deliberately disabled
//! feature is not a server fault.

use crate::api::chat_core::{
    agent_not_found, build_state_text, peeked_answer, recheck_winner, PeekedAnswer, WinnerRecheck,
    ROUTE_STATE_BUDGET_CHARS, STALE_WINNER,
};
use crate::api::handlers::AppState;
use crate::domain::models::ChatMessage;
use crate::error::AppError;
use crate::ports::router::DecisionRequest;
use crate::services::agent_router::{build_questions, route, NoRoute, RoutingDecision};
use crate::services::route_candidates::{build_candidates, RouteCandidate};
use axum::{
    extract::{FromRequest, State},
    http::HeaderMap,
    Json,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
use utoipa::ToSchema;

/// The gate: `X-Debug: true` or a `403` that describes no schema.
fn require_debug(headers: &HeaderMap) -> Result<(), AppError> {
    let ok = headers.get("x-debug").and_then(|value| value.to_str().ok()) == Some("true");
    if ok {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "route preview requires header X-Debug: true".into(),
        ))
    }
}

/// Parse a JSON body *after* the debug gate has passed.
///
/// The same extractor shape as [`crate::api::rag_api::GatedJson`] — the
/// ordering is enforced by the type system rather than by argument order in
/// the handler. It is a deliberate copy rather than a reuse because the
/// gate's `403` message must name *this* endpoint, and `rag_api` (with its
/// RAG-specific wording) is a closed module.
pub struct GatedJson<T>(pub T);

#[async_trait::async_trait]
impl<S, T> FromRequest<S> for GatedJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        require_debug(req.headers())?;
        let Json(body) = Json::<T>::from_request(req, state)
            .await
            .map_err(|err| AppError::Validation(format!("invalid request body: {err}")))?;
        Ok(GatedJson(body))
    }
}

/// What the preview is asked: the project the prompt would arrive for, and
/// the prompt itself.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RoutePreviewRequest {
    /// The project whose agents are the candidate set (globals always
    /// included), as `X-Project` would spell it.
    pub project_id: String,
    /// The prompt the decision would be made on — the same text, after the
    /// same truncation, a chat with this message would route on.
    pub prompt: String,
}

/// One agent the decision would have been offered, in the terms the decision
/// model saw it.
#[derive(Debug, Serialize, ToSchema)]
pub struct RouteCandidateView {
    /// The routing key: `project::agent` for project agents, the bare id for
    /// globals — the exact string a decision comes back as.
    pub key: String,
    /// Human-readable name (blank names fall back to the agent id).
    pub label: String,
    /// The description the candidate was scored against.
    pub criteria: String,
    /// Whether this is the ladder's fallback rather than a specialist.
    pub is_project_general: bool,
}

/// The decision a chat *would* make for this prompt, and the evidence it
/// was made from — with no chat executed.
#[derive(Debug, Serialize, ToSchema)]
pub struct RoutePreviewResponse {
    /// The candidates this prompt would have been routed between, sorted by
    /// key — what the `Choice` question offered.
    pub candidates: Vec<RouteCandidateView>,
    /// The router's answer exactly as it came back. `null` when the router
    /// did not answer; never reshaped, so the derivation beside it can be
    /// checked against the source.
    pub raw: serde_json::Value,
    /// `"agent"` or `"general"` — where the ladder lands this prompt.
    pub decision: String,
    /// Why the general agent keeps it — a
    /// [`GeneralReason`](crate::services::agent_router::GeneralReason)
    /// spelling — or `None` when a specialist won.
    pub reason: Option<String>,
    /// The model's reported choice confidence. `None` when it reported no
    /// usable value — never `0.0`, which would narrate a malformed answer as
    /// a real score.
    pub confidence: Option<f32>,
    /// The model's reported specialisation score, same discipline.
    pub specialised: Option<f32>,
    /// The distribution over candidates as it may be reported next to the
    /// decision. Kept whenever informative — a partial distribution is
    /// calibration input, so it is attached even when it names only some
    /// candidates. Empty only when the model reported none, or when a
    /// non-empty distribution contradicted its own winner;
    /// [`probabilities_note`](Self::probabilities_note) says which.
    pub probabilities: BTreeMap<String, f32>,
    /// Set whenever the distribution is not a plain account of the winner,
    /// naming which case: none reported, partial (the map is attached), or
    /// contradicting the winner — absent from its own distribution, or
    /// outscored by another candidate (the map is withheld). `raw` still
    /// carries exactly what the router answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probabilities_note: Option<String>,
    /// Wall-clock cost of the decision, in milliseconds — what one
    /// consultation of the decision model costs.
    pub router_latency_ms: u64,
}

/// `POST /api/route` — run the routing decision for a prompt and return it,
/// without executing the chat.
#[utoipa::path(
    post,
    path = "/api/route",
    tag = "Router",
    request_body = RoutePreviewRequest,
    params(
        ("X-Debug" = Option<String>, Header, description = "Must be `true`; the preview is a debug surface"),
    ),
    responses(
        (status = 200, description = "The routing decision for this prompt, without executing the chat", body = RoutePreviewResponse),
        (status = 400, description = "The project has no candidates to route between, or none to degrade to"),
        (status = 403, description = "Missing or non-`true` X-Debug header (checked before the body is parsed)"),
        (status = 501, description = "Routing is disabled (ROUTER_ENABLED=false or no router installed)"),
    )
)]
pub async fn route_preview(
    State(state): State<Arc<AppState>>,
    GatedJson(body): GatedJson<RoutePreviewRequest>,
) -> Result<Json<RoutePreviewResponse>, AppError> {
    state.observability.record_http_request();

    // The same gates `resolve_selection` passes, in the same order — a
    // preview of routing that is switched off previews nothing. Both are
    // `Unavailable`: a deliberately disabled feature is not a server fault.
    if !state.router_config.enabled {
        return Err(AppError::Unavailable(
            "routing is disabled (ROUTER_ENABLED=false)".into(),
        ));
    }
    // The `Arc` leaves the lock before any `.await`: a guard held across the
    // decision would stall every other reader for the length of one call.
    let router = state
        .router
        .read()
        .ok()
        .and_then(|guard| (*guard).clone())
        .ok_or_else(|| AppError::Unavailable("no decision router is installed".into()))?;

    let config = &state.router_config;
    let candidates = build_candidates(&state.agent_registry, &body.project_id, &config.exclude);
    if candidates.is_empty() {
        // Nothing to choose between means nothing to preview. The chat path
        // quietly does not route here; the preview's whole job is to show
        // the decision, so silence would be the one uninformative answer.
        return Err(AppError::Validation(format!(
            "no routing candidates for project '{}'; nothing to route",
            body.project_id
        )));
    }

    // The state text a chat would route on: the prompt as the newest user
    // turn, through the same builder and the same budget, so a preview never
    // routes on text the chat path would have truncated.
    let messages = [ChatMessage {
        role: "user".to_string(),
        content: body.prompt.clone(),
    }];
    let request = DecisionRequest {
        state: build_state_text(&messages, ROUTE_STATE_BUDGET_CHARS),
        questions: build_questions(&candidates),
    };

    let started_at = Instant::now();
    let result = router.decide(request).await;
    let router_latency_ms = started_at.elapsed().as_millis() as u64;

    // Every consultation records, whatever comes back — latency always,
    // health after the answer (or its absence), an error count only when the
    // call itself failed. Deliberately *not* `record_route_to_agent`/`..._to_
    // general`: those count dispatches, and a preview dispatches nothing.
    state.observability.record_router_latency(router_latency_ms);
    match &result {
        Ok(_) => state.record_router_health(true),
        Err(_) => {
            state.observability.record_router_error();
            state.record_router_health(false);
        }
    }

    // The raw answer, before `route` consumes the response. A router that
    // did not answer has no raw answer: `null`, not an empty object that
    // could pass for one.
    let raw = match &result {
        Ok(response) => serde_json::to_value(response)?,
        Err(_) => serde_json::Value::Null,
    };
    let peeked: PeekedAnswer = match &result {
        Ok(response) => peeked_answer(response, candidates.len()),
        Err(_) => PeekedAnswer::default(),
    };

    tracing::info!(
        project = %body.project_id,
        chosen_key = ?peeked.winner,
        confidence = ?peeked.confidence,
        specialised = ?peeked.specialised,
        candidates = candidates.len(),
        router_latency_ms,
        "route preview decision"
    );

    let decision = route(
        &body.project_id,
        &candidates,
        config.min_confidence,
        config.min_specialised,
        result,
    )
    .map_err(|no_route| match no_route {
        // Cannot fire: the empty set was rejected above and `route`'s first
        // rung checks the same slice. The contract stands anyway.
        NoRoute::NoCandidates => AppError::Validation(format!(
            "no routing candidates for project '{}'; nothing to route",
            body.project_id
        )),
        // The ladder's only other hard stop, and the preview reports it the
        // way the chat itself would — same words, same status — because a
        // preview that promised a decision a chat would refuse to make would
        // be lying about the thing it exists to show.
        NoRoute::NoGeneralAgent => agent_not_found(&body.project_id, &body.project_id),
    })?;

    let (decision, reason) = match &decision {
        RoutingDecision::Agent {
            project_id: winner_project,
            agent_id,
            ..
        } => {
            // The same re-check dispatch applies after `route` decides, so
            // the preview reports what the chat WOULD do: a winner a hot
            // reload removed is `general` with `stale_winner`, not `agent`.
            // Shared, not duplicated — `recheck_winner` is the one the
            // dispatch path runs, and the log line it emits is the same one.
            match recheck_winner(
                &state.agent_registry,
                &body.project_id,
                winner_project,
                agent_id,
                &candidates,
            ) {
                WinnerRecheck::Present => ("agent".to_string(), None),
                WinnerRecheck::Stale(_) => ("general".to_string(), Some(STALE_WINNER.to_string())),
                // Cannot fire: `route` only returns `Agent` when a general
                // candidate exists to degrade to. The preview reports it the
                // way the chat itself would anyway — dispatch would land on
                // the project's general agent and, with none registered,
                // fail this same 400 — because a preview that promised a
                // decision the chat would refuse to make would be lying
                // about the thing it exists to show.
                WinnerRecheck::NoFallback => {
                    return Err(agent_not_found(&body.project_id, &body.project_id));
                }
            }
        }
        RoutingDecision::ProjectGeneral { reason, .. } => {
            ("general".to_string(), Some(reason.as_str().to_string()))
        }
    };

    Ok(Json(RoutePreviewResponse {
        candidates: candidates.iter().map(candidate_view).collect(),
        raw,
        decision,
        reason,
        confidence: peeked.confidence,
        specialised: peeked.specialised,
        probabilities: peeked.probabilities,
        probabilities_note: peeked.probabilities_note,
        router_latency_ms,
    }))
}

fn candidate_view(candidate: &RouteCandidate) -> RouteCandidateView {
    RouteCandidateView {
        key: candidate.key.clone(),
        label: candidate.label.clone(),
        criteria: candidate.criteria.clone(),
        is_project_general: candidate.is_project_general,
    }
}
