//! File-backed RAG store with full scope enforcement.
//!
//! Phase 4 plan B: JSONL persistence per collection under
//! `{base}/data/lancedb/<encoded_source_id>/docs.jsonl`.
//!
//! Same port contract as [`InMemoryRagStore`]; LanceDB can replace this adapter later
//! without changing services. Uses cosine similarity over stored embeddings.

use super::namespace::{encode_source_id_for_path, validate_source_id};
use crate::domain::scope::AgentScope;
use crate::ports::rag::{EmbeddingProvider, RagChunk, RagReplaceBatch, RagStore, RagUpsert};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredDoc {
    id: String,
    text: String,
    embedding: Vec<f32>,
    metadata: serde_json::Value,
}

/// Persistent segmented store: one directory per `source_id`.
pub struct FileRagStore {
    base_dir: PathBuf,
    embeddings: Arc<dyn EmbeddingProvider>,
    /// In-process cache; reloaded from disk on first access per source.
    cache: RwLock<HashMap<String, Vec<StoredDoc>>>,
}

impl FileRagStore {
    pub fn new(base_dir: impl Into<PathBuf>, embeddings: Arc<dyn EmbeddingProvider>) -> Self {
        let base_dir = base_dir.into();
        if let Err(err) = std::fs::create_dir_all(&base_dir) {
            tracing::warn!(
                path = %base_dir.display(),
                error = %err,
                "failed to create FileRagStore base dir"
            );
        }
        Self {
            base_dir,
            embeddings,
            cache: RwLock::new(HashMap::new()),
        }
    }

    fn collection_dir(&self, source_id: &str) -> PathBuf {
        self.base_dir.join(encode_source_id_for_path(source_id))
    }

    fn docs_path(dir: &Path) -> PathBuf {
        dir.join("docs.jsonl")
    }

