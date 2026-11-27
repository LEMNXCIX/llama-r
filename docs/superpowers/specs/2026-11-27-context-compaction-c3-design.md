# Diseño: compactación real de contexto (Fase 10-C3)

- **Fecha:** 2026-11-27
- **Estado:** Diseño aprobado en brainstorming; pendiente de revisión del spec
- **Tipo:** Arquitectural — conecta un compactor real en el boot y añade summarizers por proveedor
- **Perfil:** LAN / localhost, uso personal (hereda A/B/C1/C2)
- **Parte:** 3 de 4 de la Fase 10-C (C1 embeddings ✅ → C2 SSE ✅ → C3 compactación → C4 step-replay)

## 1. Contexto y problema

La auditoría señaló que el boot instala `NoopCompactor` (`src/runtime.rs:294`), por lo que la compactación efectiva está desactivada: `ContextAssembler` (`src/services/context_assembler.rs:112-145`) consulta al compactor solo si `budget_tokens > 0`, pero el Noop devuelve el historial sin cambios y sin memoria.

**Hallazgo clave:** `SummarizingCompactor` **ya existe y está testado** (`src/adapters/context/summarizing_compactor.rs`): calcula `available = budget - reserved - system_prompt`, conserva el sufijo más reciente que cabe y resume el prefijo viejo vía `Summarizer`. El hueco es (a) cablearlo en el boot y (b) tener un `Summarizer` real para cada proveedor.

Los summarizers hoy (`src/adapters/history/summarizer.rs`): `OllamaSummarizer` (POST `/api/chat`, 30 s timeout) y `NoopSummarizer` (devuelve `""`). `build_summarizer` (`src/runtime.rs:1374-1388`) solo da el real a Ollama; los demás reciben Noop con warning. El usuario eligió que **todos los proveedores** tengan compactación real.

Objetivo: compactar el historial dentro del presupuesto de contexto para todos los proveedores, reutilizando los clients existentes, sin fabricar resúmenes, sin cambiar las secciones de instrucciones y con degradación fail-closed (un fallo del summarizer nunca rompe el chat).

## 2. Decisiones

| # | Decisión | Justificación |
|---|---|---|
| D1 | Boot instala `SummarizingCompactor` con el `Summarizer` del proveedor. | El compactor ya existe y sus tests pasan; solo falta conectarlo. |
| D2 | Nuevo `ChatCompletionsSummarizer` (`src/adapters/history/summarizer.rs`) que habla `POST {base}/chat/completions` con Bearer y lee `choices[0].message.content` — el mismo wire que `OpenAiCompatibleProvider`. | Cubre OpenRouter, OpenAI-compatible, Anthropic y Gemini (todos tienen `base_url` + key de perfil). No añade dependencia: reqwest ya está. |
| D3 | OpenCode: `OpenCodeSummarizer` que habla `POST {opencode_url}/api/experimental/generate/text` con el wire `Model.Ref`+`prompt` — **el mismo que el adapter de chat ya validó en Fase 9**. | OpenCode no tiene client de chat, pero sí generate/text; es la única ruta real. Reutiliza el wire probado (commit `3bf75f2`). |
| D4 | `build_summarizer(provider, config, default_model)` se amplía: Ollama → `OllamaSummarizer`; OpenRouter/OpenAI-compatible/Anthropic/Gemini → `ChatCompletionsSummarizer` con `base_url`+key del perfil; OpenCode → `OpenCodeSummarizer` con `opencode_url`+password. `NoopSummarizer` queda como fallback de tests y de configuración inválida (sin URL/key). | Un único punto de selección, compartido por el store de historial y el compactor. |
| D5 | El `Arc<dyn Summarizer>` se construye **una vez** en el boot y se comparte entre el `ConversationStore` (resúmenes de historial) y el `SummarizingCompactor`. | Evita dos clientes HTTP por proveedor y mantiene consistencia. |
| D6 | Degradación: si no hay URL/key usable ⇒ `NoopSummarizer` con warning; si el summarizer falla en una llamada, `ContextAssembler` ya loggea warning y usa el historial original. | Fail-closed, el chat nunca falla. Convención Fase 5. |
| D7 | Sin env vars nuevas: se reutilizan `OPENAI_BASE_URL`/profile `base_url`, `OPENAI_API_KEY`/`OPENROUTER_API_KEY`/`ANTHROPIC_API_KEY`/`GEMINI_API_KEY`, `OPENCODE_URL`/`OPENCODE_API_KEY`, `OLLAMA_URL`, `DEFAULT_MODEL`. | Config es la única fuente de verdades (Fase 9); secretos nunca en URL/log/health. |
| D8 | Sin cambio a `max_context_tokens`, `DEFAULT_RESERVED_TOKENS`, `capabilities.rs` ni al formato `## Conversation memory`. | C3 solo conecta la pieza existente; no altera el contrato de contexto. |
| D9 | Settings Apply reconstruye el compactor desde el mismo handle, igual que gate/decider/selector. | El provider puede cambiar en caliente; el compactor debe seguir al provider.
| D10 | **NUNCA instalar `SummarizingCompactor` con `NoopSummarizer`.** Si el summarizer seleccionado es Noop (sin URL/key), instalar `NoopCompactor`, no el summarizing. | `SummarizingCompactor` con Noop **descarta los turnos viejos y devuelve `memory: None`** (silent-drop de historial, test `context_pipeline_tests.rs:203-229`). Es un riesgo de pérdida de contexto; el Noop jamás debe desplegarse con el compactor real. |
| D11 | C3 es una característica del feature `history`: si el summarizer está bajo `#[cfg(feature = "history")]`, el cableo también lo está. En builds `--no-default-features` se mantiene `NoopCompactor` (sin historial que compactar). | Evita des-gatear `OllamaSummarizer` y sus dependencias en builds ligeros; consistente con que la compactación opera sobre historial de conversación. |

