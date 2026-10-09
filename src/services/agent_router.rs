//! The decision ladder: the decision model's answer in, the agent that will
//! answer the request out.
//!
//! Routing changes *who* answers a message, so d1's answer cannot be taken at
//! face value: it is a calibrated distribution over the labels it was given,
//! not a certified decision, and it arrives as two answers that gate different
//! things. The `Choice` confidence governs *trust* — how much the pick is
//! worth — and the `Noul` specialisation score governs *strength* — whether
//! any specialisation is warranted at all. A model can be 0.9 sure of the
//! topic and still concede the general agent handles it (0.2), so one
//! threshold cannot tell that apart from both being high: hence two gates,
//! checked in an order that matters (see [`route`]).
//!
//! Every rung that cannot decide lands on the project's general agent rather
//! than on an error: that is exactly what a request with an explicit
//! `X-Agent: <project>` does today, so a router that is down, malformed,
//! unsure, or simply silent degrades to today's behaviour instead of
//! inventing a new failure. The two [`NoRoute`] errors are the exceptions
//! that preserve today's contract — an empty candidate set, and a project
//! with no general agent to degrade *to*.
//!
//! The module performs no I/O, holds no `Arc` and takes the router's `Result`
//! by value: the caller owns the network call, which is what makes every rung
//! testable without a server. It also rejects answers that cannot exist — a
//! non-finite confidence or specialisation value, possible only through the
//! adapter's `f64 → f32` narrowing — as malformed, rather than reading
//! `INFINITY < threshold` (false) as maximum trust.

use crate::error::AppError;
use crate::ports::router::{Answer, DecisionResponse, Question};
use crate::services::route_candidates::RouteCandidate;
use std::collections::BTreeMap;

/// The `Choice` question's name: written by [`build_questions`], read back by
/// [`route`], so writer and reader cannot drift apart.
pub const CHOICE_KEY: &str = "agent";

/// The `Noul` question's name — same contract as [`CHOICE_KEY`].
pub const NOUL_KEY: &str = "specialized";

/// Why a request stayed with the project's general agent.
///
/// Every variant means "a specialist was not dispatched", but each names a
/// different point of the ladder where that became true, which is what makes
/// a misconfigured threshold or a broken adapter visible in the log instead
/// of as silence.
#[derive(Debug, Clone, PartialEq)]
pub enum GeneralReason {
    /// The router errored, or its answer was missing or malformed — the
    /// response did not answer what was asked.
    RouterUnavailable,
    /// The model returned a label that was not in the candidate set: it
    /// scores what it is offered, it does not certify what it returns.
    UnknownKey,
    /// A specialist won the choice, but the `Noul` says no specialisation is
    /// warranted at all.
    NotSpecialised,
    /// A specialist won the choice with specialisation warranted, but the
    /// confidence is below the threshold — the pick is not trusted.
    LowConfidence,
    /// The model's own pick was the general agent.
    GeneralWon,
}

impl GeneralReason {
    /// The spelling that reaches Task 7's decision log and Task 8's
    /// `debug_route` response — part of the interface, not prose.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RouterUnavailable => "router_unavailable",
            Self::UnknownKey => "unknown_key",
            Self::NotSpecialised => "not_specialised",
            Self::LowConfidence => "low_confidence",
            Self::GeneralWon => "general_won",
        }
    }
}

/// The ladder could not produce any decision at all.
///
/// This is the caller's problem, not a fallback: there is nothing safe left
/// to fall back *to*, which is exactly why both variants are checked before
/// the router result is examined.
#[derive(Debug, Clone, PartialEq)]
pub enum NoRoute {
    /// Nothing to choose between. No zero-option `Choice` is ever sent —
    /// [`build_questions`] returns an empty map instead, which is what makes
    /// the unanswerable question unrepresentable rather than merely unsent.
    NoCandidates,
    /// Every remaining rung degrades to the project's general agent, and
    /// this project has none.
    NoGeneralAgent,
}

