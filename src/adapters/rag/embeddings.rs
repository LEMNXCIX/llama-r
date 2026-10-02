//! Ollama embeddings provider.
//!
//! Uses `POST /api/embed` (`input` → `embeddings`), the current Ollama API, and
//! batches every text into a single request. Ollama builds that predate that
//! endpoint only expose `POST /api/embeddings` (`prompt` → `embedding`), so a 404
//! on `/api/embed` transparently falls back to the legacy route.
//!
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
    /// Whether `/api/embed` already answered 404, so the fallback is tried once
    /// per provider instead of once per embed call.
    legacy_only: std::sync::atomic::AtomicBool,
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
            legacy_only: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    fn use_legacy(&self) -> bool {
        self.legacy_only.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn mark_legacy(&self) {
        self.legacy_only
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// `POST /api/embed`: one request for all texts, returns `embeddings`.
    async fn embed_current(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let response = self
            .http
            .post(self.endpoint("/api/embed"))
            .json(&serde_json::json!({ "model": self.model, "input": texts }))
            .send()
            .await
            .map_err(|err| format!("embeddings request failed: {err}"))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err("__legacy__".to_string());
        }
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
            .get("embeddings")
            .and_then(|item| item.as_array())
            .ok_or_else(|| missing_field_error(&self.model))?;

        if items.len() != texts.len() {
            return Err(format!(
                "embeddings count mismatch: requested {}, got {}",
                texts.len(),
                items.len()
            ));
        }

        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let vector = parse_vector(item).ok_or_else(|| missing_field_error(&self.model))?;
            out.push(vector);
        }
        Ok(out)
    }

    /// `POST /api/embeddings`: legacy route, one request per text.
    async fn embed_legacy(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let response = self
                .http
                .post(self.endpoint("/api/embeddings"))
                .json(&serde_json::json!({ "model": self.model, "prompt": text }))
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
            let vector = value
                .get("embedding")
                .and_then(parse_vector)
                .ok_or_else(|| missing_field_error(&self.model))?;
            out.push(vector);
        }
        Ok(out)
    }
}

/// Sentinel used to signal "this Ollama build only has the legacy route".
const LEGACY_FALLBACK: &str = "__legacy__";

fn missing_field_error(model: &str) -> String {
    format!("missing embedding field from model '{model}' (is it pulled in Ollama?)")
}

fn parse_vector(value: &serde_json::Value) -> Option<Vec<f32>> {
    let vector: Vec<f32> = value
        .as_array()?
        .iter()
        .filter_map(|item| item.as_f64().map(|number| number as f32))
        .collect();
    if vector.is_empty() {
        None
    } else {
        Some(vector)
    }
}

#[async_trait]
impl EmbeddingProvider for OllamaEmbeddings {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let out = if self.use_legacy() {
            self.embed_legacy(texts).await?
        } else {
            match self.embed_current(texts).await {
                Ok(vectors) => vectors,
                Err(err) if err == LEGACY_FALLBACK => {
                    // This Ollama build predates /api/embed.
                    self.mark_legacy();
                    tracing::debug!(
                        "Ollama /api/embed unavailable; falling back to legacy /api/embeddings"
                    );
                    self.embed_legacy(texts).await?
                }
                Err(err) => return Err(err),
            }
        };

        for vector in &out {
            if vector.len() != self.dimensions {
                tracing::warn!(
                    model = %self.model,
                    expected = self.dimensions,
                    actual = vector.len(),
                    "embedding dimension differs from EMBEDDING_DIMENSIONS; using actual length"
                );
            }
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

    /// Serve `/api/embed` (current Ollama API) and count requests.
    async fn mock_embed_server(hits: Arc<AtomicUsize>, dims: usize) -> String {
        let app = Router::new().route(
            "/api/embed",
            post(move |Json(body): Json<serde_json::Value>| {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let input = body
                        .get("input")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([]));
                    let count = match &input {
                        serde_json::Value::Array(items) => items.len(),
                        _ => 1,
                    };
                    let embedding: Vec<f32> = (0..dims).map(|i| (i as f32) / dims as f32).collect();
                    Json(serde_json::json!({
                        "model": "mock-embed",
                        "embeddings": vec![embedding; count],
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
    async fn embed_uses_current_ollama_api_endpoint() {
        // Ollama >= 0.3 exposes POST /api/embed with an `input` field and returns
        // `embeddings`. The legacy POST /api/embeddings (`prompt`/`embedding`)
        // answers 404 "path not found" on those versions.
        let hits = Arc::new(AtomicUsize::new(0));
        let base = mock_embed_server(hits.clone(), 8).await;
        let provider = OllamaEmbeddings::new(base, "mock-embed", 8);

        let vectors = provider
            .embed(&["refund policy".to_string(), "warranty".to_string()])
            .await
            .expect("embed must work against the current Ollama API");

        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors[0].len(), 8);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "must batch all texts in one request"
        );
    }

    #[tokio::test]
    async fn embed_reports_http_error_clearly() {
        let app = Router::new().route(
            "/api/embed",
            post(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let provider = OllamaEmbeddings::new(format!("http://{addr}"), "mock", 8);
        let err = provider.embed(&["text".to_string()]).await.unwrap_err();
        assert!(err.contains("HTTP 500"), "got: {err}");
    }

    #[tokio::test]
    async fn embed_empty_input_short_circuits() {
        let hits = Arc::new(AtomicUsize::new(0));
        let base = mock_embed_server(hits.clone(), 8).await;
        let provider = OllamaEmbeddings::new(base, "mock-embed", 8);
        assert!(provider.embed(&[]).await.unwrap().is_empty());
        assert_eq!(hits.load(Ordering::SeqCst), 0, "must not call the server");
    }

    #[tokio::test]
    async fn embed_falls_back_to_legacy_endpoint() {
        // Older Ollama builds only expose POST /api/embeddings with `prompt` and a
        // single `embedding`. The provider must still work against them.
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/api/embed",
                post(|| async { (axum::http::StatusCode::NOT_FOUND, "not found") }),
            )
            .route(
                "/api/embeddings",
                post(move |Json(body): Json<serde_json::Value>| {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        let prompt = body
                            .get("prompt")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let embedding: Vec<f32> = (0..8)
                            .map(|i| (i as f32 + prompt.len() as f32) / 8.0)
                            .collect();
                        Json(serde_json::json!({ "embedding": embedding }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let provider = OllamaEmbeddings::new(format!("http://{addr}"), "mock", 8);
        let vectors = provider
            .embed(&["legacy text".to_string()])
            .await
            .expect("must fall back to the legacy endpoint");
        assert_eq!(vectors.len(), 1);
        assert_eq!(vectors[0].len(), 8);
    }
}
