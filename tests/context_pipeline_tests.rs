//! Context pipeline: `ContextCompactor` (Task 1) and `ModelSelector` (Task 2).
//!
//! These tests pin the ports without any runtime wiring: only history is ever
//! compacted, the injected `Summarizer` sees exactly the oldest messages, and
//! model selection always goes through the injected, configurable
//! `DecisionRouter` using candidate labels — never a hardcoded model. Fake
//! collaborators capture their inputs so nothing depends on a live model.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use llama_r::adapters::context::router_model_selector::QUESTION_KEY;
use llama_r::adapters::context::{
    CurrentModelSelector, NoopCompactor, RouterModelSelector, SummarizingCompactor,
};
use llama_r::domain::models::ChatMessage;
use llama_r::error::AppError;
use llama_r::ports::compaction::{CompactRequest, ContextCompactor};
use llama_r::ports::history::{ExportMessage, Summarizer};
use llama_r::ports::model_selection::{
    ModelCandidate, ModelSelection, ModelSelectionRequest, ModelSelector, SelectionReason,
};
use llama_r::ports::router::{Answer, DecisionRequest, DecisionResponse, DecisionRouter, Question};
use llama_r::providers::model_costs::load_relative_costs;

/// A summarizer with a fixed reply that records every batch it was handed.
struct FakeSummarizer {
    reply: Result<String, String>,
    captured: Arc<Mutex<Vec<Vec<ExportMessage>>>>,
}

