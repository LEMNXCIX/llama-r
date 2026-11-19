# Fase 7 — Ejecución durable (a nivel de turno)

**Fecha:** 2026-11-18
**Estado:** propuesto — aprobado en brainstorming, pendiente de revisión del spec
**Alcance:** persistencia de ejecución y reanudación a nivel de TURNO (no replay de pasos
de tool-loop), aprobaciones persistentes en SQLite, e idempotencia de herramientas por
`operation_id`. Rig sigue siendo dueño del tool-loop.

## Contexto

Tras las Fases 1-3, llama-r tiene seis providers, tools locales, skills bajo demanda y HITL
con `ApprovalGate` en memoria. Tres hechos condicionan esta fase:

1. **SQLite ya existe y es robusto.** `data/history.db` con WAL + foreign keys y tablas
   `conversations`, `messages`, `tool_events` (ya persiste `id`, `tool_name`, `tool_input`,
   `tool_output`, `success`), `summaries`. Es el lugar natural para las ejecuciones.
2. **El approval gate es in-memory** (`RwLock<HashMap<Uuid, PendingApproval>>` + oneshot).
   Se pierde en restart — deuda explícita documentada en AGENTS.md ("Fase 7 durable execution").
3. **Rig corre el tool-loop en `agent.prompt().await`**; sus hooks (`StepEvent`/`on_event`)
   son observadores, no puntos de pausa/checkpoint. La recuperación a mitad de turno
   exigiría reimplementar el loop.

Se descartó la opción A (reimplementar el tool-loop para replay de pasos) por riesgo y coste,
y la C (solo aprobaciones) por dejar el plan a medias. Se adopta la **B**.

## Goal

Que una ejecución de agente sea observable y recuperable: persistir cada turno con estado y
eventos, poder reanudar tras un crash (reintentando el turno, no replicando pasos internos),
persistir las aprobaciones para que sobrevivan a un restart, e impedir que un reintento
duplique efectos externos mediante `operation_id`.

## Non-goals

- Replay de pasos del tool-loop / checkpoint a mitad de turno (opción A — futura).
- Workflow engine externo (Temporal/Restate) ni backend Postgres ahora — solo SQLite, con la
  interfaz preparada para añadir Postgres después.
- Identidad de usuario real / auth en los endpoints de aprobación (requiere decisiones de
  autenticación fuera del alcance de esta fase — se documenta como pendiente).
- `Auto` policy con decision model (Fase 6).
- Cambiar los puertos `LLMProvider`, `AgentEngine`, `DecisionRouter`, `RagStore`, `ToolDyn`.
- Cambiar el contrato de `/chat` ni de `/api/approvals`.

## Decisiones de diseño

1. **Reanudación a nivel de turno.** Un turno interrumpido (crash durante `Running` o
   `WaitingApproval`) se marca `interrupted` al arrancar y se puede **reintentar desde cero**.
   No hay replay de la secuencia exacta de tools dentro del turno. La semilla para la opción A
   (eventos por paso + `operation_id`) queda persistida.
2. **`operation_id` para idempotencia de tools.** Cada llamada a herramienta lleva un id
   derivado de `(execution_id, tool_name, args_hash)`. Un store de operaciones en SQLite
   registra el resultado; si un reintento encuentra el id ya completado, devuelve el resultado
   guardado en vez de ejecutar de nuevo. Cubre el riesgo real de la opción B.
3. **Aprobaciones en SQLite.** El gate mantiene el `oneshot` en memoria para el request vivo,
   pero el pending se persiste. Tras un restart, un pending `WaitingApproval` se puede decidir
   por HTTP; el turno interrumpido se reintenta con la decisión ya registrada. El `args_hash`
   sigue ligando la decisión a los argumentos.
4. **Nuevo puerto `ExecutionStore`**, con implementación SQLite en el mismo `history.db`.
   El runtime lo consulta al iniciar para recuperar ejecuciones interceptadas.
