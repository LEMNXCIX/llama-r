use crate::api::chat_core::agent_not_found;
use crate::api::handlers::AppState;
use crate::domain::models::{ChatMessage, RoutedDebug};
use crate::error::AppError;
use crate::ports::router::{Answer, DecisionRequest, DecisionResponse};
use crate::services::agent_registry::AgentRegistry;
use crate::services::agent_router::{
    build_questions, route, NoRoute, RoutingDecision, CHOICE_KEY, NOUL_KEY,
};
use crate::services::route_candidates::{build_candidates, RouteCandidate};
use std::collections::BTreeMap;
use std::time::Instant;

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
/// else — so `engine_target` in the chat module cannot tell a routed request from a
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

    pub(crate) fn requested_target(&self, fallback_model: &str) -> String {
        match (&self.project_id, &self.agent_id) {
            (Some(project_id), Some(agent_id)) => format!("{}/{}", project_id, agent_id),
            (Some(project_id), None) => format!("{}/{}", project_id, project_id),
            (None, Some(agent_id)) => agent_id.clone(),
            (None, None) => fallback_model.to_string(),
        }
    }
}

/// Characters of the conversation the decision model may see.
///
/// 8 000 is comfortably inside d1-3B's 32 K context, leaving room for both
/// questions and every candidate's criteria text.
pub(crate) const ROUTE_STATE_BUDGET_CHARS: usize = 8_000;

/// The [`RoutedDebug::reason`] spelling for a winner that a hot reload
/// removed between the decision and the use — the rung the ladder cannot
/// report, because to the ladder the winner was valid.
pub(crate) const STALE_WINNER: &str = "stale_winner";

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

/// The guard both decision callers run before spending a decision — `None`
/// when a decision is worth spending, the stand-in to answer with when it
/// is not.
///
/// Every rung below rung 2 of the ladder degrades to the project's general
/// agent, and rung 2 checks that one exists *before* it reads the response:
/// for a candidate set without one the answer is already known, so deciding
/// first would buy a forward pass nobody reads. The error returned here
/// stands in for the skipped call — callers still hand it to `route`, whose
/// `NoGeneralAgent` arm turns it into the same `400` it has always
/// produced, so only the wasted call changes and the message still comes
/// from the ladder rather than from a second copy of its words.
pub(crate) fn skip_decision_without_a_fallback(candidates: &[RouteCandidate]) -> Option<AppError> {
    if candidates
        .iter()
        .any(|candidate| candidate.is_project_general)
    {
        None
    } else {
        Some(AppError::Unavailable(
            "decision skipped: the project has no general agent to degrade to".into(),
        ))
    }
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
/// Past the gates, one decision is spent — unless
/// [`skip_decision_without_a_fallback`] has already seen that the candidate
/// set holds no general agent to degrade to, in which case [`route`] is
/// still asked and answers with today's `400` without the router being
/// called. A specialist is re-resolved through `registry` before it is
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
    let router = state.services_snapshot().await.router.clone();
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

    // The guard, in front of the decision: rung 2 of [`route`] checks for
    // the general agent *before* it reads the response, so for a set
    // without one the answer is already known — a decision here would be
    // bought only to be thrown away. The stand-in is what `route` receives
    // in place of a response, and its `NoGeneralAgent` arm turns it into
    // the same 400 as ever.
    let skipped = skip_decision_without_a_fallback(&candidates);
    let decided = skipped.is_none();

    let (result, latency_ms) = match skipped {
        Some(error) => (Err(error), 0),
        None => {
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
            (result, latency_ms)
        }
    };

    // The model's own answer, read before `route` consumes the response.
    // Each score is `None` when the response carried no usable value — never
    // `0.0`, which would narrate a malformed answer as a real score.
    let peeked = match &result {
        Ok(response) => peeked_answer(response, candidates.len()),
        Err(_) => PeekedAnswer::default(),
    };
    let candidate_keys: Vec<String> = candidates.iter().map(|c| c.key.clone()).collect();

    let debug = |outcome: &'static str, reason: Option<&'static str>| RoutedDebug {
        project_id: project_id.to_string(),
        outcome: outcome.to_string(),
        reason: reason.map(str::to_string),
        winner: peeked.winner.clone(),
        confidence: peeked.confidence,
        specialised: peeked.specialised,
        probabilities: peeked.probabilities.clone(),
        probabilities_note: peeked.probabilities_note.clone(),
        candidates: candidate_keys.clone(),
    };

    // One decision log per consultation, before any rung's own warning can
    // narrate over it: the project, the model's chosen key, both scores,
    // how wide the choice was, and what the decision cost. A consultation
    // the guard skipped logs nothing — there was no decision to narrate.
    if decided {
        tracing::info!(
            project = %project_id,
            chosen_key = ?peeked.winner,
            confidence = ?peeked.confidence,
            specialised = ?peeked.specialised,
            candidates = candidate_keys.len(),
            latency_ms,
            "routing decision"
        );
    }

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
            // Review Focus #4 — the hot-reload race, checked by the shared
            // [`recheck_winner`] so the preview reports the same rung this
            // dispatch does. The counters below are dispatch's own: a
            // preview decides nothing and must not inflate them.
            match recheck_winner(
                registry,
                project_id,
                &winner_project,
                &agent_id,
                &candidates,
            ) {
                WinnerRecheck::Present => {
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
                }
                WinnerRecheck::Stale(general_agent_id) => {
                    state.observability.record_route_to_general();
                    Ok(ResolvedSelection {
                        project_id: Some(project_id.to_string()),
                        agent_id: Some(general_agent_id),
                        routed: Some(debug("general", Some(STALE_WINNER))),
                    })
                }
                WinnerRecheck::NoFallback => {
                    // `recheck_winner` only reports this when the candidate
                    // set holds no general agent, which `route` never lets
                    // happen — and if it ever did, the header path is the
                    // safe answer. The counter fires as it always has on
                    // this arm.
                    state.observability.record_route_to_general();
                    Ok(ResolvedSelection::from_selection(&selection))
                }
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
            // `prepare_request`. The guard arrives here with its stand-in
            // when the set never held a general agent — a response is only
            // ever judged past rung 2 when one does — so the message keeps
            // coming from this one place.
            Err(agent_not_found(project_id, project_id))
        }
    }
}

