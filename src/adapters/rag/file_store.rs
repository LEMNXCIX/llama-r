//! File-backed RAG store with full scope enforcement.
//!
//! Phase 4 plan B: JSONL persistence per collection under
//! `{base}/data/lancedb/<encoded_source_id>/docs.jsonl`.
//!
//! Same port contract as [`InMemoryRagStore`]; LanceDB can replace this adapter later
//! without changing services. Uses cosine similarity over stored embeddings.

use super::namespace::{encode_source_id_for_path, validate_source_id};
use crate::domain::scope::AgentScope;
use crate::ports::rag::{
    write_denied, EmbeddingProvider, RagChunk, RagReplaceBatch, RagStore, RagUpsert,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

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
    ///
    /// Values are `Arc` so a reader can take a cheap snapshot and release the
    /// lock, letting a large rewrite run without blocking queries.
    cache: RwLock<HashMap<String, Arc<Vec<StoredDoc>>>>,
    /// One exclusive lock per collection, held across snapshot → write → publish.
    ///
    /// Without it, two concurrent writers each snapshot the same state and the
    /// last publish silently discards the other's write. Holding a `tokio::sync`
    /// lock across `spawn_blocking` does not block the runtime, so this
    /// serializes writers without reintroducing the blocking-I/O problem.
    /// Writers to *different* collections stay fully parallel.
    collection_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
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
            collection_locks: Mutex::new(HashMap::new()),
        }
    }

    /// The exclusive lock guarding writes to `source_id`.
    async fn collection_lock(&self, source_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.collection_locks.lock().await;
        locks
            .entry(source_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn collection_dir(&self, source_id: &str) -> PathBuf {
        self.base_dir.join(encode_source_id_for_path(source_id))
    }

    fn docs_path(dir: &Path) -> PathBuf {
        dir.join("docs.jsonl")
    }

    async fn ensure_loaded(&self, source_id: &str) -> Result<(), String> {
        {
            let cache = self.cache.read().await;
            if cache.contains_key(source_id) {
                return Ok(());
            }
        }
        let path = Self::docs_path(&self.collection_dir(source_id));
        let docs = load_from_disk_blocking(path).await?;
        let mut cache = self.cache.write().await;
        cache
            .entry(source_id.to_string())
            .or_insert_with(|| Arc::new(docs));
        Ok(())
    }
}

/// Blocking disk read, offloaded to the blocking pool.
///
/// Reading a large collection synchronously inside an async fn would stall the
/// runtime, and with it every other task on that worker thread.
async fn load_from_disk_blocking(path: PathBuf) -> Result<Vec<StoredDoc>, String> {
    tokio::task::spawn_blocking(move || load_from_disk(&path))
        .await
        .map_err(|err| format!("rag read task failed: {err}"))?
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

/// Apply `edits` to a snapshot of `current` and persist the result.
///
/// The clone, the prefix filtering and the serialization all happen on the
/// blocking pool: doing them on the async thread would block the runtime for as
/// long as the collection takes to rewrite. Returns the new collection so the
/// caller can publish it once the write succeeded.
async fn apply_and_write(
    path: PathBuf,
    current: Arc<Vec<StoredDoc>>,
    edits: Vec<(String, Vec<RagUpsert>, Vec<Vec<f32>>)>,
) -> Result<Vec<StoredDoc>, String> {
    tokio::task::spawn_blocking(move || {
        let mut updated: Vec<StoredDoc> = (*current).clone();
        for (id_prefix, docs, vectors) in edits {
            // Drop chunks from a previous, longer version of this same document.
            updated.retain(|item| !item.id.starts_with(id_prefix.as_str()));
            for (doc, embedding) in docs.into_iter().zip(vectors.into_iter()) {
                updated.push(StoredDoc {
                    id: doc.id,
                    text: doc.text,
                    embedding,
                    metadata: doc.metadata,
                });
            }
        }
        write_to_disk(&path, &updated)?;
        Ok(updated)
    })
    .await
    .map_err(|err| format!("rag write task failed: {err}"))?
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
    // Atomic-ish write via a temp file in the same directory. The name is unique
    // per write: a shared temp path would let two writers (or two processes)
    // clobber each other's bytes and publish the wrong content.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = path.with_extension(format!("{unique}.tmp"));
    std::fs::write(&tmp, &body).map_err(|err| format!("rag write '{}': {err}", tmp.display()))?;
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("rag rename to '{}': {err}", path.display()));
    }
    Ok(())
}

