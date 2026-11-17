# Multi-provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** let Ollama, OpenRouter and OpenCode serve llama-r's chat, embeddings and decision routing, switchable at startup and hot-swappable from the TUI.

**Architecture:** the three existing ports stay unchanged; new adapters translate each provider's wire format into them. `AppState` gains a `tokio::sync::RwLock<Arc<ProviderStack>>` holding all provider-dependent services; handlers clone the `Arc` before any await, so an old stack stays alive until in-flight requests finish. Config and both setup paths build a stack and install it.

**Tech Stack:** Rust 2021, reqwest 0.13 (json, stream), tokio, serde, dialoguer for CLI prompts, ratatui/crossterm for the TUI.

**Spec:** `docs/superpowers/specs/2026-11-17-multi-provider-design.md`

## Global Constraints

- One `PROVIDER` selects all three services: `ollama | openrouter | opencode`, default `ollama`.
- No port trait changes: `LLMProvider`, `EmbeddingProvider`, `DecisionRouter` keep their exact signatures.
- No `Arc<RwLock<..>>` is held across an `.await`; clone the `Arc` first, then drop the guard.
- API keys never go in a URL or into `/api/health`; they are stored in `.env`, never logged.
- No provider failover inside one request, and no streaming for non-Ollama providers — `stream: true` with OpenRouter/OpenCode returns `AppError::Unavailable`.
- Each wire adapter owns its serde structs; no provider-specific field leaks into `domain::models`.
- Commit trailer `Co-Authored-By: opencode <opencode@local>`. Conventional Commits, lower-case imperative subject, no trailing period.
- Baseline: 491 tests passing, fmt/clippy/audit clean.

## Review Focus

1. **A hot swap breaks an in-flight request or holds a lock across an await.** → Task 5: an in-flight request started before the swap completes on the old stack; a new one sees the new one; no test may pass while a guard is held across `.await`.
2. **A secret leaks into logs, health or the URL.** → Task 4 wire tests assert the key appears only in the `Authorization` header; Task 7 asserts `/api/health` never contains the key.
3. **OpenCode with no embeddings silently degrades RAG.** → Task 5 asserts the stack has `embeddings: None` and health reports why.
4. **A decision adapter mis-parses a refusal or a different answer shape.** → Task 3: OpenRouter/OpenCode decision wire tests include a `refusal`/error answer and assert `RouterUnavailable`, not a fabricated decision.
5. **The TUI Apply path changes the provider while a request is running and loses fields.** → Task 7: settings Apply test asserts provider changed, other TUI state unchanged, and `.env` persisted.

---

### Task 1: Configuration and provider selection types

**Files:**
- Modify: `src/config.rs`
- Modify: `src/runtime.rs` (startup_plan logging only if needed)

**Interfaces:**
- Produces: `ProviderKind { Ollama, OpenRouter, OpenCode }` with `FromStr`/`Display`; `Config` gains `provider: ProviderKind`, `openrouter_api_key: Option<String>`, `opencode_api_key: Option<String>`, `opencode_url: String`, `openai_base_url: Option<String>`.
- Consumes: existing `Config::from_env()` and `save_to_env()`.

- [ ] **Step 1: Write the failing config tests**

In `src/config.rs` tests:

```rust
#[test] fn provider_defaults_to_ollama() { /* env cleared; assert ProviderKind::Ollama */ }
#[test] fn provider_parses_all_three_kinds() { /* "openrouter" and "opencode" */ }
#[test] fn save_to_env_round_trips_a_key_with_equals_and_spaces() { /* key = "ab=c d" */ }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --target-dir target-tests --lib config::tests::provider`
Expected: FAIL — no `ProviderKind`.

- [ ] **Step 3: Implement**

Add `ProviderKind` with `Default = Ollama`. `from_env()` reads `PROVIDER`, `OPENROUTER_API_KEY`, `OPENCODE_API_KEY`, `OPENCODE_URL` (default `http://127.0.0.1:4096`), `OPENAI_BASE_URL`. Unknown provider value is `AppError::Config` naming the value and the three accepted ones. Extend `save_to_env()`'s `updates` array with the new keys, using the same replace-or-append logic. Set `.env` mode `0600` on Unix after write (best-effort: log on failure, never fail the save).

- [ ] **Step 4: Verify and commit**

Run: `cargo test --target-dir target-tests --lib config::tests` → PASS. Full suite → 493 passing.

