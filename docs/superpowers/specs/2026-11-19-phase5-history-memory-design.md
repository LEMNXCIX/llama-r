# Fase 5 (pieza 1) — Historial y memoria: cerrar los 8 gaps

**Fecha:** 2026-11-19
**Estado:** aprobado en brainstorming (diseño de 3 secciones), pendiente de revisión del spec
**Alcance:** cerrar los ocho huecos conocidos de `PHASE_5_HISTORY_PLAN.md:128-139` sin cambiar
la arquitectura del store, y dejar la base sana para la pieza de compaction.

## Contexto

El repositorio ya tiene historial, resúmenes e indexación en RAG: `ConversationStore`
(`src/ports/history.rs:57-111`), adaptador SQLite (`src/adapters/history/sqlite.rs`), flags
`[memory]` (`src/domain/agent.rs:186-210`), wiring en `AgentRuntime` (`src/services/agent_runtime.rs:125-271`)
y API REST (`src/api/history_api.rs`). El `ROADMAP.md` del repo marca su "Fase 5 ✅" como
historial + resúmenes, pero `PHASE_5_HISTORY_PLAN.md:128-139` enumera **ocho gaps que siguen
abiertos**. El roadmap global que venimos ejecutando define Fase 5 como "Contexto inteligente"
(compaction, memoria/contexto activo, model routing) — piezas distintas. Esta spec cubre solo
la primera: **historial/memoria**, como base de la compaction.

Los ocho gaps, verificados en código:

1. `ensure_conversation` usa `agent.config.name` en vez de `agent.id` (`sqlite.rs:140,151`).
2. `maybe_summarize_and_index` elige la conversación más reciente del agente
   (`ORDER BY updated_at DESC LIMIT 1`, `sqlite.rs:470`) en vez de la del turno.
3. El summarizer llama a Ollama real sin seam inyectable (`sqlite.rs:523-547`): el camino feliz
   no es testeable sin red.
4. La tabla `tool_events` se crea (`sqlite.rs:85`) pero nunca se escribe.
5. `ChatResponse` no devuelve `conversation_id` (`src/domain/models.rs:60-86`): el cliente solo
   puede reanudar si genera el UUID él.
6. `chat_stream` guarda el usuario pero **no** el assistant (`agent_runtime.rs:206-271`).
7. El purge usa solo `HISTORY_RETENTION_DAYS` global (`runtime.rs:512`); ignora
   `[memory] retention_days`.
8. `Mutex<Connection>` en código async (`sqlite.rs:17-24`).

El plan dice explícitamente que los gaps 4, 7 y 8 son aceptables de documentar como P2, pero el
usuario eligió **cerrar los ocho**.

## Goal

Cerrar los ocho gaps: identidad de conversación correcta, resumen del turno correcto con un
summarizer testeable, `tool_events` escritos, `conversation_id` devuelto al cliente, assistant
persistido en streaming y retención por agente — sin cambiar la arquitectura del store y sin
romper manifests v1 ni clientes existentes.

## Non-goals

- Re-inyectar resúmenes en el prompt ("aprendizaje continuo" — el plan lo deja para más tarde).
- Compaction y separación formal historial / memoria / contexto activo (piezas siguientes de Fase 5).
- Model routing por complejidad/coste (tercera pieza de Fase 5).
- Cambiar los puertos "sagrados" (`LLMProvider`, `EmbeddingProvider`, `DecisionRouter`, `AgentEngine`).
- Auth en los endpoints de historial (ya diferida).
- Migraciones destructivas de SQLite: solo columnas aditivas / `IF NOT EXISTS`.

## Decisiones de diseño

1. **Slices verticales sobre el puerto existente.** `ConversationStore` es un puerto interno de
   esta pieza (no sagrado): se amplía de forma compatible. No se reescribe el adaptador ni se
   introduce un nuevo subsistema.
2. **Gap 1 — identidad:** `ensure_conversation` usa `agent.id` (filename sin `.toml`), nunca
   `agent.config.name` (display name, colisionable). El store ya recibe `&Agent`, así que no hace
   falta nuevo argumento.
3. **Gap 2 — resumen del turno:** `maybe_summarize_and_index` recibe `conversation_id: Option<&str>`.
   Con `Some`, resume **esa** conversación; con `None`, conserva el comportamiento legacy para no
   romper implementores. El intervalo `summarize_every_n_turns` se cuenta por conversación.
4. **Gap 3 — summarizer inyectable:** trait `Summarizer { async fn summarize(&self, messages:
   &[ExportMessage]) -> Result<String, String> }` en `src/ports/history.rs`; `OllamaSummarizer` y
   `NoopSummarizer` en `src/adapters/history/`; inyectado desde `build_history_store`
   (`src/runtime.rs:1111-1154`) en lugar del par `(url, model)` hardcodeado. `FakeSummarizer` en
   tests cubre el camino feliz sin red.
5. **Gap 4 — tool_events:** nuevo `append_tool_event(conversation_id, tool_name, success)` en el
   puerto: nombre y éxito, **sin args** (no hay `log_tool_args` hoy). Se alimenta con un sink
   aditivo en `AgentRunRequest` que ambos bridges (`tools.rs`, `local_tools.rs`) invocan al emitir
   `ToolCall`/`ToolResult`; `AgentRuntime` lo enlaza al store con el `conversation_id` del turno.
   Un solo mecanismo cubre streaming y no-streaming sin meter el store dentro de los bridges.
6. **Gap 5 — respuesta:** `ChatResponse.conversation_id: Option<String>` con
   `#[serde(skip_serializing_if = "Option::is_none")]` **más** header `X-Conversation-Id` en
   `handlers.rs` (JSON y SSE). El servidor genera un UUID cuando no viene el header y se persiste
   una conversación. Cliente viejo: campo ausente = `None`, sin cambio de contrato.
