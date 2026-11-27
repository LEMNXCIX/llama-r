//! Summarizer implementations for conversation history.
//!
//! Kept out of the SQLite adapter so the HTTP wire format lives in one place:
//! the store only knows it holds an `Arc<dyn Summarizer>` and treats an empty
//! string as a deliberate "no summary" answer.

use crate::net;
use crate::ports::history::{ExportMessage, Summarizer};
use async_trait::async_trait;
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// All summarizers flatten the exported turns into the same prompt so the
/// strategy (and therefore the provider) can change without changing what the
/// model is asked to summarize.
fn build_prompt(messages: &[ExportMessage]) -> String {
    let mut prompt = String::from(
        "Summarize the following conversation turns in 2-3 sentences, focusing on key decisions and context for future reference:\n\n",
    );
    for m in messages {
        prompt.push_str(&format!("{}: {}\n", m.role, m.content));
    }
    prompt
}

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
    fn strategy_name(&self) -> &'static str {
        "ollama"
    }

    async fn summarize(&self, messages: &[ExportMessage]) -> Result<String, String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{ "role": "user", "content": build_prompt(messages) }],
            "stream": false
        });

        let http_client = net::http_client(REQUEST_TIMEOUT)
            .map_err(|e| format!("summarizer http client error: {e}"))?;
        let resp = http_client
            .post(self.endpoint())
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

/// Summarizes through any OpenAI-compatible `chat/completions` endpoint
/// (OpenRouter, a local compatible gateway, OpenAI itself).
///
/// The key travels only as an `Authorization: Bearer` header, never in a URL,
/// log line or error string.
pub struct ChatCompletionsSummarizer {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl ChatCompletionsSummarizer {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// Join the chat-completions path to the configured base.
    ///
    /// `https://openrouter.ai/api/v1` is a complete API base: its `/api/v1`
    /// prefix is **preserved**, so the request goes to
    /// `/api/v1/chat/completions`. Two ways to double a path segment are
    /// avoided: a base already ending in `/chat/completions` is used as-is,
    /// and a base ending in `/v1` (including `/api/v1`) gets only the trailing
    /// `/chat/completions`. A bare origin gets `/v1/chat/completions`, the same
    /// convention `OpenAiCompatibleProvider::endpoint` uses.
    fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        }
    }
}

#[async_trait]
impl Summarizer for ChatCompletionsSummarizer {
    fn strategy_name(&self) -> &'static str {
        "chat-completions"
    }

    async fn summarize(&self, messages: &[ExportMessage]) -> Result<String, String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{ "role": "user", "content": build_prompt(messages) }],
            "stream": false
        });

        let http_client = net::http_client(REQUEST_TIMEOUT)
            .map_err(|e| format!("summarizer http client error: {e}"))?;
        let resp = http_client
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
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
            .get("choices")
            .and_then(|choices| choices.get(0))
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(|content| content.as_str())
            .unwrap_or_default()
            .trim()
            .to_string())
    }
}

/// Summarizes through OpenCode's one-shot text endpoint.
///
/// OpenCode takes a `Model.Ref` object instead of an OpenAI-shaped message
/// list and authenticates with HTTP basic auth (`opencode:<password>`) when a
/// server password is configured. The password never appears in a URL, log or
/// error string.
pub struct OpenCodeSummarizer {
    pub url: String,
    pub password: Option<String>,
    pub model: String,
}

impl OpenCodeSummarizer {
    pub fn new(url: impl Into<String>, password: Option<String>, model: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            password,
            model: model.into(),
        }
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/api/experimental/generate/text",
            self.url.trim_end_matches('/')
        )
    }
}

#[async_trait]
impl Summarizer for OpenCodeSummarizer {
    fn strategy_name(&self) -> &'static str {
        "opencode"
    }

    async fn summarize(&self, messages: &[ExportMessage]) -> Result<String, String> {
        let body = serde_json::json!({
            "model": {
                "id": self.model,
                "providerID": "opencode",
                "modelID": self.model,
            },
            "prompt": build_prompt(messages),
        });

        let http_client = net::http_client(REQUEST_TIMEOUT)
            .map_err(|e| format!("summarizer http client error: {e}"))?;
        let mut request = http_client.post(self.endpoint()).json(&body);
        if let Some(password) = &self.password {
            request = request.basic_auth("opencode", Some(password));
        }
        let resp = request
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

        // The experimental endpoint currently wraps the text in `data`, while
        // older releases returned it at the top level; accept both.
        Ok(json_val
            .get("data")
            .and_then(|data| data.get("text"))
            .or_else(|| json_val.get("text"))
            .and_then(|text| text.as_str())
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
    fn strategy_name(&self) -> &'static str {
        "noop"
    }

    fn is_noop(&self) -> bool {
        true
    }

    async fn summarize(&self, _messages: &[ExportMessage]) -> Result<String, String> {
        Ok(String::new())
    }
}
