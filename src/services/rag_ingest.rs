//! RAG ingest pipeline: chunk text/files → upsert via [`RagStore`].
//!
//! Uses only the port trait — never imports storage crates directly.

use crate::adapters::rag::chunker::{chunk_text, ChunkConfig};
use crate::adapters::rag::namespace::validate_source_id;
use crate::domain::scope::AgentScope;
use crate::error::AppError;
use crate::ports::rag::{RagReplaceBatch, RagStore, RagUpsert};
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
    /// Requested paths that were deliberately not indexed (binary, oversized,
    /// or not a regular file), with the reason. Empty when nothing was skipped.
    pub skipped: Vec<SkippedFile>,
}

/// A requested file that was not indexed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFile {
    pub path: String,
    pub reason: String,
}

/// Separator between a document key and a chunk index inside a chunk id.
///
/// Reserved: a document key may not contain it, which keeps
/// [`chunk_id_prefix`] unambiguous.
pub const CHUNK_ID_SEPARATOR: &str = "#chunk-";

/// The id prefix owned by every chunk of `doc_key`.
pub fn chunk_id_prefix(doc_key: &str) -> String {
    format!("{doc_key}{CHUNK_ID_SEPARATOR}")
}

/// Reject a document key that would make prefix matching ambiguous, or that could
/// not address a stored chunk.
fn validate_doc_key(key: &str) -> Result<(), AppError> {
    if key.trim().is_empty() {
        return Err(AppError::Validation("empty document key".into()));
    }
    if key.contains(CHUNK_ID_SEPARATOR) || key.contains('#') {
        return Err(AppError::Validation(format!(
            "document key '{key}' contains the reserved character '#'"
        )));
    }
    Ok(())
}

/// Map a store error to the right `AppError` variant: policy denials are the
/// caller's fault, everything else is a runtime fault.
///
/// Matches the store's stable `RAG write denied` prefix rather than the bare word
/// "denied", so an I/O fault such as "Permission denied (os error 13)" stays a
/// runtime error instead of being reported to the client as a bad request.
fn map_store_err(err: String) -> AppError {
    if err.starts_with(RAG_WRITE_DENIED) {
        AppError::Validation(err)
    } else {
        AppError::Runtime(err)
    }
}

/// Prefix every store uses for a scope write-policy denial.
pub use crate::ports::rag::RAG_WRITE_DENIED;

pub struct RagIngestService {
    pub store: Arc<dyn RagStore>,
}

impl RagIngestService {
    pub fn new(store: Arc<dyn RagStore>) -> Self {
        Self { store }
    }

    pub async fn ingest(&self, req: IngestRequest) -> Result<IngestResult, AppError> {
        validate_source_id(&req.source_id).map_err(AppError::Validation)?;

        // Resolve and validate every document key up front, before any mutation.
        // Two documents sharing a key would make the second replace delete the
        // first's chunks, so that is rejected rather than silently applied.
        let mut keys: Vec<String> = Vec::with_capacity(req.documents.len());
        for (doc_index, doc) in req.documents.iter().enumerate() {
            let key = doc
                .id_hint
                .clone()
                .unwrap_or_else(|| format!("doc-{doc_index}"));
            validate_doc_key(&key)?;
            if keys.contains(&key) {
                return Err(AppError::Validation(format!(
                    "duplicate document key '{key}' in one ingest request"
                )));
            }
            keys.push(key);
        }

        // Build every upsert before touching the store, so a bad document cannot
        // leave the collection half-written.
        let mut skipped = Vec::new();
        let mut plan: Vec<(String, Vec<RagUpsert>)> = Vec::new();
        for (key, doc) in keys.iter().zip(req.documents.iter()) {
            let chunks = chunk_text(key, &doc.text, &req.chunk);
            if chunks.is_empty() {
                // An empty/whitespace document is not a deletion: replacing here
                // would wipe knowledge indexed from a previous, non-empty version
                // (e.g. a file truncated mid-write).
                skipped.push(SkippedFile {
                    path: key.clone(),
                    reason: "document produced no chunks (empty or whitespace)".into(),
                });
                continue;
            }
            let upserts = chunks
                .into_iter()
                .map(|chunk| {
                    let metadata = decorate_metadata(doc.metadata.clone(), key, chunk.index);
                    RagUpsert {
                        source_id: req.source_id.clone(),
                        id: chunk.id,
                        text: chunk.text,
                        metadata,
                    }
                })
                .collect();
            plan.push((key.clone(), upserts));
        }

        // Replace (not upsert) so chunks from a previous, longer version of a document
        // are dropped instead of lingering retrievable forever. Batched into a
        // single call so an embed or disk failure on any document leaves the whole
        // collection untouched instead of committing the earlier ones.
        let batch: RagReplaceBatch = plan
            .into_iter()
            .map(|(key, upserts)| (chunk_id_prefix(&key), upserts))
            .collect();

        let chunks_written = if batch.is_empty() {
            0
        } else {
            self.store
                .replace_batch_scoped(&req.scope, &req.source_id, batch)
                .await
                .map_err(map_store_err)?
        };

        Ok(IngestResult {
            chunks_written,
            files_read: 0,
            source_id: req.source_id,
            skipped,
        })
    }

