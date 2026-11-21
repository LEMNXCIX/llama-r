//! Per-request provider and model resolution.
//!
//! An agent manifest may name its own upstream and model. [`ModelResolver`]
//! applies the precedence `agent.provider` > global `PROVIDER` > ollama and
//! `agent.model` > `DEFAULT_MODEL`, then attaches the capabilities of the
//! resolved `(provider, model)` pair.
//!
//! The result is deliberately secret-free: it carries a [`ProviderKind`], never
//! an API key or a resolved client. Key values stay with the client built from
//! the profile's `api_key_env` name at a later stage.

use crate::config::{Config, ProviderKind};
use crate::domain::agent::AgentConfig;
use crate::error::AppError;
use crate::providers::capabilities::{capabilities_for, ModelCapabilities};
use crate::providers::registry::ProviderRegistry;

/// What a chat request will actually run against: a provider kind, a model id
/// and the capabilities that pair is known to support. No secret lives here.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub provider: ProviderKind,
    pub model: String,
    pub capabilities: ModelCapabilities,
}

/// Resolves an agent's `provider`/`model` against the running configuration and
/// the optional `providers.toml` profile registry.
pub struct ModelResolver<'a> {
    registry: &'a ProviderRegistry,
    config: &'a Config,
}

impl<'a> ModelResolver<'a> {
    pub fn new(registry: &'a ProviderRegistry, config: &'a Config) -> Self {
        Self { registry, config }
    }

    /// Resolve `agent` to a concrete provider, model and capability set.
    ///
    /// An unknown `agent.provider` is a named configuration error — the same
    /// one `ProviderKind::from_str` produces — and never a silent fallback.
    pub fn resolve(&self, agent: &AgentConfig) -> Result<ResolvedModel, AppError> {
        resolve_for(
            self.registry,
            agent,
            self.config.provider,
            &self.config.default_model,
        )
    }
}

/// The resolution core, shared with `ContextEnricher`, which holds the global
/// provider and default model as separate fields rather than a whole `Config`.
/// Keeping one implementation means the chat path and a direct `ModelResolver`
/// call can never disagree about the precedence.
pub(crate) fn resolve_for(
    registry: &ProviderRegistry,
    agent: &AgentConfig,
    global_provider: ProviderKind,
    default_model: &str,
) -> Result<ResolvedModel, AppError> {
    let provider = resolve_provider(registry, &agent.provider, global_provider)?;
    let model = if agent.model.trim().is_empty() {
        default_model.to_string()
    } else {
        agent.model.clone()
    };

    Ok(ResolvedModel {
        capabilities: capabilities_for(provider, &model),
        provider,
        model,
    })
}

/// `agent.provider` > global provider. A custom profile name from
/// `providers.toml` selects that profile's kind; otherwise the six canonical
/// spellings are accepted. An empty string means "use the global provider".
fn resolve_provider(
    registry: &ProviderRegistry,
    raw: &str,
    global_provider: ProviderKind,
) -> Result<ProviderKind, AppError> {
    let name = raw.trim();
    if name.is_empty() {
        return Ok(global_provider);
    }
    if let Some(profile) = registry.get(name) {
        return Ok(profile.kind);
    }
    // `FromStr` names the value and all accepted spellings; it reads no key.
    name.parse::<ProviderKind>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_agent_provider_uses_the_global_kind() {
        let config = Config {
            port: 3000,
            provider: ProviderKind::OpenRouter,
            ollama_url: "http://localhost:11434".to_string(),
            default_model: "global".to_string(),
            embedding_model: "nomic-embed-text".to_string(),
            embedding_dimensions: 768,
            rag_enabled: true,
            history_retention_days: 90,
            openrouter_api_key: None,
            opencode_api_key: None,
            opencode_url: "http://127.0.0.1:4096".to_string(),
            openai_base_url: None,
            ..Config::default()
        };
        let agent = AgentConfig {
            provider: "   ".to_string(),
            model: String::new(),
            ..Default::default()
        };
        let resolved = resolve_for(
            &ProviderRegistry::default(),
            &agent,
            config.provider,
            &config.default_model,
        )
        .unwrap();
        assert_eq!(resolved.provider, ProviderKind::OpenRouter);
        assert_eq!(resolved.model, "global");
    }
}
