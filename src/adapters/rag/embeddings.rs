//! Ollama embeddings provider (`POST /api/embeddings`).

use crate::ports::rag::EmbeddingProvider;
use async_trait::async_trait;
use reqwest::Client;

pub struct OllamaEmbeddings {
    pub base_url: String,
    pub model: String,
    pub dimensions: usize,
    http: Client,
}

impl OllamaEmbeddings {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>, dimensions: usize) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            dimensions,
            http: Client::new(),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for OllamaEmbeddings {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let mut out = Vec::with_capacity(texts.len());
        let endpoint = format!("{}/api/embeddings", self.base_url.trim_end_matches('/'));

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
                .map_err(|err| err.to_string())?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(format!("embeddings HTTP {status}: {body}"));
            }

            let value: serde_json::Value = response.json().await.map_err(|err| err.to_string())?;
            let embedding = value
                .get("embedding")
                .and_then(|item| item.as_array())
                .ok_or_else(|| "missing embedding field".to_string())?
                .iter()
                .filter_map(|item| item.as_f64().map(|number| number as f32))
                .collect::<Vec<_>>();

            out.push(embedding);
        }

        Ok(out)
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}
