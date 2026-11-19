# Fase 6 (6a + 6c) — Auto decision model y trazas estructuradas

**Fecha:** 2026-11-19
**Estado:** propuesto — aprobado en brainstorming, pendiente de revisión del spec
**Alcance:** cerrar la deuda de `Auto` (approval policy automático) y añadir trazas
estructuradas + coste. Se hizo 6b (evals) en un spec anterior; este cubre 6a y 6c.

## Contexto

Tres hechos del código condicionan esta pieza:

1. **`Auto` hoy es `Always`.** `PermissionEngine::authorize` (`src/domain/policy.rs:84`) es
   puro y devuelve `Approve` para `Never`→Allow y para `Once|Always|Auto`→Approve. El gate
   (`src/adapters/rig_engine/approval.rs:37::authorize`) recibe ese `Approve` y decide cómo
   resolverlo: registra un pending y bloquea (humano), o lo deja pasar. Ahí es donde se inserta
   el decision model.
2. **El `DecisionRouter` ya existe** (`src/ports/router.rs:93`): `decide(DecisionRequest) ->
   DecisionResponse` con `Question::{Choice, Noul}` y `Answer::{Choice{key, confidence,
   probabilities}, Noul{value}}`. Es el mismo d1 que hace routing de agentes (Fase 1) y el que
   el eval `judge` usa para calificar (6b). El umbral `MIN_CONFIDENCE` vive en `RouterConfig`.
3. **Observabilidad hoy**: `AppObservability` (`src/api/observability.rs`) son contadores
   atómicos (sin `auto` ni `cost`); `AgentExecution` + eventos con `seq` (Fase 7) ya registran
   cada turno con `execution_id`; `Usage::from_counts` (`src/domain/models.rs:39`) ya calcula
   `cost_usd` desde `ModelCapabilities.cost_per_1m_*` (hoy `None`). No hay OpenTelemetry.

Se descartó B en ambos ejes: OTel completo añade dependencias pesadas y un colector inexistente
en desarrollo; la heurística local para `Auto` no es un decision model y no usa probabilidades.
Se eligió **A+A**: reutilizar el router con degradación segura y trazas ligeras sobre el
execution log, con el contrato de campos listo para un exporter OTel futuro.

## Goal

Que `Auto` pueda aprobar tool calls de rutina mediante el decision model existente —sin aprobar
nunca sin evidencia, degradando a humano si el router no está— y que cada turno produzca trazas
estructuradas con `execution_id`/`session`/`agent`/`provider`/`model` y coste calculado,
preparadas para un exporter OpenTelemetry futuro sin rediseñar.

## Non-goals

- OpenTelemetry / exporter OTLP / Jaeger (se prepara el contrato, no se integra).
- Cambiar el comportamiento de `Never`/`Always`/`Once` (solo `Auto` cambia). Supresión de `Once`
  ("primera vez") sigue pendiente.
- Cambiar los puertos `LLMProvider`, `AgentEngine`, `DecisionRouter`, `RagStore`, `ToolDyn`.
- Persistir trazas en un backend nuevo (se apoya en el `ExecutionStore` de Fase 7).
- Evals (6b, ya hecho). Añadir checks de eval para `Auto` es opcional y separado.

## Decisiones de diseño

1. **Degradación segura de `Auto`.** El gate decide: si la política es `Auto` **y** hay un
   `DecisionRouter` inyectado **y** el modelo responde `Choice{key:"yes"}` con
   `confidence >= threshold` → `Allow` (sin humano). En cualquier otro caso (sin router, error,
   `no`, baja confianza, `Noul`, respuesta inusable) → `Approve` (humano). NUNCA se aprueba
   automáticamente sin una decisión afirmativa del modelo. Es el mismo ladder de degradación
   que el routing de agentes.
2. **Umbral y config.** Se reutiliza `MIN_CONFIDENCE` de `RouterConfig` como umbral por defecto
   de `Auto` (0.60), sobreescribible por un campo opcional en la política (`auto_threshold`).
   La pregunta al modelo es un `Choice` con etiquetas `yes`/`no`, `instructions` describiendo la
   comprobación de seguridad, y `criteria` con el nombre de la tool y un hash/resumen de los
   args — nunca los args completos con secretos.
