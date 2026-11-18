//! Gemini embeddings via `POST {base}/models/{model}:batchEmbedContents`.
//!
//! All texts are sent in one request. The key travels in an `x-goog-api-key`
//! header, never in the URL: a `?key=` query parameter would leak into proxy
//! logs and error strings. Each `values` vector is parsed strictly and checked
//! against the configured dimensions, matching the other embedding adapters.

use crate::ports::rag::EmbeddingProvider;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const MODEL_PREFIX: &str = "models/";
const BATCH_PATH_SUFFIX: &str = ":batchEmbedContents";

pub struct GeminiEmbeddings {
    base_url: String,
    api_key: String,
    model: String,
    dimensions: usize,
    http: Client,
}

impl GeminiEmbeddings {
    /// `base_url = None` uses Google's native `v1beta` origin.
    pub fn new(
        base_url: Option<String>,
        api_key: String,
        model: String,
        dimensions: usize,
    ) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            base_url: base_url
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            api_key,
            model,
            dimensions: dimensions.max(1),
            http,
        }
    }

    fn model_id(&self) -> &str {
        let model = self.model.trim();
        model.strip_prefix(MODEL_PREFIX).unwrap_or(model)
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/models/{}{}",
            self.base_url,
            self.model_id(),
            BATCH_PATH_SUFFIX
        )
    }
}

#[derive(Serialize)]
struct BatchRequest {
    requests: Vec<EmbedRequest>,
}

#[derive(Serialize)]
struct EmbedRequest {
    model: String,
    content: WireContent,
}

#[derive(Serialize)]
struct WireContent {
    parts: Vec<WirePart>,
}

#[derive(Serialize)]
struct WirePart {
    text: String,
}

#[derive(Deserialize)]
struct BatchResponse {
    embeddings: Vec<EmbeddingEntry>,
}

#[derive(Deserialize)]
struct EmbeddingEntry {
    values: Vec<serde_json::Value>,
}

fn parse_vector(entry: &EmbeddingEntry) -> Option<Vec<f32>> {
    let vector: Vec<f32> = entry
        .values
        .iter()
        .map(|value| value.as_f64().map(|number| number as f32))
        .collect::<Option<Vec<f32>>>()?;
    (!vector.is_empty()).then_some(vector)
}

#[async_trait]
impl EmbeddingProvider for GeminiEmbeddings {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let model = format!("{MODEL_PREFIX}{}", self.model_id());
        let requests = texts
            .iter()
            .map(|text| EmbedRequest {
                model: model.clone(),
                content: WireContent {
                    parts: vec![WirePart { text: text.clone() }],
                },
            })
            .collect();
        let response = self
            .http
            .post(self.endpoint())
            .header("x-goog-api-key", &self.api_key)
            .json(&BatchRequest { requests })
            .send()
            .await
            .map_err(|err| format!("Gemini embeddings request failed: {err}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("Gemini embeddings HTTP {status}: {body}"));
        }

        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|err| format!("Gemini embeddings JSON parse failed: {err}"))?;
        let parsed: BatchResponse = serde_json::from_value(value)
            .map_err(|err| format!("Gemini embeddings response shape invalid: {err}"))?;
        if parsed.embeddings.len() != texts.len() {
            return Err(format!(
                "Gemini embeddings count mismatch: requested {}, got {}",
                texts.len(),
                parsed.embeddings.len()
            ));
        }

        let mut out = Vec::with_capacity(parsed.embeddings.len());
        for entry in &parsed.embeddings {
            let vector = parse_vector(entry)
                .ok_or_else(|| format!("missing embedding field from model '{}'", self.model))?;
            if vector.len() != self.dimensions {
                return Err(format!(
                    "embedding dimension mismatch for model '{}': expected {}, got {}",
                    self.model,
                    self.dimensions,
                    vector.len()
                ));
            }
            out.push(vector);
        }
        Ok(out)
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}
