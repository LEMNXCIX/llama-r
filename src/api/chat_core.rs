use crate::api::handlers::AppState;
use crate::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent};
use crate::error::AppError;
use crate::optimizer::TokenOptimizer;
use crate::ports::engine::AgentRunEvent;
use crate::ports::router::{Answer, DecisionRequest, DecisionResponse};
use crate::services::agent_registry::AgentRegistry;
use crate::services::agent_router::{
    build_questions, route, NoRoute, RoutingDecision, CHOICE_KEY, NOUL_KEY,
};
use crate::services::agent_runtime::RuntimeChatRequest;
use crate::services::route_candidates::build_candidates;
use serde::Serialize;
use std::collections::BTreeMap;
use std::pin::Pin;
use std::time::Instant;
use tokio_stream::{Stream, StreamExt};
use utoipa::ToSchema;

pub type AppChatStream = Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, AppError>> + Send>>;

/// The header-derived, borrowed *input* to dispatch: `X-Project`, `X-Agent`,
/// `X-Conversation-Id` and `X-Debug` as the handlers read them.
///
/// What dispatch actually runs with is [`ResolvedSelection`] — the header's
/// answer, or the routing decision when there is no header.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgentSelection<'a> {
    pub project_id: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub conversation_id: Option<&'a str>,
    pub debug: bool,
}

/// What dispatch resolved: where the request is answered from, plus the
/// routing decision that got it there when a router was consulted.
///
/// The pair is deliberately the same one [`AgentSelection`] would have
/// carried — routing fills the hole `X-Agent` would have filled and nothing
/// else — so [`engine_target`] cannot tell a routed request from a
/// header-pinned one, and nothing downstream of it knows routing exists.
/// `routed` is `None` on every path that did not consult the router, which
/// is how the debug surface hangs off dispatch without touching it.
#[derive(Debug, Clone, Default)]
pub struct ResolvedSelection {
    pub project_id: Option<String>,
    pub agent_id: Option<String>,
    pub routed: Option<RoutedDebug>,
}

impl ResolvedSelection {
    /// The header-derived selection, owned and fully resolved.
    ///
    /// `X-Project` without `X-Agent` means the project's own agent, so the
    /// id is spelled out rather than left as `None` for
    /// `resolve_agent(Some(p), None)` to find: the registry resolves
    /// `(p, None)` and `(p, Some(p))` to the same key, and
    /// [`ResolvedSelection::requested_target`] formats both as `p/p`, so
    /// nothing downstream can tell — today's behaviour, byte for byte, with
    /// the pair made complete.
    fn from_selection(selection: &AgentSelection<'_>) -> Self {
        Self {
            project_id: selection.project_id.map(str::to_string),
            agent_id: selection
                .agent_id
                .or(selection.project_id)
                .map(str::to_string),
            routed: None,
        }
    }

    fn requested_target(&self, fallback_model: &str) -> String {
        match (&self.project_id, &self.agent_id) {
            (Some(project_id), Some(agent_id)) => format!("{}/{}", project_id, agent_id),
            (Some(project_id), None) => format!("{}/{}", project_id, project_id),
            (None, Some(agent_id)) => agent_id.clone(),
            (None, None) => fallback_model.to_string(),
        }
    }
}

/// The routing decision, for the debug surface: what was offered, what the
/// model answered, and what dispatch did about it.
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
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RoutedDebug {
    /// The project the request was routed *for*. A global winner keeps it:
    /// the empty project id lives in [`ResolvedSelection`], not here.
    pub project_id: String,
    /// `"agent"` or `"general"` — where the request was dispatched.
    pub outcome: &'static str,
    /// Why the general agent kept it, or `None` when a specialist won.
    pub reason: Option<&'static str>,
    /// The label the model chose, when it answered the choice question.
    pub winner: Option<String>,
    pub confidence: Option<f32>,
    pub specialised: Option<f32>,
    /// The full distribution, so a decision near the threshold can be
    /// judged after the fact rather than only its winner.
    pub probabilities: BTreeMap<String, f32>,
    /// Set only when the model named a winner its own `probabilities` does
    /// not contain: the distribution is withheld rather than emitted as an
    /// account of a winner it does not explain — see
    /// [`peeked_answer`], the one extractor both debug surfaces read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probabilities_note: Option<String>,
    /// The routing keys that were offered, sorted (as built).
    pub candidates: Vec<String>,
}

/// Characters of the conversation the decision model may see.
///
/// 8 000 is comfortably inside d1-3B's 32 K context, leaving room for both
/// questions and every candidate's criteria text.
pub(crate) const ROUTE_STATE_BUDGET_CHARS: usize = 8_000;

/// The [`RoutedDebug::reason`] spelling for a winner that a hot reload
/// removed between the decision and the use — the rung the ladder cannot
/// report, because to the ladder the winner was valid.
const STALE_WINNER: &str = "stale_winner";

/// The conversation, as `"{role}: {content}"` lines, newest last.
///
/// Accumulation runs from the newest message backwards (the lines are
/// reversed at the end), so a conversation longer than the budget keeps the
/// turns that say what the request *is* and drops the oldest — truncation
/// that cut the newest message would decide on a stale request. The loop
/// stops *after* the line that crosses `budget_chars`, so the newest turn
/// always survives and at most one line overshoots: an oversized
/// conversation truncates, it never fails.
///
/// `system` messages are dropped: a system prompt is not conversation, and
/// sending it would let a project's own prompt steer the decision that
/// should be made from the user's words.
pub fn build_state_text(messages: &[ChatMessage], budget_chars: usize) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0usize;
    for message in messages.iter().rev() {
        if message.role == "system" {
            continue;
        }
        // The line itself plus the newline it will be joined with.
        used += message.role.len() + message.content.len() + 3;
        kept.push(format!("{}: {}", message.role, message.content));
        if used > budget_chars {
            break;
        }
    }
    kept.reverse();
    kept.join("\n")
}

