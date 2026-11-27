//! Unit-level coverage of the OpenAI-compatible embeddings fallback helper.
//!
//! The helper is a pure `&Config` decision: it reads no environment, opens no
//! network and leaks no key. These tests pin the three gates (base URL present,
//! key present, dimensions forwarded) using the same isolated-env fixture
//! pattern as `provider_startup_tests.rs`.

#![allow(clippy::await_holding_lock)] // the env lock is held for the whole test by design

use llama_r::config::{Config, ProviderKind};
use llama_r::runtime::openai_compatible_embeddings_fallback;
use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn test_lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Env vars a developer shell could set that must not influence a fallback
/// decision. The helper itself reads only `Config`, but clearing them keeps the
/// fixture deterministic and mirrors `provider_startup_tests.rs`.
const FALLBACK_ENV_VARS: &[&str] = &[
    "OPENAI_BASE_URL",
    "OPENROUTER_API_KEY",
    "OLLAMA_URL",
    "OPENAI_API_KEY",
];

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
    prev_dir: Option<String>,
    _dir: TempDir,
}

impl EnvGuard {
    fn fresh() -> Self {
        let saved = FALLBACK_ENV_VARS
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect();
        for name in FALLBACK_ENV_VARS {
            std::env::remove_var(name);
        }
        let prev_dir = std::env::var("LLAMA_R_DIR").ok();
        let dir = tempfile::tempdir().expect("temp dir");
        std::env::set_var("LLAMA_R_DIR", dir.path());
        Self {
            saved,
            prev_dir,
            _dir: dir,
        }
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
        match &self.prev_dir {
            Some(value) => std::env::set_var("LLAMA_R_DIR", value),
            None => std::env::remove_var("LLAMA_R_DIR"),
        }
    }
}

fn config(base_url: Option<&str>, api_key: Option<&str>, dimensions: usize) -> Config {
    Config {
        port: 3000,
        provider: ProviderKind::OpenCode,
        ollama_url: String::new(),
        default_model: "test-model".to_string(),
        embedding_model: "nomic-embed-text".to_string(),
        embedding_dimensions: dimensions,
        rag_enabled: true,
        history_retention_days: 90,
        openrouter_api_key: api_key.map(str::to_string),
        opencode_api_key: None,
        opencode_url: "http://127.0.0.1:4096".to_string(),
        openai_base_url: base_url.map(str::to_string),
        ..Config::default()
    }
}

#[test]
fn openai_compatible_fallback_is_none_without_base_url() {
    let _guard = test_lock();
    let _env = EnvGuard::fresh();
    let cfg = config(None, Some("sk-secret-never-exposed"), 768);
    assert!(openai_compatible_embeddings_fallback(&cfg, "opencode").is_none());
}

#[test]
fn openai_compatible_fallback_is_none_without_api_key() {
    let _guard = test_lock();
    let _env = EnvGuard::fresh();
    let cfg = config(Some("https://openrouter.ai/api/v1"), None, 768);
    assert!(openai_compatible_embeddings_fallback(&cfg, "anthropic").is_none());
}

#[test]
fn openai_compatible_fallback_builds_embedder_with_config_dimensions() {
    let _guard = test_lock();
    let _env = EnvGuard::fresh();
    let cfg = config(Some("https://example.com/v1"), Some("sk-test-key"), 1536);
    let fb = openai_compatible_embeddings_fallback(&cfg, "opencode")
        .expect("URL + key must build an embedder");
    assert_eq!(fb.dimensions(), 1536);
}

#[test]
fn openai_compatible_fallback_rejects_blank_base_url() {
    let _guard = test_lock();
    let _env = EnvGuard::fresh();
    let cfg = config(Some("   "), Some("sk-test-key"), 768);
    assert!(openai_compatible_embeddings_fallback(&cfg, "opencode").is_none());
}

#[test]
fn openai_compatible_fallback_rejects_empty_api_key() {
    let _guard = test_lock();
    let _env = EnvGuard::fresh();
    let cfg = config(Some("https://openrouter.ai/api/v1"), Some(""), 768);
    assert!(openai_compatible_embeddings_fallback(&cfg, "anthropic").is_none());
}
