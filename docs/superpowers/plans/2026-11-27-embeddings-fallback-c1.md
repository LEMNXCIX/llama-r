# Fallback de embeddings para OpenCode/Anthropic (Fase 10-C1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Dar a OpenCode y Anthropic un embeddings provider vía `OPENAI_BASE_URL` + `OPENROUTER_API_KEY` cuando no hay Ollama, sin variables nuevas ni adaptadores nuevos.

**Architecture:** Un helper en `src/runtime.rs` complementa el `ollama_embeddings_fallback` existente; ambos se encadenan con `.or_else()` en los brazos OpenCode/Anthropic de `build_provider_stack`. Boot, RAG y Settings Apply lo heredan automáticamente. Se reutilizan `OpenRouterEmbeddings::with_endpoint` y `OpenAiCompatibleEmbeddings::new` según el origen.

**Tech Stack:** Rust 1.97, reqwest, `Arc<dyn EmbeddingProvider>`, tokio, axum, utoipa.

**Spec:** `docs/superpowers/specs/2026-11-27-embeddings-fallback-c1-design.md`

## Global Constraints

- SIN variables de entorno nuevas: se reutilizan `OPENAI_BASE_URL` (`Config.openai_base_url`) y `OPENROUTER_API_KEY` (`Config.openrouter_api_key`).
- Prioridad: Ollama (si `OLLAMA_URL` no vacío) → OpenAI-compatible → `None`.
- Sin key ⇒ `None` con razón; nunca `Bearer ` vacío. Nunca loguear/health la key.
- Origen OpenRouter ⇒ `OpenRouterEmbeddings::with_endpoint(base, key, model, dims)` (path `/api/v1/embeddings`); otro ⇒ `OpenAiCompatibleEmbeddings::new(base, key, model, dims)` (path `/v1/embeddings`).
- NO cambiar `src/providers/capabilities.rs` (catalog de modelo, distinto del stack).
- `cargo test --target-dir target-tests` MUST pasar (988 tests); fmt/clippy limpios; gates de Fase 10-B (audit/deny/coverage) verdes.

## Review Focus

- Sin URL ni key ⇒ `None`; health `embeddings: false` con reason nombrando `OLLAMA_URL` **y** `OPENAI_BASE_URL/OPENROUTER_API_KEY`; key nunca en reason. Test en Task 2.
- URL presente pero key vacía ⇒ `None` (no `Bearer ` vacío). Test en Task 2.
- Con Ollama configurada, la nueva fallback NO se usa (prioridad). Test en Task 2.
- Origen OpenRouter elige `OpenRouterEmbeddings` (path `/api/v1/embeddings`), no el compatible (`/v1/embeddings`). Test en Task 3 (fake server).
- RAG con fallback: `/api/rag/ingest` 200 en vez de 501; sin fallback sigue 501. Tests en Task 3.

---

### Task 1: Helper `openai_compatible_embeddings_fallback` (red)

**Files:**
- Modify: `src/runtime.rs` (junto a `ollama_embeddings_fallback` :1071-1088)
- Test: `tests/embeddings_fallback_tests.rs` (nuevo)

**Interfaces:**
- Consumes: `Config.openai_base_url: Option<String>`, `Config.openrouter_api_key: Option<String>` (ya existen); `OpenRouterEmbeddings::with_endpoint(base, key, model, dims)`; `OpenAiCompatibleEmbeddings::new(base, key, model, dims)`.
- Produces: `fn openai_compatible_embeddings_fallback(config: &Config, provider: &str) -> Option<Arc<dyn EmbeddingProvider>>` (debe ser `pub(crate)` o añadir `#[cfg(test)]` accessor si el test es de integración — ver nota).

- [ ] **Step 1: Write the failing tests** in `tests/embeddings_fallback_tests.rs`:

Use el patrón de `tests/provider_startup_tests.rs` (`EnvGuard::fresh`, `test_lock`, helper `config(ProviderKind)`). Exponga el helper como `pub` desde `llama_r::runtime` para el test de integración, o coloque estos tests como `#[cfg(test)] mod tests` dentro de `src/runtime.rs` si prefiere no ampliar API — **decida uno y documente la elección en el report**.

