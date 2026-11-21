//! Task 3: per-agent provider/model resolution.
//!
//! An agent manifest may name its own provider and model. The resolver applies
//! `agent.provider` > global `PROVIDER` > ollama and `agent.model` >
//! `DEFAULT_MODEL`, and attaches the capabilities of the resolved model. A
//! `ResolvedModel` carries a provider *kind*, never a key value.

use llama_r::config::{Config, ProviderKind};
use llama_r::context::analyzer::ContextEnricher;
use llama_r::domain::agent::{Agent, AgentConfig};
use llama_r::providers::registry::ProviderRegistry;
use llama_r::providers::resolver::ModelResolver;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn test_lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Points `get_base_dir` at a fresh directory so no developer `providers.toml`
/// can change the built-in profiles under a test, and restores the variable.
struct EnvGuard {
    saved: Option<String>,
    dir: tempfile::TempDir,
}

impl EnvGuard {
    fn fresh() -> Self {
        let saved = std::env::var("LLAMA_R_DIR").ok();
        let dir = tempfile::tempdir().expect("create temp base dir");
        std::env::set_var("LLAMA_R_DIR", dir.path());
        Self { saved, dir }
    }

    fn write_registry(&self, contents: &str) {
        std::fs::write(self.dir.path().join("providers.toml"), contents)
            .expect("write providers.toml");
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.saved {
            Some(value) => std::env::set_var("LLAMA_R_DIR", value),
            None => std::env::remove_var("LLAMA_R_DIR"),
        }
    }
}

fn config(provider: ProviderKind, default_model: &str) -> Config {
    Config {
        port: 3000,
        provider,
        ollama_url: "http://localhost:11434".to_string(),
        default_model: default_model.to_string(),
        embedding_model: "nomic-embed-text".to_string(),
        embedding_dimensions: 768,
        rag_enabled: true,
        history_retention_days: 90,
        openrouter_api_key: Some("sk-global-key-must-never-leak".to_string()),
        opencode_api_key: None,
        opencode_url: "http://127.0.0.1:4096".to_string(),
        openai_base_url: None,
        ..Config::default()
    }
}

fn agent(provider: &str, model: &str) -> AgentConfig {
    AgentConfig {
        name: "resolver-test".to_string(),
        provider: provider.to_string(),
        model: model.to_string(),
        system_prompt: String::new(),
        ..Default::default()
    }
}

#[test]
fn agent_provider_overrides_the_global_one() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    let registry = ProviderRegistry::load().expect("builtin registry");
    let cfg = config(ProviderKind::Ollama, "global-model");
    let resolver = ModelResolver::new(&registry, &cfg);

    let resolved = resolver
        .resolve(&agent("anthropic", "claude-3-5-sonnet"))
        .expect("resolve anthropic agent");

    assert_eq!(resolved.provider, ProviderKind::Anthropic);
    assert_eq!(resolved.model, "claude-3-5-sonnet");
    assert!(resolved.capabilities.streaming);
    assert!(resolved.capabilities.tool_calling);
    assert!(resolved.capabilities.vision);
}

#[test]
fn empty_agent_provider_falls_back_to_global() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    let registry = ProviderRegistry::load().expect("builtin registry");
    let cfg = config(ProviderKind::OpenRouter, "global-model");
    let resolver = ModelResolver::new(&registry, &cfg);

    let resolved = resolver
        .resolve(&agent("", ""))
        .expect("fall back to the global provider and model");

    assert_eq!(resolved.provider, ProviderKind::OpenRouter);
    assert_eq!(resolved.model, "global-model");
}

#[test]
fn unknown_agent_provider_is_a_named_config_error() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    let registry = ProviderRegistry::load().expect("builtin registry");
    let cfg = config(ProviderKind::Ollama, "global-model");
    let resolver = ModelResolver::new(&registry, &cfg);

    let err = resolver
        .resolve(&agent("telepathy", "any-model"))
        .expect_err("an unknown provider must not fall back silently")
        .to_string();

    assert!(err.contains("telepathy"), "name the received value: {err}");
    for (spelling, _) in llama_r::config::PROVIDER_KIND_SPELLINGS {
        assert!(
            err.contains(spelling),
            "name accepted value {spelling}: {err}"
        );
    }
}

#[test]
fn resolved_model_carries_no_api_key() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    std::env::set_var("ANTHROPIC_API_KEY", "sk-agent-secret-never-in-a-result");
    let registry = ProviderRegistry::load().expect("builtin registry");
    let cfg = config(ProviderKind::Ollama, "global-model");
    let resolver = ModelResolver::new(&registry, &cfg);

    let resolved = resolver
        .resolve(&agent("anthropic", "claude-3-5-sonnet"))
        .expect("resolve anthropic agent");

    let dump = format!("{:?}", resolved);
    assert!(!dump.contains("sk-agent-secret-never-in-a-result"));
    assert!(!dump.contains("sk-global-key-must-never-leak"));
    std::env::remove_var("ANTHROPIC_API_KEY");
}

#[test]
fn a_profile_name_from_the_registry_selects_its_kind() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    _env.write_registry(
        r#"
[providers.local-openai]
type = "openai-compatible"
base_url = "http://127.0.0.1:8000/v1"
api_key_env = "LOCAL_OPENAI_KEY"
"#,
    );
    let registry = ProviderRegistry::load().expect("registry with custom profile");
    let cfg = config(ProviderKind::Ollama, "global-model");
    let resolver = ModelResolver::new(&registry, &cfg);

    let resolved = resolver
        .resolve(&agent("local-openai", "gpt-4o"))
        .expect("resolve a custom profile by name");

    assert_eq!(resolved.provider, ProviderKind::OpenAiCompatible);
    assert_eq!(resolved.model, "gpt-4o");
    assert!(resolved.capabilities.streaming);
}

#[test]
fn context_enricher_exposes_both_a_full_resolve_and_the_string_shim() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    let registry = Arc::new(ProviderRegistry::load().expect("builtin registry"));
    let enricher = ContextEnricher::new(
        Arc::new(llama_r::context::store::ContextStore::new()),
        Arc::new(llama_r::services::skill_manager::SkillManager::new()),
        "global-model".to_string(),
        registry,
        ProviderKind::Ollama,
    );
    let agent = Agent {
        id: "researcher".to_string(),
        project_id: None,
        config: agent("anthropic", "claude-3-5-sonnet"),
    };

    let resolved = enricher.resolve(&agent);
    assert_eq!(resolved.provider, ProviderKind::Anthropic);
    assert_eq!(resolved.model, "claude-3-5-sonnet");
    assert!(resolved.capabilities.vision);
    assert_eq!(enricher.resolve_model(&agent), resolved.model);
}

#[test]
fn context_enricher_degrades_an_unknown_agent_provider_to_the_global_one() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    let enricher = ContextEnricher::new(
        Arc::new(llama_r::context::store::ContextStore::new()),
        Arc::new(llama_r::services::skill_manager::SkillManager::new()),
        "global-model".to_string(),
        Arc::new(ProviderRegistry::load().expect("builtin registry")),
        ProviderKind::OpenRouter,
    );
    let agent = Agent {
        id: "mistyped".to_string(),
        project_id: None,
        config: agent("telepathy", "some-model"),
    };

    // The infallible chat-path shim keeps the request alive with the global
    // provider rather than failing; the resolver itself reports the error.
    let resolved = enricher.resolve(&agent);
    assert_eq!(resolved.provider, ProviderKind::OpenRouter);
    assert_eq!(resolved.model, "some-model");
}
