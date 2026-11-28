# Diseño: corrección de contrato y concurrencia (Fase 10-D1)

- **Fecha:** 2026-11-28
- **Estado:** Diseño aprobado en brainstorming; pendiente de revisión del spec
- **Tipo:** Arquitectural — cambia el contrato de gRPC, el puerto de ejecución y el flujo de resolución de agente
- **Parte:** 1 de 4 del plan de mejoras posteriores a la serie 10 (D1 contrato/concurrencia → D2 config/ciclo → D3 rendimiento → D4 limpieza)

## 1. Contexto y problema

Tras cerrar la serie 10, una auditoría encontró cuatro bugs funcionales con evidencia en código:

1. **Race de retry concurrente** — `POST /api/executions/:id/retry` comprueba el estado (`execution_api.rs:533-547`) pero el cambio a `Running` ocurre después en `chat_core.rs:270-276` con un `UPDATE` sin guarda (`sqlite.rs:255-260`). Dos peticiones simultáneas pasan el filtro y ejecutan el mismo turno dos veces; los eventos y contadores se duplican.
2. **Doble decisión d1 en turnos con aprobación** — `handlers.rs:226-240` (`turn_has_approval_rules`) resuelve el agente con `resolve_selection`, y `handlers.rs:288` vuelve a entrar en `execute_chat` que resuelve de nuevo (`chat_core.rs:1007`). El routing es per-request y no sticky, así que el pre-check puede mirar la política del agente A y el turno ejecutar el B: parqueo erróneo, latencia doble, contadores duplicados.
3. **gRPC pierde el contrato** — `grpc.rs:82-89` mapea solo 3 variantes de `AppError` a `tonic::Code` y el resto a `internal` (no distingue 401/403/501/502 ni errores de aprobación). `proto/llamar.proto:22-27` no incluye `conversation_id`/`usage`/`served_by`, así que un cliente gRPC no puede reanudar la conversación que el servidor acaba de persistir.
4. **`connect_mcp` es un eco** — `grpc.rs:155-170` devuelve los mensajes sin parsear ni despachar; el HTTP `/api/mcp` sí llama a `mcp_api::handle_message`. Dos superficies, comportamientos distintos.

Objetivo: corregir los cuatro sin cambiar el contrato HTTP existente, con cambios aditivos donde haga falta (proto, puerto).

## 2. Decisiones

| # | Decisión | Justificación |
|---|---|---|
| D1 | Añadir método **aditivo** `claim_for_retry(id, from: &[ExecutionStatus], to: ExecutionStatus) -> Result<bool, String>` al puerto `ExecutionStore`. SQLite: `UPDATE ... WHERE id=? AND status IN (...)`, devuelve `rows == 1`. Noop: `Ok(true)`. El endpoint lo llama **antes** de `retry_turn`; si `false` ⇒ `409`. | Cierra la ventana check-act con una única operación atómica. El `record_resume` posterior sigue ocurriendo, pero solo el ganador del claim llega ahí. |
| D2 | Método **default en el trait** (`get` + comparación + `set_status`, no atómico) para los 5 test fakes; **override atómico en SQLite** (`UPDATE … WHERE … RETURNING`) y **override `Ok(true)` en NoopExecutionStore**. | Evita tocar 5 ficheros de test. El override en noop es **obligatorio**: su `get()` devuelve `None`, así que el default daría `false` ⇒ 409 y rompería el retry en boot degradado (donde hoy funciona). Documentar que los fakes no son concurrentes. |
| D3 | Resolver la selección **una sola vez** en `chat()`: `let resolved = resolve_selection(...).await?;` antes de la rama `stream`/gated. Pasar `&ResolvedSelection` a `turn_has_approval_rules` y al `execute_chat` correspondiente. | Una decisión por turno, coherente entre la comprobación de política y la ejecución. |
| D4 | `execute_chat` (y la variante con id) ganan un parámetro `Option<&ResolvedSelection>`: si `Some`, se usa tal cual; si `None`, resuelven como hoy (compatibilidad con gRPC, delegación, reaper). | Mantener los otros callers sin cambio; el HTTP pasa el resuelto. |
| D5 | gRPC: tabla `AppError → tonic::Code` basada en `status_code()` (`error.rs:85-105`): Validation/ApprovalArgsChanged→InvalidArgument, Unauthorized→Unauthenticated, Forbidden→PermissionDenied, NotFound/ApprovalNotFound→NotFound, Conflict/AlreadyDecided→AlreadyExists, ApprovalExpired→DeadlineExceeded, Unavailable→Unimplemented, Provider/BadGateway→Unavailable, resto→Internal. Mantener `record_grpc_error()`. | El cliente distingue "501 deliberado" de "servidor caído". |
| D6 | Proto: añadir campos **aditivos** a `ChatResponse`: `optional string conversation_id = 5; optional string served_by = 6; optional Usage usage = 7;` con `message Usage { uint64 input_tokens = 1; uint64 output_tokens = 2; optional double cost_usd = 3; }`. Poblar desde `ChatResponse` en `grpc.rs:112-120`. `debug_route`/`debug_prompt` quedan fuera (solo debug HTTP). | Continuidad de conversación y coste real sin romper clientes (campos opcionales). |
| D7 | `connect_mcp`: parsear `msg.json_rpc` con `serde_json::from_str::<JsonRpcRequest>`, llamar a `mcp_api::handle_message(state.clone(), Json(payload)).await`, extraer el cuerpo de la `Response` con `axum::body::to_bytes`, y emitir `pb::McpMessage { json_rpc: <body>, method: String::new(), params: String::new() }`. Errores de parseo ⇒ un `McpMessage` con JSON-RPC error -32700. | Un solo dispatch real para HTTP y gRPC. Sin nuevo trait. |

