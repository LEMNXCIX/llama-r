//! OpenAI-compatible embeddings: `POST {base}/v1/embeddings`.
//!
//! The wire (Bearer key, batched `input`, `data[i].embedding`, strict dimension
//! check) is identical to OpenRouter's, so this adapter delegates to
//! `OpenRouterEmbeddings` with only the path changed. A self-hosted or hosted
//! compatible endpoint is configured by the profile's `base_url`; the secret
//! lives in its `api_key_env` variable and never in a URL, body or log line.

use crate::adapters::rag::openrouter_embeddings::OpenRouterEmbeddings;
use crate::ports::rag::EmbeddingProvider;
use async_trait::async_trait;

const EMBEDDINGS_PATH: &str = "/v1/embeddings";

pub struct OpenAiCompatibleEmbeddings {
    inner: OpenRouterEmbeddings,
}

impl OpenAiCompatibleEmbeddings {
    /// `base_url` is the origin of an OpenAI-compatible upstream (for example
    /// `https://api.openai.com`), matching the chat adapter's convention.
    pub fn new(
        base_url: impl Into<String>,
        api_key: String,
        model: String,
        dimensions: usize,
    ) -> Self {
        // `OPENAI_BASE_URL` is conventionally given with `/v1`; this adapter's
        // path already carries that prefix, so normalize it away here rather
        // than requesting `/v1/v1/embeddings`.
        let base = base_url.into().trim_end_matches('/').to_string();
        let base = base.strip_suffix("/v1").unwrap_or(&base).to_string();
        Self {
            inner: OpenRouterEmbeddings::with_path(
                base,
                EMBEDDINGS_PATH,
                api_key,
                model,
                dimensions,
            ),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiCompatibleEmbeddings {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        self.inner.embed(texts).await
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
}
