# Cierre de deudas: auth administrativa, aprobaciones aparcadas y exporter OTLP

**Fecha:** 2026-11-21
**Estado:** aprobado en brainstorming (diseño de 3 secciones), pendiente de revisión del spec
**Alcance:** tres piezas independientes tomadas como referencia de Eve (Vercel) sin copiar su
formato TS/MD ni su motor de workflows externo: (1) token compartido para endpoints
administrativos, (2) `WaitingApproval` + reanudo real de aprobaciones, (3) exporter OTLP/HTTP
opcional sobre las trazas ya existentes.

## Contexto

El roadmap global está completo (Fases 0-8) y `docs/research/llama-r-vs-eve-vercel.md` identificó
tres lagunas frente a Eve que sí valdría cerrar. El mapeo de seams confirma que casi todo el
plumbing ya existe:

- `AppError::Unauthorized` ya mapea a `401` (`src/error.rs:26-29,88`); el patrón HMAC/`auth_env`
  fail-closed de webhooks es el modelo a seguir (`src/api/webhook_api.rs:70-79`).
- El gate ya persiste pendientes y decisiones, y `request_for` ya devuelve un receiver
  **pre-resuelto** vía `decision_for` en retry (`src/services/approval.rs:197-276,426-432`). Pero
  `WaitingApproval` nunca se setea (grep: solo definido en `src/domain/execution.rs:27`), el reaper
  es solo *lazy pruning* (`:387-391`) y el request en vivo espera `timeout(120 s, rx)`
  (`src/adapters/rig_engine/approval.rs:148`) — tras expirar, solo un retry manual consume la decisión.
- `record_trace` ya produce un `Value` **redactado** y persistido (`src/api/chat_core.rs:296-346`);
  no hay crate OTel, pero `reqwest 0.13` ya está en `Cargo.toml:21`.

Se conservan deliberadamente las decisiones del proyecto: TOML + Markdown, SQLite embebido,
turn-level retry (no step-replay), sin sandbox y sin identidad de usuario multitenant.

## Goal

Cerrar las tres lagunas sin cambiar el formato de autoría, sin motor de workflows externo y sin
dependencias nuevas: admin exige token, las aprobaciones se aparcan y se reanudan solas, y las
trazas pueden exportarse a OTLP/HTTP cuando se activa.

## Non-goals

- OIDC / Passport / identidad de usuario multitenant ni aprobadores designados.
- Sandbox de ejecución (microVM), ni ejecución de código generado aislada.
- Step-level replay, `parking` entre deploys con motor externo, ni `ctx.ask`/workflow-tasks.
- Cambiar el contrato de `/chat`, `/api/chat`, `/v1/chat/completions` ni de los webhooks.
- Cambiar `X-Debug` (convive con el token) ni quitar endpoints.
- Añadir el crate `opentelemetry` — el exporter es HTTP JSON con `reqwest`.

## Decisiones de diseño

### 1. Auth administrativa

1. **`ADMIN_TOKEN`** por entorno, nombre de variable directo (como `OPENROUTER_API_KEY`), leído en
   `Config::from_env` (`src/config.rs:102-211`) y persistido por `save_to_env` (`:217-288`,
   chmod 0600 `:272-285`). **Fail-closed**: si falta o está vacío, todos los endpoints admin
   devuelven `401` con mensaje fijo, sin eco del token.
2. **Cabecera `X-Admin-Token`**, comparada en tiempo constante mediante un helper
   `constant_time_eq(&[u8], &[u8])` extraído de `src/adapters/channel/hmac.rs` (sin new dependency).
   Un único `from_fn` middleware `require_admin` se aplica a un sub-`Router` que se hace `merge`
   en el router principal (`src/runtime.rs:321-392`) — un solo punto de aplicación.
3. **Superficie protegida**: `/api/approvals` (GET), `/api/approvals/:id` (POST), `/api/executions`
   (GET), `/api/executions/:id` (GET), `/api/executions/:id/retry` (POST), `/api/rag/*` (3 rutas),
   `/api/route` (POST), `/api/contexts*` (5 rutas), `/api/agents*` (6 rutas). Sin cambios:
   `/chat`, `/api/chat`, `/v1/chat/completions`, `/api/webhooks/:channel`, `/health`, `/models`,
   `/docs`, `/openapi.json`.
4. **`X-Debug` convive**: los endpoints de ejecuciones y RAG siguen exigiéndolo además del token.
   Ningún cliente actual se rompe: el token es adicional, no sustituto. La auth es fail-closed
   incluso si `X-Debug: true`.