RFC 2119: **MUST**, **SHALL**, **SHOULD**, **MAY**, **MUST NOT**.

## 3. Details de implementación

### 3.1 `claim_for_retry`

```rust
// src/ports/execution.rs (additive)
async fn claim_for_retry(
    &self,
    id: Uuid,
    from: &[ExecutionStatus],
    to: ExecutionStatus,
) -> Result<bool, String> {
    // default: get + compare + set_status (no atómico; solo test fakes)
}
```

SQLite override con transacción implícita (una sola sentencia, patrón de `list_interrupted` en `sqlite.rs:291-299`, que usa estados serializados como JSON texto vía `to_json_text`):

```sql
UPDATE executions
   SET status = ?1, updated_at = ?2, finished_at = NULL, error = NULL
 WHERE id = ?3 AND status IN (?4, ?5, ...)
RETURNING id
```

**Debe limpiar `finished_at` y `error`**: el `set_status(Running)` actual (`sqlite.rs:255-260`) solo los escribe para estados terminales, así que sin esto la fila `Failed` reclamada conservaría marcadores terminales. Construir el `IN` con `params_from_iter` sobre los estados serializados, o un `match` para los dos conocidos (`Interrupted`/`Failed`) — decisión del implementador, pero **una sola sentencia SQL**. Usar `query_row` + `QueryReturnedNoRows ⇒ Ok(false)`. Llamar al claim (a) en `retry_execution` tras `get` y antes de `retry_turn` (`false` ⇒ mismo `409`), y (b) en `approval_reaper.rs:81` con `from=[WaitingApproval]` antes de la línea 95. `record_resume` puede quedarse (escritura idempotente post-claim).

### 3.2 Resolución única

`handlers.rs` pasa de:
```
if payload.stream { execute_chat_stream(...) }
else if turn_has_approval_rules(&state, &payload, selection).await? { ... spawn ... }
else { execute_chat(&state, payload, selection).await }
```
a:
```
let resolved = resolve_selection(&state, selection, &payload.messages).await?;
if payload.stream { execute_chat_stream_resolved(..., &resolved) }
else if turn_has_approval_rules(&resolved) { ... spawn (rebuilds owned selection inside) ... }
else { execute_chat_resolved(..., &resolved) }
```