/// Resolve the selection dispatch runs with: the header when there is one,
/// the routing decision when there is not.
///
/// Delegates to [`resolve_selection_with`] with the registry dispatch
/// itself consults, so in production the decision and its re-check read the
/// same registry — two reads of a lock that hot reload can swap between.
pub async fn resolve_selection(
    state: &AppState,
    selection: AgentSelection<'_>,
    messages: &[ChatMessage],
) -> Result<ResolvedSelection, AppError> {
    resolve_selection_with(state, &state.agent_registry, selection, messages).await
}

/// [`resolve_selection`]'s body, with the registry named — the parameter is
/// what makes the hot-reload race testable: a test can hand it a registry
/// that no longer holds the winner, which is exactly the second read of the
/// real lock after a reload.
///
/// The gates, in order. Each one returns the header untouched rather than
/// inventing a failure, so a request nobody routes behaves byte-identically
/// to how it did before routing existed:
///
/// 1. an explicit `X-Agent` pins the target — routing never spends a
///    decision on a request somebody already routed;
/// 2. no `X-Project`: there is no candidate set to choose between;
/// 3. routing switched off — an update that only ships this code changes
///    nothing;
/// 4. nobody installed a router (degraded boot, tests);
/// 5. no candidates for this project: the ladder would answer
///    `NoCandidates`, a 400 — *not routing* must not be one.
///
/// Past the gates exactly one decision is spent, and [`route`] decides from
/// it. A specialist is re-resolved through `registry` before it is
/// returned: hot reload can remove it between the decision and the use, and
/// an id that was never verified to exist must never reach dispatch.
pub async fn resolve_selection_with(
    state: &AppState,
    registry: &AgentRegistry,
    selection: AgentSelection<'_>,
    messages: &[ChatMessage],
) -> Result<ResolvedSelection, AppError> {
    if selection.agent_id.is_some() {
        return Ok(ResolvedSelection::from_selection(&selection));
    }
    let Some(project_id) = selection.project_id else {
        return Ok(ResolvedSelection::from_selection(&selection));
    };
    if !state.router_config.enabled {
        return Ok(ResolvedSelection::from_selection(&selection));
    }
    // The Arc leaves the lock before the `.await`: a guard held across the
    // decision's network call would stall every other reader — including
    // the next chat — for the length of one decision.
    let router = state.router.read().ok().and_then(|guard| (*guard).clone());
    let Some(router) = router else {
        return Ok(ResolvedSelection::from_selection(&selection));
    };

    let config = &state.router_config;
    // Candidates come from the registry dispatch resolves against; the
    // `registry` parameter is the *re-check* — the second read of a lock
    // that reload can have swapped between the two, which is precisely the
    // race under test.
    let candidates = build_candidates(&state.agent_registry, project_id, &config.exclude);
    if candidates.is_empty() {
        return Ok(ResolvedSelection::from_selection(&selection));
    }

    let request = DecisionRequest {
        state: build_state_text(messages, ROUTE_STATE_BUDGET_CHARS),
        questions: build_questions(&candidates),
    };
    let started_at = Instant::now();
    let result = router.decide(request).await;
    let latency_ms = started_at.elapsed().as_millis() as u64;

    // Every consultation records, whatever comes back: latency always,
    // health after the answer (or its absence), an error count only when
    // the call itself failed — "d1 said no" is not a router error.
    state.observability.record_router_latency(latency_ms);
    match &result {
        Ok(_) => state.record_router_health(true),
        Err(_) => {
            state.observability.record_router_error();
            state.record_router_health(false);
        }
    }

    // The model's own answer, read before `route` consumes the response.
    // Each score is `None` when the response carried no usable value — never
    // `0.0`, which would narrate a malformed answer as a real score.
    let peeked = match &result {
        Ok(response) => peeked_answer(response),
        Err(_) => PeekedAnswer::default(),
    };
    let candidate_keys: Vec<String> = candidates.iter().map(|c| c.key.clone()).collect();

    let debug = |outcome: &'static str, reason: Option<&'static str>| RoutedDebug {
        project_id: project_id.to_string(),
        outcome,
        reason,
        winner: peeked.winner.clone(),
        confidence: peeked.confidence,
        specialised: peeked.specialised,
        probabilities: peeked.probabilities.clone(),
        probabilities_note: peeked.probabilities_note.clone(),
        candidates: candidate_keys.clone(),
    };

    // One decision log per consultation, before any rung's own warning can
    // narrate over it: the project, the model's chosen key, both scores,
    // how wide the choice was, and what the decision cost.
    tracing::info!(
        project = %project_id,
        chosen_key = ?peeked.winner,
        confidence = ?peeked.confidence,
        specialised = ?peeked.specialised,
        candidates = candidate_keys.len(),
        latency_ms,
        "routing decision"
    );

    match route(
        project_id,
        &candidates,
        config.min_confidence,
        config.min_specialised,
        result,
    ) {
        Ok(RoutingDecision::Agent {
            project_id: winner_project,
            agent_id,
            ..
        }) => {
            // Review Focus #4 — the hot-reload race. `route` decided from
            // the candidates as they were; a reload can remove the winner
            // before it is used, so the key is resolved again through the
            // registry the caller passed. If it is gone — agents reloaded
            // mid-request — degrade to the project's general agent, the
            // ladder's own fallback, and say so. An id never verified to
            // exist must never reach dispatch — the general fallback taken
            // below is the one deliberate exception, and it is named there.
            let present = if winner_project.is_empty() {
                // An empty project id marks a global winner (pinned by
                // `a_global_winner_carries_an_empty_project_id`): its
                // registry key is the bare id, so `get_agent` — never
                // `get_project_agent` under the requesting project, which
                // would look for a same-named *project* agent instead.
                registry.get_agent(&agent_id).is_some()
            } else {
                registry
                    .get_project_agent(&winner_project, &agent_id)
                    .is_some()
            };

            if present {
                state.observability.record_route_to_agent();
                let winner_project = if winner_project.is_empty() {
                    None
                } else {
                    Some(winner_project)
                };
                Ok(ResolvedSelection {
                    project_id: winner_project,
                    agent_id: Some(agent_id),
                    routed: Some(debug("agent", None)),
                })
            } else {
                let vanished_key = if winner_project.is_empty() {
                    agent_id.clone()
                } else {
                    format!("{winner_project}::{agent_id}")
                };
                tracing::warn!(
                    project = %project_id,
                    vanished_key = %vanished_key,
                    "routed agent removed by a hot reload between the decision and the use; \
                     falling back to the project's general agent"
                );
                state.observability.record_route_to_general();
                let Some(general) = candidates.iter().find(|c| c.is_project_general) else {
                    // `route` only returns `Agent` when the pre-reload
                    // candidate set holds a general candidate to degrade to,
                    // so this cannot happen — and if it ever did, the header
                    // path is the safe answer.
                    return Ok(ResolvedSelection::from_selection(&selection));
                };
                // The one deliberate exception to "an id never verified to
                // exist must never reach dispatch": `general.agent_id` comes
                // from the pre-reload candidate set and is *not* re-checked
                // against `registry`. Re-checking it would have nowhere
                // safer to land — the general agent is the fallback — so if
                // the same reload removed it too, `prepare_request` finds
                // nothing and reproduces today's 400 rather than dispatching
                // to something worse than the report.
                Ok(ResolvedSelection {
                    project_id: Some(project_id.to_string()),
                    agent_id: Some(general.agent_id.clone()),
                    routed: Some(debug("general", Some(STALE_WINNER))),
                })
            }
        }
        Ok(RoutingDecision::ProjectGeneral {
            project_id: general_project,
            agent_id,
            reason,
            ..
        }) => {
            state.observability.record_route_to_general();
            Ok(ResolvedSelection {
                project_id: Some(general_project),
                agent_id: Some(agent_id),
                routed: Some(debug("general", Some(reason.as_str()))),
            })
        }
        Err(NoRoute::NoCandidates) => {
            // Cannot fire: `candidates` was checked non-empty above and
            // `route`'s first rung checks the same slice. The contract
            // stands anyway — not routing must not be a failure.
            Ok(ResolvedSelection::from_selection(&selection))
        }
        Err(NoRoute::NoGeneralAgent) => {
            // Today's 400, verbatim: a project with specialists but no
            // general agent has nothing to degrade to, and before routing
            // existed the same request failed the same way in
            // `prepare_request`.
            Err(agent_not_found(project_id, project_id))
        }
    }
}