3. **Sin secretos al decision model.** Los args se redactan con `redact_secrets` antes de formar
   la pregunta, y se incluye solo el `args_hash` (SHA-256 canónico, ya compartido). El modelo
   ve el nombre de la tool y el hash, no valores sensibles.
4. **Trazas = eventos enriquecidos del execution log.** Se amplía `EventKind` (Fase 7) con
   campos de traza o un nuevo evento `TraceRecorded` que incluya `trace_id` (= `execution_id`),
   `agent_qualified_id`, `provider`, `model`, `usage`, `cost_usd`, `latency_ms`. Se persiste vía
   `append_event` (payload redactado, `seq` asignado por el store). `GET /api/executions/:id` ya
   lo expone — sin endpoint nuevo obligatorio.
5. **Coste calculado donde ya está el `usage`.** `Usage::from_counts` ya produce `cost_usd` desde
   los precios de `ModelCapabilities`; se propaga ese valor a la traza. Los precios siguen
   siendo `None` en el catálogo hoy, así que el coste será `None` en la práctica — el
   **cálculo** queda implementado y testeado con precios inyectados en tests, listo para cuando
   se llene el catálogo.
6. **Contrato OTel-ready.** Los nombres de campo (`trace_id`, `span_name`, `agent_id`,
   `provider`, `model`, `cost_usd`) siguen la semántica de OpenTelemetry, de modo que un futuro
   exporter solo mapea el evento a un span. No se añade la dependencia ahora.

## Arquitectura

### 6a — Auto

- `src/domain/policy.rs`: `PermissionPolicy` gana `auto_threshold: Option<f32>` (serde default).
  `ApprovalPolicy::Auto` sigue devolviendo `Approve` en la función pura (sin I/O) — el cambio
  está en el gate, no aquí, para preservar la pureza y los tests existentes.
- `src/adapters/rig_engine/approval.rs`: `authorize(...)` gana un parámetro opcional
  `auto_decider: Option<&dyn AutoDecider>`. El flujo para `Approve`:
  1. Si la política es `Auto` y hay `auto_decider` → `auto_decider.should_allow(tool, args, threshold).await`.
  2. Si `Allow` → devolver `Ok(())` sin registrar pending (y emitir un evento de traza opcional).
  3. Si `Deny`/sin decider/no-auto → el camino humano actual (pending + oneshot).
- `src/ports/router.rs` o nuevo `src/ports/auto.rs`: trait `AutoDecider` con
  `async fn should_allow(&self, tool_name: &str, redacted_args: &Value, args_hash_hex: &str,
  threshold: f32) -> AutoVerdict::{Allow, Deny, Unavailable}`. Implementación `RouterAutoDecider`
  que envuelve `Arc<dyn DecisionRouter>` y formula el `Choice`.
- `src/api/handlers.rs`/`runtime.rs`: `AppState` gana `auto_decider: Option<Arc<dyn AutoDecider>>`,
  construido desde el mismo `DecisionRouter` del stack cuando `router_config.enabled`; se pasa
  al engine por `CompanionHandles` (mirror del `approval_gate`), sobreviviendo a hot Apply.

### 6c — Trazas y coste

- `src/domain/execution.rs`: `EventKind` gana `TraceRecorded { trace_id, span_name,
  agent_qualified_id, provider, model, input_tokens, output_tokens, cost_usd, latency_ms }` —
  o, si el enum es unit-only, un nuevo variant con un struct `TracePayload` (serde) en el campo.
  El payload pasa por `redact_secrets` antes de `append_event`.
- `src/api/chat_core.rs`: al finalizar un turno ( los mismos puntos que `finish_execution` de
  Fase 7), emitter el evento de traza con el `usage` de la respuesta (cuando exista),
  `provider`/`model` resueltos y la latencia medida. Un `ChatResponse.usage` ya trae `cost_usd`.