/// The re-check every winner survives after [`route`] decides, shared by
/// dispatch and the preview endpoint so the two surfaces cannot disagree
/// about the rung: hot reload can remove the winner between the decision
/// and its use, so the key is resolved again through `registry`.
///
/// `request_project` is the project the decision was made for — it appears
/// in the log line; a global winner keeps its own (empty) project id in
/// `winner_project`, pinned by
/// `a_global_winner_carries_an_empty_project_id`: its registry key is the
/// bare id, so [`AgentRegistry::get_agent`] — never `get_project_agent`
/// under the requesting project, which would look for a same-named
/// *project* agent instead.
///
/// [`WinnerRecheck::Stale`]'s general agent id comes from the pre-reload
/// candidate set and is deliberately **not** re-checked against
/// `registry`. That is the one exception to "an id never verified to exist
/// must never reach dispatch" — named in both callers — because
/// re-checking it would have nowhere safer to land: the general agent is
/// the fallback, so if the same reload removed it too, `prepare_request`
/// finds nothing and reproduces today's 400 rather than dispatching to
/// something worse than the report.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WinnerRecheck {
    /// The winner is still in `registry`; use it.
    Present,
    /// A reload removed the winner; fall back to this general agent (the
    /// ladder's own fallback, taken from the pre-reload candidate set).
    Stale(String),
    /// The candidate set held no general agent to degrade to. `route` only
    /// returns `Agent` when one exists, so this cannot happen; each caller
    /// supplies its own safe answer — the header path for dispatch, the
    /// chat's own 400 for the preview.
    NoFallback,
}

