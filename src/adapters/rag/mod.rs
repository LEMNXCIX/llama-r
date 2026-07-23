//! RAG adapters: embeddings + segmented vector store.

pub mod embeddings;
pub mod store;

pub use embeddings::OllamaEmbeddings;
pub use store::InMemoryRagStore;