/// Cosine similarity, 0.0 when the vectors are empty or of different length.
///
/// Shared with the skill index so both rank with identical semantics. A length
/// mismatch scoring 0.0 is why embedding dimensions must be validated on write.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
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
            // Cheap Arc snapshot: the read lock is released before scoring, so a
            // concurrent rewrite never blocks the query.
            let docs = {
                let cache = self.cache.read().await;
                match cache.get(source_id) {
                    Some(docs) => docs.clone(),
                    None => continue,
                }
            };
            for doc in docs.iter() {
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
                return Err(write_denied(&doc.source_id, &scope.agent_id));
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
            written += items.len();
            let collection_lock = self.collection_lock(&source_id).await;
            let _guard = collection_lock.lock().await;
            self.ensure_loaded(&source_id).await?;
            let current = {
                let cache = self.cache.read().await;
                cache
                    .get(&source_id)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(Vec::new()))
            };

            // Merge-and-write on the blocking pool.
            let (docs, vectors): (Vec<RagUpsert>, Vec<Vec<f32>>) = items.into_iter().unzip();
            let merge = |mut existing: Vec<StoredDoc>| -> Result<Vec<StoredDoc>, String> {
                for (doc, embedding) in docs.into_iter().zip(vectors) {
                    if let Some(slot) = existing.iter_mut().find(|item| item.id == doc.id) {
                        slot.text = doc.text;
                        slot.embedding = embedding;
                        slot.metadata = doc.metadata;
                    } else {
                        existing.push(StoredDoc {
                            id: doc.id,
                            text: doc.text,
                            embedding,
                            metadata: doc.metadata,
                        });
                    }
                }
                Ok(existing)
            };

            let path = Self::docs_path(&self.collection_dir(&source_id));
            let updated = tokio::task::spawn_blocking(move || -> Result<Vec<StoredDoc>, String> {
                let merged = merge((*current).clone())?;
                write_to_disk(&path, &merged)?;
                Ok(merged)
            })
            .await
            .map_err(|err| format!("rag write task failed: {err}"))??;

            self.cache
                .write()
                .await
                .insert(source_id, Arc::new(updated));
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
            return Err(write_denied(source_id, &scope.agent_id));
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

        // Serialize writers to this collection so a concurrent write cannot be
        // discarded by a stale snapshot.
        let collection_lock = self.collection_lock(source_id).await;
        let _guard = collection_lock.lock().await;

        self.ensure_loaded(source_id).await?;
        let current = self
            .cache
            .read()
            .await
            .get(source_id)
            .cloned()
            .unwrap_or_else(|| Arc::new(Vec::new()));

        // Pair each document with its vector, grouped by the prefix being replaced.
        let mut edits: Vec<(String, Vec<RagUpsert>, Vec<Vec<f32>>)> = Vec::new();
        let mut cursor = 0usize;
        for (id_prefix, docs) in batch {
            let mut group_docs = Vec::with_capacity(docs.len());
            let mut group_vectors = Vec::with_capacity(docs.len());
            for doc in docs {
                group_docs.push(doc);
                group_vectors.push(vectors[cursor].clone());
                cursor += 1;
            }
            edits.push((id_prefix, group_docs, group_vectors));
        }

        // The snapshot keeps readers consistent while the rewrite runs, and the
        // cache is only updated once the write is durable.
        let path = Self::docs_path(&self.collection_dir(source_id));
        let updated = apply_and_write(path, current, edits).await?;

        let written = all_docs.len();
        self.cache
            .write()
            .await
            .insert(source_id.to_string(), Arc::new(updated));
        Ok(written)
    }

    async fn delete_document_scoped(
        &self,
        scope: &AgentScope,
        source_id: &str,
        id_prefix: &str,
    ) -> Result<usize, String> {
        if id_prefix.is_empty() {
            return Err("delete_document_scoped requires a non-empty id_prefix".into());
        }
        validate_source_id(source_id)?;
        if !scope.allows_rag_write(source_id) {
            return Err(write_denied(source_id, &scope.agent_id));
        }

        let collection_lock = self.collection_lock(source_id).await;
        let _guard = collection_lock.lock().await;

        let dir = self.collection_dir(source_id);
        // Nothing on disk to retract: do not load or create anything, so a
        // typo'd source_id cannot materialize a phantom empty collection.
        if !dir.exists() {
            return Ok(0);
        }

        self.ensure_loaded(source_id).await?;
        let current = {
            let cache = self.cache.read().await;
            cache
                .get(source_id)
                .cloned()
                .unwrap_or_else(|| Arc::new(Vec::new()))
        };
        // Nothing matched: avoid rewriting the file for no reason.
        if !current.iter().any(|item| item.id.starts_with(id_prefix)) {
            return Ok(0);
        }

        let path = Self::docs_path(&dir);
        let id_prefix = id_prefix.to_string();
        // Filter and persist on the blocking pool, then publish.
        let (updated, removed) = tokio::task::spawn_blocking(move || {
            let updated: Vec<StoredDoc> = (*current)
                .iter()
                .filter(|item| !item.id.starts_with(id_prefix.as_str()))
                .cloned()
                .collect();
            let removed = current.len() - updated.len();
            if removed > 0 {
                write_to_disk(&path, &updated)?;
            }
            Ok::<_, String>((updated, removed))
        })
        .await
        .map_err(|err| format!("rag write task failed: {err}"))??;

        if removed > 0 {
            self.cache
                .write()
                .await
                .insert(source_id.to_string(), Arc::new(updated));
        }
        Ok(removed)
    }

    async fn delete_collection_scoped(
        &self,
        scope: &AgentScope,
        source_id: &str,
    ) -> Result<(), String> {
        if !scope.allows_rag_write(source_id) {
            return Err(write_denied(source_id, &scope.agent_id));
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

    #[tokio::test(flavor = "current_thread")]
    async fn disk_writes_do_not_block_the_async_runtime() {
        // On a current-thread runtime every task shares one thread, so any
        // blocking std::fs work inside an async fn stalls the whole runtime,
        // including timers. With the write moved to spawn_blocking, the timer
        // keeps firing while a large collection is serialized.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileRagStore::new(
            dir.path(),
            Arc::new(HashEmbeddingProvider::new(64)),
        ));
        let scope = scope_for("a", &["agent:a/memory"], RagWritePolicy::OwnMemoryOnly);

        // Pre-load a large collection. This happens outside the measured window,
        // so the only remaining work is the rewrite the replace triggers.
        {
            let docs: Vec<RagUpsert> = (0..30_000)
                .map(|index| RagUpsert {
                    source_id: "agent:a/memory".into(),
                    id: format!("doc-{index}#chunk-0"),
                    text: format!("chunk {index} with some filler text to serialize"),
                    metadata: serde_json::json!({"k": index}),
                })
                .collect();
            store
                .replace_batch_scoped(&scope, "agent:a/memory", vec![("doc-".to_string(), docs)])
                .await
                .expect("preload");
        }

        // Replacing one document rewrites the whole collection to disk.
        let replacement = vec![RagUpsert {
            source_id: "agent:a/memory".into(),
            id: "fresh#chunk-0".into(),
            text: "one small replacement document".into(),
            metadata: serde_json::json!({}),
        }];
        let write = tokio::spawn({
            let store = store.clone();
            async move {
                store
                    .replace_batch_scoped(
                        &scope,
                        "agent:a/memory",
                        vec![("fresh#chunk-".to_string(), replacement)],
                    )
                    .await
            }
        });

        // A timer that must keep ticking while the write is in flight.
        let mut ticks = 0u32;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(250);
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            ticks += 1;
        }

        write.await.unwrap().expect("write should succeed");
        assert!(
            ticks >= 5,
            "runtime was starved by blocking disk I/O: only {ticks} timer ticks in 250ms"
        );
    }

    #[tokio::test]
    async fn concurrent_writers_do_not_lose_updates() {
        // The history summarizer upserts on every agent turn, so two concurrent
        // writes to the same collection are normal. Both documents must survive.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileRagStore::new(
            dir.path(),
            Arc::new(HashEmbeddingProvider::new(16)),
        ));
        let scope = scope_for("a", &["agent:a/memory"], RagWritePolicy::OwnMemoryOnly);

        // Seed the collection so a lost update is detectable.
        store
            .upsert_scoped(
                &scope,
                vec![RagUpsert {
                    source_id: "agent:a/memory".into(),
                    id: "seed#chunk-0".into(),
                    text: "seed document".into(),
                    metadata: serde_json::json!({}),
                }],
            )
            .await
            .unwrap();

        for trial in 0..25 {
            let a = {
                let store = store.clone();
                let scope = scope.clone();
                tokio::spawn(async move {
                    store
                        .upsert_scoped(
                            &scope,
                            vec![RagUpsert {
                                source_id: "agent:a/memory".into(),
                                id: "d0#chunk-0".into(),
                                text: "first concurrent document".into(),
                                metadata: serde_json::json!({}),
                            }],
                        )
                        .await
                })
            };
            let b = {
                let store = store.clone();
                let scope = scope.clone();
                tokio::spawn(async move {
                    store
                        .upsert_scoped(
                            &scope,
                            vec![RagUpsert {
                                source_id: "agent:a/memory".into(),
                                id: "d1#chunk-0".into(),
                                text: "second concurrent document".into(),
                                metadata: serde_json::json!({}),
                            }],
                        )
                        .await
                })
            };
            a.await.unwrap().expect("writer a");
            b.await.unwrap().expect("writer b");

            let ids: HashSet<String> = store
                .query_scoped(&scope, "document", 50)
                .await
                .unwrap()
                .into_iter()
                .map(|hit| hit.id)
                .collect();
            assert!(
                ids.contains("d0#chunk-0") && ids.contains("d1#chunk-0"),
                "trial {trial}: concurrent writes lost an update, have {:?}",
                ids.iter().collect::<Vec<_>>()
            );
        }
    }

    #[tokio::test]
    async fn file_store_delete_document_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let emb: Arc<dyn EmbeddingProvider> = Arc::new(HashEmbeddingProvider::new(16));
        let scope = scope_for("a", &["agent:a/memory"], RagWritePolicy::OwnMemoryOnly);

        {
            let store = FileRagStore::new(dir.path(), emb.clone());
            store
                .upsert_scoped(
                    &scope,
                    vec![
                        RagUpsert {
                            source_id: "agent:a/memory".into(),
                            id: "keep#chunk-0".into(),
                            text: "knowledge that must survive".into(),
                            metadata: serde_json::json!({}),
                        },
                        RagUpsert {
                            source_id: "agent:a/memory".into(),
                            id: "obsolete#chunk-0".into(),
                            text: "knowledge that must be retracted".into(),
                            metadata: serde_json::json!({}),
                        },
                    ],
                )
                .await
                .unwrap();

            let removed = store
                .delete_document_scoped(&scope, "agent:a/memory", "obsolete#chunk-")
                .await
                .unwrap();
            assert_eq!(removed, 1);
        }

        // A fresh instance must see the retraction persisted, not resurrect it.
        let store = FileRagStore::new(dir.path(), emb);
        let hits = store.query_scoped(&scope, "knowledge", 10).await.unwrap();
        assert_eq!(hits.len(), 1, "retraction must survive a reload");
        assert!(hits[0].id.contains("keep"));
    }

    #[tokio::test]
    async fn file_store_delete_document_noop_does_not_create_collection() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileRagStore::new(dir.path(), Arc::new(HashEmbeddingProvider::new(16)));
        // The collection is the agent's own memory, but nothing was ever written
        // to it, so a retract must be a no-op rather than create it.
        let scope = scope_for("a", &["agent:a/memory"], RagWritePolicy::OwnMemoryOnly);

        let removed = store
            .delete_document_scoped(&scope, "agent:a/memory", "anything#chunk-")
            .await
            .unwrap();
        assert_eq!(removed, 0);
        assert!(
            !store.collection_dir("agent:a/memory").exists(),
            "a no-op delete must not create the collection directory"
        );
    }

    #[tokio::test]
    async fn file_store_delete_document_respects_write_policy() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileRagStore::new(dir.path(), Arc::new(HashEmbeddingProvider::new(16)));
        let scope = scope_for("a", &["agent:a/memory"], RagWritePolicy::None);
        let err = store
            .delete_document_scoped(&scope, "agent:a/memory", "doc#chunk-")
            .await
            .unwrap_err();
        assert!(
            err == write_denied("agent:a/memory", "a"),
            "denial must carry the shared prefix, got: {err}"
        );
    }

    #[tokio::test]
    async fn file_store_delete_document_rejects_empty_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileRagStore::new(dir.path(), Arc::new(HashEmbeddingProvider::new(16)));
        let scope = scope_for("a", &["agent:a/memory"], RagWritePolicy::OwnMemoryOnly);
        store
            .upsert_scoped(
                &scope,
                vec![RagUpsert {
                    source_id: "agent:a/memory".into(),
                    id: "doc#chunk-0".into(),
                    text: "must not be wiped".into(),
                    metadata: serde_json::json!({}),
                }],
            )
            .await
            .unwrap();

        let err = store
            .delete_document_scoped(&scope, "agent:a/memory", "")
            .await
            .unwrap_err();
        assert!(err.contains("non-empty"), "got: {err}");
        let hits = store.query_scoped(&scope, "must not", 5).await.unwrap();
        assert_eq!(hits.len(), 1, "collection must be untouched");
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