```bash
git add src/config.rs
git commit -m "feat(config): provider selection and credentials

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 2: Chat adapters for OpenRouter and OpenCode

**Files:**
- Create: `src/providers/openai_compatible.rs`, `src/providers/opencode.rs`
- Modify: `src/providers/mod.rs`
- Create: `tests/provider_chat_wire_tests.rs`

**Interfaces:**
- Produces: `OpenAiCompatibleProvider::new(base_url: String, api_key: String, model: String) -> Self` and `OpenCodeProvider::new(base_url: String, api_key: Option<String>, model: String) -> Self`, both implementing `LLMProvider`. OpenCode's `api_key` is its server password sent as HTTP basic auth — never a Bearer token.
- Consumes: `LLMProvider` signature from `src/providers/mod.rs`.

- [ ] **Step 1: Write the failing wire tests**

In `tests/provider_chat_wire_tests.rs`, using a real `TcpListener` that records the request and returns a canned response:

```rust
#[test] fn openrouter_sends_bearer_and_uses_v1_chat_completions() {
    // assert path == "/v1/chat/completions", Authorization: Bearer <key>,
    // body contains model and messages; response maps to ChatResponse
}
#[test] fn opencode_uses_generate_text_without_an_openai_path() {
    // assert path == "/api/experimental/generate/text", no Bearer when key is None
}
#[test] fn a_stream_request_returns_unavailable_for_both() {
    // chat_stream() -> Err(AppError::Unavailable) naming the provider
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --target-dir target-tests --test provider_chat_wire_tests`
Expected: FAIL — adapters do not exist.

- [ ] **Step 3: Implement the adapters**

`OpenAiCompatibleProvider`: `POST {base}/v1/chat/completions` with `Authorization: Bearer {key}`, body `{model, messages, stream:false}`; map `choices[0].message.content` to `ChatResponse.message.content`. `list_models()` → `GET /v1/models` → `data[].id`. `health_check()` → same endpoint, success = healthy.

`OpenCodeProvider`: `POST {base}/api/experimental/generate/text` with body `{model, text}` (or the documented one-shot schema) and optional basic auth from `api_key`; map the returned text into `ChatResponse`. `list_models()` → `GET /api/model`; `health_check()` → `GET /api/info`.

Both `chat_stream()` return `Err(AppError::Unavailable("streaming is not supported by <provider>"))` without opening a connection.

- [ ] **Step 4: Verify and commit**

Run: `cargo test --target-dir target-tests --test provider_chat_wire_tests` → PASS. Full suite → 496 passing.

```bash
git add src/providers tests/provider_chat_wire_tests.rs
git commit -m "feat(providers): OpenRouter and OpenCode chat adapters

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 3: Decision and embedding adapters

**Files:**
- Create: `src/adapters/router/openai_decisions.rs`, `src/adapters/router/opencode_decisions.rs`
- Create: `src/adapters/rag/openrouter_embeddings.rs`
- Modify: `src/adapters/router/mod.rs`, `src/adapters/rag/mod.rs`
- Create: `tests/provider_decision_wire_tests.rs`

**Interfaces:**
- Produces: `OpenRouterRouter::new(api_key: String, model: String, timeout_ms: u64)`, `OpenCodeRouter::new(base_url: String, api_key: Option<String>, model: String, timeout_ms: u64)` implementing `DecisionRouter`; `OpenRouterEmbeddings::new(api_key: String, model: String, dimensions: usize)` implementing `EmbeddingProvider`.
- Consumes: `DecisionRequest`, `DecisionResponse`, `Question`, `Answer` from `src/ports/router.rs`; `EmbeddingProvider::embed(texts) -> Result<Vec<Vec<f32>>, String>`.

- [ ] **Step 1: Write the failing wire tests**

`tests/provider_decision_wire_tests.rs`:

```rust
#[test] fn openrouter_decisions_posts_to_alpha_decisions() {
    // path, Bearer header, body.model/state/questions; response answers map
}
#[test] fn opencode_decisions_posts_to_experimental_generate_decisions() { /* same */ }
#[test] fn a_refusal_answer_becomes_router_unavailable() {
    // a `type: "refusal"` answer must not be read as a DecisionAnswer
}
#[test] fn openrouter_embeddings_sends_a_batched_input() {
    // POST /api/v1/embeddings, Bearer, input = all texts in one call
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --target-dir target-tests --test provider_decision_wire_tests`
Expected: FAIL.

