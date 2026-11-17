//! OpenRouter embeddings provider: `POST /api/v1/embeddings`.
//!
//! One batched request carries every text in `input`, and each vector is read
//! from `data[i].embedding`. The API key travels only as
//! `Authorization: Bearer` — never in a URL, a body, or a log line. Like the
//! Ollama provider's upsert contract, a dimension mismatch is reported as an
//! error rather than silently resizing vectors to fit.

use crate::ports::rag::EmbeddingProvider;
use async_trait::async_trait;
use reqwest::Client;
use std::time::Duration;

const OPENROUTER_BASE: &str = "https://openrouter.ai";
const EMBEDDINGS_PATH: &str = "/api/v1/embeddings";

pub struct OpenRouterEmbeddings {
    base_url: String,
    api_key: String,
    model: String,
    dimensions: usize,
    http: Client,
}

impl OpenRouterEmbeddings {
    /// Production constructor, pinned to OpenRouter's hosted origin.
    pub fn new(api_key: String, model: String, dimensions: usize) -> Self {
        Self::with_endpoint(OPENROUTER_BASE, api_key, model, dimensions)
    }

    /// Override the origin while keeping the same path and wire format. Used by
    /// the wire tests against a local fake server; also useful behind a proxy.
    pub fn with_endpoint(
        base_url: impl Into<String>,
        api_key: String,
        model: String,
        dimensions: usize,
    ) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            model,
            dimensions: dimensions.max(1),
            http,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}{EMBEDDINGS_PATH}", self.base_url)
    }
}

fn parse_vector(value: &serde_json::Value) -> Option<Vec<f32>> {
    let array = value.as_array()?;
    // Strict: one non-numeric entry makes the whole vector unusable — silently
    // dropping it would hand back a shorter, fabricated embedding.
    let vector: Vec<f32> = array
        .iter()
        .map(|item| item.as_f64().map(|number| number as f32))
        .collect::<Option<Vec<f32>>>()?;
    if vector.is_empty() {
        None
    } else {
        Some(vector)
    }
}

#[async_trait]
impl EmbeddingProvider for OpenRouterEmbeddings {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let response = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({ "model": self.model, "input": texts }))
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
        let items = value
            .get("data")
            .and_then(|value| value.as_array())
            .ok_or_else(|| format!("missing embedding field from model '{}'", self.model))?;

        if items.len() != texts.len() {
            return Err(format!(
                "embeddings count mismatch: requested {}, got {}",
                texts.len(),
                items.len()
            ));
        }

        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let vector = item
                .get("embedding")
                .and_then(parse_vector)
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    async fn mock_server(hits: Arc<AtomicUsize>, dims: usize) -> String {
        let app = Router::new().route(
            "/api/v1/embeddings",
            post(move |Json(body): Json<serde_json::Value>| {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let count = body
                        .get("input")
                        .and_then(|value| value.as_array())
                        .map(|items| items.len())
                        .unwrap_or(0);
                    let embedding: Vec<f32> = (0..dims).map(|i| i as f32 / dims as f32).collect();
                    Json(serde_json::json!({
                        "model": "mock",
                        "data": (0..count).map(|index| serde_json::json!({
                            "object": "embedding",
                            "index": index,
                            "embedding": embedding,
                        })).collect::<Vec<_>>(),
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn embed_batches_all_texts_and_uses_configured_dimensions() {
        let hits = Arc::new(AtomicUsize::new(0));
        let base = mock_server(hits.clone(), 3).await;
        let provider =
            OpenRouterEmbeddings::with_endpoint(base, "sk-key".to_string(), "mock".to_string(), 3);
        let vectors = provider
            .embed(&["a".to_string(), "b".to_string()])
            .await
            .unwrap();
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors[0].len(), 3);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "one batched request");
    }

    #[tokio::test]
    async fn embed_empty_input_short_circuits() {
        let hits = Arc::new(AtomicUsize::new(0));
        let base = mock_server(hits.clone(), 3).await;
        let provider =
            OpenRouterEmbeddings::with_endpoint(base, "sk-key".to_string(), "mock".to_string(), 3);
        assert!(provider.embed(&[]).await.unwrap().is_empty());
        assert_eq!(hits.load(Ordering::SeqCst), 0, "must not call the server");
    }

    #[tokio::test]
    async fn embed_reports_a_dimension_mismatch_instead_of_resizing() {
        let hits = Arc::new(AtomicUsize::new(0));
        let base = mock_server(hits, 3).await;
        let provider =
            OpenRouterEmbeddings::with_endpoint(base, "sk-key".to_string(), "mock".to_string(), 8);
        let err = provider.embed(&["text".to_string()]).await.unwrap_err();
        assert!(err.contains("dimension mismatch"), "{err}");
        assert!(err.contains("got 3"), "{err}");
    }
}