```rust
#[test]
fn openai_compatible_fallback_is_none_without_base_url() {
    let cfg = /* Config con openai_base_url=None, openrouter_api_key=Some("k") */;
    assert!(llama_r::runtime::openai_compatible_embeddings_fallback(&cfg, "opencode").is_none());
}

#[test]
fn openai_compatible_fallback_is_none_without_api_key() {
    let cfg = /* Config con openai_base_url=Some("https://openrouter.ai/api/v1"), openrouter_api_key=None */;
    assert!(llama_r::runtime::openai_compatible_embeddings_fallback(&cfg, "anthropic").is_none());
}

#[test]
fn openai_compatible_fallback_builds_embedder_with_config_dimensions() {
    let cfg = /* openai_base_url=Some("https://example.com/v1"), api_key=Some("k"), embedding_dimensions=1536 */;
    let fb = llama_r::runtime::openai_compatible_embeddings_fallback(&cfg, "opencode")
        .expect("URL + key must build an embedder");
    assert_eq!(fb.dimensions(), 1536);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --target-dir target-tests --test embeddings_fallback_tests`
Expected: FAIL — función no existe.

- [ ] **Step 3: Implement the helper**

En `src/runtime.rs`, junto a `ollama_embeddings_fallback`:

```rust
pub fn openai_compatible_embeddings_fallback(
    config: &Config,
    provider: &str,
) -> Option<Arc<dyn EmbeddingProvider>> {
    let base = config.openai_base_url.as_ref().filter(|u| !u.trim().is_empty())?;
    if config.openrouter_api_key.as_ref().filter(|k| !k.is_empty()).is_none() {
        tracing::warn!(provider, "embeddings fallback: OPENAI_BASE_URL is set but OPENROUTER_API_KEY is empty");
        return None;
    }
    let key = config.openrouter_api_key.clone().unwrap();
    let model = config.embedding_model.clone();
    let dims = config.embedding_dimensions;
    let embedded: Arc<dyn EmbeddingProvider> = if base.contains("openrouter.ai") || base.trim_end_matches('/').ends_with("/api/v1") {
        Arc::new(OpenRouterEmbeddings::with_endpoint(base.clone(), key, model, dims))
    } else {
        Arc::new(OpenAiCompatibleEmbeddings::new(base.clone(), key, model, dims))
    };
    Some(embedded)
}
```

