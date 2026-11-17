use crate::adapters::rig_engine::EngineTargetKind;
use crate::ports::rag::{EmbeddingProvider, RagStore};
use crate::ports::router::DecisionRouter;
use crate::providers::LLMProvider;
use crate::services::agent_runtime::AgentRuntime;
use crate::services::rag_ingest::RagIngestService;
use std::sync::Arc;

/// The provider-dependent half of `AppState`.
///
/// Held behind `Arc<RwLock<ProviderStack>>` so a running process can install a
/// new provider, model and its companion services without rebuilding `AppState`
/// (which `TuiApp`, HTTP and gRPC all hold an `Arc` to). Every accessor takes a
/// snapshot `Arc` out of the lock before any `.await`, so a request that began
/// before a swap finishes on the old stack and a request that begins after it
/// uses the new one.
#[derive(Clone)]
pub struct ProviderStack {
    pub provider: Arc<dyn LLMProvider + Send + Sync>,
    pub agent_runtime: Option<Arc<AgentRuntime>>,
    pub rag_store: Option<Arc<dyn RagStore>>,
    pub rag_ingest: Option<Arc<RagIngestService>>,
    pub embeddings: Option<Arc<dyn EmbeddingProvider>>,
    pub router: Option<Arc<dyn DecisionRouter>>,
    pub default_model: String,
    pub provider_name: String,
    /// Non-secret mirror of the engine's provider target, recorded when the
    /// companions are built. Lets health and tests see which upstream the
    /// engine was built for without downcasting `agent_runtime`'s trait object.
    /// `None` when the selected provider has no engine (e.g. OpenCode).
    pub engine_target: Option<EngineTargetKind>,
}

impl ProviderStack {
    /// A minimal stack around a chat provider and a default model. Every
    /// optional companion service defaults to `None`; callers that need one set
    /// the corresponding public field before handing the stack to
    /// `build_app_state` or `replace_services`.
    pub fn new(
        provider: Arc<dyn LLMProvider + Send + Sync>,
        default_model: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            agent_runtime: None,
            rag_store: None,
            rag_ingest: None,
            embeddings: None,
            router: None,
            default_model: default_model.into(),
            provider_name: "ollama".to_string(),
            engine_target: None,
        }
    }
}
