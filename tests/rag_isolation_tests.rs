//! Integration-style isolation tests for RAG (no Ollama, no LanceDB).
//!
//! Uses InMemory + HashEmbedding and FileRagStore in a tempdir.

use llama_r::adapters::rag::file_store::FileRagStore;
use llama_r::adapters::rag::store::{HashEmbeddingProvider, InMemoryRagStore};
use llama_r::domain::agent::RagWritePolicy;
use llama_r::domain::scope::{AgentScope, ToolsAllow};
use llama_r::ports::rag::{RagStore, RagUpsert};
use llama_r::services::rag_ingest::{IngestDocument, IngestRequest, RagIngestService};
use std::collections::HashSet;
use std::sync::Arc;

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

async fn assert_cross_agent_isolation(store: Arc<dyn RagStore>) {
    let writer = scope_for("a", &["agent:a/memory"], RagWritePolicy::OwnMemoryOnly);
    store
        .upsert_scoped(
            &writer,
            vec![RagUpsert {
                source_id: "agent:a/memory".into(),
                id: "secret-1".into(),
                text: "top secret of agent a only".into(),
                metadata: serde_json::json!({}),
            }],
        )
        .await
        .expect("upsert a");

    let reader_b = scope_for("b", &["agent:b/memory"], RagWritePolicy::None);
    let hits_b = store
        .query_scoped(&reader_b, "secret agent a", 5)
        .await
        .expect("query b");
    assert!(hits_b.is_empty(), "agent b must not see agent a collection");

    let reader_a = scope_for("a", &["agent:a/memory"], RagWritePolicy::None);
    let hits_a = store
        .query_scoped(&reader_a, "secret agent a", 5)
        .await
        .expect("query a");
    assert_eq!(hits_a.len(), 1);
    assert!(hits_a[0].text.contains("agent a"));
}

#[tokio::test]
async fn in_memory_cross_agent_isolation() {
    let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
        HashEmbeddingProvider::new(32),
    )));
    assert_cross_agent_isolation(store).await;
}

#[tokio::test]
async fn file_store_reingest_shrunk_file_drops_stale_chunks() {
    // Drives the real ingest pipeline (chunker + file_id_hint + replace_scoped)
    // against the persistent store, so a change to the chunk-id format cannot
    // silently turn replace_scoped into an upsert that leaves stale chunks behind.
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FileRagStore::new(
        dir.path(),
        Arc::new(HashEmbeddingProvider::new(32)),
    ));
    let ingest = RagIngestService::new(store.clone());
    let sc = scope_for(
        "demo",
        &["agent:demo/memory"],
        RagWritePolicy::OwnMemoryOnly,
    );

    let file = dir.path().join("policy.md");
    std::fs::write(&file, "Refund policy detail sentence. ".repeat(120)).unwrap();

    let first = ingest
        .ingest_files(&sc, "agent:demo/memory", &[file.clone()], Some(dir.path()))
        .await
        .expect("first ingest");
    assert!(first.chunks_written >= 4, "expected several chunks");

    // The same file, now much shorter.
    std::fs::write(&file, "Short updated policy.").unwrap();
    let second = ingest
        .ingest_files(&sc, "agent:demo/memory", &[file.clone()], Some(dir.path()))
        .await
        .expect("second ingest");
    assert_eq!(second.chunks_written, 1);

    let hits = store
        .query_scoped(&sc, "refund policy detail sentence", 50)
        .await
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "stale chunks survived the file re-ingest: {:?}",
        hits.iter().map(|h| &h.id).collect::<Vec<_>>()
    );
    assert!(hits[0].text.contains("Short updated policy"));
}

#[tokio::test]
async fn file_store_empty_file_does_not_wipe_index() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FileRagStore::new(
        dir.path(),
        Arc::new(HashEmbeddingProvider::new(32)),
    ));
    let ingest = RagIngestService::new(store.clone());
    let sc = scope_for(
        "demo",
        &["agent:demo/memory"],
        RagWritePolicy::OwnMemoryOnly,
    );

    let file = dir.path().join("notes.md");
    std::fs::write(&file, "Refunds are allowed within 24 hours.").unwrap();
    ingest
        .ingest_files(&sc, "agent:demo/memory", &[file.clone()], Some(dir.path()))
        .await
        .expect("ingest");

    // Truncate the file to nothing, as an interrupted write would.
    std::fs::write(&file, "").unwrap();
    let result = ingest
        .ingest_files(&sc, "agent:demo/memory", &[file.clone()], Some(dir.path()))
        .await
        .expect("second ingest");
    assert_eq!(result.chunks_written, 0);
    assert_eq!(result.skipped.len(), 1);

    let hits = store.query_scoped(&sc, "refunds", 10).await.unwrap();
    assert_eq!(
        hits.len(),
        1,
        "an emptied file must not wipe previously indexed knowledge"
    );
}

#[tokio::test]
async fn file_store_cross_agent_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn RagStore> = Arc::new(FileRagStore::new(
        dir.path(),
        Arc::new(HashEmbeddingProvider::new(32)),
    ));
    assert_cross_agent_isolation(store).await;
}

#[tokio::test]
async fn ingest_service_respects_write_policy_and_queries() {
    let store: Arc<dyn RagStore> = Arc::new(InMemoryRagStore::new(Arc::new(
        HashEmbeddingProvider::new(32),
    )));
    let ingest = RagIngestService::new(store.clone());
    let writer = scope_for(
        "demo",
        &["agent:demo/memory"],
        RagWritePolicy::OwnMemoryOnly,
    );

    let result = ingest
        .ingest(IngestRequest {
            scope: writer.clone(),
            source_id: "agent:demo/memory".into(),
            documents: vec![IngestDocument {
                id_hint: Some("policy".into()),
                text: "Cancellation policy allows full refunds within twenty four hours.".into(),
                metadata: serde_json::json!({}),
            }],
            chunk: Default::default(),
        })
        .await
        .expect("ingest");
    assert!(result.chunks_written >= 1);

    let hits = store
        .query_scoped(&writer, "refunds cancellation", 3)
        .await
        .expect("query");
    assert!(!hits.is_empty());
}