5. OpenAPI (`src/api/docs.rs:14-61`) declara un esquema `securitySchemes.apiToken` (apiKey en
   header) y lo asocia a las rutas admin documentadas; el token nunca aparece como valor.

### 2. Aprobaciones aparcadas

6. **Sin timeout cuando se aparca**: nuevo flag `APPROVAL_PARK` (default `true`) y
   `APPROVAL_TIMEOUT_SECS` (default `120`, `0` = sin timeout). `ApprovalGate.timeout()` pasa a
   `Option<Duration>`; `src/adapters/rig_engine/approval.rs:148` usa
   `match timeout { Some(d) => tokio::time::timeout(d, rx).await, None => rx.await }`. Con park,
   el gate espera la decisión sin expirar el turno.
7. **`WaitingApproval` real**: `request_for` (`src/services/approval.rs:197-276`), tras persistir
   el pending, llama `store.set_status(execution_id, WaitingApproval, None)` y registra el id en un
   `parked_live: RwLock<HashSet<Uuid>>` del gate. El `Started`/`WaitingApproval` se refleja en el
   execution log (el port ya tiene `EventKind::WaitingApproval`, `src/domain/execution.rs:75`).
8. **Handler no bloqueante**: para turnos cuyo scope tiene reglas de aprobación
   (`PermissionPolicy.approvals` no vacío, resoluble antes del engine), `execute_chat` corre en
   `tokio::spawn` con un `oneshot`; el handler HTTP espera una gracia `PARK_GRACE_MS` (default
   1000) y, si el turno sigue pendiente, responde `202 { execution_id, approval_id? }` en vez de
   `ChatResponse`. Si termina dentro de la gracia, responde normalmente. Los turnos sin reglas de
   aprobación siguen siendo sincrónicos — comportamiento por defecto intacto. Un `ChatAck` DTO
   aditivo (`execution_id`, `approval_id: Option<String>`) se añade a `src/domain/models.rs`.
9. **Reaper en background** (nuevo, reemplaza el comentario obsoleto): al boot se spawn una tarea
   que cada `REAPER_INTERVAL_SECS` (default 5) consulta `ExecutionStore::list_interrupted` —
   que ya selecciona `Running` + `WaitingApproval` (`sqlite.rs:278-286`) — y, para cada ejecución
   `WaitingApproval` **ausente de `parked_live`** (huérfana por restart), comprueba si su pending
   ya tiene decisión (`decision_for`) y re-ejecuta el turno bajo el **mismo `execution_id`** por la
   ruta de retry existente (`src/api/execution_api.rs:228-300`). `request_for` devuelve receiver
   pre-resuelto; la idempotencia por `operation_id` replaya efectos ya completados. Al reanudar,
   se limpia el flag y se setea `Running`.
10. **Decisiones en vivo**: `gate.decide` ya dispara el oneshot (`:361-365`) y quita el id de
    `parked_live`; la tarea original continúa y finaliza el turno. No hay doble reanudo: el reaper
    ignora los ids en `parked_live`. Deny / hash mismatch / expired siguen siendo errores nombrados,
    nunca un park.
11. **Secretos y args**: sin cambios — `args_hash` canónico, redacción previa, endpoints con
    token, ningún body/firma/token en logs ni respuestas.

### 3. Exporter OTLP opcional

12. **Puerto aditivo** `TraceExporter { async fn export(&self, trace: &Value) }` en
    `src/ports/telemetry.rs`; adapter `OtlpHttpExporter` en `src/adapters/telemetry/` con
    `reqwest` (ya presente). `NoopExporter` por defecto.
13. **Config**: `OTEL_ENABLED` (default `false`), `OTEL_ENDPOINT` (default
    `http://127.0.0.1:4318/v1/traces`), `OTEL_SERVICE_NAME` (default `llama-r`) en `Config`
    (`src/config.rs:77-100`), `from_env`, `save_to_env`, `CONFIG_ENV_VARS` (`:324-337`) y
    `.env.example`.
14. **Wire format**: `POST` JSON OTLP/HTTP — `resourceSpans[0].resource.attributes` con
    `service.name`, `scopeSpans[0].spans[0]` con `traceId` (hex de 16 bytes del `execution_id`),
    `spanId` (hex de los primeros 8 bytes), `name = "agent.turn"`,
    `startTimeUnixNano`/`endTimeUnixNano` desde `latency_ms`, y `attributes` con los campos ya
    redactados del payload (`agent_qualified_id`, `provider`, `model`, `input_tokens`,
    `output_tokens`, `cost_usd`, jerarquía). Ningún arg ni secreto.
