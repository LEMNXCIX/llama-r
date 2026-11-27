# Diseño: fallback de embeddings para OpenCode/Anthropic (Fase 10-C1)

- **Fecha:** 2026-11-27
- **Estado:** Diseño aprobado en brainstorming; pendiente de revisión del spec
- **Tipo:** Arquitectural — cambia cómo se construye el stack de embeddings para dos providers
- **Perfil:** LAN / localhost, uso personal (hereda A y B)
- **Parte:** 1 de 4 de la Fase 10-C (C1 embeddings → C2 SSE → C3 compactación → C4 step-replay)

## 1. Contexto y problema

`build_provider_stack` (`src/runtime.rs:1097-1257`) da embeddings nativos a Ollama, OpenRouter, OpenAI-compatible y Gemini. Para **OpenCode** (`:1179`) y **Anthropic** (`:1228`) usa `ollama_embeddings_fallback(config, provider)` (`:1071-1088`), que devuelve `Some` solo si `OLLAMA_URL` no está vacío. Sin Ollama, el stack queda sin embeddings, `build_rag` no construye store (`:1281-1283`) y `/api/rag/*` responde `501` con un mensaje que nombra embeddings. `GET /api/health` reporta `embeddings: false` con `reason: "…no OLLAMA_URL fallback is configured"` (`src/api/health.rs:75-82`).

Existe `OpenAiCompatibleEmbeddings::new(base_url, api_key, model, dimensions)` (`src/adapters/rag/openai_compatible_embeddings.rs:19-39`), que ya habla `POST {base}/v1/embeddings` con Bearer y delegación a `OpenRouterEmbeddings`. No hace falta un adaptador nuevo: basta una segunda fallback que lo construya con la key de OpenRouter ya presente en `Config`.

Objetivo: que OpenCode y Anthropic puedan tener RAG cuando el operador configura un endpoint de embeddings OpenAI-compatible, sin Ollama, y sin inventar credenciales ni vectores.

## 2. Decisiones

| # | Decisión | Justificación |
|---|---|---|
| D1 | Para OpenCode/Anthropic, orden de fallback: **1º Ollama** (existente) → **2º OpenAI-compatible** (nuevo) → `None`. | No rompe despliegues que ya usan Ollama como fallback. |
| D2 | El fallback OpenAI-compatible se activa solo con `OPENAI_BASE_URL` (`Config.openai_base_url`) **no vacío**; la key se reutiliza de `OPENROUTER_API_KEY` (`Config.openrouter_api_key`). | Usuario eligió reutilizar la key de OpenRouter. `OPENAI_BASE_URL` ya existe en Config — **no se añade variable nueva**. Si el origen es OpenRouter (`https://openrouter.ai/api/v1`), se usa `OpenRouterEmbeddings` (path `/api/v1/embeddings`); si es otro compatible, `OpenAiCompatibleEmbeddings` (path `/v1/embeddings`). El operador es responsable de que la key sirva para ese origen. |
| D3 | Si `OPENROUTER_API_KEY` está vacío/missing, el fallback **no se construye** y se reporta `None` con razón explícita. | Nunca se envía `Bearer ` vacío ni se inventa una key. |
| D4 | Modelo y dimensiones: `EMBEDDING_MODEL` / `EMBEDDING_DIMENSIONS` existentes (defaults `nomic-embed-text`/768 → el operador debe poner `openai/text-embedding-3-small`/1536 para OpenRouter). | No se añaden variables nuevas para modelo/dims. |
| D5 | **No se cambia** `ModelCapabilities.embeddings` en `src/providers/capabilities.rs` para OpenCode/Anthropic. | Esa tabla describe la capability *del modelo en ese proveedor*; el fallback es una decisión de stack, distinta. Mezclarlas cambiaría el comportamiento de streaming/tools. |
| D6 | `reason` de health y log de degrade se actualizan para nombrar **ambas** rutas agotadas ("no OLLAMA_URL and no OPENAI_BASE_URL/OPENROUTER_API_KEY fallback"). | El operador debe saber por qué falta embeddings sin leer código. Ningún valor de key en el mensaje. |
| D7 | `OPENROUTER_API_KEY` sigue siendo secreto: nunca en URL, log, health ni error. | Convención de todo el proyecto (Fase 9). |