/// What the ladder decided: a specialist, or the general agent with the
/// reason it kept the request.
#[derive(Debug, Clone, PartialEq)]
pub enum RoutingDecision {
    /// A specialist won outright; dispatch to it.
    Agent {
        /// The winner's project id. **Empty when the winner is a global
        /// agent** — globals have no project, and an empty id is how the
        /// caller tells a global pick from a project pick (the registry key
        /// of a global is its bare id, of a project agent `project::agent`).
        project_id: String,
        agent_id: String,
        /// The `Choice` confidence behind the pick; finite, or this would
        /// not be an `Agent`.
        confidence: f32,
        /// The `Noul` specialisation score; finite, and at least
        /// `min_specialised`, or this would not be an `Agent`.
        specialised: f32,
    },
    /// The request stays with the project's general agent.
    ProjectGeneral {
        project_id: String,
        agent_id: String,
        /// The confidence of the choice that was degraded, or `0.0` when
        /// there was no usable choice to report one for — the field narrates
        /// the model, it never carries a value the model did not produce.
        confidence: f32,
        reason: GeneralReason,
    },
}

/// The two questions one forward pass must answer, keyed by [`CHOICE_KEY`]
/// and [`NOUL_KEY`] — the same constants [`route`] reads back out of the
/// response, so the two cannot drift.
///
/// An empty candidate set builds an **empty** map rather than a `Choice` with
/// no criteria: a zero-option `Choice` is not answerable, so it is made
/// unrepresentable here instead of merely unsent, and a caller that finds no
/// questions has nothing to ask and never spends a decision.
pub fn build_questions(candidates: &[RouteCandidate]) -> BTreeMap<String, Question> {
    if candidates.is_empty() {
        return BTreeMap::new();
    }

    let criteria: BTreeMap<String, String> = candidates
        .iter()
        .map(|candidate| (candidate.key.clone(), candidate.criteria.clone()))
        .collect();

    BTreeMap::from([
        (
            CHOICE_KEY.to_string(),
            Question::Choice {
                instructions: "Which agent should handle this request? \
                     Pick the general agent if no specialisation fits."
                    .to_string(),
                criteria,
            },
        ),
        (
            NOUL_KEY.to_string(),
            Question::Noul {
                instructions: "Does this request need a specialised agent?".to_string(),
            },
        ),
    ])
}

