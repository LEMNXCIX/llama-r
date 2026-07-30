//! RAG port: embeddings + vector store, always scoped per agent.
//!
//! Contract is frozen for Phase 4. Adapters (`InMemoryRagStore`, `FileRagStore`)
//! and services (`RagIngestService`, Rig `prepare`) must honor scope checks:
//! - `query_scoped` only reads `scope.rag_sources`
//! - `upsert_scoped` only writes when `scope.allows_rag_write`

use crate::domain::scope::AgentScope;
use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct RagChunk {
    pub id: String,
    pub source_id: String,
    pub text: String,
    pub score: f32,
    pub metadata: Value,
}

#[derive(Debug, Clone)]
pub struct RagUpsert {
    pub source_id: String,
    pub id: String,
    pub text: String,
    pub metadata: Value,
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
    fn dimensions(&self) -> usize;
}

#[async_trait]
pub trait RagStore: Send + Sync {
    /// Query only collections listed in the agent scope.
    async fn query_scoped(
        &self,
        scope: &AgentScope,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<RagChunk>, String>;

    /// Upsert only if the scope write policy allows the target collection.
    async fn upsert_scoped(
        &self,
        scope: &AgentScope,
        docs: Vec<RagUpsert>,
    ) -> Result<usize, String>;

    async fn delete_collection(&self, source_id: &str) -> Result<(), String>;
}
