//! Static model capability catalog.
//!
//! Callers ask [`capabilities_for`] what an upstream can do before they try to
//! use a feature (streaming, tools, embeddings, …). The values here mirror the
//! behaviour the running providers already expose today: they are a
//! description of current reality, not a wish list of features to add. A model
//! that is not in the table gets the conservative all-false answer, so a new
//! or renamed model can never make a caller *assume* a capability that is not
//! there.

use crate::config::ProviderKind;

/// What a given `(provider, model)` pair is known to support.
///
/// [`Default`] is the conservative answer: no feature is assumed and no
/// numeric limit or price is claimed.
#[derive(Debug, Clone)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub vision: bool,
    pub json_mode: bool,
    pub reasoning: bool,
    pub embeddings: bool,
    pub max_context_tokens: Option<u32>,
    pub cost_per_1m_input: Option<f64>,
    pub cost_per_1m_output: Option<f64>,
}

impl ModelCapabilities {
    /// The all-false / all-`None` answer. `const` so the static table can use
    /// it as the base of struct-update expressions without repeating fields.
    const fn conservative() -> Self {
        Self {
            streaming: false,
            tool_calling: false,
            vision: false,
            json_mode: false,
            reasoning: false,
            embeddings: false,
            max_context_tokens: None,
            cost_per_1m_input: None,
            cost_per_1m_output: None,
        }
    }
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self::conservative()
    }
}

/// A model together with the provider that serves it and its capabilities.
#[derive(Debug, Clone)]
pub struct ModelDescriptor {
    pub id: String,
    pub provider: ProviderKind,
    pub capabilities: ModelCapabilities,
}

/// One row of the static catalog: a model *prefix* served by a provider.
struct CapabilityEntry {
    provider: ProviderKind,
    model_prefix: &'static str,
    capabilities: ModelCapabilities,
}

