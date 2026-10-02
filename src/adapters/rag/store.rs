//! In-memory RAG store with full scope enforcement.
//!
//! Used as a drop-in until LanceDB is wired (see ARCHITECTURE_PLAN §6.7).
//! Same port contract → swapping the adapter does not change services.

use crate::domain::scope::AgentScope;
use crate::ports::rag::{EmbeddingProvider, RagChunk, RagReplaceBatch, RagStore, RagUpsert};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
struct StoredDoc {
    id: String,
    text: String,
    embedding: Vec<f32>,
    metadata: serde_json::Value,
}

/// Segmented store: `source_id` → documents. Never crosses agent scopes.
pub struct InMemoryRagStore {
    embeddings: Arc<dyn EmbeddingProvider>,
    collections: RwLock<HashMap<String, Vec<StoredDoc>>>,
}

impl InMemoryRagStore {
    pub fn new(embeddings: Arc<dyn EmbeddingProvider>) -> Self {
        Self {
            embeddings,
            collections: RwLock::new(HashMap::new()),
        }
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
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
impl RagStore for InMemoryRagStore {
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

        let collections = self.collections.read().await;
        let mut hits = Vec::new();

        for source_id in &scope.rag_sources {
            if !scope.allows_rag_read(source_id) {
                return Err(format!(
                    "RAG read denied for source '{source_id}' on agent '{}'",
                    scope.agent_id
                ));
            }
            let Some(docs) = collections.get(source_id) else {
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

        // Deny before embed (defense in depth + saves CPU).
        for doc in &docs {
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

        let mut collections = self.collections.write().await;
        let mut written = 0usize;

        for (doc, embedding) in docs.into_iter().zip(vectors.into_iter()) {
            if !scope.allows_rag_write(&doc.source_id) {
                return Err(format!(
                    "RAG write denied for source '{}' on agent '{}'",
                    doc.source_id, scope.agent_id
                ));
            }
            let entry = collections.entry(doc.source_id).or_default();
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

        // Embed everything before touching the collection, so a provider failure
        // on any document leaves the collection untouched.
        let all_docs: Vec<RagUpsert> = batch
            .iter()
            .flat_map(|(_, docs)| docs.iter().cloned())
            .collect();
        let texts: Vec<String> = all_docs.iter().map(|doc| doc.text.clone()).collect();
        let vectors = self.embeddings.embed(&texts).await?;
        if vectors.len() != all_docs.len() {
            return Err("embedding count mismatch".to_string());
        }

        let mut collections = self.collections.write().await;
        let entry = collections.entry(source_id.to_string()).or_default();
        let written = all_docs.len();

        let mut cursor = 0usize;
        for (id_prefix, docs) in batch {
            // Drop chunks from a previous, longer version of this same document.
            entry.retain(|item| !item.id.starts_with(id_prefix.as_str()));
            for doc in docs {
                let embedding = vectors[cursor].clone();
                cursor += 1;
                entry.push(StoredDoc {
                    id: doc.id,
                    text: doc.text,
                    embedding,
                    metadata: doc.metadata,
                });
            }
        }

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
        self.collections.write().await.remove(source_id);
        Ok(())
    }

    async fn delete_collection(&self, source_id: &str) -> Result<(), String> {
        self.collections.write().await.remove(source_id);
        Ok(())
    }
}

/// Deterministic fake embeddings for unit tests (no network).
pub struct HashEmbeddingProvider {
    dimensions: usize,
}

impl HashEmbeddingProvider {
    pub fn new(dimensions: usize) -> Self {
        Self {
            dimensions: dimensions.max(8),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for HashEmbeddingProvider {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts
            .iter()
            .map(|text| {
                let mut vec = vec![0.0f32; self.dimensions];
                for (index, byte) in text.bytes().enumerate() {
                    vec[index % self.dimensions] += (byte as f32) / 255.0;
                }
                // L2 normalize
                let norm = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for value in &mut vec {
                        *value /= norm;
                    }
                }
                vec
            })
            .collect())
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    async fn isolation_prevents_cross_agent_reads() {
        let store = InMemoryRagStore::new(Arc::new(HashEmbeddingProvider::new(16)));
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
    async fn write_denied_outside_policy() {
        let store = InMemoryRagStore::new(Arc::new(HashEmbeddingProvider::new(16)));
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

    #[tokio::test]
    async fn listed_write_policy_allows_rag_sources_only() {
        let store = InMemoryRagStore::new(Arc::new(HashEmbeddingProvider::new(16)));
        let scope = scope_for(
            "ops",
            &["fudi/policies", "other/docs"],
            RagWritePolicy::Listed,
        );
        store
            .upsert_scoped(
                &scope,
                vec![RagUpsert {
                    source_id: "fudi/policies".into(),
                    id: "p1".into(),
                    text: "policy text".into(),
                    metadata: serde_json::json!({}),
                }],
            )
            .await
            .unwrap();

        let err = store
            .upsert_scoped(
                &scope,
                vec![RagUpsert {
                    source_id: "secret/other".into(),
                    id: "x".into(),
                    text: "nope".into(),
                    metadata: serde_json::json!({}),
                }],
            )
            .await
            .unwrap_err();
        assert!(err.contains("denied"));
    }

    #[tokio::test]
    async fn own_memory_write_matches_scope_helper() {
        let store = InMemoryRagStore::new(Arc::new(HashEmbeddingProvider::new(16)));
        let mut scope = scope_for(
            "ops",
            &["agent:fudi/ops/memory"],
            RagWritePolicy::OwnMemoryOnly,
        );
        scope.project_id = Some("fudi".into());
        let own = scope.own_memory_source_id();
        assert_eq!(own, "agent:fudi/ops/memory");
        store
            .upsert_scoped(
                &scope,
                vec![RagUpsert {
                    source_id: own,
                    id: "m1".into(),
                    text: "memory note".into(),
                    metadata: serde_json::json!({}),
                }],
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn query_does_not_return_other_collection() {
        let store = InMemoryRagStore::new(Arc::new(HashEmbeddingProvider::new(16)));
        let writer = scope_for("a", &["col/a", "col/b"], RagWritePolicy::Listed);
        store
            .upsert_scoped(
                &writer,
                vec![
                    RagUpsert {
                        source_id: "col/a".into(),
                        id: "1".into(),
                        text: "alpha only content unique-aaa".into(),
                        metadata: serde_json::json!({}),
                    },
                    RagUpsert {
                        source_id: "col/b".into(),
                        id: "2".into(),
                        text: "beta only content unique-bbb".into(),
                        metadata: serde_json::json!({}),
                    },
                ],
            )
            .await
            .unwrap();

        let reader = scope_for("a", &["col/a"], RagWritePolicy::None);
        let hits = store
            .query_scoped(&reader, "unique-bbb unique-aaa", 10)
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.source_id == "col/a"));
        assert!(hits.iter().all(|h| !h.text.contains("unique-bbb")));
    }
}