impl FakeSummarizer {
    fn ok(text: &str) -> Self {
        Self {
            reply: Ok(text.to_string()),
            captured: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn failing(reason: &str) -> Self {
        Self {
            reply: Err(reason.to_string()),
            captured: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn captured(&self) -> Vec<Vec<ExportMessage>> {
        self.captured.lock().expect("capture lock").clone()
    }
}

#[async_trait]
impl Summarizer for FakeSummarizer {
    async fn summarize(&self, messages: &[ExportMessage]) -> Result<String, String> {
        self.captured
            .lock()
            .expect("capture lock")
            .push(messages.to_vec());
        self.reply.clone()
    }
}

fn message(role: &str, content: &str) -> ChatMessage {
    ChatMessage {
        role: role.to_string(),
        content: content.to_string(),
    }
}

/// `ChatMessage` has no `PartialEq`, so compare its observable fields.
fn pairs(messages: &[ChatMessage]) -> Vec<(&str, &str)> {
    messages
        .iter()
        .map(|m| (m.role.as_str(), m.content.as_str()))
        .collect()
}

/// Four oversized old turns and two tiny new ones: any token-budget-aware
/// compactor must drop the old ones and keep the new ones intact.
fn long_history() -> Vec<ChatMessage> {
    let old = "x".repeat(4_000);
    vec![
        message("user", &format!("old question 1 {old}")),
        message("assistant", &format!("old answer 1 {old}")),
        message("user", &format!("old question 2 {old}")),
        message("assistant", &format!("old answer 2 {old}")),
        message("user", "new question"),
        message("assistant", "new answer"),
    ]
}

fn request(history: Vec<ChatMessage>) -> CompactRequest {
    CompactRequest {
        system_prompt: "You are a careful assistant. Follow every rule.".to_string(),
        history,
        budget_tokens: 200,
        reserved_tokens: 0,
    }
}

#[tokio::test]
async fn noop_compactor_returns_history_unchanged() {
    let history = vec![message("user", "hello"), message("assistant", "hi")];
    let expected = pairs(&history);
    let req = CompactRequest {
        system_prompt: "instructions".to_string(),
        history: history.clone(),
        budget_tokens: 1,
        reserved_tokens: 0,
    };

    let result = NoopCompactor
        .compact(req)
        .await
        .expect("noop compaction is infallible");

    assert_eq!(
        pairs(&result.history),
        expected,
        "noop must not drop any message"
    );
    assert_eq!(result.memory, None, "noop must not invent a memory");
}

#[tokio::test]
async fn summarizing_compactor_summarizes_only_oldest_turns() {
    let summarizer = Arc::new(FakeSummarizer::ok("The old turns discussed refunds."));
    let compactor = SummarizingCompactor::new(summarizer.clone());
    let history = long_history();
    let newest = history[4..].to_vec();

    let result = compactor
        .compact(request(history))
        .await
        .expect("summarizing compaction");

    assert_eq!(
        pairs(&result.history),
        pairs(&newest),
        "only the newest turns may survive"
    );
    assert_eq!(
        result.memory.as_deref(),
        Some("The old turns discussed refunds."),
        "the summary becomes the memory"
    );

    let captured = summarizer.captured();
    assert_eq!(
        captured.len(),
        1,
        "the summarizer must be called exactly once"
    );
    assert_eq!(
        captured[0].len(),
        4,
        "exactly the four old turns are summarized"
    );
    let seen: Vec<_> = captured[0]
        .iter()
        .map(|m| (m.role.as_str(), m.content.as_str()))
        .collect();
    assert!(
        seen.iter()
            .all(|(role, _)| *role == "user" || *role == "assistant"),
        "roles survive the conversion to ExportMessage"
    );
    let ids: Vec<_> = captured[0].iter().map(|m| m.id.as_str()).collect();
    let timestamps: Vec<_> = captured[0].iter().map(|m| m.created_at).collect();
    assert_eq!(
        ids,
        vec!["history-0", "history-1", "history-2", "history-3"]
    );
    assert_eq!(
        timestamps,
        vec![0, 1, 2, 3],
        "positional ids keep the order"
    );
    assert!(
        seen[0].1.contains("old question 1") && seen[3].1.contains("old answer 2"),
        "the summarizer sees the oldest turns in their original order"
    );
    assert!(
        seen.iter().all(
            |(_, content)| !content.contains("new question") && !content.contains("new answer")
        ),
        "the newest turns are never handed to the summarizer"
    );
}

#[tokio::test]
async fn empty_summary_means_no_memory() {
    let summarizer = Arc::new(FakeSummarizer::ok("   "));
    let compactor = SummarizingCompactor::new(summarizer.clone());
    let history = long_history();
    let newest = history[4..].to_vec();

    let result = compactor
        .compact(request(history))
        .await
        .expect("a quiet summarizer is not an error");

    assert_eq!(
        pairs(&result.history),
        pairs(&newest),
        "old turns are dropped even with no summary"
    );
    assert_eq!(
        result.memory, None,
        "no summary means no memory, not an empty string"
    );
    assert_eq!(
        summarizer.captured().len(),
        1,
        "the summarizer was still consulted"
    );
}

#[tokio::test]
async fn summarizer_error_is_a_named_error() {
    let summarizer = Arc::new(FakeSummarizer::failing("upstream exploded"));
    let compactor = SummarizingCompactor::new(summarizer);

    let err = compactor
        .compact(request(long_history()))
        .await
        .expect_err("a summarizer failure must not be swallowed");

    assert!(
        err.contains("summarizer") && err.contains("upstream exploded"),
        "the error names its source and the underlying cause: {err}"
    );
}

// ---------------------------------------------------------------------------
// Task 2: `ModelSelector` port, its two adapters, and the optional cost table.
//
// The fake router returns a fixed reply and records the `DecisionRequest` so
// the tests can assert the criteria are candidate labels (model ids appear
// only in descriptions) and that no decision is spent when there is nothing
// to decide. No live model is involved.
// ---------------------------------------------------------------------------

/// What the fake router answers. `key` is an arbitrary router label; the
/// selector's mapping is `complex` → complex tier, anything else → simple.
#[derive(Debug)]
enum SelectorReply {
    Choice {
        key: &'static str,
        confidence: f32,
    },
    Noul {
        value: f32,
    },
    /// A successful call with no answer for the question we asked.
    Missing,
    Fail,
}

struct SelectorFakeRouter {
    reply: SelectorReply,
    seen: Arc<Mutex<Option<DecisionRequest>>>,
}

#[async_trait]
impl DecisionRouter for SelectorFakeRouter {
    async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, AppError> {
        *self.seen.lock().unwrap() = Some(req);
        if matches!(self.reply, SelectorReply::Fail) {
            return Err(AppError::Provider("fake router unreachable".into()));
        }
        let answer = match &self.reply {
            SelectorReply::Choice { key, confidence } => Answer::Choice {
                key: (*key).to_string(),
                confidence: *confidence,
                probabilities: BTreeMap::from([((*key).to_string(), *confidence)]),
            },
            SelectorReply::Noul { value } => Answer::Noul { value: *value },
            SelectorReply::Missing => {
                return Ok(DecisionResponse {
                    answers: BTreeMap::new(),
                })
            }
            SelectorReply::Fail => unreachable!(),
        };
        Ok(DecisionResponse {
            answers: BTreeMap::from([(QUESTION_KEY.to_string(), answer)]),
        })
    }
}

fn fake_selector_router(
    reply: SelectorReply,
) -> (Arc<dyn DecisionRouter>, Arc<Mutex<Option<DecisionRequest>>>) {
    let seen = Arc::new(Mutex::new(None));
    let router = SelectorFakeRouter {
        reply,
        seen: seen.clone(),
    };
    (Arc::new(router), seen)
}

fn candidate(model: &str, cost: u32, hint: Option<&str>) -> ModelCandidate {
    ModelCandidate {
        model: model.to_string(),
        provider_hint: hint.map(str::to_string),
        relative_cost: cost,
    }
}

fn selection_request(models: &[(&str, u32)]) -> ModelSelectionRequest {
    ModelSelectionRequest {
        candidates: models
            .iter()
            .map(|(model, cost)| candidate(model, *cost, None))
            .collect(),
        prompt_hint: "explain quantum tunnelling".to_string(),
        current_model: "current-model".to_string(),
    }
}

/// Drive one router-backed selection and return the result plus the captured
/// request handle. Threshold defaults to the fail-closed 0.6.
async fn select_with(
    reply: SelectorReply,
    req: ModelSelectionRequest,
) -> (ModelSelection, Arc<Mutex<Option<DecisionRequest>>>) {
    let (router, seen) = fake_selector_router(reply);
    let selector = RouterModelSelector::new(router, 0.6);
    let selection = selector.select(req).await;
    (selection, seen)
}

/// The outgoing question must name candidates by label, never by model: a
/// model id is only ever a description the router reads, not a key it returns.
fn assert_labelled_question(req: &DecisionRequest, models: &[&str]) {
    let question = req
        .questions
        .get(QUESTION_KEY)
        .expect("complexity question asked");
    let Question::Choice {
        instructions,
        criteria,
    } = question
    else {
        panic!("expected a Choice question, got {question:?}");
    };
    assert!(!instructions.trim().is_empty(), "instructions required");
    assert_eq!(criteria.len(), models.len(), "one label per candidate");
    let labels: Vec<_> = (0..models.len())
        .map(|i| format!("candidate-{i}"))
        .collect();
    for label in &labels {
        let description = criteria
            .get(label)
            .unwrap_or_else(|| panic!("missing candidate label {label}"));
        assert!(
            description.contains("relative_cost"),
            "description must carry the cost: {description}"
        );
    }
    for model in models {
        assert!(
            !criteria.contains_key(*model),
            "model id {model} must never be a criteria key"
        );
        assert!(
            criteria.values().any(|d| d.contains(model)),
            "model id {model} must appear only in a description"
        );
    }
    assert!(
        !req.state.is_empty(),
        "the prompt hint must reach the decision router as state"
    );
}

#[tokio::test]
async fn selector_returns_current_without_candidates() {
    let (selection, seen) = select_with(
        SelectorReply::Choice {
            key: "complex",
            confidence: 0.99,
        },
        selection_request(&[]),
    )
    .await;

    assert_eq!(selection.model, "current-model");
    assert_eq!(selection.reason, SelectionReason::NoCandidates);
    assert!(
        seen.lock().unwrap().is_none(),
        "no decision may be spent with no candidates"
    );
}

#[tokio::test]
async fn simple_picks_first_candidate() {
    let (selection, seen) = select_with(
        SelectorReply::Choice {
            key: "candidate-0",
            confidence: 0.9,
        },
        selection_request(&[("cheap-model", 1), ("pricey-model", 3)]),
    )
    .await;

    assert_eq!(selection.model, "cheap-model");
    assert_eq!(selection.reason, SelectionReason::Chosen);
    assert_labelled_question(
        &seen.lock().unwrap().clone().expect("router was called"),
        &["cheap-model", "pricey-model"],
    );
}

#[tokio::test]
async fn complex_picks_complex_candidate() {
    let (selection, _) = select_with(
        SelectorReply::Choice {
            key: "complex",
            confidence: 0.9,
        },
        selection_request(&[("cheap-model", 1), ("pricey-model", 3)]),
    )
    .await;

    assert_eq!(selection.model, "pricey-model");
    assert_eq!(selection.reason, SelectionReason::Chosen);
}

#[tokio::test]
async fn selector_degrades_to_current_model_without_router() {
    // No router at all: the CurrentModelSelector keeps the current model.
    let no_router = CurrentModelSelector
        .select(selection_request(&[("cheap-model", 1)]))
        .await;
    assert_eq!(no_router.model, "current-model");
    assert_eq!(no_router.reason, SelectionReason::RouterUnavailable);

    // Router error, a Noul, a missing answer and a malformed confidence are
    // all "no usable decision": keep the current model, never fabricate one.
    for reply in [
        SelectorReply::Fail,
        SelectorReply::Noul { value: 0.99 },
        SelectorReply::Missing,
    ] {
        let label = format!("{reply:?}");
        let (selection, _) = select_with(
            reply,
            selection_request(&[("cheap-model", 1), ("pricey-model", 3)]),
        )
        .await;
        assert_eq!(selection.model, "current-model", "reply {label}");
        assert_eq!(
            selection.reason,
            SelectionReason::RouterUnavailable,
            "reply {label}"
        );
    }

    // A low-confidence choice is a distinct, named degradation.
    let (low, _) = select_with(
        SelectorReply::Choice {
            key: "complex",
            confidence: 0.4,
        },
        selection_request(&[("cheap-model", 1), ("pricey-model", 3)]),
    )
    .await;
    assert_eq!(low.model, "current-model");
    assert_eq!(low.reason, SelectionReason::LowConfidence);
}

#[tokio::test]
async fn no_cost_table_means_first_candidate_wins() {
    // With no cost table every candidate carries 1, so "complex" and "simple"
    // both land on the one cost tier and the first declared candidate wins.
    for key in ["complex", "candidate-0"] {
        let (selection, _) = select_with(
            SelectorReply::Choice {
                key,
                confidence: 0.9,
            },
            selection_request(&[("first-model", 1), ("second-model", 1)]),
        )
        .await;
        assert_eq!(selection.model, "first-model", "key {key}");
        assert_eq!(selection.reason, SelectionReason::Chosen);
    }
}

#[tokio::test]
async fn relative_cost_breaks_ties_only() {
    // Equal complexity (same relative cost): the lower rank wins, even when
    // the router asked for the complex candidate.
    let (tied, _) = select_with(
        SelectorReply::Choice {
            key: "complex",
            confidence: 0.9,
        },
        selection_request(&[("tier-a", 2), ("tier-b", 2), ("tier-c", 2)]),
    )
    .await;
    assert_eq!(tied.model, "tier-a", "equal cost → first declared wins");

    // Different complexity: the router's signal wins over declaration order —
    // the pricey candidate is declared first, yet "simple" still picks cheap.
    let req = selection_request(&[("pricey-model", 3), ("cheap-model", 1)]);
    let (simple, _) = select_with(
        SelectorReply::Choice {
            key: "candidate-0",
            confidence: 0.9,
        },
        req.clone(),
    )
    .await;
    assert_eq!(simple.model, "cheap-model", "simple → lowest cost");
    let (complex, _) = select_with(
        SelectorReply::Choice {
            key: "complex",
            confidence: 0.9,
        },
        req,
    )
    .await;
    assert_eq!(complex.model, "pricey-model", "complex → highest cost");

    // A tie at the complex end still resolves to the first declared candidate
    // of that tier, never to the later one.
    let (complex_tie, _) = select_with(
        SelectorReply::Choice {
            key: "complex",
            confidence: 0.9,
        },
        selection_request(&[("cheap", 1), ("capable-a", 3), ("capable-b", 3)]),
    )
    .await;
    assert_eq!(complex_tie.model, "capable-a");
}

/// A tracing writer that can be read back, to assert the malformed-file
/// warning actually names the path (rather than being swallowed).
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn malformed_models_toml_is_warned_and_empty() {
    use tracing::subscriber::with_default;

    let dir = std::env::temp_dir().join(format!("llama-r-models-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("models.toml");
    let malformed = "this is = [not valid toml";
    std::fs::write(&path, malformed).expect("write malformed models.toml");

    let buf = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .without_time()
        .with_target(false)
        .finish();

    let costs = with_default(subscriber, || load_relative_costs(&dir));

    assert!(costs.is_empty(), "a malformed table must be empty");
    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("models.toml"),
        "warning must name the file: {log:?}"
    );
    assert!(
        log.contains(path.to_string_lossy().as_ref()),
        "warning must name the full path: {log:?}"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn valid_models_toml_loads_relative_costs() {
    let dir = std::env::temp_dir().join(format!("llama-r-models-ok-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("models.toml");
    std::fs::write(&path, "\"cheap-model\" = 1\n\"pricey-model\" = 3\n")
        .expect("write models.toml");

    let costs = load_relative_costs(&dir);
    assert_eq!(costs.get("cheap-model"), Some(&1));
    assert_eq!(costs.get("pricey-model"), Some(&3));

    // A missing file is fine: an empty table, not an error.
    let missing =
        std::env::temp_dir().join(format!("llama-r-models-absent-{}", std::process::id()));
    assert!(load_relative_costs(&missing).is_empty());

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}