- [ ] **Step 3: Implement**

The two routers map `encode_question`-style bodies to each upstream's `questions` object and parse `answers.<name>` back into `Answer::Choice` / `Answer::Noul`. A `refusal` or missing answer returns `Err(AppError::Provider(..))`, which the ladder already reads as `RouterUnavailable`. Reuse the port's `Question`/`Answer` types; no new domain types.

`OpenRouterEmbeddings` sends one batched `POST /api/v1/embeddings` with `input: texts`, `model`, and reads `data[i].embedding`. It retains the legacy Ollama fallback pattern of reporting dimension mismatch as an error, not a silent resize.

- [ ] **Step 4: Verify and commit**

Run: `cargo test --target-dir target-tests --test provider_decision_wire_tests` → PASS. Full suite → 500 passing.

```bash
git add src/adapters tests/provider_decision_wire_tests.rs
git commit -m "feat(providers): decision and embedding adapters

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 4: `ProviderStack` and hot-swap accessors

**Files:**
- Modify: `src/api/handlers.rs`
- Modify: `src/services/mod.rs`
- Create: `src/services/provider_stack.rs`
- Create: `tests/api_integration.rs` (add hot-swap test)

**Interfaces:**
- Produces: `pub struct ProviderStack { provider, agent_runtime, rag_store, rag_ingest, embeddings, router, default_model, provider_name }`, all `pub`; `AppState.services: Arc<tokio::sync::RwLock<ProviderStack>>`; accessors `state.services_snapshot() -> Arc<ProviderStack>` and `state.replace_services(ProviderStack)`.
- Consumes: types from Tasks 2 and 3.

- [ ] **Step 1: Write the failing hot-swap test**

In `tests/api_integration.rs`:

```rust
#[tokio::test]
async fn a_provider_swap_does_not_disturb_requests_already_in_flight() {
    // start with provider A (slow fake), begin a chat request,
    // replace_services(provider B), assert A's response completes,
    // send a second request and assert B answered it
}
```

- [ ] **Step 2: Run it to verify failure**

Run: `cargo test --target-dir target-tests --test api_integration a_provider_swap`
Expected: FAIL — `AppState` has no `services`.

- [ ] **Step 3: Implement**

Define `ProviderStack` in `src/services/provider_stack.rs`. Add `services` to `AppState`; remove the direct `provider`, `agent_runtime`, `rag_store`, `rag_ingest`, `default_model` and `router` fields. Add the two accessors; both clone out of the `RwLock` before returning/awaiting. Update `chat_core`, `handlers`, `health`, `runtime` and the tests to use the accessors. `build_app_state` gains one `stack: ProviderStack` parameter replacing the five provider-dependent ones — update all five call sites.

- [ ] **Step 4: Verify and commit**

Run: `cargo test --target-dir target-tests --test api_integration a_provider_swap` → PASS. Full suite → 501 passing. `cargo clippy --all-targets -- -D warnings` clean — a guard held across await is a failure here.

```bash
git add src/api src/services tests/api_integration.rs
git commit -m "refactor: swap providers without rebuilding AppState

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 5: Wire the stack from config and degrade honestly

**Files:**
- Modify: `src/runtime.rs`, `src/api/health.rs`, `src/config.rs`
- Create: `tests/provider_startup_tests.rs`

**Interfaces:**
- Consumes: `ProviderKind` from Task 1, adapters from Tasks 2–3, `ProviderStack` from Task 4.
- Produces: `pub fn build_provider_stack(config: &Config) -> Result<ProviderStack, AppError>` in `runtime.rs`.

- [ ] **Step 1: Write the failing startup tests**

```rust
#[test] fn opencode_without_ollama_has_no_embeddings_and_health_says_why() {
    // build a stack for ProviderKind::OpenCode with OLLAMA_URL empty
    // assert stack.embeddings.is_none() and the health reason names embeddings
}
#[test] fn openrouter_uses_its_embeddings_adapter() { /* ... */ }
```

- [ ] **Step 2: Run to verify failure**
- [ ] **Step 3: Implement `build_provider_stack`**

Match on `config.provider`. Ollama → existing providers. OpenRouter → `OpenAiCompatibleProvider` + `OpenRouterRouter` + `OpenRouterEmbeddings`. OpenCode → `OpenCodeProvider` + `OpenCodeRouter`, embeddings from Ollama only when `OLLAMA_URL` is non-empty, else `None`. Health gains a `provider` object with `name`, `embeddings: bool` and an optional `reason` when a capability is missing. No API key in any field.

