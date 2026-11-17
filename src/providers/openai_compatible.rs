//! OpenAI-compatible chat adapter (the wire format OpenRouter speaks).
//!
//! This module owns every OpenAI-specific wire shape so none leaks into
//! `domain::models`. The API key travels only as an `Authorization: Bearer`
//! header — never in a URL, never in a log line — and streaming is deliberately
//! unsupported: a 501 ("Unavailable") is more honest than pretending a
//! non-streaming upstream can stream.

use crate::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use crate::error::AppError;
use crate::providers::LLMProvider;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::pin::Pin;
use tokio_stream::Stream;

const PROVIDER_NAME: &str = "openrouter";

/// An upstream that implements OpenAI's chat-completions API. OpenRouter is the
/// first such upstream; `openai_base_url` lets a self-hosted compatible server
/// use the same adapter.
pub struct OpenAiCompatibleProvider {
    client: Client,
    base_url: String,
    api_key: String,
    /// Configured default model. The per-request model wins when present
    /// (matching `OllamaProvider`); this is the fallback and the stack's
    /// identity.
    model: String,
}

impl OpenAiCompatibleProvider {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
        }
    }

    /// Join like `LlamaCppRouter::endpoint`: a trailing `/` in the configured
    /// base must not produce `/v1//chat/completions`.
    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

#[derive(Serialize)]
struct ChatCompletionRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    model: String,
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

#[async_trait]
impl LLMProvider for OpenAiCompatibleProvider {
    fn get_base_url(&self) -> String {
        self.base_url.clone()
    }

    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Same endpoint as `list_models`: a 2xx on /v1/models proves the key
        // and the origin both work, without touching the generative endpoint.
        let resp = self
            .client
            .get(self.endpoint("/v1/models"))
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "OpenAI-compatible health check failed with status: {}",
                resp.status()
            )
            .into())
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        let resp = self
            .client
            .get(self.endpoint("/v1/models"))
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(format!("OpenAI-compatible error: {}", resp.status()).into());
        }
        let body: ModelsResponse = resp.json().await?;
        Ok(body
            .data
            .into_iter()
            .map(|m| ModelInfo {
                name: m.id,
                modified_at: String::new(),
                size: 0,
            })
            .collect())
    }

    async fn chat(
        &self,
        request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        let model = if request.model.is_empty() {
            &self.model
        } else {
            &request.model
        };
        let wire = ChatCompletionRequest {
            model,
            messages: &request.messages,
            stream: false,
        };
        let resp = self
            .client
            .post(self.endpoint("/v1/chat/completions"))
            .bearer_auth(&self.api_key)
            .json(&wire)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("OpenAI-compatible error: {status} - {text}").into());
        }
        let body: ChatCompletionResponse = resp.json().await?;
        let choice = body.choices.into_iter().next().ok_or_else(|| {
            AppError::Provider("chat-completions response contained no choices".into())
        })?;
        Ok(ChatResponse {
            model: body.model,
            created_at: chrono::Utc::now().to_rfc3339(),
            message: choice.message,
            done: true,
            debug_prompt: None,
            debug_route: None,
        })
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        // No connection is opened: the error is decided before any I/O.
        Err(Box::new(AppError::Unavailable(format!(
            "streaming is not supported by {PROVIDER_NAME}"
        ))))
    }
}