15. **Best-effort**: `record_trace` (`src/api/chat_core.rs:337-346`), tras persistir, llama al
    exporter; error → `tracing::warn!`, el chat nunca falla. Exporter en `AppState` fuera del
    `ProviderStack`, companion vía `CompanionHandles` (patrón Fase 4/5), boot y Apply comparten ruta.
16. Sin dependencia nueva (`reqwest`, `serde_json`, `uuid` ya están). Ningún puerto sagrado cambia.

## Arquitectura

- `src/ports/{auth,telemetry}.rs` (o un solo `src/ports/admin.rs`), `src/adapters/telemetry/`,
  `src/middleware/` o `src/api/auth.rs` para `require_admin`, `src/services/approval_reaper.rs`.
- `src/api/{docs,handlers,approval_api,execution_api,rag_api,route_api,context_api,agent_api}.rs`
  solo para el router group / DTO `ChatAck`.
- `src/runtime.rs`: sub-router admin + reaper + exporter companion.
- `src/config.rs`, `.env.example`, `src/domain/models.rs`, `src/error.rs` (si hiciera falta un
  `ApprovalPending` status DTO), `src/adapters/rig_engine/approval.rs`, `src/api/chat_core.rs`.

## Flujo de datos

1. Request a `/api/approvals` → middleware compara `X-Admin-Token` con `ADMIN_TOKEN` en tiempo
   constante → `401` genérico si falta/mal; si pasa, `require_debug` (si aplica) y handler.
2. Turno con reglas de aprobación → `request_for` persiste pending, setea `WaitingApproval`,
   añade id a `parked_live` → el gate espera sin timeout → handler devuelve `202` tras la gracia.
3. Humano decide → `gate.decide` persiste + dispara oneshot + quita `parked_live` → la tarea
   original continúa y finaliza. Si el proceso murió, el reaper ve `WaitingApproval` huérfana +
   decisión persistida → retry con el mismo id → effects replayados → `Completed`.
4. `record_trace` persiste el evento redactado → si `OTEL_ENABLED`, POST OTLP/HTTP best-effort.

## Manejo de errores

- Token ausente/inválido → `401` fijo; ningún valor de token en logs/respuesta.
- Park: deny / hash mismatch / expired → errores nombrados, nunca park. Fallo del reaper → warning,
  reintenta en el próximo tick. Fallo de `set_status` → warning, el park sigue (best-effort como
  el resto del store).
- Exporter: endpoint caído o payload inválido → warning, el chat sigue; disabled → cero requests.

## Testing

- Auth: `admin_token_required_on_all_admin_routes` (tabla de rutas, sin token → 401),
  `valid_token_reaches_handler`, `token_never_in_logs_or_response`, `chat_and_webhooks_unauthenticated`,
  `x_debug_still_required_for_executions`, `constant_time_eq_vectors`.
- Park: `approval_turn_returns_202_with_execution_id`, `gate_sets_waiting_approval`,
  `decision_resumes_original_turn`, `restart_reaper_resumes_decided_orphan` (simula parked_live vacío),
  `no_double_resume_when_live`, `deny_and_hash_mismatch_are_named_errors`.
- OTLP: `otel_disabled_sends_nothing`, `otel_export_posts_expected_span`,
  `otel_failure_is_best_effort`, `otel_payload_has_no_secret`.
- Gates: fmt / clippy `-D warnings` / audit / suite completa (932+) / build `--no-default-features`.

## Riesgos

- **Doble reanudo:** mitigado con `parked_live`; el reaper solo toma huérfanos. Coste si falla:
  tool effects duplicados — el riesgo principal de esta pieza.
- **Romper clientes actuales:** mitigado haciendo el token adicional y `X-Debug` conviviente;
  los turnos sin aprobaciones siguen sincrónicos. Coste si falla: regresión HTTP amplia.
- **Exporter con secretos:** mitigado exportando solo el payload ya redactado de `TraceRecorded`.
  Coste si falla: fuga al backend de observabilidad.
- **Nueva dependencia:** ninguna (reqwest/serde_json/uuid presentes); verificar `Cargo.toml` sin diff.
- **Park sin motor externo:** el reaper es en-proceso; tras un crash se recupera por el store, pero
  no hay `parking` entre deploys como Eve. Aceptado explícitamente (non-goal).

## Exit criteria

- Todos los endpoints admin devuelven 401 sin token; el resto de superficies siguen igual.
- Una aprobación larga deja el turno `WaitingApproval`, devuelve `202` y se reanuda sola al decidir
  (también tras restart); sin doble reanudo.
- Con `OTEL_ENABLED=true` las trazas se envían a OTLP/HTTP con campos redactados; con false, cero
  requests y ningún fallo de chat. Sin dependencia nueva, sin cambio de puertos sagrados, gates verdes.