- [ ] **Step 4: Verify and commit** — full suite → 503 passing, audit clean.

```bash
git add src/runtime.rs src/api/health.rs src/config.rs tests/provider_startup_tests.rs
git commit -m "feat: build the provider stack from configuration

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 6: Extend CLI setup for the new providers

**Files:** Modify `src/cli/interactive.rs`, `src/runtime.rs`

**Interfaces:**
- Produces: `run_interactive_setup` returns `(ProviderStack, String)` instead of `(Arc<dyn LLMProvider>, String)`.
- Consumes: `build_provider_stack` from Task 5.

- [ ] **Step 1: Write a test that the provider list offers all three**

Unit-test the pure list function if one exists; otherwise extract `fn provider_choices() -> &'static [&'static str]` returning `["Ollama", "OpenRouter", "OpenCode"]` and test it.

- [ ] **Step 2: Run to verify failure**
- [ ] **Step 3: Implement**

Replace the placeholder with the three choices. For OpenRouter/OpenCode, prompt with `dialoguer::Password` for the key (accept empty to reuse the env value), then build a stack from a temporary `Config` and run `health_check()` with the existing Retry / Edit / Exit loop. Model selection: Ollama/OpenRouter from `list_models()`, OpenCode by text input. Return the stack. Setup remains non-fatal in `runtime.rs`.

- [ ] **Step 4: Verify and commit** — full suite → 504 passing.

```bash
git add src/cli/interactive.rs src/runtime.rs
git commit -m "feat(setup): choose a provider and enter credentials

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 7: TUI settings screen with hot Apply

**Files:**
- Modify: `src/tui/keys.rs`, `src/tui/app.rs`, `src/tui/app/tests.rs`, `src/tui/views/mod.rs`
- Create: `src/tui/views/settings.rs`

**Interfaces:**
- Produces: `CurrentView::Settings` variant, `KeyTarget::Settings` if it needs its own keys, `render_settings(f, body, app_state) -> ...`, and `TuiApp::apply_settings()`.
- Consumes: `Config::save_to_env`, `build_provider_stack`, `state.replace_services`.

- [ ] **Step 1: Write the failing settings tests**

In `src/tui/app/tests.rs`:

```rust
#[test] fn settings_apply_swaps_the_provider_and_persists_it() { /* ... */ }
#[test] fn settings_rejects_an_empty_openrouter_key_without_changing_the_stack() { /* ... */ }
```

Plus a render test in `tests/tui_visual_tests.rs` asserting the settings view keeps the chrome.

- [ ] **Step 2: Run to verify failure**
- [ ] **Step 3: Implement**

Add `Settings` to `CurrentView` and `KeyTarget`; extend `hints_for` with its pairs. The screen is a **modal over Dashboard**, not a seventh bar name — the bar-width guard in `tests/tui_chrome.rs` pins six names. Three fields (provider, masked key, model) and `Apply`/`Cancel`, following `views/agent_form.rs` navigation. `apply_settings()` validates, builds the stack asynchronously with an "Applying…" state, calls `save_to_env()`, then `replace_services()`. On failure it shows the error inline and keeps the old stack.

- [ ] **Step 4: Verify and commit** — full suite → 507 passing, chrome guards still green.

```bash
git add src/tui tests/tui_visual_tests.rs
git commit -m "feat(tui): settings screen with hot provider swap

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 8: Documentation

**Files:** Modify `AGENTS.md`, `.env.example`, `README.md`

- [ ] **Step 1: Document the three providers**, their env vars, the streaming limitation, the embeddings exception, and the settings screen. Add the new endpoints to `AGENTS.md`'s API section only if llama-r's own API changed (it should not).
- [ ] **Step 2: Align `.env.example`** with `Config::from_env()` defaults, key by key.
- [ ] **Step 3: Verify and commit** — `grep -c PROVIDER` non-zero in both; full gates green.

```bash
git add AGENTS.md .env.example README.md
git commit -m "docs: describe the three provider backends

Co-Authored-By: opencode <opencode@local>"
```

---

## Final verification

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo audit`, `cargo test --target-dir target-tests` — all clean, 507 passing.
- No API key appears in `/api/health`, logs, or any URL, asserted by tests.
- A live smoke against OpenRouter with a real key is optional and must be labelled as such; CI uses recorded fixtures.
