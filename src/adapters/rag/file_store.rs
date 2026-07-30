//! File-backed RAG store with full scope enforcement.
//!
//! Phase 4 plan B: JSONL persistence per collection under
//! `{base}/data/lancedb/<encoded_source_id>/docs.jsonl`.
//!
//! Same port contract as [`InMemoryRagStore`]; LanceDB can replace this adapter later
//! without changing services. Uses cosine similarity over stored embeddings.

use super::namespace::{encode_source_id_for_path, validate_source_id};
use crate::domain::scope::AgentScope;
use crate::ports::rag::{EmbeddingProvider, RagChunk, RagStore, RagUpsert};
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