RFC 2119: **MUST**, **SHALL**, **SHOULD**, **MAY**, **MUST NOT**.

## 3. Configuración

**No se añade ninguna variable nueva.** Se reutilizan las existentes en `Config`:

- `Config.openai_base_url: Option<String>` — origen OpenAI-compatible (ya existe; en `.env` se documenta como `https://openrouter.ai/api/v1`). Vacío ⇒ sin fallback.
- `Config.openrouter_api_key: Option<String>` — key Bearer (ya existe, secreta). Vacía ⇒ sin fallback.
- `Config.embedding_model` / `embedding_dimensions` — ya existentes. Si el origen es OpenRouter, el operador debe poner un modelo de embedding válido (p. ej. `openai/text-embedding-3-small`) y `EMBEDDING_DIMENSIONS=1536`; el default `nomic-embed-text`/768 es de Ollama y daría dimension mismatch.

Se actualiza el comentario de `.env.example` para documentar este uso de `OPENAI_BASE_URL` + `OPENROUTER_API_KEY` como fallback de embeddings para OpenCode/Anthropic. No se toca `CONFIG_ENV_VARS`.

## 4. Implementación

### 4.1 Helper en `src/runtime.rs`

```rust
/// OpenAI-compatible embeddings fallback for providers with no endpoint of
/// their own. Uses the OpenRouter API key already present in Config; when the
/// URL is set but the key is missing, degrades to None with a named reason
/// rather than sending an empty Bearer credential.
fn openai_compatible_embeddings_fallback(
    config: &Config,
    provider: &str,
) -> Option<Arc<dyn EmbeddingProvider>>;
```

Lógica:
1. `let Some(base) = config.openai_base_url.as_ref().filter(|u| !u.trim().is_empty()) else { tracing::warn!(provider, "embeddings fallback: OPENAI_BASE_URL is empty"); return None; }`
2. `let Some(key) = config.openrouter_api_key.as_ref().filter(|k| !k.is_empty()) else { warn; return None; }` — **nunca** loguear el valor de `key`.
3. Si `base` apunta a OpenRouter (contiene `openrouter.ai`, o termina en `/api/v1`), construir `OpenRouterEmbeddings::with_endpoint(base, key.clone(), model, dims)` para usar el path `/api/v1/embeddings`; si no, `OpenAiCompatibleEmbeddings::new(base, key.clone(), model, dims)` (path `/v1/embeddings`). Ambas ya existen y no requieren adaptador nuevo.

### 4.2 Cableado en `build_provider_stack`

- `ProviderKind::OpenCode` (`:1179`): `embeddings = ollama_embeddings_fallback(config, "opencode").or_else(|| openai_compatible_embeddings_fallback(config, "opencode"));`
- `ProviderKind::Anthropic` (`:1228`): idem con `"anthropic"`.
- Los otros cuatro brazos quedan intactos.
- `build_rag` (`src/runtime.rs:1281`) sigue igual: si hay embeddings, construye store; si no, `Unavailable`.

### 4.3 Reason strings

En `ollama_embeddings_fallback` y en el nuevo helper, los warnings naming `provider` deletrean la ruta agotada. En `src/api/health.rs:75-82` el `reason` cuando `embeddings == false` pasa a:

```
"embeddings are disabled: the '{provider}' provider does not serve them and no OLLAMA_URL or OPENAI_BASE_URL/OPENROUTER_API_KEY fallback is configured"
```

El chat-time degradation warning debe usar la misma frase (buscar el string en `src/api/chat_core.rs` y actualizarlo). Ningún mensaje incluye la key.

## 5. Testing