El `ResolvedSelection` es `Clone`: **pasar una copia owned al task spawned** y usar una variante `execute_chat_with_new_id_resolved` que reciba `&ResolvedSelection`. Eso es lo que cierra el bug: si el task volviera a resolver, la comprobación de política podría inspeccionar al agente A mientras el turno parqueado ejecuta al B. El task ya reconstruye su `AgentSelection` como `'static` (`:298-308`); se añade el `resolved` como dato owned junto a esos strings. **No se permite ninguna segunda resolución en la rama gated.** (Corrección del auditor: la redacción original de esta sección decía lo contrario y era incorrecta.)

### 3.3 gRPC code mapping

Extraer `fn grpc_code(&AppError) -> Code` en `src/api/grpc.rs` (o en `error.rs` como `tonic_code()` bajo feature grpc — decidir y documentar). `to_status` usa `Status::new(code, error.to_string())` con `message` sin secretos (el `Display` de `AppError` ya evita claves).

## 4. Seguridad

- Ningún mensaje de error nuevo incluye valores de credenciales; se reutiliza `AppError::Display`.
- `claim_for_retry` no expone estados ajenos: devuelve `bool`, no el estado actual.
- `connect_mcp` usa el mismo `handle_message` que HTTP (mismos límites de validación y redacción).

## 5. Testing

### 5.1 Unit / integración

- `claim_for_retry`: (a) desde `Interrupted` ⇒ `true` y estado `Running`; (b) desde `Completed` ⇒ `false` y estado intacto; (c) dos claims concurrentes sobre la misma fila ⇒ solo uno `true` (test con `tokio::join!` y store SQLite en memoria o fichero temporal).
- `turn_has_approval_rules(resolved)`: test con un `ResolvedSelection` prefijado ⇒ no consume decisión d1 (fake router cuenta llamadas == 0) y devuelve la política correcta.
- `grpc_code`: tabla de todas las variantes de `AppError` → `Code` esperado (test exhaustivo).
- Proto: `ChatResponse` con `conversation_id`/`usage` ⇒ campos presentes en el decode del cliente.
- `connect_mcp`: `initialize` y `tools/list` por gRPC ⇒ mismos resultados que HTTP (comparar JSON).

### 5.2 Regresión

`cargo test --target-dir target-tests` (1045+ verdes), fmt, clippy, `bash tests/ci_config_tests.sh`. Los tests HTTP existentes de retry (`execution_api_tests.rs:469-620`) y aprobación (`approval_tests.rs:195-262`) siguen verdes.

## 6. Riesgos

| Riesgo | Mitigación |
|---|---|
| Cambiar la firma de `execute_chat` rompa gRPC/delegación/reaper | Parámetro `Option<&ResolvedSelection>` con `None` por defecto en esos callers; compilación driven. |
| `claim_for_retry` default no atómico en fakes enmascare el bug | Test explícito de concurrencia contra SQLite real, no contra fake. |
| Proto aditivo rompa clientes viejos | Campos `optional`; números 5-7 nuevos; clientes existentes ignoran los nuevos. |
| `connect_mcp` bloquee el stream esperando un human approval | `handle_message` es async pero no bloquea (las aprobaciones son requests separadas); timeout existente del gate. |

## 7. Fuera de alcance

D2 (Apply split-brain, hot-reload), D3 (compaction/caching/paginación/semáforos), D4 (docs/limpieza), cambio de contrato HTTP, nuevo endpoint.

## 8. Plan de implementación (siguiente paso)

Tras revisión, invocar `writing-plans`:

1. `claim_for_retry` en puerto + SQLite + fallback + test de concurrencia.
2. Resolución única en `chat` + parámetro `Option<&ResolvedSelection>` + test sin doble decisión.
3. `grpc_code` + campos proto + test exhaustivo y de continuidad.
4. `connect_mcp` real + test de paridad HTTP/gRPC.
5. Docs (si aplica) + regresión.

Criterio de aceptación: dos retries concurrentes ⇒ uno solo 200 y otro 409; un turno gated ⇒ una sola decisión d1; gRPC distingue 501 de 500 y devuelve `conversation_id`; `connect_mcp` despacha igual que HTTP.