pub(crate) fn recheck_winner(
    registry: &AgentRegistry,
    request_project: &str,
    winner_project: &str,
    agent_id: &str,
    candidates: &[RouteCandidate],
) -> WinnerRecheck {
    let present = if winner_project.is_empty() {
        registry.get_agent(agent_id).is_some()
    } else {
        registry
            .get_project_agent(winner_project, agent_id)
            .is_some()
    };
    if present {
        return WinnerRecheck::Present;
    }
    let vanished_key = if winner_project.is_empty() {
        agent_id.to_string()
    } else {
        format!("{winner_project}::{agent_id}")
    };
    tracing::warn!(
        project = %request_project,
        vanished_key = %vanished_key,
        "routed agent removed by a hot reload between the decision and the use; \
         falling back to the project's general agent"
    );
    match candidates.iter().find(|c| c.is_project_general) {
        Some(general) => WinnerRecheck::Stale(general.agent_id.clone()),
        None => WinnerRecheck::NoFallback,
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
    /// The distribution as the model reported it, whenever it is
    /// informative — a partial distribution is exactly what threshold
    /// calibration reads, so it is kept even when it names only some
    /// candidates (with [`probabilities_note`](Self::probabilities_note)
    /// saying how much of the choice it covers). Empty only when the model
    /// reported none, or when a non-empty distribution contradicted its own
    /// winner.
    pub probabilities: BTreeMap<String, f32>,
    /// Set whenever the distribution is not a plain account of the winner,
    /// naming which case: the model reported no probabilities at all; the
    /// probabilities cover only some of the offered candidates (the map is
    /// attached anyway); the winner is absent from its own distribution, or
    /// another candidate outscores it (the map is withheld — an account of
    /// a choice it contradicts must not be presented as evidence for it).
    /// Both debug surfaces — the preview endpoint and
    /// `ChatResponse::debug_route` — read this one extractor, so they
    /// cannot disagree about it.
    pub probabilities_note: Option<String>,
}

/// `offered` is the number of candidates the choice question named, so a
/// distribution that covers only some of them can be kept *and* labelled
/// rather than mistaken for a complete account.
pub(crate) fn peeked_answer(response: &DecisionResponse, offered: usize) -> PeekedAnswer {
    let mut peeked = PeekedAnswer::default();
    if let Some(Answer::Choice {
        key,
        confidence,
        probabilities,
    }) = response.answers.get(CHOICE_KEY)
    {
        peeked.winner = Some(key.clone());
        peeked.confidence = confidence.is_finite().then_some(*confidence);
        // The adapter never checks `probabilities` against the winner or
        // the offered set, so the wire can carry a distribution that is
        // empty, partial, or flatly contradictory. It is kept whenever
        // it is informative — calibration is the preview's reason to
        // exist — and withheld only when it is empty or contradicts the
        // winner, with the note naming which case. The ladder still
        // decides from the winner either way, and `raw` on the preview
        // keeps exactly what the router answered: nothing is lost
        // silently, only a misleading presentation is.
        let top = probabilities
            .values()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max);
        match probabilities.get(key) {
            // The winner is in its own distribution and top-scored (a
            // tie at the top is not a contradiction): keep whatever the
            // model offered, and say when it named only some candidates
            // rather than letting a partial map read as a full account.
            Some(score) if *score >= top => {
                if probabilities.len() < offered {
                    peeked.probabilities_note = Some(format!(
                        "the model's probabilities cover {} of {} candidates; \
                         the partial distribution is attached",
                        probabilities.len(),
                        offered
                    ));
                }
                peeked.probabilities = probabilities.clone();
            }
            // The winner is in the map but another candidate outscores
            // it: the model contradicted its own choice. Withheld.
            Some(_) => {
                tracing::warn!(
                    winner = %key,
                    "router probabilities score a candidate above the router's own winner; \
                     withholding the distribution"
                );
                peeked.probabilities_note = Some(format!(
                    "the model chose '{}' but its own probabilities score another \
                     candidate higher; the distribution is withheld rather than \
                     presented as an account of a choice it contradicts",
                    key
                ));
            }
            // The winner is absent from the map: either the model
            // reported no distribution at all, or a non-empty one that
            // does not contain its own winner. Both are said; only the
            // second is a contradiction.
            None => {
                if probabilities.is_empty() {
                    tracing::warn!(
                        winner = %key,
                        "router reported no probabilities beside its winner"
                    );
                    peeked.probabilities_note = Some(format!(
                        "the model chose '{}' but reported no probabilities at all",
                        key
                    ));
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
        }
    }
    peeked.specialised = match response.answers.get(NOUL_KEY) {
        Some(Answer::Noul { value }) => value.is_finite().then_some(*value),
        _ => None,
    };
    peeked
}