/// Capabilities observed from the providers as they run today. Prefixes are
/// lower-case family names; a model matches when it starts with one. Costs and
/// token limits are intentionally left unset — nothing quotes them reliably.
static CAPABILITY_TABLE: &[CapabilityEntry] = &[
    // Ollama: the local server streams, attaches tools and serves embeddings.
    CapabilityEntry {
        provider: ProviderKind::Ollama,
        model_prefix: "llama",
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            embeddings: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::Ollama,
        model_prefix: "mistral",
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            embeddings: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::Ollama,
        model_prefix: "qwen",
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            embeddings: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::Ollama,
        model_prefix: "gemma",
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            embeddings: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::Ollama,
        model_prefix: "phi",
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            embeddings: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::Ollama,
        model_prefix: "deepseek",
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            embeddings: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::Ollama,
        model_prefix: "nomic-embed-text",
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            embeddings: true,
            ..ModelCapabilities::conservative()
        },
    },
    // OpenRouter: tools + embeddings are served, but streaming returns 501
    // today, so it is deliberately not claimed here.
    CapabilityEntry {
        provider: ProviderKind::OpenRouter,
        model_prefix: "openai/",
        capabilities: ModelCapabilities {
            tool_calling: true,
            embeddings: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::OpenRouter,
        model_prefix: "anthropic/",
        capabilities: ModelCapabilities {
            tool_calling: true,
            embeddings: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::OpenRouter,
        model_prefix: "google/",
        capabilities: ModelCapabilities {
            tool_calling: true,
            embeddings: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::OpenRouter,
        model_prefix: "meta-llama/",
        capabilities: ModelCapabilities {
            tool_calling: true,
            embeddings: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::OpenRouter,
        model_prefix: "mistralai/",
        capabilities: ModelCapabilities {
            tool_calling: true,
            embeddings: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::OpenRouter,
        model_prefix: "deepseek/",
        capabilities: ModelCapabilities {
            tool_calling: true,
            embeddings: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::OpenRouter,
        model_prefix: "qwen/",
        capabilities: ModelCapabilities {
            tool_calling: true,
            embeddings: true,
            ..ModelCapabilities::conservative()
        },
    },
    // OpenCode is a harness with no tools-engine, no streaming and no
    // embeddings of its own. The empty prefix matches every OpenCode model.
    CapabilityEntry {
        provider: ProviderKind::OpenCode,
        model_prefix: "",
        capabilities: ModelCapabilities::conservative(),
    },
    // Anthropic, Gemini and OpenAI-compatible clients: tools + streaming +
    // vision.
    CapabilityEntry {
        provider: ProviderKind::Anthropic,
        model_prefix: "claude",
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            vision: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::Gemini,
        model_prefix: "gemini",
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            vision: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::OpenAiCompatible,
        model_prefix: "gpt",
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            vision: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::OpenAiCompatible,
        model_prefix: "o1",
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            vision: true,
            ..ModelCapabilities::conservative()
        },
    },
    CapabilityEntry {
        provider: ProviderKind::OpenAiCompatible,
        model_prefix: "o3",
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            vision: true,
            ..ModelCapabilities::conservative()
        },
    },
];

/// Look up the capabilities of `model` when served by `provider`.
///
/// Matching is case-insensitive on a model prefix; a model with no matching
/// entry returns [`ModelCapabilities::default`] (every boolean `false`).
pub fn capabilities_for(provider: ProviderKind, model: &str) -> ModelCapabilities {
    let model = model.trim().to_ascii_lowercase();
    CAPABILITY_TABLE
        .iter()
        .find(|entry| entry.provider == provider && model.starts_with(entry.model_prefix))
        .map(|entry| entry.capabilities.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_claims_streaming_and_tools() {
        let caps = capabilities_for(ProviderKind::Ollama, "llama3.2");
        assert!(caps.streaming, "ollama streams today");
        assert!(caps.tool_calling, "ollama exposes tools today");
        assert!(caps.embeddings, "ollama serves embeddings today");
    }

    #[test]
    fn opencode_claims_no_streaming_and_no_embeddings() {
        let caps = capabilities_for(ProviderKind::OpenCode, "opencode-model");
        assert!(!caps.streaming, "opencode returns 501 for streaming today");
        assert!(!caps.embeddings, "opencode has no embeddings endpoint");
        assert!(!caps.tool_calling, "opencode has no tools-engine");
    }

    #[test]
    fn an_unknown_model_gets_conservative_capabilities() {
        let caps = capabilities_for(ProviderKind::Ollama, "totally-unknown-model");
        assert!(!caps.tool_calling);
        assert!(!caps.streaming);
        assert!(!caps.vision);
        assert!(!caps.json_mode);
        assert!(!caps.reasoning);
        assert!(!caps.embeddings);
    }

    #[test]
    fn costs_are_optional_and_absent_by_default() {
        let caps = capabilities_for(ProviderKind::Ollama, "llama3.2");
        assert_eq!(caps.max_context_tokens, None);
        assert_eq!(caps.cost_per_1m_input, None);
        assert_eq!(caps.cost_per_1m_output, None);
    }

    #[test]
    fn openrouter_has_tools_and_embeddings_but_no_streaming() {
        let caps = capabilities_for(ProviderKind::OpenRouter, "openai/gpt-4o");
        assert!(caps.tool_calling);
        assert!(caps.embeddings);
        assert!(!caps.streaming, "openrouter streaming is 501 today");
    }

    #[test]
    fn frontier_providers_claim_streaming_tools_and_vision() {
        for (provider, model) in [
            (ProviderKind::Anthropic, "claude-3-5-sonnet"),
            (ProviderKind::Gemini, "gemini-1.5-pro"),
            (ProviderKind::OpenAiCompatible, "gpt-4o"),
        ] {
            let caps = capabilities_for(provider, model);
            assert!(caps.streaming, "{provider} streams");
            assert!(caps.tool_calling, "{provider} calls tools");
            assert!(caps.vision, "{provider} sees images");
        }
    }

    #[test]
    fn model_descriptor_carries_provider_and_capabilities() {
        let descriptor = ModelDescriptor {
            id: "llama3.2".to_string(),
            provider: ProviderKind::Ollama,
            capabilities: capabilities_for(ProviderKind::Ollama, "llama3.2"),
        };
        assert_eq!(descriptor.id, "llama3.2");
        assert_eq!(descriptor.provider, ProviderKind::Ollama);
        assert!(descriptor.capabilities.streaming);
    }
}
