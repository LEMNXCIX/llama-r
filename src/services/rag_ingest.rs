//! RAG ingest pipeline: chunk text/files → upsert via [`RagStore`].
//!
//! Uses only the port trait — never imports storage crates directly.

use crate::adapters::rag::chunker::{chunk_text, ChunkConfig};
use crate::adapters::rag::namespace::validate_source_id;
use crate::domain::scope::AgentScope;
use crate::error::AppError;
use crate::ports::rag::{RagStore, RagUpsert};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Max bytes read per file during ingest (512 KiB).
pub const MAX_FILE_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone)]
pub struct IngestDocument {
    /// Optional stable id prefix; defaults to `doc-{index}`.
    pub id_hint: Option<String>,
    pub text: String,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct IngestRequest {
    pub scope: AgentScope,
    pub source_id: String,
    pub documents: Vec<IngestDocument>,
    pub chunk: ChunkConfig,
}

#[derive(Debug, Clone)]
pub struct IngestResult {
    pub chunks_written: usize,
    pub files_read: usize,
    pub source_id: String,
}

pub struct RagIngestService {
    pub store: Arc<dyn RagStore>,
}

impl RagIngestService {
    pub fn new(store: Arc<dyn RagStore>) -> Self {
        Self { store }
    }

    pub async fn ingest(&self, req: IngestRequest) -> Result<IngestResult, AppError> {
        validate_source_id(&req.source_id).map_err(AppError::Validation)?;

        let mut upserts = Vec::new();
        for (doc_index, doc) in req.documents.iter().enumerate() {
            let key = doc
                .id_hint
                .clone()
                .unwrap_or_else(|| format!("doc-{doc_index}"));
            let chunks = chunk_text(&key, &doc.text, &req.chunk);
            for chunk in chunks {
                let mut metadata = doc.metadata.clone();
                if let Some(obj) = metadata.as_object_mut() {
                    obj.insert("chunk_index".into(), json!(chunk.index));
                    obj.insert("source_key".into(), json!(key));
                }
                upserts.push(RagUpsert {
                    source_id: req.source_id.clone(),
                    id: chunk.id,
                    text: chunk.text,
                    metadata,
                });
            }
        }

        let chunks_written = self
            .store
            .upsert_scoped(&req.scope, upserts)
            .await
            .map_err(|err| {
                if err.contains("denied") {
                    AppError::Validation(err)
                } else {
                    AppError::Runtime(err)
                }
            })?;

        Ok(IngestResult {
            chunks_written,
            files_read: 0,
            source_id: req.source_id,
        })
    }

    pub async fn ingest_project_context(
        &self,
        scope: &AgentScope,
        project_id: &str,
        context_md: &str,
        source_id: &str,
    ) -> Result<IngestResult, AppError> {
        self.ingest(IngestRequest {
            scope: scope.clone(),
            source_id: source_id.to_string(),
            documents: vec![IngestDocument {
                id_hint: Some(format!("project-{project_id}-context")),
                text: context_md.to_string(),
                metadata: json!({
                    "kind": "project_context",
                    "project_id": project_id,
                }),
            }],
            chunk: ChunkConfig::default(),
        })
        .await
    }

    pub async fn ingest_files(
        &self,
        scope: &AgentScope,
        source_id: &str,
        paths: &[PathBuf],
        base_allow: Option<&Path>,
    ) -> Result<IngestResult, AppError> {
        validate_source_id(source_id).map_err(AppError::Validation)?;

        let mut documents = Vec::new();
        let mut files_read = 0usize;

        for path in paths {
            if let Some(base) = base_allow {
                if !is_path_under_base(path, base) {
                    return Err(AppError::Validation(format!(
                        "file path '{}' is outside allowed base directory",
                        path.display()
                    )));
                }
            }

            let meta = std::fs::metadata(path).map_err(|err| {
                AppError::Validation(format!("cannot read '{}': {err}", path.display()))
            })?;
            if !meta.is_file() {
                continue;
            }
            if meta.len() > MAX_FILE_BYTES {
                tracing::warn!(
                    path = %path.display(),
                    size = meta.len(),
                    limit = MAX_FILE_BYTES,
                    "skipping oversized file during RAG ingest"
                );
                continue;
            }

            let bytes = std::fs::read(path).map_err(|err| {
                AppError::Validation(format!("cannot read '{}': {err}", path.display()))
            })?;
            if looks_binary(&bytes) {
                tracing::warn!(path = %path.display(), "skipping binary file during RAG ingest");
                continue;
            }

            let text = String::from_utf8_lossy(&bytes).into_owned();
            let hint = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_string();
            documents.push(IngestDocument {
                id_hint: Some(hint),
                text,
                metadata: json!({
                    "kind": "file",
                    "path": path.display().to_string(),
                }),
            });
            files_read += 1;
        }

        let mut result = self
            .ingest(IngestRequest {
                scope: scope.clone(),
                source_id: source_id.to_string(),
                documents,
                chunk: ChunkConfig::default(),
            })
            .await?;
        result.files_read = files_read;
        Ok(result)
    }
}

fn looks_binary(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    // NUL in first 8 KiB → treat as binary
    let sample = &bytes[..bytes.len().min(8192)];
    sample.contains(&0)
}

fn is_path_under_base(path: &Path, base: &Path) -> bool {
    let Ok(canon_base) = base.canonicalize() else {
        return false;
    };
    // If path does not exist yet, resolve parent + file name.
    let canon_path = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            let parent = path.parent().unwrap_or(path);
            let Ok(cp) = parent.canonicalize() else {
                return false;
            };
            cp.join(path.file_name().unwrap_or_default())
        }
    };
    canon_path.starts_with(&canon_base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::rag::store::{HashEmbeddingProvider, InMemoryRagStore};
    use crate::domain::agent::RagWritePolicy;
    use crate::domain::scope::ToolsAllow;
    use std::collections::HashSet;

    fn scope(agent: &str, sources: &[&str], write: RagWritePolicy) -> AgentScope {
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
    async fn ingest_then_query_returns_chunk() {
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
            HashEmbeddingProvider::new(16),
        )));
        let ingest = RagIngestService::new(store.clone());
        let sc = scope(
            "demo",
            &["agent:demo/memory"],
            RagWritePolicy::OwnMemoryOnly,
        );

        let result = ingest
            .ingest(IngestRequest {
                scope: sc.clone(),
                source_id: "agent:demo/memory".into(),
                documents: vec![IngestDocument {
                    id_hint: Some("policy".into()),
                    text: "Cancellation allows refunds within 24 hours of purchase.".into(),
                    metadata: json!({}),
                }],
                chunk: ChunkConfig::default(),
            })
            .await
            .unwrap();
        assert!(result.chunks_written >= 1);

        let hits = store
            .query_scoped(&sc, "refunds cancellation", 3)
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].text.to_lowercase().contains("refund"));
    }

    #[tokio::test]
    async fn ingest_write_denied() {
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
            HashEmbeddingProvider::new(16),
        )));
        let ingest = RagIngestService::new(store);
        let sc = scope("demo", &["agent:demo/memory"], RagWritePolicy::None);
        let err = ingest
            .ingest(IngestRequest {
                scope: sc,
                source_id: "agent:demo/memory".into(),
                documents: vec![IngestDocument {
                    id_hint: None,
                    text: "secret".into(),
                    metadata: json!({}),
                }],
                chunk: ChunkConfig::default(),
            })
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(msg.contains("denied")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }
}