RFC 2119: **MUST**, **SHALL**, **SHOULD**, **MAY**, **MUST NOT**.

## 3. Implementación

### 3.1 `ChatCompletionsSummarizer` (`src/adapters/history/summarizer.rs`)

```rust
pub struct ChatCompletionsSummarizer {
    pub base_url: String,   // origin, se normaliza el sufijo /v1 como en openai_compatible_embeddings
    pub api_key: String,
    pub model: String,
}
```

- `summarize()` construye el **mismo prompt** que `OllamaSummarizer` (`"Summarize the following conversation turns in 2-3 sentences…"` + líneas `role: content`), y hace `POST {base}/chat/completions` con `{"model", "messages":[{"role":"user","content":prompt}], "stream": false}` y `Authorization: Bearer <key>`.
- Lee `choices[0].message.content`, `trim`, vacío ⇒ `Ok("")`. Errores ⇒ `Err` con mensaje sin la key (mismo patrón que `OllamaSummarizer`).
- Usa `net::http_client(Duration::from_secs(30))` (Fase 10-A) en vez de `Client::new()`, para heredar timeouts.
- Endpoint: si `base_url` termina en `/v1`, quitarlo y añadir `/chat/completions` (evita `/v1/v1/...`). OpenRouter usa `/api/v1` → quitar `/api/v1` y añadir `/chat/completions` (path estándar de `openrouter.ai/api/v1/chat/completions` es `/api/v1/chat/completions`; verificar con el adapter `ChatCompletions` existente y ajustar).

### 3.2 `OpenCodeSummarizer` (`src/adapters/history/summarizer.rs`)

```rust
pub struct OpenCodeSummarizer {
    pub url: String,        // OPENCODE_URL
    pub password: Option<String>, // OPENCODE_API_KEY → Basic auth "opencode:<pw>"
    pub model: String,
}
```

- `summarize()` hace `POST {url}/api/experimental/generate/text` con:
  `{"model":{"id":<model>,"providerID":"opencode","modelID":<model>},"prompt":<prompt>}` — **exactamente** el wire que `src/providers/opencode.rs` validó contra servidor falso y real (Fase 9).
- Lee `data.text` o `text` (ambos aceptados por `extract_text` en el adapter). Basic auth con password si presente; timeout 30 s vía `net::http_client`.
- Errores ⇒ `Err` sin password.

### 3.3 `build_summarizer` ampliado (`src/runtime.rs:1374`)

Firma nueva: `pub fn build_summarizer(config: &Config, provider: ProviderKind, default_model: &str) -> Arc<dyn Summarizer>` (o mantener la firma actual y añadir un `&Config` — decisión del implementador, documentar). Match:

| Provider | Summarizer | Fuente de URL/key |
|---|---|---|
| Ollama | OllamaSummarizer | `config.ollama_url`, `default_model` |
| OpenRouter | ChatCompletionsSummarizer | `OPENAI_BASE_URL` o `https://openrouter.ai/api/v1`, `config.openrouter_api_key` |
| OpenAI-compatible | ChatCompletionsSummarizer | `config.openai_base_url` o profile `base_url`, key del profile |
| Anthropic | ChatCompletionsSummarizer | profile `base_url`, key `ANTHROPIC_API_KEY` |
| Gemini | ChatCompletionsSummarizer | profile `base_url`, key `GEMINI_API_KEY` |
| OpenCode | OpenCodeSummarizer | `config.opencode_url`, `config.opencode_api_key` |