    fn load_from_disk(path: &Path) -> Result<Vec<StoredDoc>, String> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(path)
            .map_err(|err| format!("rag read '{}': {err}", path.display()))?;
        let mut docs = Vec::new();
        for (line_no, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let doc: StoredDoc = serde_json::from_str(line)
                .map_err(|err| format!("rag parse '{}':{}: {err}", path.display(), line_no + 1))?;
            docs.push(doc);
        }
        Ok(docs)
    }

    fn write_to_disk(path: &Path, docs: &[StoredDoc]) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("rag mkdir '{}': {err}", parent.display()))?;
        }
        let mut body = String::new();
        for doc in docs {
            let line = serde_json::to_string(doc)
                .map_err(|err| format!("rag serialize doc '{}': {err}", doc.id))?;
            body.push_str(&line);
            body.push('\n');
        }
        // Atomic-ish write via temp file in same directory.
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, &body)
            .map_err(|err| format!("rag write '{}': {err}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .map_err(|err| format!("rag rename to '{}': {err}", path.display()))?;
        Ok(())
    }

    async fn ensure_loaded(&self, source_id: &str) -> Result<(), String> {
        {
            let cache = self.cache.read().await;
            if cache.contains_key(source_id) {
                return Ok(());
            }
        }
        let path = Self::docs_path(&self.collection_dir(source_id));
        let docs = Self::load_from_disk(&path)?;
        let mut cache = self.cache.write().await;
        cache.entry(source_id.to_string()).or_insert(docs);
        Ok(())
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

#[async_trait]
impl RagStore for FileRagStore {
    async fn query_scoped(
        &self,
        scope: &AgentScope,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<RagChunk>, String> {
        if scope.rag_sources.is_empty() || top_k == 0 {
            return Ok(Vec::new());
        }

        let query_vec = self
            .embeddings
            .embed(&[query.to_string()])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| "empty embedding for query".to_string())?;

        let mut hits = Vec::new();

        for source_id in &scope.rag_sources {
            if !scope.allows_rag_read(source_id) {
                return Err(format!(
                    "RAG read denied for source '{source_id}' on agent '{}'",
                    scope.agent_id
                ));
            }
            validate_source_id(source_id)?;

            let dir = self.collection_dir(source_id);
            if !dir.exists() {
                continue;
            }

            self.ensure_loaded(source_id).await?;
            let cache = self.cache.read().await;
            let Some(docs) = cache.get(source_id) else {
                continue;
            };
            for doc in docs {
                let score = cosine_similarity(&query_vec, &doc.embedding);
                hits.push(RagChunk {
                    id: doc.id.clone(),
                    source_id: source_id.clone(),
                    text: doc.text.clone(),
                    score,
                    metadata: doc.metadata.clone(),
                });
            }
        }

        hits.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(top_k);
        Ok(hits)
    }

    async fn upsert_scoped(
        &self,
        scope: &AgentScope,
        docs: Vec<RagUpsert>,
    ) -> Result<usize, String> {
        if docs.is_empty() {
            return Ok(0);
        }

        // Deny before embed (saves CPU) — defense in depth.
        for doc in &docs {
            validate_source_id(&doc.source_id)?;
            if !scope.allows_rag_write(&doc.source_id) {
                return Err(format!(
                    "RAG write denied for source '{}' on agent '{}'",
                    doc.source_id, scope.agent_id
                ));
            }
        }

        let texts = docs.iter().map(|doc| doc.text.clone()).collect::<Vec<_>>();
        let vectors = self.embeddings.embed(&texts).await?;
        if vectors.len() != docs.len() {
            return Err("embedding count mismatch".to_string());
        }
        // Reject dimension mismatches loudly. Storing vectors whose length differs
        // from the collection's would make cosine_similarity return 0.0, surfacing
        // stale/unrelated documents as the top hits with no signal to the caller.
        let expected = self.embeddings.dimensions();
        for (index, vector) in vectors.iter().enumerate() {
            if vector.len() != expected {
                return Err(format!(
                    "embedding dimension mismatch for doc '{}': expected {expected}, got {} \
                     (re-ingest the collection after changing EMBEDDING_MODEL/EMBEDDING_DIMENSIONS)",
                    docs[index].id,
                    vector.len()
                ));
            }
        }

        // Group by source_id for batch disk writes.
        let mut by_source: HashMap<String, Vec<(RagUpsert, Vec<f32>)>> = HashMap::new();
        for (doc, embedding) in docs.into_iter().zip(vectors.into_iter()) {
            by_source
                .entry(doc.source_id.clone())
                .or_default()
                .push((doc, embedding));
        }

        let mut written = 0usize;
        for (source_id, items) in by_source {
            self.ensure_loaded(&source_id).await?;
            let mut cache = self.cache.write().await;
            let entry = cache.entry(source_id.clone()).or_default();

            for (doc, embedding) in items {
                if let Some(existing) = entry.iter_mut().find(|item| item.id == doc.id) {
                    existing.text = doc.text;
                    existing.embedding = embedding;
                    existing.metadata = doc.metadata;
                } else {
                    entry.push(StoredDoc {
                        id: doc.id,
                        text: doc.text,
                        embedding,
                        metadata: doc.metadata,
                    });
                }
                written += 1;
            }

            let path = Self::docs_path(&self.collection_dir(&source_id));
            Self::write_to_disk(&path, entry)?;
        }

        Ok(written)
    }

    async fn replace_scoped(
        &self,
        scope: &AgentScope,
        source_id: &str,
        id_prefix: &str,
        docs: Vec<RagUpsert>,
    ) -> Result<usize, String> {
        self.replace_batch_scoped(scope, source_id, vec![(id_prefix.to_string(), docs)])
            .await
    }

    async fn replace_batch_scoped(
        &self,
        scope: &AgentScope,
        source_id: &str,
        batch: RagReplaceBatch,
    ) -> Result<usize, String> {
        // An empty prefix matches every id: refuse before anything else, so this
        // can never be used to wipe a collection.
        for (id_prefix, _) in &batch {
            if id_prefix.is_empty() {
                return Err("replace_batch_scoped requires a non-empty id_prefix".into());
            }
        }
        validate_source_id(source_id)?;
        // Deny before embed (defense in depth + saves CPU).
        if !scope.allows_rag_write(source_id) {
            return Err(format!(
                "RAG write denied for source '{source_id}' on agent '{}'",
                scope.agent_id
            ));
        }
        for (id_prefix, docs) in &batch {
            for doc in docs {
                if doc.source_id != source_id {
                    return Err(format!(
                        "replace doc '{}' does not belong to source '{source_id}'",
                        doc.id
                    ));
                }
                // Every doc must belong to the prefix being replaced, or this call
                // would delete another document's chunks while writing outside the
                // declared scope.
                if !doc.id.starts_with(id_prefix.as_str()) {
                    return Err(format!(
                        "replace doc '{}' does not match id_prefix '{id_prefix}'",
                        doc.id
                    ));
                }
            }
        }

        // Embed everything before touching the collection or the disk, so a
        // provider failure on any document leaves both untouched.
        let all_docs: Vec<RagUpsert> = batch
            .iter()
            .flat_map(|(_, docs)| docs.iter().cloned())
            .collect();
        let texts: Vec<String> = all_docs.iter().map(|doc| doc.text.clone()).collect();
        let vectors = self.embeddings.embed(&texts).await?;
        if vectors.len() != all_docs.len() {
            return Err("embedding count mismatch".to_string());
        }
        let expected = self.embeddings.dimensions();
        for (index, vector) in vectors.iter().enumerate() {
            if vector.len() != expected {
                return Err(format!(
                    "embedding dimension mismatch for doc '{}': expected {expected}, got {} \
                     (re-ingest the collection after changing EMBEDDING_MODEL/EMBEDDING_DIMENSIONS)",
                    all_docs[index].id,
                    vector.len()
                ));
            }
        }

        self.ensure_loaded(source_id).await?;
        let mut cache = self.cache.write().await;

        // Build the new collection without mutating the cache, so a failed disk
        // write cannot leave the cache permanently missing the old chunks (which
        // a later successful write would then persist, making the loss permanent).
        let mut updated: Vec<StoredDoc> = cache.get(source_id).cloned().unwrap_or_default();
        let mut cursor = 0usize;
        for (id_prefix, docs) in batch {
            // Drop chunks from a previous, longer version of this same document.
            updated.retain(|item| !item.id.starts_with(id_prefix.as_str()));
            for doc in docs {
                let embedding = vectors[cursor].clone();
                cursor += 1;
                updated.push(StoredDoc {
                    id: doc.id,
                    text: doc.text,
                    embedding,
                    metadata: doc.metadata,
                });
            }
        }

        let path = Self::docs_path(&self.collection_dir(source_id));
        Self::write_to_disk(&path, &updated)?;

        // Disk is authoritative: publish the new state only after it is durable.
        let written = all_docs.len();
        cache.insert(source_id.to_string(), updated);
        Ok(written)
    }

    async fn delete_collection_scoped(
        &self,
        scope: &AgentScope,
        source_id: &str,
    ) -> Result<(), String> {
        if !scope.allows_rag_write(source_id) {
            return Err(format!(
                "RAG write denied for source '{source_id}' on agent '{}'",
                scope.agent_id
            ));
        }
        self.delete_collection(source_id).await
    }

    async fn delete_collection(&self, source_id: &str) -> Result<(), String> {
        validate_source_id(source_id)?;
        self.cache.write().await.remove(source_id);
        let dir = self.collection_dir(source_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|err| format!("rag delete_collection '{source_id}': {err}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::rag::store::HashEmbeddingProvider;
    use crate::domain::agent::RagWritePolicy;
    use crate::domain::scope::{AgentScope, ToolsAllow};
    use std::collections::HashSet;

    fn scope_for(agent: &str, sources: &[&str], write: RagWritePolicy) -> AgentScope {
        AgentScope {
            agent_id: agent.into(),
            project_id: None,
            mcp_sources: HashSet::new(),
            tools_allow: ToolsAllow::DenyAll,
            rag_sources: sources.iter().map(|s| (*s).to_string()).collect(),
            rag_write: write,
            max_tool_calls: 8,
            max_iterations: 6,
            timeout_secs: 90,
        }
    }

    #[tokio::test]
    async fn file_store_rejects_vector_dimension_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        // Provider claims 32 dims but emits 16-dim vectors: a misconfigured
        // EMBEDDING_DIMENSIONS / EMBEDDING_MODEL mismatch.
        struct WrongDim;
        #[async_trait]
        impl EmbeddingProvider for WrongDim {
            async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
                Ok(texts
                    .iter()
                    .map(|_| vec![0.5f32; 16])
                    .collect::<Vec<Vec<f32>>>())
            }
            fn dimensions(&self) -> usize {
                32
            }
        }

        let store = FileRagStore::new(dir.path(), Arc::new(WrongDim));
        let scope = scope_for("a", &["agent:a/memory"], RagWritePolicy::OwnMemoryOnly);
        let err = store
            .upsert_scoped(
                &scope,
                vec![RagUpsert {
                    source_id: "agent:a/memory".into(),
                    id: "1".into(),
                    text: "some knowledge".into(),
                    metadata: serde_json::json!({}),
                }],
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("dimension"),
            "dim mismatch must fail loudly, got: {err}"
        );
    }

    #[tokio::test]
    async fn file_isolation_prevents_cross_agent_reads() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileRagStore::new(dir.path(), Arc::new(HashEmbeddingProvider::new(16)));
        let writer = scope_for("a", &["agent:a/memory"], RagWritePolicy::OwnMemoryOnly);
        store
            .upsert_scoped(
                &writer,
                vec![RagUpsert {
                    source_id: "agent:a/memory".into(),
                    id: "1".into(),
                    text: "secret of agent a".into(),
                    metadata: serde_json::json!({}),
                }],
            )
            .await
            .unwrap();

        let reader_b = scope_for("b", &["agent:b/memory"], RagWritePolicy::None);
        let hits = store.query_scoped(&reader_b, "secret", 5).await.unwrap();
        assert!(hits.is_empty());

        let reader_a = scope_for("a", &["agent:a/memory"], RagWritePolicy::None);
        let hits = store.query_scoped(&reader_a, "secret", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("agent a"));
    }

    #[tokio::test]
    async fn file_store_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let emb: Arc<dyn EmbeddingProvider> = Arc::new(HashEmbeddingProvider::new(16));
        {
            let store = FileRagStore::new(dir.path(), emb.clone());
            let writer = scope_for("a", &["agent:a/memory"], RagWritePolicy::OwnMemoryOnly);
            store
                .upsert_scoped(
                    &writer,
                    vec![RagUpsert {
                        source_id: "agent:a/memory".into(),
                        id: "persist-1".into(),
                        text: "persisted knowledge about refunds".into(),
                        metadata: serde_json::json!({"k": "v"}),
                    }],
                )
                .await
                .unwrap();
        }
        // New process-like instance
        let store = FileRagStore::new(dir.path(), emb);
        let reader = scope_for("a", &["agent:a/memory"], RagWritePolicy::None);
        let hits = store.query_scoped(&reader, "refunds", 3).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "persist-1");
    }

    #[tokio::test]
    async fn file_store_replace_drops_stale_chunks_on_disk() {
        use crate::adapters::rag::chunker::{chunk_text, ChunkConfig};

        let dir = tempfile::tempdir().unwrap();
        let emb: Arc<dyn EmbeddingProvider> = Arc::new(HashEmbeddingProvider::new(16));
        let scope = scope_for("a", &["agent:a/memory"], RagWritePolicy::OwnMemoryOnly);

        // Long version: many chunks.
        {
            let store = FileRagStore::new(dir.path(), emb.clone());
            let chunks = chunk_text(
                "policy",
                &"Refund policy detail sentence. ".repeat(120),
                &ChunkConfig {
                    max_chars: 200,
                    overlap_chars: 20,
                },
            );
            let docs: Vec<RagUpsert> = chunks
                .into_iter()
                .map(|c| RagUpsert {
                    source_id: "agent:a/memory".into(),
                    id: c.id,
                    text: c.text,
                    metadata: serde_json::json!({}),
                })
                .collect();
            assert!(docs.len() >= 4);
            store
                .replace_scoped(&scope, "agent:a/memory", "policy#chunk-", docs)
                .await
                .unwrap();
        }

        // Short version in a fresh instance: replaces, and the deletion must be
        // persisted, not just cached.
        {
            let store = FileRagStore::new(dir.path(), emb.clone());
            let chunks = chunk_text("policy", "Short updated policy.", &ChunkConfig::default());
            let docs: Vec<RagUpsert> = chunks
                .into_iter()
                .map(|c| RagUpsert {
                    source_id: "agent:a/memory".into(),
                    id: c.id,
                    text: c.text,
                    metadata: serde_json::json!({}),
                })
                .collect();
            store
                .replace_scoped(&scope, "agent:a/memory", "policy#chunk-", docs)
                .await
                .unwrap();
        }

        // Third read: only the short chunk remains on disk.
        let store = FileRagStore::new(dir.path(), emb);
        let hits = store
            .query_scoped(&scope, "refund policy detail sentence", 50)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "stale chunks survived on disk");
        assert!(hits[0].text.contains("Short updated policy"));
    }

    #[tokio::test]
    async fn file_store_replace_respects_write_policy() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileRagStore::new(dir.path(), Arc::new(HashEmbeddingProvider::new(16)));
        let scope = scope_for("a", &["shared/docs"], RagWritePolicy::None);
        let err = store
            .replace_scoped(
                &scope,
                "shared/docs",
                "policy#chunk-",
                vec![RagUpsert {
                    source_id: "shared/docs".into(),
                    id: "policy#chunk-0".into(),
                    text: "nope".into(),
                    metadata: serde_json::json!({}),
                }],
            )
            .await
            .unwrap_err();
        assert!(err.contains("denied"), "got: {err}");
    }

    #[tokio::test]
    async fn file_store_replace_rejects_empty_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileRagStore::new(dir.path(), Arc::new(HashEmbeddingProvider::new(16)));
        let scope = scope_for("a", &["agent:a/memory"], RagWritePolicy::OwnMemoryOnly);
        store
            .upsert_scoped(
                &scope,
                vec![RagUpsert {
                    source_id: "agent:a/memory".into(),
                    id: "summary-abc".into(),
                    text: "a summary written by the history summarizer".into(),
                    metadata: serde_json::json!({}),
                }],
            )
            .await
            .unwrap();

        // An empty prefix matches every id and must be refused, not wipe the collection.
        let err = store
            .replace_scoped(&scope, "agent:a/memory", "", vec![])
            .await
            .unwrap_err();
        assert!(err.contains("non-empty"), "got: {err}");

        let hits = store.query_scoped(&scope, "summary", 5).await.unwrap();
        assert_eq!(hits.len(), 1, "collection must be untouched");
    }

    #[tokio::test]
    async fn file_store_replace_rejects_doc_outside_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileRagStore::new(dir.path(), Arc::new(HashEmbeddingProvider::new(16)));
        let scope = scope_for("a", &["agent:a/memory"], RagWritePolicy::OwnMemoryOnly);
        store
            .upsert_scoped(
                &scope,
                vec![RagUpsert {
                    source_id: "agent:a/memory".into(),
                    id: "alpha#chunk-0".into(),
                    text: "alpha knowledge".into(),
                    metadata: serde_json::json!({}),
                }],
            )
            .await
            .unwrap();

        // Declaring prefix "alpha#chunk-" while writing a "beta" id would delete
        // alpha's chunks and write beta's under an undeclared prefix.
        let err = store
            .replace_scoped(
                &scope,
                "agent:a/memory",
                "alpha#chunk-",
                vec![RagUpsert {
                    source_id: "agent:a/memory".into(),
                    id: "beta#chunk-0".into(),
                    text: "beta knowledge".into(),
                    metadata: serde_json::json!({}),
                }],
            )
            .await
            .unwrap_err();
        assert!(err.contains("id_prefix"), "got: {err}");

        // alpha must be untouched.
        let hits = store
            .query_scoped(&scope, "alpha knowledge", 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "rejected replace must not delete data");
    }

    #[tokio::test]
    async fn file_store_replace_returns_docs_written_not_collection_size() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileRagStore::new(dir.path(), Arc::new(HashEmbeddingProvider::new(16)));
        let scope = scope_for("a", &["agent:a/memory"], RagWritePolicy::OwnMemoryOnly);

        // Three separate single-chunk documents in one collection.
        let mut total = 0usize;
        for index in 0..3 {
            let key = format!("doc-{index}");
            let written = store
                .replace_scoped(
                    &scope,
                    "agent:a/memory",
                    &format!("{key}#chunk-"),
                    vec![RagUpsert {
                        source_id: "agent:a/memory".into(),
                        id: format!("{key}#chunk-0"),
                        text: format!("knowledge number {index}"),
                        metadata: serde_json::json!({}),
                    }],
                )
                .await
                .unwrap();
            assert_eq!(written, 1, "each replace writes exactly one doc");
            total += written;
        }
        assert_eq!(total, 3);
    }

    #[tokio::test]
    async fn file_store_delete_scoped_enforces_policy() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileRagStore::new(dir.path(), Arc::new(HashEmbeddingProvider::new(16)));
        let writer = scope_for("a", &["agent:a/memory"], RagWritePolicy::OwnMemoryOnly);
        store
            .upsert_scoped(
                &writer,
                vec![RagUpsert {
                    source_id: "agent:a/memory".into(),
                    id: "1".into(),
                    text: "knowledge".into(),
                    metadata: serde_json::json!({}),
                }],
            )
            .await
            .unwrap();

        // An agent with no write rights must not be able to delete the collection.
        let bystander = scope_for("b", &["agent:a/memory"], RagWritePolicy::None);
        let err = store
            .delete_collection_scoped(&bystander, "agent:a/memory")
            .await
            .unwrap_err();
        assert!(err.contains("denied"), "got: {err}");

        // The owner's collection is still intact.
        let hits = store.query_scoped(&writer, "knowledge", 5).await.unwrap();
        assert_eq!(hits.len(), 1, "denied delete must not remove data");

        // The owner can delete it.
        store
            .delete_collection_scoped(&writer, "agent:a/memory")
            .await
            .unwrap();
        let hits = store.query_scoped(&writer, "knowledge", 5).await.unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn file_write_denied_outside_policy() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileRagStore::new(dir.path(), Arc::new(HashEmbeddingProvider::new(16)));
        let scope = scope_for("a", &["shared/docs"], RagWritePolicy::None);
        let err = store
            .upsert_scoped(
                &scope,
                vec![RagUpsert {
                    source_id: "shared/docs".into(),
                    id: "1".into(),
                    text: "nope".into(),
                    metadata: serde_json::json!({}),
                }],
            )
            .await
            .unwrap_err();
        assert!(err.contains("denied"));
    }
}
