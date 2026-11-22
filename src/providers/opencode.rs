//! OpenCode chat adapter.
//!
//! OpenCode exposes a stateless one-shot text endpoint — not an OpenAI-shaped
//! chat endpoint — so this module owns its wire shapes. An optional server
//! password is sent as HTTP **basic auth** (`opencode:<password>`), never as a
//! Bearer token and never in a URL. The one-shot endpoint takes a `Model.Ref`
//! object (`id`/`providerID`/`modelID`) and a `prompt` — resolved from
//! `/api/model` when available, synthesized otherwise. Streaming is unsupported:
//! `chat_stream` returns `AppError::Unavailable` before opening a connection.

use crate::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use crate::error::AppError;
use crate::providers::LLMProvider;
use async_trait::async_trait;
use reqwest::{Client, RequestBuilder};
use std::error::Error;
use std::pin::Pin;
use tokio_stream::Stream;

const PROVIDER_NAME: &str = "opencode";

pub struct OpenCodeProvider {
    client: Client,
    base_url: String,
    /// OpenCode's server password, sent as basic auth. `None` means "the server
    /// has no password set", in which case no Authorization header is sent.
    api_key: Option<String>,
    model: String,
}

impl OpenCodeProvider {
    pub fn new(base_url: String, api_key: Option<String>, model: String) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Attach basic auth only when a password was configured. OpenCode expects
    /// `Authorization: Basic base64("opencode:<password>")`; a Bearer header is
    /// wrong for this server and is never sent.
    fn with_auth(&self, builder: RequestBuilder) -> RequestBuilder {
        match &self.api_key {
            Some(password) => builder.basic_auth("opencode", Some(password)),
            None => builder,
        }
    }

    /// Build the `Model.Ref` the generate endpoint expects. `/api/model` is the
    /// authority for `providerID`/`modelID` (a bare id does not say which
    /// provider serves it). A missing/unreachable catalog falls back to a
    /// synthesized ref so an older server still works.
    async fn model_ref(&self, model: &str) -> serde_json::Value {
        let lookup = self
            .with_auth(self.client.get(self.endpoint("/api/model")))
            .send()
            .await;
        if let Ok(response) = lookup {
            if response.status().is_success() {
                if let Ok(value) = response.json::<serde_json::Value>().await {
                    let entries = value
                        .as_array()
                        .or_else(|| value.get("data").and_then(|data| data.as_array()));
                    if let Some(entry) = entries.and_then(|entries| {
                        entries
                            .iter()
                            .find(|entry| entry.get("id").and_then(|id| id.as_str()) == Some(model))
                    }) {
                        return serde_json::json!({
                            "id": entry.get("id").and_then(|id| id.as_str()).unwrap_or(model),
                            "providerID": entry
                                .get("providerID")
                                .and_then(|id| id.as_str())
                                .unwrap_or("opencode"),
                            "modelID": entry
                                .get("modelID")
                                .and_then(|id| id.as_str())
                                .unwrap_or(model),
                        });
                    }
                }
            }
        }
        serde_json::json!({
            "id": model,
            "providerID": "opencode",
            "modelID": model,
        })
    }
}

/// The one-shot endpoint takes a single prompt string. History is flattened
/// into one text block: each message contributes its content, in order.
fn flatten_messages(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Read the generated text out of a `/generate/text` response.
///
/// The endpoint is experimental. The currently documented envelope is
/// `{"data": {"text": "..."}}`, while earlier releases returned `{"text":
/// "..."}`; accept both so a small upstream change cannot turn a successful
/// generation into a routing/agent failure. A response with neither shape is
/// refused rather than fabricated.
fn extract_text(value: &serde_json::Value) -> Result<String, AppError> {
    let text = value
        .get("data")
        .and_then(|data| data.get("text"))
        .or_else(|| value.get("text"))
        .and_then(|text| text.as_str())
        .ok_or_else(|| AppError::Provider("generate/text response contained no text".into()))?;
    Ok(text.to_string())
}

/// `/api/model` returns a list of model descriptors; older builds wrapped it
/// in `{"data": [...]}`. Each descriptor carries an `id`.
fn extract_model_ids(value: &serde_json::Value) -> Result<Vec<String>, AppError> {
    let entries = value
        .as_array()
        .or_else(|| value.get("data").and_then(|data| data.as_array()))
        .ok_or_else(|| AppError::Provider("model list response was not an array".into()))?;
    Ok(entries
        .iter()
        .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
        .map(str::to_string)
        .collect())
}

#[async_trait]
impl LLMProvider for OpenCodeProvider {
    fn get_base_url(&self) -> String {
        self.base_url.clone()
    }

    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let resp = self
            .with_auth(self.client.get(self.endpoint("/api/info")))
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "OpenCode health check failed with status: {}",
                resp.status()
            )
            .into())
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        let resp = self
            .with_auth(self.client.get(self.endpoint("/api/model")))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(format!("OpenCode error: {}", resp.status()).into());
        }
        let value: serde_json::Value = resp.json().await?;
        Ok(extract_model_ids(&value)?
            .into_iter()
            .map(|name| ModelInfo {
                name,
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
        let body = serde_json::json!({
            "model": self.model_ref(model).await,
            "prompt": flatten_messages(&request.messages),
        });
        let resp = self
            .with_auth(
                self.client
                    .post(self.endpoint("/api/experimental/generate/text"))
                    .json(&body),
            )
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("OpenCode error: {status} - {text}").into());
        }
        let value: serde_json::Value = resp.json().await?;
        Ok(ChatResponse {
            model: model.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            message: ChatMessage {
                role: "assistant".to_string(),
                content: extract_text(&value)?,
            },
            done: true,
            usage: None,
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
        // Decided before any I/O: no connection is opened.
        Err(Box::new(AppError::Unavailable(format!(
            "streaming is not supported by {PROVIDER_NAME}"
        ))))
    }
}