/// The model's answer, read out of the response before [`route`] consumes
/// it. Each field is `None` when the response did not carry a usable value.
///
/// The same discipline `route` applies rung by rung — a missing, wrong-kind
/// or non-finite number reads as *no value*, never as `0.0`: a malformed
/// answer narrated as a real score is how a broken adapter starts looking
/// like a model with an opinion.
#[derive(Debug, Clone, Default)]
pub struct PeekedAnswer {
    pub winner: Option<String>,
    pub confidence: Option<f32>,
    pub specialised: Option<f32>,
    pub probabilities: BTreeMap<String, f32>,
    /// Set only when `winner` is a label the returned `probabilities` does
    /// not contain: the distribution is withheld and the inconsistency is
    /// said out loud, because emitting it beside the winner would present it
    /// as an account of a decision it does not explain. Both debug surfaces
    /// — the preview endpoint and `ChatResponse::debug_route` — read this
    /// one extractor, so they cannot disagree about it.
    pub probabilities_note: Option<String>,
}

pub(crate) fn peeked_answer(response: &DecisionResponse) -> PeekedAnswer {
    let mut peeked = PeekedAnswer::default();
    match response.answers.get(CHOICE_KEY) {
        Some(Answer::Choice {
            key,
            confidence,
            probabilities,
        }) => {
            peeked.winner = Some(key.clone());
            peeked.confidence = confidence.is_finite().then_some(*confidence);
            // The adapter never checks that a `Choice`'s `key` appears among
            // its own `probabilities`, so an answer whose distribution does
            // not account for its own winner can arrive from the wire. The
            // ladder still decides from the winner — the distribution is
            // evidence, not the decision — so nothing is dropped silently:
            // `raw` on the preview keeps exactly what the router answered.
            if probabilities.contains_key(key) {
                peeked.probabilities = probabilities.clone();
            } else {
                tracing::warn!(
                    winner = %key,
                    "router named a winner its own probabilities do not contain; \
                     withholding the distribution"
                );
                peeked.probabilities_note = Some(format!(
                    "the model chose '{}' but its probabilities do not contain it; \
                     the distribution is withheld rather than presented as an account \
                     of a winner it does not explain",
                    key
                ));
            }
        }
        _ => {}
    }
    peeked.specialised = match response.answers.get(NOUL_KEY) {
        Some(Answer::Noul { value }) => value.is_finite().then_some(*value),
        _ => None,
    };
    peeked
}

/// Split prepared messages into system prompt, prior turns, and current user message.
fn split_engine_messages(
    messages: &[ChatMessage],
) -> Result<(Option<String>, Vec<ChatMessage>, String), AppError> {
    let system = messages
        .iter()
        .find(|m| m.role == "system")
        .map(|m| m.content.clone());

    let last_user_idx = messages
        .iter()
        .rposition(|m| m.role == "user")
        .ok_or_else(|| AppError::Validation("missing user message".into()))?;

    let user_message = messages[last_user_idx].content.clone();
    let history: Vec<ChatMessage> = messages
        .iter()
        .enumerate()
        .filter(|(i, m)| *i != last_user_idx && m.role != "system")
        .map(|(_, m)| m.clone())
        .collect();

    Ok((system, history, user_message))
}

/// Decide whether the engine path should run, and which project/agent ids to resolve.
///
/// Engine is used when the resolved selection names a project or an agent —
/// header-pinned or router-chosen, indistinguishable here on purpose — or
/// when `model` matches a global agent id. That first arm is the whole
/// trick: because a routed selection produces the same `(project_id,
/// agent_id)` pair an `X-Agent` header would have, nothing below this
/// function can tell the two apart, which is why routing had to change no
/// code downstream of it.
fn engine_target(
    state: &AppState,
    selection: &ResolvedSelection,
    original_model: &str,
) -> Option<(Option<String>, Option<String>)> {
    if selection.project_id.is_some() || selection.agent_id.is_some() {
        return Some((selection.project_id.clone(), selection.agent_id.clone()));
    }

    if state.agent_registry.get_agent(original_model).is_some() {
        return Some((None, Some(original_model.to_string())));
    }

    None
}

