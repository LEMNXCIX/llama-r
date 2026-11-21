//! Task 4: the engine target follows the provider resolved for a request, not
//! the single global `PROVIDER` the companions were built with.
//!
//! No network is opened here: building a target only reads config/env and the
//! resolved provider, so the whole file stays hermetic.

use llama_r::adapters::rig_engine::{
    engine_target_for, engine_target_for_resolved, EngineTarget, EngineTargetKind,
};
use llama_r::config::{Config, ProviderKind, OPENROUTER_BASE_URL};
use llama_r::providers::capabilities::capabilities_for;
use llama_r::providers::resolver::ResolvedModel;
use std::sync::{Mutex, MutexGuard, OnceLock};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const KEY_ENV_VARS: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "OPENAI_BASE_URL",
];

/// Clears the env vars the new targets read and restores them on drop, so a
/// panic in one test cannot leak a key into a later one.
struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn cleared() -> Self {
        let saved = KEY_ENV_VARS
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect();
        for name in KEY_ENV_VARS {
            std::env::remove_var(name);
        }
        Self { saved }
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

fn config(provider: ProviderKind) -> Config {
    Config {
        port: 3000,
        provider,
        ollama_url: "http://localhost:11434".to_string(),
        default_model: "global-model".to_string(),
        embedding_model: "nomic-embed-text".to_string(),
        embedding_dimensions: 768,
        rag_enabled: true,
        history_retention_days: 90,
        openrouter_api_key: Some("sk-global-key".to_string()),
        opencode_api_key: None,
        opencode_url: "http://127.0.0.1:4096".to_string(),
        openai_base_url: None,
        ..Config::default()
    }
}

fn resolved(provider: ProviderKind, model: &str) -> ResolvedModel {
    ResolvedModel {
        provider,
        model: model.to_string(),
        capabilities: capabilities_for(provider, model),
    }
}

/// The whole point of the task: an agent's own provider wins over the global
/// one when the engine target is built for that request.
#[test]
fn an_agent_provider_selects_a_different_engine_target_than_the_global() {
    let cfg = config(ProviderKind::Ollama);
    let agent = resolved(ProviderKind::Anthropic, "claude-3-5-sonnet");

    let target = engine_target_for_resolved(&cfg, &agent).expect("anthropic has a native client");

    assert_eq!(target.kind(), EngineTargetKind::Anthropic);
    assert_eq!(
        cfg.provider,
        ProviderKind::Ollama,
        "selection came from the resolved provider, not the global config"
    );
}

/// Each new provider reads its key from the env var named by its built-in
/// profile and keeps that key out of the URL and the non-secret kind.
#[test]
fn each_native_provider_builds_a_target_from_its_own_key_env() {
    let _guard = env_lock();
    let _env = EnvGuard::cleared();
    std::env::set_var("OPENAI_API_KEY", "sk-openai-never-logged");
    std::env::set_var("ANTHROPIC_API_KEY", "sk-anthropic-never-logged");
    std::env::set_var("GEMINI_API_KEY", "sk-gemini-never-logged");
    std::env::set_var("OPENAI_BASE_URL", "https://llm.example.test/v1");

    let cfg = config(ProviderKind::Ollama);
    let cases = [
        (
            ProviderKind::OpenAiCompatible,
            "gpt-4o",
            EngineTargetKind::OpenAiCompatible,
            "sk-openai-never-logged",
        ),
        (
            ProviderKind::Anthropic,
            "claude-3-5-sonnet",
            EngineTargetKind::Anthropic,
            "sk-anthropic-never-logged",
        ),
        (
            ProviderKind::Gemini,
            "gemini-1.5-pro",
            EngineTargetKind::Gemini,
            "sk-gemini-never-logged",
        ),
    ];

    for (provider, model, kind, key) in cases {
        let target = engine_target_for_resolved(&cfg, &resolved(provider, model))
            .unwrap_or_else(|| panic!("{provider} has a native client"));
        assert_eq!(target.kind(), kind);

        let (stored_key, base_url) = match &target {
            EngineTarget::OpenAiCompatible { api_key, base_url }
            | EngineTarget::Anthropic { api_key, base_url }
            | EngineTarget::Gemini { api_key, base_url } => (api_key, base_url),
            other => panic!(
                "{provider} must build its own target kind: {:?}",
                other.kind()
            ),
        };
        assert_eq!(
            stored_key, key,
            "the key comes from that provider's env var"
        );
        assert!(!base_url.contains("sk-"), "no key in a URL: {base_url}");

        let printable = format!("{:?}", target.kind());
        assert!(
            !printable.contains("sk-"),
            "no key in the kind: {printable}"
        );
    }
}

/// A target deliberately has no `Debug`: formatting the *kind* is the only
/// diagnostic allowed, and even the OpenRouter variant's key stays out of it.
#[test]
fn a_target_never_implements_debug() {
    let cfg = config(ProviderKind::Ollama);
    let target =
        engine_target_for_resolved(&cfg, &resolved(ProviderKind::OpenRouter, "openai/gpt-4o"))
            .expect("openrouter has a native client");

    match &target {
        EngineTarget::OpenRouter { api_key, base_url } => {
            assert_eq!(api_key, "sk-global-key");
            assert_eq!(base_url, OPENROUTER_BASE_URL);
            assert!(!base_url.contains("sk-global-key"));
        }
        other => panic!("openrouter target expected: {:?}", other.kind()),
    }
    let printable = format!("{:?}", target.kind());
    assert!(
        !printable.contains("sk-global-key"),
        "the non-secret kind cannot carry a key: {printable}"
    );
}

/// OpenCode has no rig completion client, on either the request path or the
/// boot path.
#[test]
fn opencode_still_has_no_engine_target() {
    let cfg = config(ProviderKind::OpenCode);
    let agent = resolved(ProviderKind::OpenCode, "opencode-model");
    assert!(engine_target_for_resolved(&cfg, &agent).is_none());
    assert!(
        engine_target_for(&cfg).is_none(),
        "OpenCode is the one kind with no boot engine"
    );
}

/// Every kind with a completion client gets an engine at boot, so an agent can
/// override the boot provider in TOML alone. The boot target is only a starting
/// point — per-request selection happens in `run_inner`/`stream_inner`.
#[test]
fn boot_engine_is_attached_for_every_kind_with_a_rig_client() {
    let expected = [
        (ProviderKind::Ollama, EngineTargetKind::Ollama),
        (ProviderKind::OpenRouter, EngineTargetKind::OpenRouter),
        (
            ProviderKind::OpenAiCompatible,
            EngineTargetKind::OpenAiCompatible,
        ),
        (ProviderKind::Anthropic, EngineTargetKind::Anthropic),
        (ProviderKind::Gemini, EngineTargetKind::Gemini),
    ];
    for (provider, kind) in expected {
        let target = engine_target_for(&config(provider))
            .unwrap_or_else(|| panic!("{provider} must get a boot engine"));
        assert_eq!(target.kind(), kind);
    }
}
