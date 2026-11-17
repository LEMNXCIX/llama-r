# Multi-provider support — design

**Date:** 2026-11-17
**Status:** proposed
**Scope:** make llama-r independent of Ollama for chat, embeddings and the router decision model. Add OpenRouter and OpenCode as selectable providers, configurable at first-run setup and from a TUI settings screen, with hot application.

## Problem

`build_app_state` hard-codes `OllamaProvider` and `OllamaEmbeddings`; `run` hard-codes a `LlamaCppRouter`. `src/config.rs` reads only `OLLAMA_URL`. The interactive setup offers "Ollama" and a placeholder. A user who has an OpenRouter key, or who runs OpenCode, cannot use llama-r at all.

The three services have different upstream shapes, so a single "OpenAI-compatible provider" is not sufficient:

| Service | Ollama | OpenRouter | OpenCode |
|---|---|---|---|
| Chat | `POST /api/chat` | `POST /v1/chat/completions` (Bearer) | `POST /api/experimental/generate/text` |
| Router d1 | `POST /v1/systemone` on llama.cpp | `POST /api/alpha/decisions` (Bearer) | `POST /api/experimental/generate/decisions` |
| Embeddings | `POST /api/embed` | `POST /api/v1/embeddings` (Bearer) | no endpoint found |

OpenCode has no documented embeddings endpoint. When `PROVIDER=opencode`, RAG and semantic skill ranking keep using Ollama embeddings if `OLLAMA_URL` is set, or disable themselves with a warning if it is not. This is explicit and logged — never a silent fallback.