7. **Gap 6 — streaming:** `chat_stream` hace `append_assistant` al completar (evento `Completed`
   o tras drenar el receiver), con el mismo `conversation_id` que `append_user`.
8. **Gap 7 — retención:** `purge_old_conversations` acepta un mapa opcional `agent_id →
   retention_days`, construido en el boot y en el purge diario desde `AgentRegistry::list()`.
   Conversaciones sin agente usan `HISTORY_RETENTION_DAYS`. Un valor inválido se ignora con
   warning (agente + valor) y cae al global — nunca clamp silencioso.
9. **Gap 8 — decisión, no refactor:** se mantiene `Mutex<Connection>` + WAL (un proceso). Se cierra
   con una decisión documentada en AGENTS.md, un test de dos appends concurrentes sin pérdida, y
   la nota de que `spawn_blocking` se consideraría solo con contención **medida**. El plan lo
   desaconseja sin esa medición.
10. **Best-effort:** fallo de summarizer, `append_*`, `append_tool_event` o purge → warning
    nombrando el motivo; el chat nunca falla por persistencia u observabilidad. Sin
    `persist_history` no se genera UUID ni se emite header.

## Arquitectura

### Puerto y adaptador

- `src/ports/history.rs`: `Summarizer`, parámetro `conversation_id`, `append_tool_event`, purge
  con mapa de retención; `ConversationSummary`/`ConversationRecord` sin cambios.
- `src/adapters/history/sqlite.rs`: `agent.id` en INSERT, resumen por id, escritura de
  `tool_events`, purge por agente; agregar una columna es `IF NOT EXISTS` / aditiva.
- `src/adapters/history/`: `OllamaSummarizer`, `NoopSummarizer`, `FakeSummarizer` (tests).

### Servicio y API

- `src/services/agent_runtime.rs`: `RuntimeChatResult { text, conversation_id }` (cambio de tipo
  de servicio, no de puerto), generación del UUID, persistencia simétrica en `chat`/`chat_stream`,
  sink de tool events, resumen con el id del turno.
- `src/api/chat_core.rs`: copia el id a `ChatResponse`; `src/api/handlers.rs`: header en JSON y SSE.
- `src/runtime.rs`: construcción del summarizer inyectado y del mapa de retención; purge diario.

## Flujo de datos

1. `X-Conversation-Id` llega en `AgentSelection`. Si falta y `persist_history`, se genera UUID
   antes de `append_user`.
2. `chat`: `append_user` → engine (bridges escriben `tool_events` vía sink) → `append_assistant`
   → resumen de esa conversación.
3. `chat_stream`: igual, pero `append_assistant` al `Completed` / tras drenar.
4. `ChatResponse.conversation_id` + header `X-Conversation-Id` devuelven el id; el SSE lo emite
   en la respuesta antes de abrir el stream.
5. Boot y purge diario aplican retención por agente; sin agente, la global.

## Manejo de errores

- Summarizer / `append_*` / `append_tool_event` / purge fallan → `tracing::warn!` nombrando el
  motivo; el turno continúa. Nunca `Err` del chat por observabilidad.
- `retention_days` inválido → warning con agente + valor, fallback al global.
- `FakeSummarizer`/`NoopSummarizer` sin red: el happy path y el camino deshabilitado se prueban
  sin Ollama.

## Testing

`tests/history_integration_tests.rs` (nuevo) + extensiones a los tests existentes:

- **Gap 1:** dos agentes con el mismo `name` e ids distintos → conversaciones separadas; cambiar
  el display name no cambia la titularidad.
- **Gap 2:** dos conversaciones abiertas → el resumen se aplica a la del turno, no a la más reciente.
- **Gap 3:** `FakeSummarizer` captura mensajes; happy path sin red; `NoopSummarizer` no resume.
- **Gap 4:** turno con tools → `tool_events` con nombre + éxito, sin args; replay idempotente no
  duplica eventos.
- **Gap 5:** sin header → UUID generado, DTO y header coinciden; con header → se conserva;
  `persist_history=false` → `None` y sin header; respuesta sin el campo sigue parseando.
- **Gap 6:** `chat_stream` persiste el texto final del assistant, igual que no-stream.
- **Gap 7:** purge con retención por agente (uno alto sobrevive, uno bajo se purga, sin agente →
  global).
- **Gap 8:** dos appends concurrentes dejan ambas filas (WAL + mutex).

Gates: fmt / clippy `-D warnings` / audit / suite completa (855+) / build `--no-default-features`.

## Riesgos

- **Cambio de firma en el puerto `ConversationStore`:** es interno de esta pieza, no sagrado; se
  mitiga con defaults en los métodos nuevos y actualizando todos los implementores.
- **El sink de tool events cruza capas:** se mantiene como campo aditivo en `AgentRunRequest`, el
  mismo patrón ya probado en Fase 4 (`shared_budget`, `delegation_context`), sin meter el store en
  los bridges.
- **Generar UUID en servidor cambia el contrato:** es aditivo (campo opcional + header); los
  clientes que ya envían id no notan nada.
- **Retención por agente mal configurada:** warning + fallback global, mismo discipline que los
  manifests malformados.
- **Gap 8 "cerrado por decisión":** honestidad documentada — no hay refactor async; se reconsidera
  solo con medición de contención.

## Exit criteria

- Los ocho gaps están cerrados: siete en código y el octavo con decisión documentada + test.
- El cliente puede reanudar por id devuelto; el resumen y los tool events corresponden al turno
  correcto; la retención respeta `[memory] retention_days`.
- Ningún puerto sagrado cambia; manifests v1 y clientes viejos siguen funcionando; gates verdes.