5. **Sin background task** para el reaper: la recuperación ocurre al boot (marcar
   `interrupted`) y el reaper sigue bajo demanda, igual que hoy.

## Arquitectura

### Modelo de dominio (`src/domain/execution.rs` nuevo)

- `ExecutionStatus { Pending, Running, WaitingApproval, Completed, Failed, Interrupted, Cancelled }`.
- `AgentExecution { id: Uuid, conversation_id: Option<String>, agent_qualified_id: String,
  provider: String, model: String, status: ExecutionStatus, current_step: u32,
  user_message_hash: String, created_at: DateTime<Utc>, updated_at: DateTime<Utc>,
  finished_at: Option<DateTime<Utc>>, error: Option<String> }`.
- `ExecutionEvent { id: Uuid, execution_id: Uuid, seq: u32, kind: EventKind, payload: Value,
  created_at }` — `EventKind` cubre `Started`, `ModelRequested`, `ToolCallRequested`,
  `ToolCallCompleted`, `CheckpointCreated`, `WaitingApproval`, `ApprovalGranted`,
  `ExecutionResumed`, `ExecutionCompleted`, `ExecutionFailed`, `Interrupted`.

### Puerto (`src/ports/execution.rs` nuevo)

`ExecutionStore: Send + Sync` con:
- `create(execution) -> Result<(), String>`
- `append_event(execution_id, event) -> Result<(), String>` (seq monótono por ejecución)
- `set_status(execution_id, status, error?) -> Result<(), String>`
- `get(execution_id) -> Result<Option<AgentExecution>, String>`
- `list_interrupted() -> Result<Vec<AgentExecution>, String>`
- `record_operation(operation_id, result_json) -> Result<(), String>` y
  `get_operation(operation_id) -> Result<Option<Value>, String>` (idempotencia)
- `save_pending_approval(...)`, `load_pending_approvals()`, `decide_pending(...)` (aprobaciones)

### Implementación SQLite (`src/adapters/execution/sqlite.rs` nuevo)

- Tablas `executions`, `execution_events`, `tool_operations`, `pending_approvals` en el MISMO
  `history.db` (mismo patrón WAL/foreign keys que `sqlite.rs` de history). `operation_id`
  PRIMARY KEY en `tool_operations` hace la idempotencia atómica por construcción.
- `PendingApproval` persiste: id, tool_name, args (JSON), args_hash, execution_id, created_at,
  decided, decision. El `oneshot` sigue en memoria, keyed por el mismo id.

### Integración

- `AppState` gana `execution_store: Arc<dyn ExecutionStore>` (construido en `runtime.rs` sobre
  el mismo path de `history.db`). Hot-swap no lo toca (fuera de `ProviderStack`).
- `chat_core::execute_chat`/`execute_chat_stream`: al resolver un agente, crear una ejecución,
  emitir `Started`, pasar `execution_id` al `AgentRunRequest`; al terminar, `Completed`/`Failed`.
  El fallback a legacy chat también registra el evento — la ejecución es observable aunque el
  engine falle.
- `AgentRunRequest` gana `execution_id: Uuid` (y `scope` ya existe). Los bridges, antes de
  ejecutar una tool, calculan `operation_id` y consultan `get_operation`; si existe, devuelven
  el resultado guardado (y emiten `ToolCallCompleted` con `replayed: true`).
- Boot (`runtime.rs`): `list_interrupted()` marca los turnos a media ejecución como
  `Interrupted` y lo registra en un log/warning; los pending `WaitingApproval` se recargan en
  el gate para que se puedan decidir.
- `GET /api/executions`, `GET /api/executions/:id` (eventos) — superficie de observación, con
  `X-Debug: true` igual que RAG/Router. `POST /api/executions/:id/retry` para reanudar un
  turno `Interrupted`/`Failed` desde cero.

## Flujo de datos