### 5.1 Unit / integración

- `openai_compatible_embeddings_fallback`: (a) `OPENAI_BASE_URL`+`OPENROUTER_API_KEY` ⇒ `Some` y `dimensions()` = `EMBEDDING_DIMENSIONS`; (b) URL sin key ⇒ `None`; (c) sin URL ⇒ `None`. No hay campo de Config nuevo que testear.
- `build_provider_stack` con `PROVIDER=opencode` / `anthropic`: sin Ollama pero con URL+key ⇒ `stack.embeddings.is_some()`; sin ninguno ⇒ `None`.
- Regresión: `tests/provider_companions_tests.rs::rag_store_is_absent_when_the_selected_provider_has_no_embeddings` sigue pasando (el fake no configura la URL nueva).

### 5.2 E2E (binario real, fake embeddings server local)

1. Arrancar un servidor HTTP falso que responda `POST /v1/embeddings` con vectores de dimensión configurada (patrón de `tests/` con ephemeral server).
2. Arrancar llama-r con `PROVIDER=opencode`, `OLLAMA_URL=` (vacío), `OPENAI_BASE_URL=http://127.0.0.1:<fake>`, `OPENROUTER_API_KEY=<dummy>` y `EMBEDDING_MODEL`/`DIMS` acordes (el fake sirve `/v1/embeddings`).
3. `GET /api/health` ⇒ `embeddings: true`, `reason: null`.
4. `POST /api/rag/ingest` con `X-Debug: true` + `X-Admin-Token` ⇒ `200`, `chunks_written > 0` (antes 501).
5. Repetir con `OPENROUTER_API_KEY` vacío ⇒ health `embeddings: false` con razón que nombra ambas rutas; `/api/rag/ingest` ⇒ 501.
6. Verificar que el log no contiene el valor de la key dummy.

### 5.3 Regresión

`cargo test --target-dir target-tests` (988+ verdes), fmt, clippy, y los gates de CI de Fase 10-B.

## 6. Seguridad

- `OPENROUTER_API_KEY` se envía como `Authorization: Bearer` al endpoint configurado; **MUST NOT** aparecer en URL, log, health ni mensajes de error.
- `OPENAI_BASE_URL` es una URL no secreta; si el operador pone credenciales en la query, es responsabilidad suya y se documenta como tal.
- El fake server del E2E usa un dummy key — nunca la key real de `.env`.

## 7. Riesgos

| Riesgo | Mitigación |
|---|---|
| `OPENROUTER_API_KEY` no válida para el endpoint configurado | El proveedor devuelve error en el primer embed; `embed()` ya propaga el error y RAG degrada. Documentar que la URL debe aceptar esa key. |
| Cambiar el reason string rompa tests que comparan el mensaje exacto | Actualizar los tests que lo pinnean en el mismo cambio; buscar con grep antes de editar. |
| Ollama y la nueva fallback both activos | D1 da prioridad a Ollama; el nuevo helper solo se llama si el primero devuelve `None`. |

## 8. Fuera de alcance

C2 (SSE), C3 (compactación), C4 (step-replay), cambiar `capabilities.rs`, y añadir variables nuevas de embeddings (decisión D2/D3: se reutilizan `OPENAI_BASE_URL` y `OPENROUTER_API_KEY`).

## 9. Plan de implementación (siguiente paso)

Tras la revisión de este spec, invocar `writing-plans`:

1. `Config.embeddings_base_url` + env guard + `.env.example`.
2. Helper `openai_compatible_embeddings_fallback` + cableado en OpenCode/Anthropic + reason strings.
3. Tests de stack/health + E2E con fake embeddings server.
4. Docs (`AGENTS.md`, `README.md`) + regresión completa.

Criterio de aceptación: con `PROVIDER=opencode`/`anthropic` sin Ollama pero con `OPENAI_BASE_URL`+`OPENROUTER_API_KEY`, health reporta `embeddings: true` y `/api/rag/ingest` devuelve 200.
