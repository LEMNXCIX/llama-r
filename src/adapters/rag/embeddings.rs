//! Ollama embeddings provider (`POST /api/embeddings`).
//!
//! Sequential batching is intentional for Phase 4 (simple + reliable).
//! Boot does not fail if the embedding model is missing; queries surface clear errors.

use crate::ports::rag::EmbeddingProvider;
use async_trait::async_trait;
use reqwest::Client;
use std::time::Duration;

pub struct OllamaEmbeddings {
    pub base_url: String,
    pub model: String,
    pub dimensions: usize,
    http: Client,
}

impl OllamaEmbeddings {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>, dimensions: usize) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            base_url: base_url.into(),
            model: model.into(),
            dimensions: dimensions.max(1),
            http,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/api/embeddings", self.base_url.trim_end_matches('/'))
    }
}

#[async_trait]
impl EmbeddingProvider for OllamaEmbeddings {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut out = Vec::with_capacity(texts.len());
        let endpoint = self.endpoint();

        for text in texts {
            let response = self
                .http
                .post(&endpoint)
                .json(&serde_json::json!({
                    "model": self.model,
                    "prompt": text,
                }))
                .send()
                .await
                .map_err(|err| format!("embeddings request failed: {err}"))?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(format!("embeddings HTTP {status}: {body}"));
            }

            let value: serde_json::Value = response
                .json()
                .await
                .map_err(|err| format!("embeddings JSON parse failed: {err}"))?;
            let embedding = value
                .get("embedding")
                .and_then(|item| item.as_array())
                .ok_or_else(|| {
                    format!(
                        "missing embedding field from model '{}' (is it pulled in Ollama?)",
                        self.model
                    )
                })?
                .iter()
                .filter_map(|item| item.as_f64().map(|number| number as f32))
                .collect::<Vec<_>>();

            if embedding.is_empty() {
                return Err(format!(
                    "empty embedding returned by model '{}'",
                    self.model
                ));
            }

            // Soft dim check: warn via error only on hard mismatch when configured dim is set
            // and vector length differs (callers may still use actual vectors for search).
            if embedding.len() != self.dimensions {
                tracing::warn!(
                    model = %self.model,
                    expected = self.dimensions,
                    actual = embedding.len(),
                    "embedding dimension differs from EMBEDDING_DIMENSIONS; using actual length"
                );
            }

            out.push(embedding);
        }

        Ok(out)
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}