/// Decide which agent answers this request, strictly in ladder order.
///
/// The ladder degrades at every rung rather than failing: `Ok(ProjectGeneral)`
/// is the answer for anything short of a trusted specialist, and the two
/// `Err(NoRoute)` variants are reserved for the cases with nothing safe to
/// degrade to (checked **before** the router result is examined, for exactly
/// that reason).
///
/// `project_id` is the project the request came in for; `min_confidence`
/// governs trust in the `Choice`, `min_specialised` governs whether any
/// specialisation is warranted, and `response` is the router's already-fetched
/// `Result` — this function performs no I/O of its own.
pub fn route(
    project_id: &str,
    candidates: &[RouteCandidate],
    min_confidence: f32,
    min_specialised: f32,
    response: Result<DecisionResponse, AppError>,
) -> Result<RoutingDecision, NoRoute> {
    // Rung 1 — an empty set short-circuits before anything is examined. It
    // matches `build_questions(&[]) == {}`: no options means no answerable
    // question, so there is nothing to degrade with either.
    if candidates.is_empty() {
        return Err(NoRoute::NoCandidates);
    }

    // Rung 2 — the fallback target must exist before the response is even
    // looked at, because every remaining rung degrades to the general agent
    // and there would be nothing to degrade to.
    let general = candidates
        .iter()
        .find(|candidate| candidate.is_project_general);
    let Some(general) = general else {
        return Err(NoRoute::NoGeneralAgent);
    };

    // The shape every later rung degrades into: the general agent, the
    // confidence the model reported for the choice it did not get, and the
    // rung's reason.
    let degrade = |confidence: f32, reason: GeneralReason| RoutingDecision::ProjectGeneral {
        project_id: project_id.to_string(),
        agent_id: general.agent_id.clone(),
        confidence,
        reason,
    };

    // Rung 3 — the router did not answer at all.
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "decision router failed; falling back to the project's general agent"
            );
            return Ok(degrade(0.0, GeneralReason::RouterUnavailable));
        }
    };

    // Rung 4 — the answer must first *be* an answer to the question asked.
    // The finiteness check lives here, with the extraction, because a
    // confidence that cannot exist is the same defect as a missing one: the
    // adapter narrows f64 to f32, so a wire value like `"confidence":
    // 1e300` arrives as f32::INFINITY — and `INFINITY < min_confidence` is
    // false, which would read it as MAXIMUM confidence and trust it. A
    // decision model cannot emit infinity.
    let Some(Answer::Choice {
        key, confidence, ..
    }) = response.answers.get(CHOICE_KEY)
    else {
        tracing::warn!(
            "decision response did not answer the choice question; \
             falling back to the project's general agent"
        );
        return Ok(degrade(0.0, GeneralReason::RouterUnavailable));
    };
    if !confidence.is_finite() {
        tracing::warn!(
            confidence,
            "decision response carried a non-finite confidence; \
             treating it as malformed, not as maximum trust"
        );
        return Ok(degrade(0.0, GeneralReason::RouterUnavailable));
    }
    let confidence = *confidence;

    // Rung 5 — the model returns the highest-probability label, not a
    // certified member of the set it was given, so a key outside the
    // candidates is discarded rather than trusted. Security invariant: this
    // rung exists so the router's output is untrusted input to dispatch.
    let Some(winner) = candidates.iter().find(|candidate| candidate.key == *key) else {
        tracing::warn!(
            key = %key,
            "decision model returned a key outside the candidate set; discarding it"
        );
        return Ok(degrade(confidence, GeneralReason::UnknownKey));
    };

    // Rung 6 — read the specialisation score. Absent → 0.0: an absent gate
    // must never default to trusting a specialist, so the missing Noul reads
    // as "no specialisation". Present but answering the wrong question is
    // the same defect as a missing Choice (rung 4): the response did not
    // answer what was asked. And like the confidence, a specialisation
    // score that cannot exist is malformed — NaN and infinity both pass a
    // `< min_specialised` comparison as *false*, which would read an
    // impossible value as maximum strength.
    let specialised = match response.answers.get(NOUL_KEY) {
        None => 0.0,
        Some(Answer::Noul { value }) if value.is_finite() => *value,
        Some(Answer::Noul { value }) => {
            tracing::warn!(
                specialised = value,
                "decision response carried a non-finite specialisation score; \
                 treating it as malformed, not as maximum strength"
            );
            return Ok(degrade(confidence, GeneralReason::RouterUnavailable));
        }
        Some(_) => {
            tracing::warn!(
                "the specialised question was not answered with a noul; \
                 falling back to the project's general agent"
            );
            return Ok(degrade(confidence, GeneralReason::RouterUnavailable));
        }
    };

    // Rung 7 — who the model picked, before either gate. The gates exist to
    // veto routing *away* from the general agent; a choice that already
    // names the general agent has nowhere else to go, so "the general agent
    // won" is the precise report (`the_general_agent_winning_is_reported_as_such`
    // hands the general agent a specialisation of 0.05, below any sane
    // threshold, and must still read GeneralWon). The routing outcome is
    // identical either way — every rung lands on the general agent for a
    // general winner — only the reported reason differs, and only here.
    if winner.is_project_general {
        return Ok(degrade(confidence, GeneralReason::GeneralWon));
    }

    // Rung 8 — the specialisation gate, checked BEFORE the confidence gate
    // on purpose: a low-confidence answer to "does this need a specialist"
    // and a low-confidence answer to "which specialist" are different
    // problems, and only the first one licenses ignoring the second. The
    // model can be 0.9 sure of the topic and still concede the general
    // agent handles it (a `Noul` of 0.2) — one threshold over the `Choice`
    // confidence cannot tell that apart from both being high, so strength
    // is settled first and trust only matters once specialisation is
    // actually warranted.
    if specialised < min_specialised {
        return Ok(degrade(confidence, GeneralReason::NotSpecialised));
    }

    // Rung 9 — trust: the choice is a specialist and specialisation is
    // warranted, but the pick itself is below threshold. The fallback is
    // the general agent, not the most capable specialist — deferring to a
    // specialist *because* the router was unconfident is the exact
    // misrouting the threshold exists to prevent.
    if confidence < min_confidence {
        return Ok(degrade(confidence, GeneralReason::LowConfidence));
    }

    // Rung 10 — a trusted specialist wins.
    Ok(RoutingDecision::Agent {
        project_id: winner.project_id.clone().unwrap_or_default(),
        agent_id: winner.agent_id.clone(),
        confidence,
        specialised,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The brief's fixtures: `(key, is_project_general)` pairs, with the rest
    /// of a `RouteCandidate` derived rather than hand-typed — `agent_id` and
    /// `project_id` are split out of the key in `candidate_key`'s scheme, so
    /// a fixture cannot claim a key its own fields would contradict.
    fn candidates(specs: &[(&str, bool)]) -> Vec<RouteCandidate> {
        specs
            .iter()
            .map(|(key, is_project_general)| {
                let (project_id, agent_id) = match key.split_once("::") {
                    Some((project, agent)) => (Some(project.to_string()), agent.to_string()),
                    None => (None, (*key).to_string()),
                };
                RouteCandidate {
                    key: (*key).to_string(),
                    agent_id: agent_id.clone(),
                    project_id,
                    label: agent_id,
                    criteria: format!("criteria for {key}"),
                    is_project_general: *is_project_general,
                }
            })
            .collect()
    }

    /// A response that answers only the `Choice` question — the shape the
    /// unknown-key rung and the missing-`Noul` rung each need. The brief
    /// names both call sites separately, so both names exist; they are one
    /// shape seen from two rungs.
    fn choice_response(key: &str, confidence: f32) -> DecisionResponse {
        DecisionResponse {
            answers: BTreeMap::from([(
                CHOICE_KEY.to_string(),
                Answer::Choice {
                    key: key.to_string(),
                    confidence,
                    probabilities: BTreeMap::from([(key.to_string(), confidence)]),
                },
            )]),
        }
    }

    /// [`choice_response`] under the name the missing-`Noul` test reads at
    /// its call site: the `Choice` is answered and nothing else is.
    fn choice_only(key: &str, confidence: f32) -> DecisionResponse {
        choice_response(key, confidence)
    }

    /// Both answers in one response — the shape one forward pass returns.
    fn choice_and_noul(key: &str, confidence: f32, specialised: f32) -> DecisionResponse {
        let mut response = choice_response(key, confidence);
        response
            .answers
            .insert(NOUL_KEY.to_string(), Answer::Noul { value: specialised });
        response
    }

    /// The answer a router *would* hand back, had one been consulted:
    /// fabricating it records a call in `calls`, so a test asserting the
    /// counter stayed at zero fails the moment anything actually asks for an
    /// answer. A marker value nothing could ever raise would pass regardless
    /// and prove nothing; this one is proven live in its own test.
    fn unreachable_response(calls: &std::cell::Cell<u32>) -> DecisionResponse {
        calls.set(calls.get() + 1);
        choice_and_noul("fudi::fudi", 1.0, 0.0)
    }

    /// Every degrading rung must land on the project's general agent — by its
    /// **bare `agent_id`** (`"fudi"`), because `get_project_agent` resolves
    /// ids and not routing keys — under the project the request came in for.
    /// The identity is the point of this helper: a `matches!` ending in `..`
    /// would pass a fallback wired to `general.key` (`"fudi::fudi"`), which
    /// would then miss on every degraded request in the next task while all
    /// reason assertions stayed green.
    ///
    /// Both literals are `"fudi"` on purpose: it is the fixture's project
    /// throughout this module, and its general agent's id equals the project
    /// id — that is what `is_project_general` means. `context` names the rung
    /// (or the offending value) in every panic.
    fn assert_degrades_to_general(
        decision: Result<RoutingDecision, NoRoute>,
        reason: GeneralReason,
        context: &str,
    ) {
        match decision {
            Ok(RoutingDecision::ProjectGeneral {
                project_id,
                agent_id,
                reason: got,
                ..
            }) => {
                assert_eq!(
                    project_id, "fudi",
                    "{context}: the fallback stays in the request's project"
                );
                assert_eq!(
                    agent_id, "fudi",
                    "{context}: the fallback is the general candidate's bare agent_id, \
                     not its routing key"
                );
                assert_eq!(got, reason, "{context}: wrong rung reported");
            }
            other => panic!("{context}: expected ProjectGeneral({reason:?}), got {other:?}"),
        }
    }

    // Review Focus #5: an empty set must short-circuit, not send a zero-option Choice.
    #[test]
    fn no_candidates_never_reaches_the_router() {
        // The counter is live, not decorative: fabricating an answer records a call.
        let probe = std::cell::Cell::new(0);
        unreachable_response(&probe);
        assert_eq!(probe.get(), 1, "the counter must actually record a call");

        // What the caller does: consult the router only when `build_questions`
        // hands it something to ask. With no candidates there is nothing to
        // ask, so no answer is ever fabricated — and the value `route` does
        // get is a router *error*, which must still lose to the empty set
        // (rung 1 above rung 3).
        let calls = std::cell::Cell::new(0);
        let response = if build_questions(&[]).is_empty() {
            Err(AppError::Provider("never consulted".into()))
        } else {
            Ok(unreachable_response(&calls))
        };

        let err = route("fudi", &[], 0.6, 0.5, response);
        assert!(matches!(err, Err(NoRoute::NoCandidates)));
        assert_eq!(
            calls.get(),
            0,
            "the router must not be called with no options"
        );
    }

    #[test]
    fn a_router_error_degrades_to_the_projects_general_agent() {
        let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
        let decision = route(
            "fudi",
            &candidates,
            0.6,
            0.5,
            Err(AppError::Provider("down".into())),
        );
        assert_degrades_to_general(
            decision,
            GeneralReason::RouterUnavailable,
            "router error rung",
        );
    }

    /// Finding 1: the arm a mismatched or partially-parsing adapter lands
    /// on. The response never answered the choice question at all — neither
    /// shape has an entry under `CHOICE_KEY` — and the reason must be
    /// `RouterUnavailable`, the string Task 7's log line and Task 8's
    /// `debug_route` key on, not `NotSpecialised`/`LowConfidence` (wrong
    /// rung named) and not an `Err` (a 400 for a project that routes fine
    /// today).
    #[test]
    fn a_response_that_never_answered_the_choice_question_is_router_unavailable() {
        let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
        let partial = [
            ("empty answers map", BTreeMap::new()),
            (
                "specialised answer without a choice answer",
                BTreeMap::from([(NOUL_KEY.to_string(), Answer::Noul { value: 0.91 })]),
            ),
        ];
        for (context, answers) in partial {
            let decision = route(
                "fudi",
                &candidates,
                0.6,
                0.5,
                Ok(DecisionResponse { answers }),
            );
            assert_degrades_to_general(decision, GeneralReason::RouterUnavailable, context);
        }
    }

    /// Finding 1, second shape: there *is* an entry under `CHOICE_KEY`, but
    /// it answers a different question. Same rung, same reason.
    #[test]
    fn a_choice_slot_holding_a_noul_is_router_unavailable() {
        let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
        let response = DecisionResponse {
            answers: BTreeMap::from([(CHOICE_KEY.to_string(), Answer::Noul { value: 0.91 })]),
        };
        let decision = route("fudi", &candidates, 0.6, 0.5, Ok(response));
        assert_degrades_to_general(
            decision,
            GeneralReason::RouterUnavailable,
            "choice slot holds a noul",
        );
    }

    #[test]
    fn a_key_outside_the_candidate_set_is_discarded() {
        // The module's security invariant: the model returns the highest-probability
        // label, not a certified member of the set it was given.
        let candidates = candidates(&[("fudi::fudi", true)]);
        let response = choice_response("fudi::ops", 0.99);
        let decision = route("fudi", &candidates, 0.6, 0.5, Ok(response));
        assert_degrades_to_general(decision, GeneralReason::UnknownKey, "unknown key rung");
    }

    #[test]
    fn low_specialisation_forces_the_general_agent_even_when_confidence_is_high() {
        let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
        let decision = route(
            "fudi",
            &candidates,
            0.6,
            0.5,
            Ok(choice_and_noul("fudi::ops", 0.95, 0.10)),
        );
        assert_degrades_to_general(
            decision,
            GeneralReason::NotSpecialised,
            "specialisation gate",
        );
    }

    #[test]
    fn low_confidence_forces_the_general_agent_even_when_specialisation_is_strong() {
        let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
        let decision = route(
            "fudi",
            &candidates,
            0.6,
            0.5,
            Ok(choice_and_noul("fudi::ops", 0.30, 0.90)),
        );
        assert_degrades_to_general(decision, GeneralReason::LowConfidence, "confidence gate");
    }

    #[test]
    fn the_general_agent_winning_is_reported_as_such() {
        let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
        let decision = route(
            "fudi",
            &candidates,
            0.6,
            0.5,
            Ok(choice_and_noul("fudi::fudi", 0.99, 0.05)),
        );
        assert_degrades_to_general(decision, GeneralReason::GeneralWon, "who-won rung");
    }

    #[test]
    fn a_confident_specialist_wins() {
        let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
        let decision = route(
            "fudi",
            &candidates,
            0.6,
            0.5,
            Ok(choice_and_noul("fudi::ops", 0.88, 0.91)),
        );
        match decision.unwrap() {
            RoutingDecision::Agent {
                project_id,
                agent_id,
                confidence,
                specialised,
            } => {
                assert_eq!(project_id, "fudi", "a project winner keeps its project");
                assert_eq!(agent_id, "ops");
                assert!((confidence - 0.88).abs() < 1e-6);
                assert!((specialised - 0.91).abs() < 1e-6);
            }
            other => panic!("expected a specialist, got {other:?}"),
        }
    }

    /// Finding 4: pins the global-winner contract Task 7 resolves on — an
    /// empty `project_id` means "the winner is a global agent", so it is
    /// resolved with `get_agent` and never looked up as a project agent of
    /// the requesting project. Reusing the request's project here
    /// (`project_id.to_string()` at rung 10) would compile and pass every
    /// other test while routing a global pick to a same-named *project*
    /// agent.
    #[test]
    fn a_global_winner_carries_an_empty_project_id() {
        let candidates = candidates(&[("fudi::fudi", true), ("ops", false)]);
        let decision = route(
            "fudi",
            &candidates,
            0.6,
            0.5,
            Ok(choice_and_noul("ops", 0.88, 0.91)),
        );
        match decision.unwrap() {
            RoutingDecision::Agent {
                project_id,
                agent_id,
                confidence,
                specialised,
            } => {
                assert_eq!(project_id, "", "a global agent has no project");
                assert_eq!(agent_id, "ops");
                assert!((confidence - 0.88).abs() < 1e-6);
                assert!((specialised - 0.91).abs() < 1e-6);
            }
            other => panic!("expected the global specialist, got {other:?}"),
        }
    }

    #[test]
    fn a_project_with_no_general_agent_cannot_degrade() {
        let candidates = candidates(&[("fudi::ops", false)]);
        let decision = route(
            "fudi",
            &candidates,
            0.6,
            0.5,
            Err(AppError::Provider("down".into())),
        );
        assert!(matches!(decision, Err(NoRoute::NoGeneralAgent)));
    }

    #[test]
    fn a_missing_noul_answer_is_treated_as_no_specialisation() {
        let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
        let decision = route(
            "fudi",
            &candidates,
            0.6,
            0.5,
            Ok(choice_only("fudi::ops", 0.99)),
        );
        assert_degrades_to_general(
            decision,
            GeneralReason::NotSpecialised,
            "an absent gate must not default to trusting a specialist",
        );
    }

    /// Finding 3: the mirror of rung 4, added deliberately in Task 5 and
    /// untested until now — the specialised question *was* answered, just
    /// not with a `Noul`, so the response did not answer what was asked and
    /// the reason is `RouterUnavailable`, not a gate that never ran.
    #[test]
    fn a_specialised_question_answered_with_a_choice_is_router_unavailable() {
        let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
        let response = DecisionResponse {
            answers: BTreeMap::from([
                (
                    CHOICE_KEY.to_string(),
                    Answer::Choice {
                        key: "fudi::ops".into(),
                        confidence: 0.99,
                        probabilities: BTreeMap::new(),
                    },
                ),
                (
                    NOUL_KEY.to_string(),
                    Answer::Choice {
                        key: "fudi::fudi".into(),
                        confidence: 0.9,
                        probabilities: BTreeMap::new(),
                    },
                ),
            ]),
        };
        let decision = route("fudi", &candidates, 0.6, 0.5, Ok(response));
        assert_degrades_to_general(
            decision,
            GeneralReason::RouterUnavailable,
            "noul slot holds a choice",
        );
    }

    // Controller ruling (Task 2's deferred minor, promoted): the adapter
    // narrows f64 to f32, so a wire value like `"confidence": 1e300` arrives
    // as f32::INFINITY — and `INFINITY < min_confidence` is false, which
    // would read an impossible value as MAXIMUM confidence and trust it. A
    // decision model cannot emit infinity; a value that cannot exist is a
    // malformed response, and the rung for a response that did not answer
    // what was asked is RouterUnavailable.
    #[test]
    fn a_non_finite_confidence_is_a_malformed_answer_not_maximum_trust() {
        let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
        for confidence in [f32::INFINITY, f32::NAN] {
            let decision = route(
                "fudi",
                &candidates,
                0.6,
                0.5,
                Ok(choice_and_noul("fudi::ops", confidence, 0.91)),
            );
            assert_degrades_to_general(
                decision,
                GeneralReason::RouterUnavailable,
                &format!("confidence {confidence} must not be trusted"),
            );
        }
    }

    /// The same hole through the other gate: NaN and infinity both pass a
    /// `< min_specialised` comparison as *false*, so an impossible
    /// specialisation score would be read as maximum strength and the
    /// specialist would be routed to.
    #[test]
    fn a_non_finite_specialisation_is_a_malformed_answer_not_maximum_strength() {
        let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
        for specialised in [f32::INFINITY, f32::NAN] {
            let decision = route(
                "fudi",
                &candidates,
                0.6,
                0.5,
                Ok(choice_and_noul("fudi::ops", 0.99, specialised)),
            );
            assert_degrades_to_general(
                decision,
                GeneralReason::RouterUnavailable,
                &format!("specialised {specialised} must not be trusted"),
            );
        }
    }

    /// `CHOICE_KEY`/`NOUL_KEY` exist so `build_questions` (which writes
    /// them) and `route` (which reads them back) cannot drift — this pins
    /// the content on the writing side, and the zero-option guarantee on the
    /// empty set.
    #[test]
    fn the_two_questions_share_the_keys_route_reads_back() {
        let candidates = candidates(&[("fudi::fudi", true), ("fudi::ops", false)]);
        let questions = build_questions(&candidates);
        assert_eq!(questions.len(), 2, "one forward pass, two questions");

        match questions.get(CHOICE_KEY) {
            Some(Question::Choice {
                instructions,
                criteria,
            }) => {
                assert_eq!(
                    instructions,
                    "Which agent should handle this request? \
                     Pick the general agent if no specialisation fits."
                );
                let expected: BTreeMap<String, String> = candidates
                    .iter()
                    .map(|candidate| (candidate.key.clone(), candidate.criteria.clone()))
                    .collect();
                assert_eq!(
                    criteria, &expected,
                    "criteria must be key → criteria text for every candidate"
                );
            }
            other => panic!("expected a Choice under {CHOICE_KEY}, got {other:?}"),
        }

        match questions.get(NOUL_KEY) {
            Some(Question::Noul { instructions }) => {
                assert_eq!(instructions, "Does this request need a specialised agent?");
            }
            other => panic!("expected a Noul under {NOUL_KEY}, got {other:?}"),
        }

        // Review Focus #5 again, from the writing side: an empty set builds
        // no questions at all, so the zero-option Choice is unrepresentable
        // rather than merely unsent.
        assert!(build_questions(&[]).is_empty());
    }

    /// `as_str`'s spelling reaches Task 7's decision log and Task 8's
    /// `debug_route` response, so it is interface, not prose.
    #[test]
    fn general_reasons_render_as_the_strings_downstream_logs() {
        assert_eq!(
            GeneralReason::RouterUnavailable.as_str(),
            "router_unavailable"
        );
        assert_eq!(GeneralReason::UnknownKey.as_str(), "unknown_key");
        assert_eq!(GeneralReason::NotSpecialised.as_str(), "not_specialised");
        assert_eq!(GeneralReason::LowConfidence.as_str(), "low_confidence");
        assert_eq!(GeneralReason::GeneralWon.as_str(), "general_won");
    }
}