El warning MUST NO incluir el valor de la key. Importe `OpenAiCompatibleEmbeddings` y `OpenRouterEmbeddings` si aún no están en scope (ya están en `src/runtime.rs:6-7`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --target-dir target-tests --test embeddings_fallback_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/runtime.rs tests/embeddings_fallback_tests.rs
git commit -m "feat(rag): add OpenAI-compatible embeddings fallback helper"
```

---

### Task 2: Cableado en OpenCode/Anthropic + reason strings

**Files:**
- Modify: `src/runtime.rs` (brazos OpenCode :1179 y Anthropic :1228; `ollama_embeddings_fallback` :1078; `build_rag` warn :1283)
- Modify: `src/api/health.rs:75-82`
- Modify: `src/api/rag_api.rs:32-39` (`rag_unavailable`)
- Modify: `src/api/chat_core.rs` (chat-time degradation warning — grep por el string de OLLAMA_URL)
- Test: `tests/provider_startup_tests.rs`, `tests/provider_companions_tests.rs` (actualizar aserciones)

**Interfaces:**
- Consumes: `openai_compatible_embeddings_fallback` (Task 1, `pub` desde `runtime`).
- Produces: OpenCode/Anthropic con embeddings cuando hay URL+key; unchanged para otros 4 providers.

- [ ] **Step 1: Write the failing tests**

En `tests/provider_startup_tests.rs`, añada un test que configure `OPENAI_BASE_URL`+`OPENROUTER_API_KEY` y `OLLAMA_URL` vacío y compruebe `stack.embeddings.is_some()` y health `embeddings == true` para `ProviderKind::OpenCode` y `Anthropic`. En `tests/provider_companions_tests.rs`, añada `opencode_with_embeddings_fallback_builds_rag_store` que verifique `services.rag_store.is_some()` con la fallback. Mantenga los tests existentes de "sin Ollama ⇒ None" pero **asegúrese de que el EnvGuard deje `OPENAI_BASE_URL` vacío** (si no, fallarán — actualice el fixture o el test según corresponda).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --target-dir target-tests --test provider_startup_tests --test provider_companions_tests`
Expected: FAIL — los brazos aún no usan la fallback.

- [ ] **Step 3: Implement the wiring**

- En los brazos OpenCode y Anthropic de `build_provider_stack`:
  `embeddings = ollama_embeddings_fallback(config, "<kind>").or_else(|| openai_compatible_embeddings_fallback(config, "<kind>"));`
- Actualice el reason string en `src/api/health.rs` a:
  `"embeddings are disabled: the '{provider}' provider does not serve them and no OLLAMA_URL or OPENAI_BASE_URL/OPENROUTER_API_KEY fallback is configured"`.
- Actualice el warn en `ollama_embeddings_fallback` (:1078) y `build_rag` (:1283) y `rag_unavailable()` en `src/api/rag_api.rs` para nombrar ambas rutas. Busque y actualice el chat-time warning con el mismo fraseo (grep `OLLAMA_URL` en `src/api/chat_core.rs`).
- Ningún mensaje incluye la key.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --target-dir target-tests --test provider_startup_tests --test provider_companions_tests && cargo test --target-dir target-tests`
Expected: PASS; suite verde.

- [ ] **Step 5: Commit**

```bash
git add src/runtime.rs src/api/health.rs src/api/rag_api.rs src/api/chat_core.rs tests/provider_startup_tests.rs tests/provider_companions_tests.rs
git commit -m "feat(rag): wire embeddings fallback into OpenCode and Anthropic stacks"
```

---

### Task 3: E2E con fake embeddings server

**Files:**
- Create: `tests/embeddings_fallback_wire_tests.rs` (o script temporal + test de integración)

**Interfaces:**
- Consumes: make stack de Task 1/2.
- Produces: evidencia E2E de que `/api/rag/ingest` pasa de 501 a 200.

- [ ] **Step 1: Write the failing test**

Un test `#[tokio::test]` que:
1. Arranca un servidor axum local efímero con `POST /v1/embeddings` que devuelve `{"data":[{"embedding":[0.1, 0.2, ...]}]}` con la dimensión configurada (patrón de `src/adapters/rag/openrouter_embeddings.rs` tests).
2. Construye `Config` con `PROVIDER=opencode`, `OLLAMA_URL=""`, `OPENAI_BASE_URL=http://127.0.0.1:<puerto>`, `OPENROUTER_API_KEY=dummy`, `EMBEDDING_MODEL=test-model`, `EMBEDDING_DIMENSIONS=<N>`, `RAG_ENABLED=true`.
3. Construye el stack y comprueba `stack.embeddings.is_some()`.
4. Llama directamente a `stack.embeddings.unwrap().embed(&["hola".into()])` y verifica que devuelve 1 vector de dimensión N.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --target-dir target-tests --test embeddings_fallback_wire_tests`
Expected: FAIL (o pase solo si el helper ya funciona — si pasa, documente que el test es de verificación, no TDD puro).

- [ ] **Step 3: Implement / verify**

Si el test falla por path incorrecto (`/api/v1/embeddings` vs `/v1/embeddings`), ajuste la detección de OpenRouter en el helper (Task 1) para que el fake con `/v1/embeddings` use `OpenAiCompatibleEmbeddings`. No cambie los adapters.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --target-dir target-tests --test embeddings_fallback_wire_tests && cargo test --target-dir target-tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add tests/embeddings_fallback_wire_tests.rs src/runtime.rs
git commit -m "test(rag): verify embeddings fallback wire against local fake server"
```

---

### Task 4: Docs y regresión final

**Files:**
- Modify: `AGENTS.md` (líneas 21, 46, 186, 611-617), `README.md`, `.env.example` (comentario de `OPENAI_BASE_URL`)

**Interfaces:** ninguna nueva.

- [ ] **Step 1: Update docs**

En `AGENTS.md:21` cambiar "with no `OLLAMA_URL` fallback RAG is disabled" por "with no `OLLAMA_URL` or `OPENAI_BASE_URL`/`OPENROUTER_API_KEY` fallback RAG is disabled". Igual en `:46`, `:186`, y en el bullet de RAG `:614-617` añadir que `OPENAI_BASE_URL` + `OPENROUTER_API_KEY` pueden servir embeddings a OpenCode/Anthropic. En `.env.example` añadir comentario junto a `OPENAI_BASE_URL`. En `README.md` sección RAG, nota equivalente.

- [ ] **Step 2: Run E2E manual (si aplica) + full regression**

Arranque el binario con `PROVIDER=opencode`, `OLLAMA_URL=`, `OPENAI_BASE_URL` apuntando al fake, `OPENROUTER_API_KEY=dummy`, puerto libre, `LLAMA_R_DIR` aislado; verifique `GET /api/health` ⇒ `embeddings: true` y `POST /api/rag/ingest` (X-Debug + X-Admin-Token) ⇒ 200. Mate el servidor. Entonces `cargo test --target-dir target-tests && cargo fmt --check && cargo clippy --all-targets -- -D warnings && bash tests/ci_config_tests.sh`. Todo verde; sin servidores vivos.

- [ ] **Step 3: Commit**

```bash
git add AGENTS.md README.md .env.example
git commit -m "docs: document embeddings fallback for OpenCode and Anthropic"
```