1. Request con agente → crear `AgentExecution` (`Running`) + evento `Started`.
2. Engine corre el turno (Rig dueño del loop). Cada tool call: calcular `operation_id`,
   consultar store → si completado, reutilizar resultado; si no, ejecutar y `record_operation`.
   Eventos `ToolCallRequested`/`ToolCallCompleted` por herramienta.
3. Tool con aprobación → `WaitingApproval` + pending persistido; el oneshot sigue en memoria.
4. Decisión HTTP → `decide_pending` persiste la decisión y resuelve el oneshot vivo.
5. Fin del turno → `Completed`/`Failed` con `finished_at`. Un crash deja `Running`/
   `WaitingApproval`; el boot lo marca `Interrupted`.
6. `POST /api/executions/:id/retry` reintenta el turno desde cero con el mensaje original;
   las tools ya registradas por `operation_id` no repiten efectos.

## Manejo de errores

- Store SQLite indisponible → el chat NO falla: se degrada a ejecución no persistida con un
  warning (el sistema hoy funciona sin execution store; no se introduce un nuevo modo de fallback
  silencioso — se registra explícitamente).
- Reintento con `operation_id` duplicado → resultado guardado, no re-ejecución. El evento marca
  `replayed: true` para que sea auditable.
- Pending decidido tras restart: la decisión persistida gana; si el turno ya se reintentó, se
  aplica al reintento (bound por `args_hash`).
- Secretos: `args` de tools se redactan antes de persistir en `execution_events` y antes de
  serializar en la API. **Ruling:** extraer la función `redact_secrets` hoy en
  `src/api/approval_api.rs` a un módulo compartido (p. ej. `src/services/redaction.rs`) y
  usarla tanto en `/api/approvals` como en el store de ejecuciones — una sola implementación,
  probada una sola vez, para que no puedan divergir.

## Testing

- `tests/execution_store_tests.rs`: CRUD de ejecuciones, seq de eventos, `list_interrupted`,
  idempotencia de `tool_operations` (mismo operation_id dos veces → un solo efecto),
  persistencia de pending + decisión.
- `tests/execution_recovery_tests.rs`: boot con una ejecución `Running` → marcada
  `Interrupted`; retry desde cero → tools con operation_id ya guardado no se re-ejecutan
  (fake tool con contador); aprobación decidida tras restart se aplica al reintento.
- Integración: un turno con agente produce eventos observables en `GET /api/executions/:id`.
- Gates: fmt/clippy/audit/696+ tests/`--no-default-features`.

## Riesgos

- **Reintento desde cero puede duplicar texto/efectos no-idempotentes** aunque las tools lo
  sean: el propio modelo se reinvoca y puede producir una respuesta distinta. Mitigación:
  documentar que la reanudación es a nivel de turno; `operation_id` protege los efectos
  externos, no la generación. La opción A (replay de pasos) es la única que elimina esto.
- **Esquema SQLite nuevo en un DB existente**: `CREATE TABLE IF NOT EXISTS` + índices;
  ninguna migración destructiva. Verificar que un `history.db` viejo siga abriendo.
- **Redacción de args en eventos**: si se olvida, un tool con un secreto en args queda en
  disco. Mitigación: redactar en el punto de `append_event`, con test de leak.
- **Complejidad del gate híbrido** (oneshot en memoria + pending en SQLite): riesgo de
  divergencia. Mitigación: el id es la clave en ambos; `decide` actualiza SQLite y el oneshot
  bajo el mismo lock; tras restart no hay oneshot y la decisión se aplica al reintento.

## Exit criteria

- Un turno de agente crea una ejecución con eventos consultables por API.
- Un crash deja la ejecución `Interrupted` y `POST /retry` la reanuda sin duplicar efectos de
  tools (operation_id).
- Un pending de aprobación sobrevive a restart y se puede decidir; el reintento lo consume.
- Ningún secreto en eventos ni en la API de ejecuciones.
- Puertos sin cambios; SQLite viejo abre; tests nuevos + gates verdes; docs alineadas.