- `src/api/observability.rs`: `AppObservability` gana `auto_approved: AtomicU64`,
  `auto_denied: AtomicU64`, `auto_unavailable: AtomicU64`, `cost_usd_total_micros: AtomicU64`
  (coste acumulado en microdólares para evitar float atómico) y se exponen en `snapshot()`.
  El dashboard/TUI puede mostrarlos; sin cambio de contrato de health.
- No se añade endpoint: las trazas se leen por `GET /api/executions/:id` (ya existe).

## Flujo de datos

1. Tool call con política `Auto`: el bridge llama `authorize` con el `auto_decider` del run.
2. `RouterAutoDecider` redacta args, calcula `args_hash_hex`, formula `Question::Choice` con
   `yes`/`no` sobre `{tool_name, hash}` y llama al `DecisionRouter`.
3. `yes` + confianza ≥ umbral → `Allow` (el tool ejecuta, budget se carga después como siempre);
   cualquier otra cosa → camino humano (pending + SSE/poll). `Unavailable` se contabiliza.
4. Al finalizar el turno, `chat_core` emite `TraceRecorded` al store con provider/model/usage/
   cost/latencia; `AppObservability` incrementa los contadores de `auto` y coste.
5. Un exporter OTel futuro solo mapea `TraceRecorded` a un span — sin tocar el runtime.

## Manejo de errores

- Router ausente/error/respuesta inusable → `Unavailable` → humano + contador; nunca `Allow`.
- Baja confianza o `no` → `Deny` → humano. Ninguno de estos ejecuta la tool sin aprobación.
- Los args redactados y el hash son la única información que sale del proceso hacia el modelo;
  test de leak con un secreto en args.
- Fallos al persistir la traza → warning y continuar (misma disciplina best-effort de Fase 7),
  nunca fallar el chat por observabilidad.
- Precio `None` en el catálogo → `cost_usd: None` en la traza, sin fabricar costes.

## Testing

- `tests/auto_decider_tests.rs` (nuevo): `yes`+alta confianza→Allow; `no`→Deny; baja
  confianza→Deny/Unavailable según contrato; sin router→Unavailable; router error→Unavailable;
  secreto en args nunca aparece en la pregunta; umbral configurable.
- `tests/tracing_tests.rs` (nuevo): un turno emite `TraceRecorded` con trace_id=execution_id,
  provider/model correctos, latencia ≥ 0, y `cost_usd` calculado cuando se inyectan precios;
  secreto en payload redactado en el store; contadores de observabilidad se incrementan;
  persistencia fallida → warning sin fallar el chat.
- Gates: fmt/clippy/audit/773+ tests/lib-only --no-default-features (AutoDecider debe existir
  también sin router sin feature gates problemáticos).

## Riesgos

- **Aprobar de más (falso positivo del modelo):** mitificado con umbral alto por defecto
  (MIN_CONFIDENCE 0.60), redacción de args y degradación a humano ante duda. Un `yes` con
  confianza alta es una decisión consciente del modelo, auditada en la traza.
- **El d1 no está disponible en la práctica:** en la mayoría de despliegues `Auto` seguirá
  pidiendo humano; la funcionalidad queda implementada y testeada con router fake, igual que
  el eval `judge`. Documentar honestamente.
- **Coste con precios `None`:** el cálculo existe pero no produce números hasta que se llene el
  catálogo; testear con precios inyectados para no dar falsa sensación de coste real.
- **Trazas crecen el event log:** un evento más por turno; el store ya es append-only con seq,
  sin migración. Aceptable.

## Exit criteria

- `Auto` aprueba sin humano solo con `yes`+confianza≥umbral del decision model; sin router o
  con duda cae a humano; ningún secreto en la pregunta ni en la traza.
- Un turno emite `TraceRecorded` consultable en `GET /api/executions/:id` con coste calculado
  cuando hay precios; contadores `auto_*` y coste en el snapshot.
- Puertos sin cambios; v1 manifests siguen igual; OTel NO integrado (solo contrato listo);
  docs alineadas; gates verdes.
