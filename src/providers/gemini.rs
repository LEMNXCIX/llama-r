//! Google Gemini `generateContent` adapter.
//!
//! Gemini is a genuinely different wire from OpenAI's chat completions: the
//! key is sent as an `x-goog-api-key` **header** (never `?key=` in the URL, so
//! it cannot reach a log line or an error), `systemInstruction` is a top-level
//! field, assistant turns use the role `model`, and the answer lives in
//! `candidates[].content.parts[].text`. This module owns all of that.

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

const PROVIDER_NAME: &str = "gemini";
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const MODEL_PREFIX: &str = "models/";

pub struct GeminiProvider {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl GeminiProvider {
    /// `base_url = None` uses Google's native `v1beta` origin. A profile's
    /// `base_url` overrides it for a compatible gateway or a wire test.
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

    /// The key travels in a header. A `?key=` query parameter would put it in
    /// the request line, which is logged by proxies and error messages.
    fn with_auth(&self, builder: RequestBuilder) -> RequestBuilder {
        builder.header("x-goog-api-key", &self.api_key)
    }

    fn chat_endpoint(&self, model: &str) -> String {
        format!(
            "{}/models/{}:generateContent",
            self.base_url,
            strip_model_prefix(model)
        )
    }

    fn model_for<'a>(&'a self, requested: &'a str) -> &'a str {
        if requested.is_empty() {
            &self.model
        } else {
            requested
        }
    }
}

/// The models list uses `models/gemini-…` names; the chat URL and the returned
/// `ModelInfo` use the bare id, so a caller can pass either spelling.
fn strip_model_prefix(model: &str) -> &str {
    let model = model.trim();
    model.strip_prefix(MODEL_PREFIX).unwrap_or(model)
}

#[derive(Serialize)]
struct GenerateRequest {
    contents: Vec<WireContent>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<WireContent>,
}

#[derive(Serialize)]
struct WireContent {
    role: String,
    parts: Vec<WirePart>,
}

#[derive(Serialize)]
struct WirePart {
    text: String,
}

#[derive(Deserialize)]
struct GenerateResponse {
    candidates: Vec<Candidate>,
    #[serde(rename = "modelVersion")]
    model_version: Option<String>,
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: Option<TokenUsage>,
}

#[derive(Deserialize)]
struct TokenUsage {
    #[serde(rename = "promptTokenCount", default)]
    prompt_tokens: u64,
    #[serde(rename = "candidatesTokenCount", default)]
    completion_tokens: u64,
}

#[derive(Deserialize)]
struct Candidate {
    content: Option<ResponseContent>,
}

#[derive(Deserialize)]
struct ResponseContent {
    parts: Option<Vec<ResponsePart>>,
}

#[derive(Deserialize)]
struct ResponsePart {
    text: Option<String>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    models: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    name: String,
}

fn text_content(text: &str, role: &str) -> WireContent {
    WireContent {
        role: role.to_string(),
        parts: vec![WirePart {
            text: text.to_string(),
        }],
    }
}

/// Split the domain messages into Gemini's top-level system instruction and
/// its `user`/`model` turns. Unknown roles are kept as user-side context.
fn wire_messages(messages: &[ChatMessage]) -> (Option<WireContent>, Vec<WireContent>) {
    let system = messages
        .iter()
        .filter(|message| message.role == "system")
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let contents = messages
        .iter()
        .filter(|message| message.role != "system")
        .map(|message| {
            text_content(
                &message.content,
                if message.role == "assistant" {
                    "model"
                } else {
                    "user"
                },
            )
        })
        .collect();
    let instruction = (!system.is_empty()).then(|| text_content(&system, "system"));
    (instruction, contents)
}

#[async_trait]
impl LLMProvider for GeminiProvider {
    fn get_base_url(&self) -> String {
        self.base_url.clone()
    }

    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let response = self
            .with_auth(self.client.get(self.endpoint("/models")))
            .send()
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "Gemini health check failed with status: {}",
                response.status()
            )
            .into())
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        let response = self
            .with_auth(self.client.get(self.endpoint("/models")))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(format!("Gemini error: {}", response.status()).into());
        }
        let body: ModelsResponse = response.json().await?;
        Ok(body
            .models
            .into_iter()
            .map(|model| ModelInfo {
                name: strip_model_prefix(&model.name).to_string(),
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
        let (system_instruction, contents) = wire_messages(&request.messages);
        let wire = GenerateRequest {
            contents,
            system_instruction,
        };
        let response = self
            .with_auth(self.client.post(self.chat_endpoint(model)))
            .json(&wire)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!("Gemini error: {status} - {text}").into());
        }
        let body: GenerateResponse = response.json().await?;
        let caps = capabilities_for(ProviderKind::Gemini, model);
        let usage = body.usage_metadata.as_ref().map(|tokens| {
            Usage::from_counts(
                tokens.prompt_tokens,
                tokens.completion_tokens,
                caps.cost_per_1m_input,
                caps.cost_per_1m_output,
            )
        });
        let content = body
            .candidates
            .into_iter()
            .next()
            .and_then(|candidate| candidate.content)
            .and_then(|content| content.parts)
            .into_iter()
            .flatten()
            .filter_map(|part| part.text)
            .collect::<Vec<_>>()
            .join("");
        Ok(ChatResponse {
            model: body.model_version.unwrap_or_else(|| model.to_string()),
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
            conversation_id: None,
        })
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        Err(Box::new(AppError::Unavailable(format!(
            "streaming is not supported by {PROVIDER_NAME}"
        ))))
    }
}