    /// Remove every chunk belonging to one document.
    ///
    /// This is the explicit way to retract a document. It is deliberately *not*
    /// the same as ingesting an empty document: an empty document is treated as
    /// "nothing to index" (a file truncated mid-write must not wipe good data),
    /// whereas this call is an intentional deletion and goes through the store's
    /// scoped delete.
    ///
    /// Returns the number of chunks removed.
    pub async fn delete_document(
        &self,
        scope: &AgentScope,
        source_id: &str,
        doc_key: &str,
    ) -> Result<usize, AppError> {
        validate_source_id(source_id).map_err(AppError::Validation)?;
        validate_doc_key(doc_key)?;

        let removed = self
            .store
            .delete_document_scoped(scope, source_id, &chunk_id_prefix(doc_key))
            .await
            .map_err(map_store_err)?;
        Ok(removed)
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
        let mut skipped = Vec::new();

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
                skipped.push(SkippedFile {
                    path: path.display().to_string(),
                    reason: "not a regular file".into(),
                });
                continue;
            }
            if meta.len() > MAX_FILE_BYTES {
                tracing::warn!(
                    path = %path.display(),
                    size = meta.len(),
                    limit = MAX_FILE_BYTES,
                    "skipping oversized file during RAG ingest"
                );
                skipped.push(SkippedFile {
                    path: path.display().to_string(),
                    reason: format!("exceeds size limit of {MAX_FILE_BYTES} bytes"),
                });
                continue;
            }

            let bytes = std::fs::read(path).map_err(|err| {
                AppError::Validation(format!("cannot read '{}': {err}", path.display()))
            })?;
            if looks_binary(&bytes) {
                tracing::warn!(path = %path.display(), "skipping binary file during RAG ingest");
                skipped.push(SkippedFile {
                    path: path.display().to_string(),
                    reason: "binary content".into(),
                });
                continue;
            }

            let text = String::from_utf8_lossy(&bytes).into_owned();
            let hint = file_id_hint(path, base_allow);
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
        // Preserve the documents `ingest` skipped (e.g. empty content) alongside
        // the files skipped here, rather than overwriting them.
        skipped.append(&mut result.skipped);
        result.skipped = skipped;
        Ok(result)
    }
}

/// Attach chunk provenance to a document's metadata.
///
/// Object metadata is extended in place. Non-object metadata (e.g. a bare string
/// from an API caller) is preserved under `original` rather than being silently
/// dropped, so `chunk_index` / `source_key` are always present.
fn decorate_metadata(
    metadata: serde_json::Value,
    source_key: &str,
    chunk_index: usize,
) -> serde_json::Value {
    let mut obj = match metadata {
        serde_json::Value::Object(map) => map,
        serde_json::Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("original".into(), other);
            map
        }
    };
    obj.insert("chunk_index".into(), json!(chunk_index));
    obj.insert("source_key".into(), json!(source_key));
    serde_json::Value::Object(obj)
}

