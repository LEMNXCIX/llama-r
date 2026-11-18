//! RAG adapters: embeddings + segmented vector store.
//!
//! - [`InMemoryRagStore`]: unit tests / ephemeral
//! - [`FileRagStore`]: disk persistence (Phase 4 plan-B; same scope semantics as LanceDB would)
//! - Namespace helpers for safe `source_id` paths
//! - Chunker for ingest pipeline
//! - OpenRouter embeddings via `POST /api/v1/embeddings`

pub mod chunker;
pub mod embeddings;
pub mod file_store;
pub mod gemini_embeddings;
pub mod namespace;
pub mod openai_compatible_embeddings;
pub mod openrouter_embeddings;
pub mod store;

pub use embeddings::OllamaEmbeddings;
pub use file_store::FileRagStore;
pub use gemini_embeddings::GeminiEmbeddings;
pub use openai_compatible_embeddings::OpenAiCompatibleEmbeddings;
pub use openrouter_embeddings::OpenRouterEmbeddings;
pub use store::{HashEmbeddingProvider, InMemoryRagStore};
