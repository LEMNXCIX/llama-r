//! RAG port: embeddings + vector store, always scoped per agent.
//!
//! Contract is frozen for Phase 4. Adapters (`InMemoryRagStore`, `FileRagStore`)
//! and services (`RagIngestService`, Rig `prepare`) must honor scope checks:
//! - `query_scoped` only reads `scope.rag_sources`
//! - `upsert_scoped` only writes when `scope.allows_rag_write`

use crate::domain::scope::AgentScope;
use async_trait::async_trait;
use serde_json::Value;

/// Prefix every store uses for a scope write-policy denial.
///
/// Callers classify errors by this prefix rather than by the bare word "denied",
/// so an I/O fault like "Permission denied (os error 13)" stays a server-side
/// error instead of being reported to the client as a bad request.
///
/// Always build these with [`write_denied`], never by hand: a hand-written
/// literal that drifts from this prefix would silently turn a policy denial
/// into a 500.
pub const RAG_WRITE_DENIED: &str = "RAG write denied";

/// Build a write-policy denial error carrying [`RAG_WRITE_DENIED`].
pub fn write_denied(source_id: &str, agent_id: &str) -> String {
    format!("{RAG_WRITE_DENIED} for source '{source_id}' on agent '{agent_id}'")
}

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

/// A set of documents to replace within one collection, as
/// `(id_prefix, docs)` pairs.
pub type RagReplaceBatch = Vec<(String, Vec<RagUpsert>)>;

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

    /// Atomically replace several documents in `source_id`.
    ///
    /// Equivalent to calling [`RagStore::replace_scoped`] once per prefix, except
    /// that either every replacement is applied or none is: a failure partway
    /// through (e.g. the embedding provider erroring on the third document)
    /// leaves the collection exactly as it was, instead of leaving the earlier
    /// documents committed while the caller is told the ingest failed.
    async fn replace_batch_scoped(
        &self,
        scope: &AgentScope,
        source_id: &str,
        batch: RagReplaceBatch,
    ) -> Result<usize, String>;

    /// Atomically replace every chunk in `source_id` whose id starts with
    /// `id_prefix` by `docs`.
    ///
    /// Chunk ids are `{source_key}#chunk-{index}`, so a plain upsert leaves the
    /// high-index chunks of a previously longer document behind forever. This
    /// removes them in the same operation that writes the new ones.
    ///
    /// Contract:
    /// - `id_prefix` must be non-empty; an empty prefix would match every id.
    /// - `docs` must all belong to `source_id`.
    /// - `docs` empty deletes every chunk matching the prefix. Callers must not
    ///   use this to represent "the document is now empty" — see
    ///   `RagIngestService`, which skips empty documents instead.
    /// - On error, the collection must be left exactly as it was.
    async fn replace_scoped(
        &self,
        scope: &AgentScope,
        source_id: &str,
        id_prefix: &str,
        docs: Vec<RagUpsert>,
    ) -> Result<usize, String>;

    /// Delete every chunk in `source_id` whose id starts with `id_prefix`.
    ///
    /// The scoped way to retract a single document. Returns the number of chunks
    /// removed. An empty `id_prefix` is refused: it would match every id and
    /// destroy the whole collection.
    async fn delete_document_scoped(
        &self,
        scope: &AgentScope,
        source_id: &str,
        id_prefix: &str,
    ) -> Result<usize, String>;

    /// Delete a collection only if the scope's write policy allows it.
    ///
    /// Scoped counterpart to [`RagStore::delete_collection`], which is unscoped and
    /// therefore must never be reachable from a request handler.
    ///
    /// A policy denial must be returned via [`write_denied`] so callers can tell
    /// it apart from an I/O failure (see [`RAG_WRITE_DENIED`]).
    async fn delete_collection_scoped(
        &self,
        scope: &AgentScope,
        source_id: &str,
    ) -> Result<(), String>;

    /// Unscoped removal. Admin/internal only: no scope is checked, so any caller
    /// can delete any collection. Never call this from a request handler.
    async fn delete_collection(&self, source_id: &str) -> Result<(), String>;
}
