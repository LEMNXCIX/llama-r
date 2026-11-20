//! Task 1: `ContextCompactor` port and its Noop / Summarizing adapters.
//!
//! These tests pin the compaction contract without any runtime wiring: only
//! history is ever compacted, the injected `Summarizer` sees exactly the
//! oldest messages, and an empty answer means "no memory" rather than a
//! failure. A fake summarizer captures its input so nothing depends on a live
//! model or decision router.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use llama_r::adapters::context::{NoopCompactor, SummarizingCompactor};
use llama_r::domain::models::ChatMessage;
use llama_r::ports::compaction::{CompactRequest, ContextCompactor};
use llama_r::ports::history::{ExportMessage, Summarizer};

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
