# Compactación real de contexto (Fase 10-C3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Instalar `SummarizingCompactor` en el boot con un summarizer real para los 6 proveedores, sin descartar historial con summarizers vacíos.

**Architecture:** `SummarizingCompactor` ya existe; C3 añade dos summarizers (`ChatCompletionsSummarizer`, `OpenCodeSummarizer`), amplía `build_summarizer` a los 6 providers y cablea el compactor en `runtime.rs:294`, compartiendo el mismo `Arc<dyn Summarizer>` con el store de historial. Si no hay URL/key usable, se instala `NoopCompactor` (nunca summarizing+noop).

**Tech Stack:** Rust 1.97, reqwest vía `net::http_client`, axum fake servers en tests, tokio.

**Spec:** `docs/superpowers/specs/2026-11-27-context-compaction-c3-design.md`

## Global Constraints

- Summarizers nuevos: `ChatCompletionsSummarizer { base_url, api_key, model }` (POST `{base}/chat/completions`, Bearer, lee `choices[0].message.content`) y `OpenCodeSummarizer { url, password: Option<String>, model }` (POST `{url}/api/experimental/generate/text`, body `Model.Ref`+prompt, Basic auth, lee `data.text`/`text`).
- Mismo prompt que `OllamaSummarizer`; timeout 30 s vía `net::http_client`; errores sin key/password en el mensaje; vacío ⇒ `Ok("")`.
- `build_summarizer` selecciona por ProviderKind usando URL/key existentes (perfil para Anthropic/Gemini/OpenAI-compatible; `openrouter_api_key` para OpenRouter; `opencode_url`/`opencode_api_key` para OpenCode; `ollama_url` para Ollama). Sin URL/key ⇒ Noop con warning.
- **NUNCA `SummarizingCompactor` con `NoopSummarizer`** ⇒ instalar `NoopCompactor` en ese caso (D10).
- Feature `history`: cableo bajo `#[cfg(feature = "history")]`; light builds mantienen `NoopCompactor` (D11).
- Compartir el mismo `Arc<dyn Summarizer>` entre store de historial y compactor; Apply lo reconstruye desde el mismo handle.
- Sin env vars nuevas, sin tokenizer, sin cambio a `capabilities.rs` ni `## Conversation memory`.
- `cargo test --target-dir target-tests` MUST pasar (1004 tests); fmt/clippy limpios.

## Review Focus

- Noop summarizer + summarizing compactor = silent-drop de historial (`context_pipeline_tests.rs:203-229`). Test en Task 3 verifica que el boot instala `NoopCompactor` cuando el summarizer es Noop.
- `ChatCompletionsSummarizer` con base `https://openrouter.ai/api/v1` ⇒ path correcto `/api/v1/chat/completions` (no duplicado `/api/v1/api/v1`). Test en Task 1.
- `OpenCodeSummarizer` usa el wire `Model.Ref` validado (`{"model":{"id","providerID","modelID"},"prompt"}`) y Basic auth sin password en logs. Test en Task 1.
- Build `--no-default-features` sigue compilando (compactor bajo feature gate). Verificado en Task 3.
- Settings Apply cambia el provider y el compactor se reconstruye (no queda el del boot). Test en Task 3/4.

---

### Task 1: Summarizers `ChatCompletionsSummarizer` y `OpenCodeSummarizer`

**Files:**
- Modify: `src/adapters/history/summarizer.rs` (añadir ambos structs + impl `Summarizer`)
- Test: `tests/summarizer_wire_tests.rs` (nuevo)

**Interfaces:**
- Consumes: `Summarizer`/`ExportMessage` de `src/ports/history.rs:58-67`; `net::http_client(Duration)` (Fase 10-A).
- Produces: `pub struct ChatCompletionsSummarizer` y `pub struct OpenCodeSummarizer`, ambos `impl Summarizer`.

- [ ] **Step 1: Write the failing tests** (fake servers locales):

```rust
#[tokio::test]
async fn chat_completions_summarizer_posts_bearer_and_parses_choice() { /* fake axum en 127.0.0.1:0 */ }
// asserted: request path == "/api/v1/chat/completions" (o "/v1/chat/completions" según base);
//           header Authorization == "Bearer dummy"; body.model == "m"; body.stream == false;
//           summarize() == "resumen" (choices[0].message.content trim)

#[tokio::test]
async fn open_code_summarizer_posts_model_ref_with_basic_auth() { /* fake axum */ }
// asserted: path == "/api/experimental/generate/text"; body.model.id/providerID/modelID == "m";
//           body.prompt contiene el texto del mensaje; respuesta {"data":{"text":" resumen "}} ⇒ "resumen"

#[tokio::test]
async fn summarizers_return_empty_on_empty_content_and_error_without_leaking_key() { /* ... */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --target-dir target-tests --test summarizer_wire_tests`
Expected: FAIL — structs no existen.