/// Today's "agent not found" complaint, in one place: `prepare_request`
/// says it for a header naming a missing agent, and the ladder's
/// `NoGeneralAgent` says it for a project with no general agent to degrade
/// to. Same words, so a client cannot tell which path produced it — and a
/// project that has no general agent failed exactly like this before
/// routing existed.
pub(crate) fn agent_not_found(target: &str, project_id: &str) -> AppError {
    AppError::Validation(format!(
        "Agent '{}' not found for project '{}'. Ensure the agent exists in 'contextos/projects/{}/agents/'.",
        target, project_id, project_id
    ))
}

fn build_runtime_request(
    state: &AppState,
    prepared: &ChatRequest,
    project_id: Option<String>,
    agent_id: Option<String>,
    conversation_id: Option<String>,
) -> Result<RuntimeChatRequest, AppError> {
    let (system, history, user_message) = split_engine_messages(&prepared.messages)?;
    Ok(RuntimeChatRequest {
        project_id,
        agent_id,
        conversation_id,
        user_message,
        history,
        model_override: Some(prepared.model.clone()),
        system_prompt: system,
        default_model: state.default_model.clone(),
    })
}

fn engine_events_to_chat_stream(
    model: String,
    mut rx: tokio::sync::mpsc::Receiver<AgentRunEvent>,
) -> AppChatStream {
    Box::pin(async_stream::stream! {
        let mut final_text = String::new();
        let mut saw_completed = false;

        while let Some(event) = rx.recv().await {
            match event {
                AgentRunEvent::Token { text } => {
                    final_text = text.clone();
                    yield Ok(ChatStreamEvent {
                        model: model.clone(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        message: ChatMessage {
                            role: "assistant".to_string(),
                            content: text,
                        },
                        done: false,
                    });
                }
                AgentRunEvent::Completed { result } => {
                    saw_completed = true;
                    // If we already streamed the same text as a Token, only emit the done marker.
                    let content = if final_text == result.text && !final_text.is_empty() {
                        String::new()
                    } else {
                        result.text
                    };
                    yield Ok(ChatStreamEvent {
                        model: model.clone(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        message: ChatMessage {
                            role: "assistant".to_string(),
                            content,
                        },
                        done: true,
                    });
                }
                AgentRunEvent::Error { message } => {
                    yield Err(AppError::Runtime(message));
                }
                AgentRunEvent::ToolCall { .. } | AgentRunEvent::ToolResult { .. } => {
                    // Tool progress is not mapped to ChatStreamEvent yet (Nivel-A streaming).
                }
            }
        }

        if !saw_completed && !final_text.is_empty() {
            yield Ok(ChatStreamEvent {
                model,
                created_at: chrono::Utc::now().to_rfc3339(),
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: String::new(),
                },
                done: true,
            });
        }
    })
}

async fn prepare_request(
    state: &AppState,
    mut payload: ChatRequest,
    selection: &ResolvedSelection,
) -> Result<ChatRequest, AppError> {
    if payload.messages.is_empty() {
        return Err(AppError::Validation(
            "Missing required field: 'messages' cannot be empty.".to_string(),
        ));
    }

    // Read as `Option<&str>` so this function cannot tell a header-derived
    // selection from a routed one — it sees the same pair either way. With
    // routing, a project that has a general agent always resolves, so the
    // 400 below only fires for a genuinely agentless project, which is what
    // it always meant.
    let project_id = selection.project_id.as_deref();
    let agent_id = selection.agent_id.as_deref();
    let selected_agent = if project_id.is_some() || agent_id.is_some() {
        let agent = state.agent_registry.resolve_agent(project_id, agent_id);

        if agent.is_none() {
            if let Some(project_id) = project_id {
                let target = agent_id.unwrap_or(project_id);
                return Err(agent_not_found(target, project_id));
            }
        }
        agent
    } else {
        state.agent_registry.get_agent(&payload.model)
    };

    if let Some(agent) = selected_agent {
        let mut prepared_messages = Vec::with_capacity(payload.messages.len() + 1);
        let sys_prompt = state.context_enricher.build_system_prompt(&agent).await;
        prepared_messages.push(ChatMessage {
            role: "system".to_string(),
            content: sys_prompt,
        });
        prepared_messages.append(&mut payload.messages);

        // Only for the requests this module sends itself. When an agent runtime is
        // present the request goes to `AgentRuntime`, which applies the agent's
        // tools on its way to the engine; doing it here as well ran every tool
        // twice, and the second application is the kind of thing that stays
        // invisible until a tool turns out not to be idempotent.
        //
        // The condition is the same field the dispatch below branches on, so the
        // two cannot disagree about which engine is going to run.
        if state.agent_runtime.is_none() {
            let optimizer = TokenOptimizer::new(agent.config.optimize.clone());
            for message in prepared_messages.iter_mut() {
                let original_len = message.content.len();
                message.content = optimizer.optimize(&message.content);
                state
                    .metrics
                    .record_optimization(original_len, message.content.len());
            }
        }

        let optimized_messages = prepared_messages;

        let resolved_model = state.context_enricher.resolve_model(&agent);
        tracing::info!(
            agent_id = %agent.qualified_id(),
            resolved_model = %resolved_model,
            "Resolved chat request to configured agent"
        );
        payload.model = resolved_model;
        payload.messages = optimized_messages;
    }

    Ok(payload)
}

async fn run_with_fallback(
    state: &AppState,
    mut request: ChatRequest,
    requested_model: &str,
) -> Result<ChatResponse, AppError> {
    match state.provider.chat(request.clone()).await {
        Ok(response) => Ok(response),
        Err(error) => {
            state.observability.record_provider_error();
            let fallback_model = state.default_model.trim();
            if !fallback_model.is_empty()
                && requested_model != fallback_model
                && request.model != fallback_model
            {
                tracing::warn!(
                    requested_model = %requested_model,
                    attempted_model = %request.model,
                    fallback_model = %fallback_model,
                    error = %error,
                    "Primary chat attempt failed; retrying with DEFAULT_MODEL"
                );
                state.observability.record_fallback();
                request.model = fallback_model.to_string();
                state
                    .provider
                    .chat(request)
                    .await
                    .map_err(|fallback_error| {
                        state.observability.record_provider_error();
                        AppError::Provider(format!("Fallback also failed: {}", fallback_error))
                    })
            } else {
                Err(AppError::Provider(error.to_string()))
            }
        }
    }
}

async fn run_stream_with_fallback(
    state: &AppState,
    mut request: ChatRequest,
    requested_model: &str,
) -> Result<AppChatStream, AppError> {
    match state.provider.chat_stream(request.clone()).await {
        Ok(stream) => Ok(Box::pin(stream.map(
            |event: Result<ChatStreamEvent, Box<dyn std::error::Error + Send + Sync>>| {
                event.map_err(|error| AppError::Provider(error.to_string()))
            },
        ))),
        Err(error) => {
            state.observability.record_provider_error();
            let fallback_model = state.default_model.trim();
            if !fallback_model.is_empty()
                && requested_model != fallback_model
                && request.model != fallback_model
            {
                tracing::warn!(
                    requested_model = %requested_model,
                    attempted_model = %request.model,
                    fallback_model = %fallback_model,
                    error = %error,
                    "Primary streaming chat attempt failed; retrying with DEFAULT_MODEL"
                );
                state.observability.record_fallback();
                request.model = fallback_model.to_string();
                state
                    .provider
                    .chat_stream(request)
                    .await
                    .map(|stream| {
                        Box::pin(stream.map(
                            |event: Result<
                                ChatStreamEvent,
                                Box<dyn std::error::Error + Send + Sync>,
                            >| {
                                event.map_err(|stream_error| {
                                    AppError::Provider(stream_error.to_string())
                                })
                            },
                        )) as AppChatStream
                    })
                    .map_err(|fallback_error| {
                        state.observability.record_provider_error();
                        AppError::Provider(format!("Fallback also failed: {}", fallback_error))
                    })
            } else {
                Err(AppError::Provider(error.to_string()))
            }
        }
    }
}

pub async fn execute_chat(
    state: &AppState,
    payload: ChatRequest,
    selection: AgentSelection<'_>,
) -> Result<ChatResponse, AppError> {
    // Resolve first: from here on the selection is the router's answer or
    // the header's, in one shape, and every step below reads that shape.
    let resolved = resolve_selection(state, selection, &payload.messages).await?;
    let requested_model = resolved.requested_target(&payload.model);
    let original_model = payload.model.clone();
    let engine_ids = engine_target(state, &resolved, &original_model);
    let prepared = prepare_request(state, payload, &resolved).await?;

    let debug_prompt = if selection.debug {
        Some(
            prepared
                .messages
                .iter()
                .map(|m| format!("[{}] {}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n---\n"),
        )
    } else {
        None
    };
    // The routing decision is debug output too, gated exactly like
    // `debug_prompt` and built beside it: without `X-Debug: true` the field
    // is `None`, so an ordinary caller never receives the project's agent
    // roster and the distribution over it.
    let debug_route = if selection.debug {
        resolved.routed
    } else {
        None
    };

    // Use agent engine when headers or model resolve an agent and runtime is available.
    if let Some((project_id, agent_id)) = engine_ids {
        if let Some(runtime) = &state.agent_runtime {
            let conversation_id = selection.conversation_id.map(|s| s.to_string());
            let rt_req =
                build_runtime_request(state, &prepared, project_id, agent_id, conversation_id)?;
            let started_at = Instant::now();
            match runtime.chat(rt_req).await {
                Ok(text) => {
                    let response = ChatResponse {
                        model: prepared.model.clone(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        message: ChatMessage {
                            role: "assistant".to_string(),
                            content: text,
                        },
                        done: true,
                        debug_prompt,
                        debug_route,
                    };
                    state
                        .observability
                        .record_chat_request(started_at.elapsed().as_millis() as u64);
                    tracing::info!(
                        requested_model = %requested_model,
                        final_model = %response.model,
                        "Completed chat request via agent engine"
                    );
                    return Ok(response);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "agent engine failed; falling back to legacy chat");
                }
            }
        }
    }

    let started_at = Instant::now();
    let mut response = run_with_fallback(state, prepared, &requested_model).await?;
    response.debug_prompt = debug_prompt;
    // The decision rides here too — `None` unless `X-Debug: true` asked for
    // it, same as the engine path above. The streaming path has no field to
    // carry it and gRPC drops it deliberately.
    response.debug_route = debug_route;

    state
        .observability
        .record_chat_request(started_at.elapsed().as_millis() as u64);
    tracing::info!(
        requested_model = %requested_model,
        final_model = %response.model,
        "Completed chat request"
    );
    Ok(response)
}

pub async fn execute_chat_stream(
    state: &AppState,
    mut payload: ChatRequest,
    selection: AgentSelection<'_>,
) -> Result<AppChatStream, AppError> {
    payload.stream = true;
    // Same resolve-first shape as `execute_chat`: one selection type feeds
    // `requested_target`, `engine_target` and `prepare_request` here too.
    let resolved = resolve_selection(state, selection, &payload.messages).await?;
    let requested_model = resolved.requested_target(&payload.model);
    let original_model = payload.model.clone();
    let engine_ids = engine_target(state, &resolved, &original_model);
    let prepared = prepare_request(state, payload, &resolved).await?;
    let started_at = Instant::now();

    if let Some((project_id, agent_id)) = engine_ids {
        if let Some(runtime) = &state.agent_runtime {
            let conversation_id = selection.conversation_id.map(|s| s.to_string());
            let rt_req =
                build_runtime_request(state, &prepared, project_id, agent_id, conversation_id)?;
            match runtime.chat_stream(rt_req).await {
                Ok(rx) => {
                    state
                        .observability
                        .record_chat_request(started_at.elapsed().as_millis() as u64);
                    tracing::info!(
                        requested_model = %requested_model,
                        "Started streaming chat request via agent engine"
                    );
                    return Ok(engine_events_to_chat_stream(prepared.model.clone(), rx));
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "agent engine stream failed; falling back to legacy stream"
                    );
                }
            }
        }
    }

    let stream = run_stream_with_fallback(state, prepared, &requested_model).await?;
    state
        .observability
        .record_chat_request(started_at.elapsed().as_millis() as u64);
    tracing::info!(requested_model = %requested_model, "Started streaming chat request");
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::{execute_chat, execute_chat_stream, AgentSelection};
    use crate::adapters::mcp::StaticMcpRegistry;
    use crate::api::handlers::AppState;
    use crate::api::observability::AppObservability;
    use crate::context::analyzer::ContextEnricher;
    use crate::context::store::{ContextStore, ProjectContext};
    use crate::domain::agent::{AgentConfig, OptimizeConfig};
    use crate::domain::models::{
        ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo,
    };
    use crate::optimizer::metrics::TokenMetrics;
    use crate::ports::engine::{AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult};
    use crate::providers::LLMProvider;
    use crate::services::agent_registry::AgentRegistry;
    use crate::services::agent_runtime::AgentRuntime;
    use crate::services::router_config::RouterConfig;
    use crate::services::skill_manager::SkillManager;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::error::Error;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, RwLock};
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio_stream::{Stream, StreamExt};

    struct FakeEngine {
        response: String,
        fail: bool,
    }

    #[async_trait]
    impl AgentEngine for FakeEngine {
        async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
            if self.fail {
                return Err("engine down".into());
            }
            let hist_len = req.history.len();
            Ok(AgentRunResult {
                text: format!("{}|{}|h{}", self.response, req.user_message, hist_len),
                tool_calls: 0,
                iterations: 1,
                model: req.model,
            })
        }

        async fn run_stream(
            &self,
            req: AgentRunRequest,
        ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
            let (tx, rx) = mpsc::channel(4);
            let result = self.run(req).await?;
            let _ = tx
                .send(AgentRunEvent::Token {
                    text: result.text.clone(),
                })
                .await;
            let _ = tx.send(AgentRunEvent::Completed { result }).await;
            Ok(rx)
        }
    }

    fn lock_env() -> MutexGuard<'static, ()> {
        crate::core::paths::lock_env_for_tests()
    }

    /// The enricher a test-built runtime needs.
    ///
    /// The real thing, not a stub: `AgentRuntime` enriches whenever the caller
    /// brings no system prompt of its own, and a fake here would have kept these
    /// tests green while the production runtime quietly dropped the agent's rules.
    fn test_enricher() -> Arc<crate::context::analyzer::ContextEnricher> {
        Arc::new(crate::context::analyzer::ContextEnricher::new(
            Arc::new(crate::context::store::ContextStore::new()),
            Arc::new(crate::services::skill_manager::SkillManager::new()),
            "test-model".to_string(),
        ))
    }

    struct FakeProvider {
        fail_primary_once: AtomicBool,
        fail_always: bool,
    }

    #[async_trait]
    impl LLMProvider for FakeProvider {
        fn get_base_url(&self) -> String {
            "http://fake-provider".to_string()
        }

        async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
            Ok(())
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
            Ok(vec![
                ModelInfo {
                    name: "fallback-model".to_string(),
                    modified_at: "now".to_string(),
                    size: 1,
                },
                ModelInfo {
                    name: "agent-model".to_string(),
                    modified_at: "now".to_string(),
                    size: 1,
                },
            ])
        }

        async fn chat(
            &self,
            request: ChatRequest,
        ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
            if self.fail_always {
                return Err("always fail".into());
            }
            if request.model == "agent-model"
                && self.fail_primary_once.swap(false, Ordering::SeqCst)
            {
                return Err("fail once".into());
            }
            Ok(ChatResponse {
                model: request.model,
                created_at: "now".to_string(),
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: request
                        .messages
                        .last()
                        .map(|msg| msg.content.clone())
                        .unwrap_or_default(),
                },
                done: true,
                debug_prompt: None,
                debug_route: None,
            })
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<
                Box<
                    dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send,
                >,
            >,
            Box<dyn Error + Send + Sync>,
        > {
            if request.model == "agent-model"
                && self.fail_primary_once.swap(false, Ordering::SeqCst)
            {
                return Err("fail once".into());
            }
            Ok(Box::pin(tokio_stream::iter(vec![Ok(ChatStreamEvent {
                model: request.model,
                created_at: "now".to_string(),
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: request
                        .messages
                        .last()
                        .map(|msg| msg.content.clone())
                        .unwrap_or_default(),
                },
                done: true,
            })])))
        }
    }

    fn test_state(provider: Arc<dyn LLMProvider + Send + Sync>) -> (TempDir, Arc<AppState>) {
        test_state_with_runtime(provider, None)
    }

    fn test_state_with_runtime(
        provider: Arc<dyn LLMProvider + Send + Sync>,
        agent_runtime: Option<Arc<AgentRuntime>>,
    ) -> (TempDir, Arc<AppState>) {
        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_var("LLAMA_R_DIR", temp_dir.path());
        let agent_registry = agent_runtime
            .as_ref()
            .map(|rt| rt.registry.clone())
            .unwrap_or_else(|| Arc::new(AgentRegistry::new()));
        let _ = agent_registry.reload_all(&[]);
        let skill_manager = Arc::new(SkillManager::new());
        let context_store = Arc::new(ContextStore::new());
        let context_enricher = Arc::new(ContextEnricher::new(
            context_store.clone(),
            skill_manager.clone(),
            "fallback-model".to_string(),
        ));
        let state = Arc::new(AppState {
            provider,
            agent_registry,
            skill_manager,
            context_store,
            context_enricher,
            metrics: Arc::new(TokenMetrics::new()),
            observability: Arc::new(AppObservability::new()),
            default_model: "fallback-model".to_string(),
            api_running: AtomicBool::new(false),
            grpc_running: AtomicBool::new(false),
            logs: Arc::new(Mutex::new(VecDeque::new())),
            known_mcp_servers: RwLock::new(Vec::new()),
            mcp_registry: Arc::new(StaticMcpRegistry::new()),
            agent_runtime,
            rag_store: None,
            rag_ingest: None,
            history_store: None,
            // No test here consults the router; the routing ladder is injected
            // through these fields by the tests that do.
            router: RwLock::new(None),
            router_config: Arc::new(RouterConfig::default()),
            router_healthy: AtomicBool::new(false),
        });
        (temp_dir, state)
    }

    #[tokio::test]
    async fn direct_request_should_succeed_without_agent() {
        let _guard = lock_env();
        let (_dir, state) = test_state(Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        }));
        let response = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                stream: false,
            },
            AgentSelection::default(),
        )
        .await
        .unwrap();
        assert_eq!(response.model, "fallback-model");
    }

    #[tokio::test]
    async fn project_header_without_agent_should_use_project_general_agent() {
        let _guard = lock_env();
        let (_dir, state) = test_state(Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        }));
        state
            .context_store
            .save_context(ProjectContext {
                project_id: "demo".to_string(),
                path: ".".to_string(),
                context_md: "project context".to_string(),
                project_type: "rust".to_string(),
                skills_injected: vec![],
                last_analyzed: chrono::Utc::now(),
                custom_rules: String::new(),
            })
            .unwrap();
        let config = AgentConfig {
            name: "Demo".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "You are helpful".to_string(),
            context_project: Some("demo".to_string()),
            optimize: OptimizeConfig::default(),
            ..Default::default()
        };
        let dir = crate::core::paths::get_project_agents_dir("demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("demo.toml"), toml::to_string(&config).unwrap()).unwrap();
        state.agent_registry.reload_all(&[]).unwrap();

        let response = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "question".to_string(),
                }],
                stream: false,
            },
            AgentSelection {
                project_id: Some("demo"),
                agent_id: None,
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(response.model, "agent-model");
    }

    #[tokio::test]
    async fn project_and_agent_headers_should_resolve_specific_project_agent() {
        let _guard = lock_env();
        let (_dir, state) = test_state(Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        }));
        let config = AgentConfig {
            name: "Reviewer".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "Review this code".to_string(),
            context_project: Some("demo".to_string()),
            optimize: OptimizeConfig::default(),
            ..Default::default()
        };
        let dir = crate::core::paths::get_project_agents_dir("demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("reviewer.toml"), toml::to_string(&config).unwrap()).unwrap();
        state.agent_registry.reload_all(&[]).unwrap();

        let response = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "question".to_string(),
                }],
                stream: false,
            },
            AgentSelection {
                project_id: Some("demo"),
                agent_id: Some("reviewer"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(response.model, "agent-model");
    }

    #[tokio::test]
    async fn project_with_non_existent_agent_should_fail() {
        let _guard = lock_env();
        let (_dir, state) = test_state(Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        }));

        // We don't even need to create the project dir, resolve_agent will return None
        let err = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                stream: false,
            },
            AgentSelection {
                project_id: Some("non-existent-project"),
                agent_id: Some("ghost-agent"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("Agent 'ghost-agent' not found for project 'non-existent-project'"));
    }

    #[tokio::test]
    async fn fallback_failure_should_return_error() {
        let _guard = lock_env();
        let (_dir, state) = test_state(Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: true,
        }));
        let err = execute_chat(
            &state,
            ChatRequest {
                model: "agent-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                stream: false,
            },
            AgentSelection::default(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Provider error"));
    }

    #[tokio::test]
    async fn streaming_request_should_apply_same_resolution_path() {
        let _guard = lock_env();
        let (_dir, state) = test_state(Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        }));
        let mut stream = execute_chat_stream(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                stream: true,
            },
            AgentSelection::default(),
        )
        .await
        .unwrap();
        let event = stream.next().await.unwrap().unwrap();
        assert_eq!(event.model, "fallback-model");
    }

    #[tokio::test]
    async fn agent_runtime_returns_engine_text() {
        let _guard = lock_env();
        let engine = Arc::new(FakeEngine {
            response: "engine-echo".to_string(),
            fail: false,
        });
        let registry = Arc::new(AgentRegistry::new());
        let runtime = Arc::new(AgentRuntime {
            registry: registry.clone(),
            engine,
            history: None,
            rag: None,
            enricher: test_enricher(),
        });
        let provider = Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        });
        let (_dir, state) = test_state_with_runtime(provider, Some(runtime));

        // Create a project agent
        use crate::domain::agent::AgentConfig;
        let config = AgentConfig {
            name: "test-agent".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "You are helpful".to_string(),
            context_project: Some("test-proj".to_string()),
            ..Default::default()
        };
        let dir = crate::core::paths::get_project_agents_dir("test-proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("test-agent.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        registry.reload_all(&[]).unwrap();

        let response = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "ping".to_string(),
                }],
                stream: false,
            },
            AgentSelection {
                project_id: Some("test-proj"),
                agent_id: Some("test-agent"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap();
        assert!(response.message.content.contains("engine-echo"));
        assert!(response.message.content.contains("ping"));
    }

    #[tokio::test]
    async fn chat_uses_engine_when_agent_present() {
        let _guard = lock_env();
        // Provider panics if called (engine should handle it)
        let engine = Arc::new(FakeEngine {
            response: "from-engine".to_string(),
            fail: false,
        });
        let registry = Arc::new(AgentRegistry::new());
        let runtime = Arc::new(AgentRuntime {
            registry: registry.clone(),
            engine,
            history: None,
            rag: None,
            enricher: test_enricher(),
        });
        let provider = Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: true,
        });
        let (_dir, state) = test_state_with_runtime(provider, Some(runtime));

        let config = AgentConfig {
            name: "engine-agent".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "You are helpful".to_string(),
            context_project: Some("engine-proj".to_string()),
            ..Default::default()
        };
        let dir = crate::core::paths::get_project_agents_dir("engine-proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("engine-agent.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        registry.reload_all(&[]).unwrap();

        let response = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                stream: false,
            },
            AgentSelection {
                project_id: Some("engine-proj"),
                agent_id: Some("engine-agent"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap();
        assert!(response.message.content.contains("from-engine"));
    }

    #[tokio::test]
    async fn chat_falls_back_when_engine_fails() {
        let _guard = lock_env();
        let engine = Arc::new(FakeEngine {
            response: "".to_string(),
            fail: true,
        });
        let registry = Arc::new(AgentRegistry::new());
        let runtime = Arc::new(AgentRuntime {
            registry: registry.clone(),
            engine,
            history: None,
            rag: None,
            enricher: test_enricher(),
        });
        let provider = Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        });
        let (_dir, state) = test_state_with_runtime(provider, Some(runtime));

        let config = AgentConfig {
            name: "fallback-agent".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "You are helpful".to_string(),
            context_project: Some("fallback-proj".to_string()),
            ..Default::default()
        };
        let dir = crate::core::paths::get_project_agents_dir("fallback-proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fallback-agent.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        registry.reload_all(&[]).unwrap();

        // Engine fails -> falls back to FakeProvider
        let response = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "fallback-msg".to_string(),
                }],
                stream: false,
            },
            AgentSelection {
                project_id: Some("fallback-proj"),
                agent_id: Some("fallback-agent"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap();
        assert!(response.message.content.contains("fallback-msg"));
    }

    #[tokio::test]
    async fn project_missing_agent_still_400() {
        let _guard = lock_env();
        let (_dir, state) = test_state(Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: false,
        }));

        let err = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                stream: false,
            },
            AgentSelection {
                project_id: Some("ghost-project"),
                agent_id: Some("ghost-agent"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("Agent 'ghost-agent' not found for project 'ghost-project'"));
    }

    #[tokio::test]
    async fn engine_receives_prior_history_turns() {
        let _guard = lock_env();
        let engine = Arc::new(FakeEngine {
            response: "hist".to_string(),
            fail: false,
        });
        let registry = Arc::new(AgentRegistry::new());
        let runtime = Arc::new(AgentRuntime {
            registry: registry.clone(),
            engine,
            history: None,
            rag: None,
            enricher: test_enricher(),
        });
        let provider = Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: true,
        });
        let (_dir, state) = test_state_with_runtime(provider, Some(runtime));

        let config = AgentConfig {
            name: "hist-agent".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "You are helpful".to_string(),
            context_project: Some("hist-proj".to_string()),
            ..Default::default()
        };
        let dir = crate::core::paths::get_project_agents_dir("hist-proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("hist-agent.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        registry.reload_all(&[]).unwrap();

        let response = execute_chat(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![
                    ChatMessage {
                        role: "user".to_string(),
                        content: "first".to_string(),
                    },
                    ChatMessage {
                        role: "assistant".to_string(),
                        content: "reply".to_string(),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: "second".to_string(),
                    },
                ],
                stream: false,
            },
            AgentSelection {
                project_id: Some("hist-proj"),
                agent_id: Some("hist-agent"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap();
        // FakeEngine encodes history length as hN; two prior turns (user+assistant).
        assert!(
            response.message.content.contains("|h2"),
            "expected history length 2, got {}",
            response.message.content
        );
        assert!(response.message.content.contains("second"));
    }

    #[tokio::test]
    async fn streaming_uses_engine_when_agent_present() {
        let _guard = lock_env();
        let engine = Arc::new(FakeEngine {
            response: "stream-engine".to_string(),
            fail: false,
        });
        let registry = Arc::new(AgentRegistry::new());
        let runtime = Arc::new(AgentRuntime {
            registry: registry.clone(),
            engine,
            history: None,
            rag: None,
            enricher: test_enricher(),
        });
        let provider = Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: true,
        });
        let (_dir, state) = test_state_with_runtime(provider, Some(runtime));

        let config = AgentConfig {
            name: "stream-agent".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "You are helpful".to_string(),
            context_project: Some("stream-proj".to_string()),
            ..Default::default()
        };
        let dir = crate::core::paths::get_project_agents_dir("stream-proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("stream-agent.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        registry.reload_all(&[]).unwrap();

        let mut stream = execute_chat_stream(
            &state,
            ChatRequest {
                model: "fallback-model".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hi".to_string(),
                }],
                stream: true,
            },
            AgentSelection {
                project_id: Some("stream-proj"),
                agent_id: Some("stream-agent"),
                conversation_id: None,
                debug: false,
            },
        )
        .await
        .unwrap();

        let mut saw_engine = false;
        let mut saw_done = false;
        while let Some(item) = stream.next().await {
            let event = item.unwrap();
            if event.message.content.contains("stream-engine") {
                saw_engine = true;
            }
            if event.done {
                saw_done = true;
            }
        }
        assert!(saw_engine, "stream should include engine text");
        assert!(saw_done, "stream should complete");
    }

    #[tokio::test]
    async fn engine_used_when_model_is_global_agent_id() {
        let _guard = lock_env();
        let engine = Arc::new(FakeEngine {
            response: "by-model".to_string(),
            fail: false,
        });
        let registry = Arc::new(AgentRegistry::new());
        let runtime = Arc::new(AgentRuntime {
            registry: registry.clone(),
            engine,
            history: None,
            rag: None,
            enricher: test_enricher(),
        });
        let provider = Arc::new(FakeProvider {
            fail_primary_once: AtomicBool::new(false),
            fail_always: true,
        });
        let (_dir, state) = test_state_with_runtime(provider, Some(runtime));

        // Global agent (no project) whose id matches the request model field.
        let config = AgentConfig {
            name: "global-bot".to_string(),
            model: "agent-model".to_string(),
            system_prompt: "You are helpful".to_string(),
            ..Default::default()
        };
        let dir = crate::core::paths::get_agents_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("global-bot.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        registry.reload_all(&[]).unwrap();

        let response = execute_chat(
            &state,
            ChatRequest {
                model: "global-bot".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "ping".to_string(),
                }],
                stream: false,
            },
            AgentSelection::default(),
        )
        .await
        .unwrap();
        assert!(response.message.content.contains("by-model"));
    }

    #[test]
    fn split_engine_messages_extracts_history() {
        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: "sys".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "u1".into(),
            },
            ChatMessage {
                role: "assistant".into(),
                content: "a1".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "u2".into(),
            },
        ];
        let (system, history, user) = super::split_engine_messages(&messages).unwrap();
        assert_eq!(system.as_deref(), Some("sys"));
        assert_eq!(user, "u2");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "u1");
        assert_eq!(history[1].content, "a1");
    }
}
