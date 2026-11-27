//! End-to-end wire coverage for the OpenAI-compatible embeddings fallback.
//!
//! An OpenCode boot with no `OLLAMA_URL` must still serve embeddings when an
//! OpenAI-compatible origin and a key are configured. This drives the real
//! `build_provider_stack` path against a local axum fake that speaks
//! `POST /v1/embeddings`, proving the fallback selects the compatible adapter
//! (not OpenRouter's `/api/v1/embeddings`) and parses the returned vector.

#![allow(clippy::await_holding_lock)] // the env lock is held for the whole test by design

use axum::{routing::post, Json, Router};
use llama_r::config::{Config, ProviderKind};
use llama_r::runtime::build_provider_stack;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;
use tokio::task::JoinHandle;

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// These variables are process-wide; isolate the test from a developer shell
/// and from other integration tests that mutate them.
const ISOLATED_ENV_VARS: &[&str] = &[
    "LLAMA_R_DIR",
    "OPENAI_BASE_URL",
    "OPENROUTER_API_KEY",
    "OLLAMA_URL",
];

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
    _dir: TempDir,
}

impl EnvGuard {
    fn fresh() -> Self {
        let saved = ISOLATED_ENV_VARS
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect();
        for name in ISOLATED_ENV_VARS {
            std::env::remove_var(name);
        }
        // The stack loads `providers.toml` / `router.toml` from the base dir;
        // an empty temp dir keeps those optional files out of the test.
        let dir = tempfile::tempdir().expect("temp base dir");
        std::env::set_var("LLAMA_R_DIR", dir.path());
        Self { saved, _dir: dir }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// Ephemeral OpenAI-compatible embeddings server. The task is aborted on drop
/// so no listener (or port) is leaked when the test ends.
struct FakeEmbeddingsServer {
    url: String,
    hits: Arc<AtomicUsize>,
    _task: JoinHandle<()>,
}

impl FakeEmbeddingsServer {
    async fn start(dims: usize) -> Self {
        let hits = Arc::new(AtomicUsize::new(0));
        let handler_hits = Arc::clone(&hits);
        let app = Router::new().route(
            "/v1/embeddings",
            post(move |Json(body): Json<serde_json::Value>| {
                let hits = handler_hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let count = body
                        .get("input")
                        .and_then(|value| value.as_array())
                        .map(|items| items.len())
                        .unwrap_or(0);
                    let embedding: Vec<f32> = (0..dims).map(|i| i as f32 / 100.0).collect();
                    Json(serde_json::json!({
                        "model": "fake-embed",
                        "data": (0..count).map(|index| serde_json::json!({
                            "object": "embedding",
                            "index": index,
                            "embedding": embedding,
                        })).collect::<Vec<_>>(),
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral listener");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("fake server");
        });
        Self {
            url,
            hits,
            _task: task,
        }
    }
}

impl Drop for FakeEmbeddingsServer {
    fn drop(&mut self) {
        self._task.abort();
    }
}

#[tokio::test]
async fn opencode_stack_embeds_through_openai_compatible_fallback() {
    let _lock = env_lock();
    let _env = EnvGuard::fresh();
    const DIMS: usize = 7;
    let server = FakeEmbeddingsServer::start(DIMS).await;

    // No OLLAMA_URL (so the Ollama rung is skipped) but an OpenAI-compatible
    // origin plus a key: the fallback must supply an embedder. The loopback URL
    // deliberately contains neither "openrouter.ai" nor "/api/v1".
    let cfg = Config {
        provider: ProviderKind::OpenCode,
        ollama_url: String::new(),
        default_model: "test-model".to_string(),
        embedding_model: "fake-embed".to_string(),
        embedding_dimensions: DIMS,
        openrouter_api_key: Some("dummy".to_string()),
        openai_base_url: Some(server.url.clone()),
        rag_enabled: true,
        ..Config::default()
    };

    let stack = build_provider_stack(&cfg).expect("provider stack must build");
    let embeddings = stack
        .embeddings
        .expect("OPENAI_BASE_URL + OPENROUTER_API_KEY must supply embeddings");
    assert_eq!(embeddings.dimensions(), DIMS);

    let vectors = embeddings
        .embed(&["hola".to_string()])
        .await
        .expect("embed must succeed through the fake server");
    assert_eq!(vectors.len(), 1);
    assert_eq!(vectors[0].len(), DIMS);
    assert_eq!(
        server.hits.load(Ordering::SeqCst),
        1,
        "exactly one batched POST to /v1/embeddings"
    );
}
