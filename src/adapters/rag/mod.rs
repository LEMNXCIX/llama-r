//! RAG adapters: embeddings + segmented vector store.
//!
//! - [`InMemoryRagStore`]: unit tests / ephemeral
//! - [`FileRagStore`]: disk persistence (Phase 4 plan-B; same scope semantics as LanceDB would)
//! - Namespace helpers for safe `source_id` paths
//! - Chunker for ingest pipeline

pub mod chunker;
pub mod embeddings;
pub mod file_store;
pub mod namespace;
pub mod store;

pub use embeddings::OllamaEmbeddings;
pub use file_store::FileRagStore;
pub use store::{HashEmbeddingProvider, InMemoryRagStore};
