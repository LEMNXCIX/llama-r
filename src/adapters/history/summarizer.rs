//! Summarizer implementations for conversation history.
//!
//! Kept out of the SQLite adapter so the HTTP wire format lives in one place:
//! the store only knows it holds an `Arc<dyn Summarizer>` and treats an empty
//! string as a deliberate "no summary" answer.

use crate::ports::history::{ExportMessage, Summarizer};
use async_trait::async_trait;

/// Prompts for the summary and speaks the Ollama `/api/chat` wire format.
///
/// The URL is the provider origin (e.g. `http://localhost:11434`); `/api/chat`
/// is appended here, with a trailing slash handled so a misconfigured origin
/// still produces a single slash.
pub struct OllamaSummarizer {
    pub url: String,
    pub model: String,
}

impl OllamaSummarizer {
    pub fn new(url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            model: model.into(),
        }
    }

    fn endpoint(&self) -> String {
        if self.url.ends_with('/') {
            format!("{}api/chat", self.url)
        } else {
            format!("{}/api/chat", self.url)
        }
    }
}

#[async_trait]
impl Summarizer for OllamaSummarizer {
    async fn summarize(&self, messages: &[ExportMessage]) -> Result<String, String> {
        let mut prompt = String::from(
            "Summarize the following conversation turns in 2-3 sentences, focusing on key decisions and context for future reference:\n\n",
        );
        for m in messages {
            prompt.push_str(&format!("{}: {}\n", m.role, m.content));
        }

        let body = serde_json::json!({
            "model": self.model,
            "messages": [{ "role": "user", "content": prompt }],
            "stream": false
        });

        let http_client = reqwest::Client::new();
        let resp = http_client
            .post(self.endpoint())
            .timeout(std::time::Duration::from_secs(30))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("summarizer http request error: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("summarizer http error status: {}", resp.status()));
        }

        let json_val: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("summarizer json parse error: {e}"))?;

        Ok(json_val
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .trim()
            .to_string())
    }
}

/// A summarizer that deliberately produces nothing. Useful when no provider
/// speaks the Ollama wire format: conversations still persist, summaries are
/// simply skipped without pointing at a wrong/empty URL.
pub struct NoopSummarizer;

#[async_trait]
impl Summarizer for NoopSummarizer {
    async fn summarize(&self, _messages: &[ExportMessage]) -> Result<String, String> {
        Ok(String::new())
    }
}