Verified from primary docs: [OpenRouter Decisions API](https://openrouter.ai/docs/api-reference/decisions), [OpenRouter embeddings](https://openrouter.ai/docs/api-reference/embeddings), [OpenCode API](https://opencode.ai/docs/api/).

## Goal

- Any of the three providers can serve chat and the decision model, chosen by `PROVIDER`.
- OpenRouter can additionally serve embeddings; Ollama remains supported for all three.
- A first-run CLI setup lets the user choose provider, enter a key and verify health.
- A TUI settings screen changes provider/key/model **and applies it in the running process** without restarting.
- No change to llama-r's own HTTP/gRPC API, agent manifests or storage format.

## Non-goals

- Per-service provider mixing (chat from OpenCode, router from OpenRouter, etc.). One `PROVIDER` selects all three; the embeddings exception above is the only split.
- Multi-provider failover inside one request. Selection is explicit.
- Streaming from OpenCode or OpenRouter. Both are used for non-streaming chat; `stream: true` with a non-Ollama provider returns a clear `AppError::Unavailable` rather than pretending to stream.
- Changing the router's question schema. `DecisionRequest`/`DecisionResponse` stay as they are; the new adapters translate to them.

## Architecture

### Ports stay unchanged

`LLMProvider`, `EmbeddingProvider` and `DecisionRouter` already express the three seams. No trait changes. New adapters implement them:

| Adapter | Port | Upstream |
|---|---|---|
| `OllamaProvider` (existing) | `LLMProvider` | `/api/chat` |
| `OpenAiCompatibleProvider` | `LLMProvider` | `/v1/chat/completions`, Bearer |
| `OpenCodeProvider` | `LLMProvider` | `/api/experimental/generate/text` |
| `OllamaEmbeddings` (existing) | `EmbeddingProvider` | `/api/embed` |
| `OpenRouterEmbeddings` | `EmbeddingProvider` | `/api/v1/embeddings`, Bearer |
| `LlamaCppRouter` (existing) | `DecisionRouter` | `/v1/systemone` |
| `OpenRouterRouter` | `DecisionRouter` | `/api/alpha/decisions`, Bearer |
| `OpenCodeRouter` | `DecisionRouter` | `/api/experimental/generate/decisions` |

Each adapter owns its wire structs, error mapping and header construction, following the existing `src/adapters/rag/embeddings.rs` and `src/adapters/router/llamacpp.rs` pattern. A shared `bearer_client(token)` helper in `src/adapters/` builds a `reqwest::Client` with the auth header; no key is ever put in a URL.

`OpenCodeProvider` and `OpenCodeRouter` share one base URL (default `http://127.0.0.1:4096`). They do not require an API key unless `OPENCODE_SERVER_PASSWORD` is set on the server, in which case basic auth is used.

### ProviderStack and hot swap (Review Focus #1, #2)

`AppState` gains one field:

```rust
pub services: Arc<RwLock<ProviderStack>>,
```

`ProviderStack` holds the provider-dependent pieces:

`services` uses `tokio::sync::RwLock` (not `std::sync`), because Apply holds the
write lock across the async build of the new stack. Reads clone the inner `Arc`
and release the guard before any await.

```rust
pub struct ProviderStack {
    pub provider: Arc<dyn LLMProvider + Send + Sync>,
    pub agent_runtime: Option<Arc<AgentRuntime>>,
    pub rag_store: Option<Arc<dyn RagStore>>,
    pub rag_ingest: Option<Arc<RagIngestService>>,
    pub embeddings: Option<Arc<dyn EmbeddingProvider>>,
    pub router: Option<Arc<dyn DecisionRouter>>,
    pub default_model: String,
    pub provider_name: String,
}
```

Handlers resolve the stack **once per request** by cloning the `Arc` out of the `RwLock` before any `.await`. No lock is held across an await — this is the same discipline already used for `AppState.router` and documented in `src/api/chat_core.rs`.

The existing direct fields (`provider`, `agent_runtime`, `rag_store`, `rag_ingest`, `default_model`) are removed from `AppState` and replaced by accessors that read `services`. `AppState.router` is folded into the stack. Health, model listing and the chat handlers switch to the accessors.

**Why this over rebuilding `AppState`:** `TuiApp` holds a cloned `Arc<AppState>`. Rebuilding would require `Arc<RwLock<Arc<AppState>>>` through TUI, HTTP, gRPC and every test. `ProviderStack` confines the mutation to five fields, keeps one stable `AppState` identity, and lets in-flight requests finish on the old stack — old `Arc`s stay alive until the last reference drops.

### Configuration

`src/config.rs` gains:

```
PROVIDER              ollama | openrouter | opencode      (default: ollama)
OPENROUTER_API_KEY    (secret, no default)
OPENCODE_API_KEY      (secret, no default; optional basic-auth password)
OPENCODE_URL          (default: http://127.0.0.1:4096)
OPENAI_BASE_URL       (optional override for OpenAI-compatible endpoints)
```

Existing `OLLAMA_URL`, `DEFAULT_MODEL`, `EMBEDDING_MODEL`, `EMBEDDING_DIMENSIONS` and `RAG_ENABLED` remain. Default model names are per provider: Ollama keeps the current value; OpenRouter uses `openai/gpt-4o-mini`; OpenCode uses its configured default model.

`Config::save_to_env` is extended to write the new keys. It already writes `PORT`, `OLLAMA_URL` and `DEFAULT_MODEL`; the same replace-or-append logic is reused. API keys are written last and `.env` gets mode `0600` on Unix when the file is created by the app.

### CLI setup

`src/cli/interactive.rs` replaces the "Explore more in future..." placeholder with the three providers. Flow:

1. Select provider.
2. If OpenRouter/OpenCode: prompt for the API key (hidden input via `dialoguer::Password`), or accept an existing env value.
3. Prompt for base URL (with provider-specific default); for Ollama this is the existing step.
4. Build the chosen provider and run `health_check()` with the existing Retry / Edit / Exit loop.
5. `list_models()` and select a default model — except OpenCode, whose one-shot generate endpoint has no model listing; there the model is entered manually.
6. Return `(ProviderStack, default_model)` so `runtime.rs` installs it before serving.

Setup stays non-fatal: failure keeps the degraded server, as today.

### TUI settings screen

New `Settings` variant in `CurrentView`, reachable with a key from the context bar (the sixth view name stays; the settings screen is a modal over Dashboard, not a new bar entry — adding a seventh name would break the bar-width guard in `tests/tui_chrome.rs`).

The screen has three fields: provider (cycle with arrows), API key (masked input), model (text). A final row has `[Apply]` and `[Cancel]`. It reuses `views/agent_form.rs` field-navigation patterns and `keys.rs`' `hints_for`.

On Apply:

1. Validate the key is present for OpenRouter; OpenCode allows empty.
2. Build a new `ProviderStack` asynchronously (`chat_input_action` pattern: the event loop cannot await, so the build happens in a spawned task with a "Applying…" state).
3. On success, `config` is updated, `save_to_env()` is called, and `*state.services.write().await = new_stack`.
4. On failure, show the error inline and keep the old stack.

The `#[cfg(test)]` lock-order discipline in `app/tests.rs` applies to any new test that mutates the environment.

## Testing

- One wire test per adapter: a local `TcpListener` captures the real request (path, Bearer header, body) and returns a recorded response. Reuse `tests/router_wire_tests.rs` for the decision adapters and the pattern from `tests/headless_server_tests.rs` for one-shot chat.
- A hot-swap test in `tests/api_integration.rs`: start with a fake provider A, replace the stack with fake provider B via the accessor, send a request, assert B answered. Also assert an in-flight request that started before the swap still completes on A.
- Config tests: each provider's defaults, key validation, `save_to_env` round-trip including a key with `=` and spaces.
- TUI tests: `Settings` renders into `body` with no box, hints come only from `hints_for`, and Apply with an empty OpenRouter key shows an error without changing the stack.
- Full gates: fmt, clippy `-D warnings`, 490+ tests, `cargo audit`.

## Risks

- OpenCode's generate/decisions schemas are experimental; they will be pinned by captured fixtures, as d1's was.
- Hot swap changes what a conversation's next turn sees. This is intended and must be logged.
- API keys in `.env` are already in the repo's ignore rules. File mode 0600 is best-effort on Windows.
- A provider with no embeddings (OpenCode) degrades RAG; the health endpoint must say why, not just "degraded".