- [ ] **Step 3: Implement**

En `summarizer.rs`, reutilizar el prompt builder de `OllamaSummarizer` (extraer `fn build_prompt(messages) -> String` compartido si hace falta). `ChatCompletionsSummarizer::summarize`: normalizar `base_url` (si termina `/api/v1`, quitarlo y usar `/api/v1/chat/completions`; si termina `/v1`, quitarlo y usar `/v1/chat/completions`; si no, `/v1/chat/completions`) — **verificar el path real con el fake** y ajustar. `net::http_client(30s)`, `.bearer_auth(&key)`, body `{"model","messages":[{"role":"user","content":prompt}],"stream":false}`, parse `choices[0].message.content`. `OpenCodeSummarizer::summarize`: `POST {url}/api/experimental/generate/text` con `{"model":{"id":model,"providerID":"opencode","modelID":model},"prompt":prompt}`, `.basic_auth("opencode", password.as_deref())` si Some, parse `data.text` o `text`. Errores: `format!("...: {e}")` sin key/password.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --target-dir target-tests --test summarizer_wire_tests && cargo test --target-dir target-tests`
Expected: PASS; suite verde.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/history/summarizer.rs tests/summarizer_wire_tests.rs
git commit -m "feat(summarizer): add chat-completions and opencode summarizers"
```

---

### Task 2: Ampliar `build_summarizer` a los 6 providers

**Files:**
- Modify: `src/runtime.rs:1364-1390` (`build_summarizer`)
- Modify: `tests/history_integration_tests.rs:1264,1289` (llamadas a la firma antigua)
- Test: `tests/summarizer_selection_tests.rs` (nuevo) o ampliar `history_integration_tests.rs`

**Interfaces:**
- Consumes: los dos summarizers de Task 1; `profile_for`/`profile_api_key` (`src/runtime.rs:968-990`) para Anthropic/Gemini/OpenAI-compatible; `Config` con `openrouter_api_key`, `opencode_url`, `opencode_api_key`, `ollama_url`.
- Produces: `build_summarizer` con nueva firma y selección por kind + detección de Noop.

- [ ] **Step 1: Write the failing tests**

Para cada `ProviderKind` con URL/key válidos, asserts de tipoStrategy (use `format!("{:?}")` del Arc o un `fn strategy_name()` en el trait/impls; elija uno y documente). Sin URL/key ⇒ Noop + warning. Cubra: Ollama, OpenRouter, OpenAI-compatible, Anthropic, Gemini, OpenCode, y un caso sin key.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --target-dir target-tests --test summarizer_selection_tests`
Expected: FAIL — firma/selección antigua.

- [ ] **Step 3: Implement**

Cambiar firma a `pub fn build_summarizer(config: &Config, provider: ProviderKind, default_model: &str) -> Arc<dyn Summarizer>` (o mantener la vieja y añadir una variante — documentar). Match:
Ollama → `OllamaSummarizer(config.ollama_url, model)`; OpenRouter → `ChatCompletionsSummarizer` con `config.openai_base_url` o `https://openrouter.ai/api/v1` + `config.openrouter_api_key`; OpenAI-compatible/Anthropic/Gemini → `ChatCompletionsSummarizer` con `profile_for(...).base_url` + `profile_api_key(...)`; OpenCode → `OpenCodeSummarizer(config.opencode_url, config.opencode_api_key, model)`. Si la URL o key resulta vacía ⇒ `NoopSummarizer` + warning nombrando proveedor y variable (nunca valor). Actualizar los 2 callers de `history_integration_tests.rs`. Añadir un mecanismo para detectar Noop (p. ej. `fn is_noop(&self) -> bool { false }` en el trait con override en `NoopSummarizer`, o un enum `SummarizerKind` devuelto por `build_summarizer`) — **documentar la elección**; se usará en Task 3.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --target-dir target-tests --test summarizer_selection_tests && cargo test --target-dir target-tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/runtime.rs tests/history_integration_tests.rs tests/summarizer_selection_tests.rs
git commit -m "feat(runtime): select a real summarizer for all six providers"
```

---

### Task 3: Cablear `SummarizingCompactor` en boot y Apply

**Files:**
- Modify: `src/runtime.rs:284-294` (boot compactor), `:1436` (store usa el mismo summarizer), `:1644` (comentario CompanionHandles)
- Modify: `src/tui/app.rs` (finish_apply, si es necesario — verificar que ya reconstruye desde AppState)
- Test: `tests/compaction_wiring_tests.rs` (nuevo)

**Interfaces:**
- Consumes: `build_summarizer` (Task 2), `SummarizingCompactor`, `NoopCompactor`; mecanismo de detección Noop (Task 2).
- Produces: boot instala summarizing (real) o noop (sin summarizer); Apply reconstruye igual.

- [ ] **Step 1: Write the failing tests**

- Boot con `PROVIDER=ollama` ⇒ el `Arc<dyn ContextCompactor>` en AppState es `SummarizingCompactor` (no Noop).
- Boot con URL/key vacíos ⇒ es `NoopCompactor` (D10 — verificar que NO es summarizing+noop).
- Boot con `PROVIDER=opencode` + fake ⇒ summarizing con `OpenCodeSummarizer`.
- Apply: cambiar provider en config y llamar `finish_apply` ⇒ el compactor en el estado nuevo es del tipo nuevo.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --target-dir target-tests --test compaction_wiring_tests`
Expected: FAIL — boot instala Noop fijo.