Si falta URL o key ⇒ `NoopSummarizer` con warning nombrando proveedor y variable (nunca el valor). `#[cfg(feature = "history")]` se mantiene.

### 3.4 Boot y Apply (`src/runtime.rs`)

- Boot (`:294`): `let summarizer = build_summarizer(&config, config.provider, &config.default_model);` — si el summarizer es `NoopSummarizer` (sin URL/key usable), instalar `NoopCompactor` (D10); si no, `Arc::new(SummarizingCompactor::new(summarizer.clone()))`. Bajo `#[cfg(feature = "history")]` (D11).
- Pasar el mismo `summarizer.clone()` a `build_app_state`/store de historial (`:1436`) para compartirlo.
- Guardar `compactor` en `CompanionHandles` (o donde esté el `Arc<dyn ContextCompactor>` actual) y reconstruirlo en `finish_apply` del TUI (`src/tui/app.rs`) con la config aplicada.
- `AGENTS.md`/comentario de runtime: actualizar de "Boot installs NoopCompactor by default; deployed summarization awaits a real strategy" a "Boot installs SummarizingCompactor with a provider-appropriate Summarizer; Noop only when no usable URL/key".

## 4. Seguridad

- Las keys viajan solo como `Authorization: Bearer` o Basic auth en header; **nunca** en URL, log, health ni mensaje de error (convención Fase 1/9).
- El prompt del summary contiene contenido del historial — se envía al mismo upstream del chat, que ya es de confianza para ese contenido. No se envía a terceros.
- `NoopSummarizer` sigue siendo el fallback seguro.

## 5. Testing

### 5.1 Unit / integración

- Summarizers nuevos contra fake servers locales: `ChatCompletionsSummarizer` (path `/chat/completions`, Bearer, parse `choices[0].message.content`, error sin key en mensaje) y `OpenCodeSummarizer` (path `/api/experimental/generate/text`, wire `Model.Ref`, Basic auth, parse `data.text`).
- `build_summarizer` para los 6 providers: tipo correcto (Ollama/ChatCompletions/OpenCode/Noop) sin exponer secretos; falta URL/key ⇒ Noop + warning.
- Boot: `stack`/companion carries `SummarizingCompactor` no Noop; Apply reconstruye con nueva config.
- Regresión: `tests/context_pipeline_tests.rs` (compactor en sí, instrucciones byte-for-byte) sigue verde; `SummarizingCompactor` tests actuales siguen verdes.

### 5.2 E2E

- Conversación multi-turno con `PROVIDER=ollama` y un `max_context_tokens` bajo (o historial largo) ⇒ el system prompt de un turno contiene `## Conversation memory` con el resumen, y el sufijo reciente sigue presente. Usar summarizer fake o el real de Ollama.
- Repetir con `PROVIDER=opencode` + fake generate server ⇒ misma aserción.
- Sin URL/key ⇒ Noop, sin `## Conversation memory`, chat sigue 200.

### 5.3 Regresión

`cargo test --target-dir target-tests` (1004+ verdes), fmt, clippy, gates Fase 10-B.

## 6. Riesgos

| Riesgo | Mitigación |
|---|---|
| Summarizer con wire equivocado por proveedor | Tests de wire con fake servers por tipo; reutilizar el wire validado de `opencode.rs` y `openai_compatible.rs`. |
| Coste/latencia extra por resumir cada vez que hay presión de contexto | El compactor solo se consulta si `budget_tokens > 0` y recorta; timeout 30 s; degradación a historial original si falla. |
| OpenCode generate no devuelve texto útil | `Err` ⇒ `ContextAssembler` degrada sin romper el chat; documentado. |
| Cambiar la firma de `build_summarizer` rompa tests | Actualizar los tests que la llaman en el mismo cambio. |

## 7. Fuera de alcance

C4 (step-replay), nuevo tokenizer, compactar prompts internos de tools/RAG, re-inyectar resúmenes del store de historial (distinto de `## Conversation memory`), env vars nuevas, cambio de `capabilities.rs`.

## 8. Plan de implementación (siguiente paso)

Tras revisión, invocar `writing-plans`:

1. `ChatCompletionsSummarizer` + `OpenCodeSummarizer` en `summarizer.rs` + tests de wire.
2. Ampliar `build_summarizer` a los 6 providers + tests de selección/degradación.
3. Cablear `SummarizingCompactor` en boot + Apply, compartir summarizer.
4. Docs (AGENTS/README) + E2E + regresión.

Criterio de aceptación: con presión de contexto, el historial se compacta y `## Conversation memory` aparece para los 6 proveedores (o Noop explícito sin URL/key), sin romper el chat ni las secciones de instrucciones.
