//! Anthropic Messages API adapter.
//!
//! Anthropic is a genuinely different wire from OpenAI's chat completions:
//! the key travels in `x-api-key` (never in a URL, a Bearer header, a body or a
//! log line), `anthropic-version` is mandatory, `system` is a top-level field
//! rather than a message role, and the answer is a list of content blocks.
//! This module owns all of that so `domain::models` never sees it.

use crate::config::ProviderKind;
use crate::domain::models::{
    ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo, Usage,
};
use crate::error::AppError;
use crate::providers::capabilities::capabilities_for;
use crate::providers::LLMProvider;
use async_trait::async_trait;
use reqwest::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::pin::Pin;
use std::time::Duration;
use tokio_stream::Stream;

const PROVIDER_NAME: &str = "anthropic";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
/// Anthropic's completion model refuses a request without an explicit output
/// cap; the same floor is used by the Rig engine wrapper.
const MAX_TOKENS: u64 = 4_096;

pub struct AnthropicProvider {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl AnthropicProvider {
    /// `base_url = None` uses Anthropic's native origin. A profile's `base_url`
    /// overrides it for a compatible gateway or a wire test.
    pub fn new(base_url: Option<String>, api_key: String, model: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            client,
            base_url: base_url
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            api_key,
            model,
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Every request carries the key and the pinned API version; a key is
    /// never placed in the URL.
    fn with_auth(&self, builder: RequestBuilder) -> RequestBuilder {
        builder
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
    }

    fn model_for<'a>(&'a self, requested: &'a str) -> &'a str {
        if requested.is_empty() {
            &self.model
        } else {
            requested
        }
    }
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u64,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    model: Option<String>,
    #[serde(default)]
    usage: Option<TokenUsage>,
}

#[derive(Deserialize)]
struct TokenUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// Split the domain messages into Anthropic's `system` field and its
/// `user`/`assistant` turns. Unknown roles are kept as user-side context so a
/// tool message is not silently dropped.
fn wire_messages(messages: &[ChatMessage]) -> (Option<String>, Vec<WireMessage<'_>>) {
    let system = messages
        .iter()
        .filter(|message| message.role == "system")
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let turns = messages
        .iter()
        .filter(|message| message.role != "system")
        .map(|message| WireMessage {
            role: if message.role == "assistant" {
                "assistant"
            } else {
                "user"
            },
            content: &message.content,
        })
        .collect();
    (
        if system.is_empty() {
            None
        } else {
            Some(system)
        },
        turns,
    )
}

#[async_trait]
impl LLMProvider for AnthropicProvider {
    fn get_base_url(&self) -> String {
        self.base_url.clone()
    }

    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let response = self
            .with_auth(self.client.get(self.endpoint("/v1/models")))
            .send()
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "Anthropic health check failed with status: {}",
                response.status()
            )
            .into())
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        let response = self
            .with_auth(self.client.get(self.endpoint("/v1/models")))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(format!("Anthropic error: {}", response.status()).into());
        }
        let body: ModelsResponse = response.json().await?;
        Ok(body
            .data
            .into_iter()
            .map(|model| ModelInfo {
                name: model.id,
                modified_at: String::new(),
                size: 0,
            })
            .collect())
    }

    async fn chat(
        &self,
        request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        let model = self.model_for(&request.model);
        let (system, messages) = wire_messages(&request.messages);
        let wire = MessagesRequest {
            model,
            max_tokens: MAX_TOKENS,
            messages,
            system: system.as_deref(),
        };
        let response = self
            .with_auth(self.client.post(self.endpoint("/v1/messages")))
            .json(&wire)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!("Anthropic error: {status} - {text}").into());
        }
        let body: MessagesResponse = response.json().await?;
        let caps = capabilities_for(ProviderKind::Anthropic, model);
        let usage = body.usage.as_ref().map(|tokens| {
            Usage::from_counts(
                tokens.input_tokens,
                tokens.output_tokens,
                caps.cost_per_1m_input,
                caps.cost_per_1m_output,
            )
        });
        let content = body
            .content
            .into_iter()
            .filter(|block| block.kind == "text")
            .filter_map(|block| block.text)
            .collect::<Vec<_>>()
            .join("");
        Ok(ChatResponse {
            model: body.model.unwrap_or_else(|| model.to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
            message: ChatMessage {
                role: "assistant".to_string(),
                content,
            },
            done: true,
            usage,
            served_by: None,
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
        // Decided before any I/O, like the OpenRouter/OpenCode adapters.
        Err(Box::new(AppError::Unavailable(format!(
            "streaming is not supported by {PROVIDER_NAME}"
        ))))
    }
}