/// Stable, collision-resistant chunk-id prefix for an ingested file.
///
/// The path relative to `base_allow` is kept verbatim (only the `#` character is
/// escaped, since it is reserved for chunk ids). Flattening separators was tried
/// and rejected: it is not injective — `a/b.md` and `a_b.md` both became
/// `a_b.md`, so re-ingesting one silently deleted the other's chunks.
fn file_id_hint(path: &Path, base_allow: Option<&Path>) -> String {
    let relative = match base_allow {
        Some(base) => path.strip_prefix(base).unwrap_or(path),
        None => path,
    };
    let escaped: String = relative
        .to_string_lossy()
        .replace('#', "%23")
        .replace('\\', "/");
    if escaped.trim().is_empty() {
        "file".to_string()
    } else {
        escaped
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
    async fn reingest_shrunk_document_drops_stale_chunks() {
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
            HashEmbeddingProvider::new(16),
        )));
        let ingest = RagIngestService::new(store.clone());
        let sc = scope(
            "demo",
            &["agent:demo/memory"],
            RagWritePolicy::OwnMemoryOnly,
        );

        // First ingest: a long document that produces several chunks.
        let long_text = "Refund policy detail sentence. ".repeat(120);
        let first = ingest
            .ingest(IngestRequest {
                scope: sc.clone(),
                source_id: "agent:demo/memory".into(),
                documents: vec![IngestDocument {
                    id_hint: Some("policy".into()),
                    text: long_text,
                    metadata: json!({}),
                }],
                chunk: ChunkConfig {
                    max_chars: 200,
                    overlap_chars: 20,
                },
            })
            .await
            .unwrap();
        assert!(first.chunks_written >= 4, "expected multiple chunks");

        // Second ingest: the same document, much shorter.
        let second = ingest
            .ingest(IngestRequest {
                scope: sc.clone(),
                source_id: "agent:demo/memory".into(),
                documents: vec![IngestDocument {
                    id_hint: Some("policy".into()),
                    text: "Short updated policy.".into(),
                    metadata: json!({}),
                }],
                chunk: ChunkConfig {
                    max_chars: 200,
                    overlap_chars: 20,
                },
            })
            .await
            .unwrap();
        assert_eq!(second.chunks_written, 1);

        // The high-index chunks from the first ingest must be gone, not still retrievable.
        let hits = store
            .query_scoped(&sc, "refund policy detail sentence", 50)
            .await
            .unwrap();
        for hit in &hits {
            assert!(
                !hit.text.contains("Refund policy detail sentence."),
                "stale chunk survived re-ingest: id={} text={}",
                hit.id,
                hit.text
            );
        }
    }

    #[tokio::test]
    async fn empty_document_does_not_delete_existing_content() {
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
            HashEmbeddingProvider::new(16),
        )));
        let ingest = RagIngestService::new(store.clone());
        let sc = scope(
            "demo",
            &["agent:demo/memory"],
            RagWritePolicy::OwnMemoryOnly,
        );

        // Index a real document.
        ingest
            .ingest(IngestRequest {
                scope: sc.clone(),
                source_id: "agent:demo/memory".into(),
                documents: vec![IngestDocument {
                    id_hint: Some("policy".into()),
                    text: "Refunds are allowed within 24 hours.".into(),
                    metadata: json!({}),
                }],
                chunk: ChunkConfig::default(),
            })
            .await
            .unwrap();
        assert_eq!(
            store.query_scoped(&sc, "refunds", 10).await.unwrap().len(),
            1
        );

        // A truncated / whitespace-only file must NOT wipe what is already indexed.
        let result = ingest
            .ingest(IngestRequest {
                scope: sc.clone(),
                source_id: "agent:demo/memory".into(),
                documents: vec![IngestDocument {
                    id_hint: Some("policy".into()),
                    text: "   \n  ".into(),
                    metadata: json!({}),
                }],
                chunk: ChunkConfig::default(),
            })
            .await
            .unwrap();
        assert_eq!(result.chunks_written, 0);
        assert_eq!(result.skipped.len(), 1, "empty doc must be reported");

        let hits = store.query_scoped(&sc, "refunds", 10).await.unwrap();
        assert_eq!(
            hits.len(),
            1,
            "empty document must not delete previously indexed knowledge"
        );
    }

    #[tokio::test]
    async fn ingest_is_all_or_nothing_when_embed_fails() {
        /// Fails the embed call containing `fail_on`, simulating an Ollama hiccup
        /// partway through a multi-document ingest.
        struct FlakyOnText {
            fail_on: String,
            armed: std::sync::atomic::AtomicBool,
        }
        #[async_trait::async_trait]
        impl crate::ports::rag::EmbeddingProvider for FlakyOnText {
            async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
                if self.armed.load(std::sync::atomic::Ordering::SeqCst)
                    && texts.iter().any(|t| t.contains(&self.fail_on))
                {
                    return Err("embeddings HTTP 500: simulated outage".into());
                }
                Ok(texts.iter().map(|_| vec![0.25f32; 16]).collect())
            }
            fn dimensions(&self) -> usize {
                16
            }
        }

        let flaky = Arc::new(FlakyOnText {
            fail_on: "SECOND DOCUMENT".to_string(),
            armed: std::sync::atomic::AtomicBool::new(true),
        });
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(flaky.clone()));
        let ingest = RagIngestService::new(store.clone());
        let sc = scope(
            "demo",
            &["agent:demo/memory"],
            RagWritePolicy::OwnMemoryOnly,
        );

        let request = IngestRequest {
            scope: sc.clone(),
            source_id: "agent:demo/memory".into(),
            documents: vec![
                IngestDocument {
                    id_hint: Some("a".into()),
                    text: "FIRST DOCUMENT about refunds.".into(),
                    metadata: json!({}),
                },
                IngestDocument {
                    id_hint: Some("b".into()),
                    text: "SECOND DOCUMENT about warranty.".into(),
                    metadata: json!({}),
                },
            ],
            chunk: ChunkConfig::default(),
        };

        // The failure targets the SECOND document, so the first one would already
        // be committed if ingest were not atomic.
        let err = ingest.ingest(request.clone()).await.unwrap_err();
        assert!(matches!(err, AppError::Runtime(_)), "got: {err:?}");

        // Let queries work again so we can inspect what the failed ingest left.
        flaky
            .armed
            .store(false, std::sync::atomic::Ordering::SeqCst);

        let hits = store
            .query_scoped(&sc, "refunds warranty", 10)
            .await
            .unwrap();
        assert!(
            hits.is_empty(),
            "a failed ingest must leave nothing behind, found: {:?}",
            hits.iter().map(|h| &h.text).collect::<Vec<_>>()
        );

        // Retrying once the outage clears must index BOTH documents.
        let result = ingest.ingest(request).await.unwrap();
        assert_eq!(result.chunks_written, 2);
        let hits = store
            .query_scoped(&sc, "refunds warranty", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "retry must index both documents");
    }

    #[tokio::test]
    async fn duplicate_document_key_in_one_request_is_rejected() {
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
            HashEmbeddingProvider::new(16),
        )));
        let ingest = RagIngestService::new(store.clone());
        let sc = scope(
            "demo",
            &["agent:demo/memory"],
            RagWritePolicy::OwnMemoryOnly,
        );

        let err = ingest
            .ingest(IngestRequest {
                scope: sc.clone(),
                source_id: "agent:demo/memory".into(),
                documents: vec![
                    IngestDocument {
                        id_hint: Some("dup".into()),
                        text: "First about refunds.".into(),
                        metadata: json!({}),
                    },
                    IngestDocument {
                        id_hint: Some("dup".into()),
                        text: "Second about warranty.".into(),
                        metadata: json!({}),
                    },
                ],
                chunk: ChunkConfig::default(),
            })
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => {
                assert!(msg.contains("duplicate"), "got: {msg}")
            }
            other => panic!("expected Validation for duplicate key, got {other:?}"),
        }

        assert!(store
            .query_scoped(&sc, "refunds warranty", 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn document_key_with_chunk_separator_is_rejected() {
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
            HashEmbeddingProvider::new(16),
        )));
        let ingest = RagIngestService::new(store.clone());
        let sc = scope(
            "demo",
            &["agent:demo/memory"],
            RagWritePolicy::OwnMemoryOnly,
        );

        // A key containing the chunk separator would make prefix matching ambiguous
        // and let one document's replace delete another's chunks.
        let err = ingest
            .ingest(IngestRequest {
                scope: sc.clone(),
                source_id: "agent:demo/memory".into(),
                documents: vec![IngestDocument {
                    id_hint: Some("policy#chunk-99".into()),
                    text: "Ambiguous key.".into(),
                    metadata: json!({}),
                }],
                chunk: ChunkConfig::default(),
            })
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => {
                assert!(msg.contains("reserved") || msg.contains("#"), "got: {msg}")
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_document_removes_only_that_document() {
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
            HashEmbeddingProvider::new(16),
        )));
        let ingest = RagIngestService::new(store.clone());
        let sc = scope(
            "demo",
            &["agent:demo/memory"],
            RagWritePolicy::OwnMemoryOnly,
        );

        ingest
            .ingest(IngestRequest {
                scope: sc.clone(),
                source_id: "agent:demo/memory".into(),
                documents: vec![
                    IngestDocument {
                        id_hint: Some("keep".into()),
                        text: "Knowledge about refunds that must survive.".into(),
                        metadata: json!({}),
                    },
                    IngestDocument {
                        id_hint: Some("drop".into()),
                        text: "Knowledge about obsolete warranty terms.".into(),
                        metadata: json!({}),
                    },
                ],
                chunk: ChunkConfig::default(),
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .query_scoped(&sc, "refunds warranty", 10)
                .await
                .unwrap()
                .len(),
            2
        );

        let removed = ingest
            .delete_document(&sc, "agent:demo/memory", "drop")
            .await
            .unwrap();
        assert_eq!(removed, 1, "should report the chunks it removed");

        let hits = store
            .query_scoped(&sc, "refunds warranty", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "only the deleted document should be gone");
        assert!(hits[0].text.contains("refunds"));
        assert!(!hits.iter().any(|h| h.text.contains("obsolete")));
    }

    #[tokio::test]
    async fn delete_document_respects_write_policy() {
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
            HashEmbeddingProvider::new(16),
        )));
        let ingest = RagIngestService::new(store.clone());
        let sc = scope("demo", &["agent:demo/memory"], RagWritePolicy::None);

        let err = ingest
            .delete_document(&sc, "agent:demo/memory", "anything")
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(msg.contains("denied"), "got: {msg}"),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_document_rejects_invalid_key() {
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
            HashEmbeddingProvider::new(16),
        )));
        let ingest = RagIngestService::new(store);
        let sc = scope(
            "demo",
            &["agent:demo/memory"],
            RagWritePolicy::OwnMemoryOnly,
        );

        let err = ingest
            .delete_document(&sc, "agent:demo/memory", "bad#chunk-key")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn ingest_files_same_basename_does_not_collide() {
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
            HashEmbeddingProvider::new(16),
        )));
        let ingest = RagIngestService::new(store.clone());
        let sc = scope(
            "demo",
            &["agent:demo/memory"],
            RagWritePolicy::OwnMemoryOnly,
        );

        let dir = tempfile::tempdir().unwrap();
        let alpha = dir.path().join("alpha").join("policy.md");
        let beta = dir.path().join("beta").join("policy.md");
        std::fs::create_dir_all(alpha.parent().unwrap()).unwrap();
        std::fs::create_dir_all(beta.parent().unwrap()).unwrap();
        std::fs::write(&alpha, "Alpha refund policy is thirty days.").unwrap();
        std::fs::write(&beta, "Beta warranty policy is two years.").unwrap();

        let result = ingest
            .ingest_files(
                &sc,
                "agent:demo/memory",
                &[alpha.clone(), beta.clone()],
                Some(dir.path()),
            )
            .await
            .unwrap();
        assert_eq!(result.files_read, 2);

        // Both files' content must survive: distinct paths must not share chunk ids.
        let hits = store.query_scoped(&sc, "refund policy", 10).await.unwrap();
        let joined = hits
            .iter()
            .map(|h| h.text.to_lowercase())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            joined.contains("thirty days"),
            "alpha content lost, hits: {joined}"
        );
        assert!(
            joined.contains("two years"),
            "beta content lost, hits: {joined}"
        );
    }

    #[tokio::test]
    async fn ingest_files_across_requests_does_not_cross_delete() {
        // Two files whose flattened hints could collide must both survive, even
        // when ingested in SEPARATE requests (the in-request duplicate guard does
        // not cover this case).
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
            HashEmbeddingProvider::new(16),
        )));
        let ingest = RagIngestService::new(store.clone());
        let sc = scope(
            "demo",
            &["agent:demo/memory"],
            RagWritePolicy::OwnMemoryOnly,
        );

        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b.md");
        let flat = dir.path().join("a_b.md");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, "Nested file knowledge about refunds.").unwrap();
        std::fs::write(&flat, "Flat file knowledge about warranty.").unwrap();

        // Separate requests, so each ingest sees one file at a time.
        ingest
            .ingest_files(
                &sc,
                "agent:demo/memory",
                &[nested.clone()],
                Some(dir.path()),
            )
            .await
            .unwrap();
        ingest
            .ingest_files(&sc, "agent:demo/memory", &[flat.clone()], Some(dir.path()))
            .await
            .unwrap();

        let hits = store
            .query_scoped(&sc, "refunds warranty", 20)
            .await
            .unwrap();
        let joined = hits
            .iter()
            .map(|h| h.text.to_lowercase())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            joined.contains("refunds"),
            "nested file lost to a colliding hint: {joined}"
        );
        assert!(
            joined.contains("warranty"),
            "flat file lost to a colliding hint: {joined}"
        );
    }

    #[tokio::test]
    async fn ingest_files_reports_skipped_files() {
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
            HashEmbeddingProvider::new(16),
        )));
        let ingest = RagIngestService::new(store);
        let sc = scope(
            "demo",
            &["agent:demo/memory"],
            RagWritePolicy::OwnMemoryOnly,
        );

        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.md");
        std::fs::write(&good, "Readable knowledge about refunds.").unwrap();

        // UTF-16-ish content with a NUL byte reads as binary.
        let binary = dir.path().join("blob.bin");
        std::fs::write(&binary, [0u8, 1, 2, 3, 0, 5]).unwrap();

        let result = ingest
            .ingest_files(
                &sc,
                "agent:demo/memory",
                &[good.clone(), binary.clone()],
                Some(dir.path()),
            )
            .await
            .unwrap();

        assert_eq!(result.files_read, 1);
        assert_eq!(
            result.skipped.len(),
            1,
            "the binary file must be reported, not silently dropped"
        );
        assert_eq!(result.skipped[0].path, binary.display().to_string());
        assert!(result.skipped[0].reason.contains("binary"));
    }

    #[tokio::test]
    async fn ingest_files_rejects_path_outside_base() {
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
            HashEmbeddingProvider::new(16),
        )));
        let ingest = RagIngestService::new(store);
        let sc = scope(
            "demo",
            &["agent:demo/memory"],
            RagWritePolicy::OwnMemoryOnly,
        );

        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.md");
        std::fs::write(&secret, "top secret outside base").unwrap();

        let err = ingest
            .ingest_files(&sc, "agent:demo/memory", &[secret], Some(base.path()))
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(msg.contains("outside allowed base")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ingest_decorates_non_object_metadata() {
        let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
            HashEmbeddingProvider::new(16),
        )));
        let ingest = RagIngestService::new(store.clone());
        let sc = scope(
            "demo",
            &["agent:demo/memory"],
            RagWritePolicy::OwnMemoryOnly,
        );

        ingest
            .ingest(IngestRequest {
                scope: sc.clone(),
                source_id: "agent:demo/memory".into(),
                documents: vec![IngestDocument {
                    id_hint: Some("weird".into()),
                    text: "Knowledge with scalar metadata.".into(),
                    // Caller supplied a non-object metadata value.
                    metadata: json!("scalar"),
                }],
                chunk: ChunkConfig::default(),
            })
            .await
            .unwrap();

        let hits = store
            .query_scoped(&sc, "knowledge scalar", 3)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        // chunk_index / source_key must survive regardless of caller's metadata shape.
        assert_eq!(hits[0].metadata["chunk_index"], json!(0));
        assert_eq!(hits[0].metadata["source_key"], json!("weird"));
        assert_eq!(hits[0].metadata["original"], json!("scalar"));
    }

    #[test]
    fn io_permission_denied_is_not_reported_as_validation_error() {
        // A disk fault must not be blamed on the caller: "Permission denied
        // (os error 13)" contains "denied" but is a server-side failure.
        let err = map_store_err(
            "rag write '/data/lancedb/x/docs.jsonl': Permission denied (os error 13)".into(),
        );
        assert!(
            matches!(err, AppError::Runtime(_)),
            "I/O fault must map to Runtime, got {err:?}"
        );

        // A genuine policy denial stays a validation error.
        let err = map_store_err("RAG write denied for source 'a' on agent 'b'".into());
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
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