- [ ] **Step 3: Implement**

En `runtime.rs:284-294`: bajo `#[cfg(feature = "history")]`, `let summarizer = build_summarizer(&config, config.provider, &config.default_model); let compactor: Arc<dyn ContextCompactor> = if summarizer_is_noop(&summarizer) { Arc::new(NoopCompactor) } else { Arc::new(SummarizingCompactor::new(summarizer.clone())) };`; bajo `#[cfg(not(feature="history"))]`, Noop. En `:1436`, reutilizar el mismo `summarizer.clone()` en vez de llamar `build_summarizer` otra vez (eliminar la segunda llamada). Actualizar el comentario de `:1644`. Verificar que `finish_apply` ya reconstruye vía `build_companions_for_state` desde AppState; si el compactor está en AppState, no hace falta cambio en app.rs — documentar.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --target-dir target-tests --test compaction_wiring_tests && cargo check --no-default-features && cargo test --target-dir target-tests`
Expected: PASS; light build compila.

- [ ] **Step 5: Commit**

```bash
git add src/runtime.rs tests/compaction_wiring_tests.rs src/tui/app.rs
git commit -m "feat(runtime): install real compaction with a provider summarizer"
```

---

### Task 4: Docs, E2E y regresión final

**Files:**
- Modify: `AGENTS.md:679`, `README.md:308` y secciones relacionadas; `.env.example` si aplica (no debería — sin vars nuevas)
- Test: script temporal `/tmp/opencode/llama-r-c3-e2e.sh`

**Interfaces:** Tasks 1-3 completos.

- [ ] **Step 1: Update docs**

Reemplazar en AGENTS.md "Boot installs `NoopCompactor` by default; deployed summarization awaits a real strategy" por "Boot installs `SummarizingCompactor` with a provider-appropriate Summarizer (Ollama/ChatCompletions/OpenCode wire); `NoopCompactor` only when no usable URL/key"; actualizar el bullet de Degradation y la nota de summarizer en runtime. README.md: equivalente en español (`:308`). No tocar topics de C4 ni capabilities.

- [ ] **Step 2: E2E**

Conversación multi-turno con `PROVIDER=ollama` y un summarizer fake (o el real), historial suficiente para que el compactor recorte (budget bajo vía `max_context_tokens` en el agente). Verificar que un turno posterior contiene `## Conversation memory` en el system prompt y que las instrucciones siguen byte-for-byte. Repetir con summarizer Noop ⇒ sin `## Conversation memory` y sin pérdida de historial. Mate el servidor.

- [ ] **Step 3: Full regression**

Run: `cargo test --target-dir target-tests && cargo fmt --check && cargo clippy --all-targets -- -D warnings && bash tests/ci_config_tests.sh && cargo check --no-default-features`
Expected: todo verde; 1004+ tests; sin servidores vivos.

- [ ] **Step 4: Commit**

```bash
git add AGENTS.md README.md
git commit -m "docs: document real context compaction across providers"
```
