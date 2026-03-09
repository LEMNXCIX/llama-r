use crate::domain::models::{ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use crate::providers::LLMProvider;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::pin::Pin;
use tokio_stream::{Stream, StreamExt};

pub struct OllamaProvider {
    client: Client,
    base_url: String,
}

impl OllamaProvider {
    pub fn new(base_url: String) -> Self {
        Self {
            client: Client::new(),
            base_url,
        }
    }
}

#[derive(Serialize)]
struct OllamaChatRequestInternal {
    model: String,
    messages: Vec<crate::domain::models::ChatMessage>,
    stream: bool,
}

#[derive(Deserialize)]
struct OllamaChatResponseInternal {
    model: String,
    created_at: String,
    message: crate::domain::models::ChatMessage,
    done: bool,
}

#[derive(Deserialize)]
struct OllamaListModelsResponseInternal {
    models: Vec<OllamaModelInfoInternal>,
}

#[derive(Deserialize)]
struct OllamaModelInfoInternal {
    name: String,
    modified_at: String,
    size: i64,
}

#[async_trait]
impl LLMProvider for OllamaProvider {
    fn get_base_url(&self) -> String {
        self.base_url.clone()
    }

    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let url = if self.base_url.ends_with('/') {
            format!("{}api/tags", self.base_url)
        } else {
            format!("{}/api/tags", self.base_url)
        };
        let resp = self.client.get(&url).send().await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("Ollama health check failed with status: {}", resp.status()).into())
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        let url = format!("{}/api/tags", self.base_url);
        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            return Err(format!("Ollama error: {}", resp.status()).into());
        }

        let body: OllamaListModelsResponseInternal = resp.json().await?;

        Ok(body
            .models
            .into_iter()
            .map(|m| ModelInfo {
                name: m.name,
                modified_at: m.modified_at,
                size: m.size,
            })
            .collect())
    }

    async fn chat(
        &self,
        request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        let url = format!("{}/api/chat", self.base_url);

        let internal_req = OllamaChatRequestInternal {
            model: request.model,
            messages: request.messages,
            stream: request.stream,
        };

        let resp = self.client.post(&url).json(&internal_req).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("Ollama error: {} - {}", status, text).into());
        }

        let body: OllamaChatResponseInternal = resp.json().await?;

        Ok(ChatResponse {
            model: body.model,
            created_at: body.created_at,
            message: body.message,
            done: body.done,
        })
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        let url = format!("{}/api/chat", self.base_url);

        let internal_req = OllamaChatRequestInternal {
            model: request.model,
            messages: request.messages,
            stream: true,
        };

        let resp = self.client.post(&url).json(&internal_req).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("Ollama error: {} - {}", status, text).into());
        }

        let mut bytes_stream = resp.bytes_stream();
        let mut buffer = Vec::new();

        let s = async_stream::try_stream! {
            while let Some(chunk_res) = bytes_stream.next().await {
                let chunk = chunk_res.map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
                buffer.extend_from_slice(&chunk);

                while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                    let line_bytes = buffer[..pos].to_vec();
                    buffer.drain(..pos + 1);

                    let line = String::from_utf8_lossy(&line_bytes).trim().to_string();
                    if line.is_empty() {
                        continue;
                    }

                    match serde_json::from_str::<OllamaChatResponseInternal>(&line) {
                        Ok(body) => {
                            yield ChatStreamEvent {
                                model: body.model,
                                created_at: body.created_at,
                                message: body.message,
                                done: body.done,
                            };
                        }
                        Err(e) => {
                            tracing::error!("Failed to parse Ollama JSON line: {}. Line: {}", e, line);
                            Err(Box::new(e) as Box<dyn Error + Send + Sync>)?;
                        }
                    }
                }
            }
        };

        Ok(Box::pin(s))
    }
}
