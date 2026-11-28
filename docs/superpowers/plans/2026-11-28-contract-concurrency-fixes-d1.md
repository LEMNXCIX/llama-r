# Corrección de contrato y concurrencia (Fase 10-D1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cerrar el race de retry concurrente, eliminar la doble decisión d1 en turnos con aprobación, alinear el contrato gRPC con HTTP y hacer que `connect_mcp` despache de verdad.

**Architecture:** `ExecutionStore` gana un claim atómico usado por retry y reaper; `chat()` resuelve una vez y pasa `ResolvedSelection` owned a la comprobación de política y al task; gRPC mapea todos los `AppError` a `tonic::Code` y carrya `conversation_id`/`usage` vía proto aditivo; `connect_mcp` reutiliza `mcp_api::handle_message`.

**Tech Stack:** Rust 1.97, rusqlite 0.43, tonic 0.14 + prost, axum 0.7, `Arc<dyn ExecutionStore>`.

**Spec:** `docs/superpowers/specs/2026-11-28-contract-concurrency-fixes-d1-design.md`

## Global Constraints

- `claim_for_retry(id, from: &[ExecutionStatus], to: ExecutionStatus) -> Result<bool, String>` aditivo; SQLite con **una sola** `UPDATE ... WHERE id=? AND status IN (...) RETURNING id` que **limpia `finished_at`/`error`**; Noop override `Ok(true)`; default del trait `get`+`set_status` para los 5 test fakes.
- Llamar al claim en `retry_execution` (tras `get`, antes de `retry_turn`; `false` ⇒ 409) y en `approval_reaper.rs:81` (`from=[WaitingApproval]`).
- Una sola resolución d1 por turno: `turn_has_approval_rules(&ResolvedSelection)`; **sin segunda resolución en el task spawned** (pasar `ResolvedSelection` owned); `execute_chat` público sin cambio de firma (variantes resolved-aware aditivas).
- gRPC: tabla exhaustiva `AppError → tonic::Code` (501→Unimplemented, 502→Unavailable, 401→Unauthenticated, etc.), `record_grpc_error()` intacto.
- Proto: campos aditivos en `ChatResponse` 5=`conversation_id`, 6=`served_by`, 7=`usage` (+ `message Usage`); `debug_*` fuera; `build.rs` regenera solo.
- `connect_mcp`: parsear `json_rpc` → `mcp_api::handle_message(self.state.clone(), ...)` → `to_bytes` → `McpMessage`; -32700 en parseo inválido; saltar 204.
- `cargo test --target-dir target-tests` MUST pasar (1045 tests); fmt/clippy limpios; CI gates verdes.

## Review Focus

- Dos retries concurrentes ⇒ exactamente uno 200 y otro 409 (test `tokio::join!` contra SQLite real). Task 1.
- Default del trait + Noop `Ok(true)`: retry en boot degradado sigue funcionando. Task 1.
- Turno gated ⇒ `decide()` del router llamado **una** vez, incluido el path spawned; política y agente ejecutado coinciden. Task 2.
- `Unavailable` → `Unimplemented` vs `Provider` → `Unavailable` (no colapsar a Internal). Task 3.
- `conversation_id` sobrevive el viaje gRPC; `connect_mcp` initialize/tools-list igual que HTTP. Tasks 3-4.

---

### Task 1: Claim atómico de retry y reaper

**Files:**
- Modify: `src/ports/execution.rs` (método aditivo + default)
- Modify: `src/adapters/execution/sqlite.rs`, `src/adapters/execution/noop.rs`
- Modify: `src/api/execution_api.rs` (`retry_execution`), `src/services/approval_reaper.rs`
- Test: `tests/execution_api_tests.rs` (ampliar)

**Interfaces:**
- Consumes: `ExecutionStatus` (`domain/execution.rs:19-36`); `set_status`/`get` del puerto; patrón `list_interrupted` en `sqlite.rs:291-299`.
- Produces: `ExecutionStore::claim_for_retry`.

- [ ] **Step 1: Write the failing tests**

En `tests/execution_api_tests.rs` (store SQLite real, fichero temporal):
1. claim desde `Interrupted` ⇒ `true`, fila en `Running`, `finished_at == None` y `error == None`.
2. claim desde `Completed` ⇒ `false`, fila intacta.
3. dos `tokio::join!` claims sobre la misma fila ⇒ exactamente uno `true`; vía HTTP, un 200 y un 409.
4. NoopExecutionStore: `claim_for_retry` ⇒ `Ok(true)` (no rompe retry degradado).
5. reaper: claim desde `WaitingApproval` ⇒ `true`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --target-dir target-tests --test execution_api_tests claim_for_retry`
Expected: FAIL — método no existe.

- [ ] **Step 3: Implement**

En `src/ports/execution.rs`, añadir el método con default:
```rust
async fn claim_for_retry(&self, id: Uuid, from: &[ExecutionStatus], to: ExecutionStatus) -> Result<bool, String> {
    // default no atómico: get + contains + set_status; SOLO para test fakes
}
```
Override en `noop.rs` ⇒ `Ok(true)`. Override en `sqlite.rs` con **una sola** sentencia:
```sql
UPDATE executions SET status=?1, updated_at=?2, finished_at=NULL, error=NULL
 WHERE id=?3 AND status IN (...) RETURNING id
```
Estados serializados con `to_json_text` (patrón `list_interrupted`); `params_from_iter`; `query_row` con `QueryReturnedNoRows ⇒ Ok(false)`. En `retry_execution` (`execution_api.rs:533-550`): tras el `get`, `if !claim_for_retry(id, &[Interrupted, Failed], Running).await? { return Err(409...) }` y luego `retry_turn`. En `approval_reaper.rs:81`: claim desde `WaitingApproval` antes de la línea 95.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --target-dir target-tests --test execution_api_tests && cargo test --target-dir target-tests`
Expected: PASS; suite verde.

- [ ] **Step 5: Commit**

```bash
git add src/ports/execution.rs src/adapters/execution src/api/execution_api.rs src/services/approval_reaper.rs tests/execution_api_tests.rs
git commit -m "feat(execution): add atomic claim_for_retry to close concurrent retry race"
```

---

### Task 2: Una sola decisión d1 en turnos gated

**Files:**
- Modify: `src/api/handlers.rs` (`turn_has_approval_rules`, `chat`)
- Modify: `src/api/chat_core.rs` (variants resolved-aware)
- Test: `tests/approval_parking_tests.rs` o `tests/router_dispatch_tests.rs`

**Interfaces:**
- Consumes: `ResolvedSelection` (`route_resolution.rs:36-41`, `Clone + Default`); `resolve_selection` (firma sin cambios — 14 callers en tests).
- Produces: `turn_has_approval_rules(&ResolvedSelection) -> Result<bool, AppError>`; variantes `execute_chat*_resolved`.

- [ ] **Step 1: Write the failing tests**

Con un `DecisionRouter` fake que cuenta `decide()`:
1. chat gated no-streaming ⇒ `decide()` llamado **una** vez (antes: 2).
2. chat gated que acaba en el task spawned (park) ⇒ `decide()` una vez y el agente cuya política se comprobó es el mismo que ejecuta (assert en el resolved que recibe el engine).
3. chat sin router / con `X-Agent` sigue sin consumir decisión.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --target-dir target-tests --test approval_parking_tests`
Expected: FAIL — contador 2.

- [ ] **Step 3: Implement**

En `handlers.rs::chat` (`:259-345`), para la rama no-streaming: `let resolved = resolve_selection(&state, selection, &payload.messages).await?;` **antes** de `turn_has_approval_rules`; cambiar esta última a `fn turn_has_approval_rules(resolved: &ResolvedSelection, model: &str) -> Result<bool, AppError>` (sin `resolve_selection` internamente, manteniendo el fallback `get_agent(model)`). Añadir en `chat_core.rs` variantes aditivas que acepten `Option<&ResolvedSelection>` y las usen en lugar de resolver (toques en `:1007` y `:1203`):
`execute_chat_resolved`, `execute_chat_with_new_id_resolved` (para el task, que recibe el `ResolvedSelection` **owned** junto a los strings ya `'static`), `execute_chat_stream_resolved`. **No cambiar las firmas públicas existentes**: envolverlas en las nuevas con `None`. `turn_has_approval_rules` tiene 1 caller (handlers.rs:288) — actualizarlo.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --target-dir target-tests --test approval_parking_tests && cargo test --target-dir target-tests`
Expected: PASS; los 14 tests de `router_dispatch_tests.rs` siguen verdes (firma de `resolve_selection` intacta).

- [ ] **Step 5: Commit**

```bash
git add src/api/handlers.rs src/api/chat_core.rs tests/approval_parking_tests.rs
git commit -m "fix(chat): resolve selection once for approval-gated turns"
```

---

### Task 3: Contrato gRPC — códigos y campos de respuesta

**Files:**
- Modify: `src/api/grpc.rs` (`to_status`, mapping de `ChatResponse`)
- Modify: `proto/llamar.proto`
- Test: `tests/grpc_wire_tests.rs`, `tests/api_integration.rs`

**Interfaces:**
- Consumes: `AppError::{code,status_code,Display}` (`error.rs:60-106`); `ChatResponse` (`models.rs:60-92`), `Usage` (`:26-32`).
- Produces: `grpc_code(&AppError) -> tonic::Code`; proto `Usage` + campos 5-7.

- [ ] **Step 1: Write the failing tests**

En `grpc_wire_tests.rs`: (a) tabla exhaustiva: para cada una de las 18 variantes de `AppError`, `grpc_code()` devuelve el `Code` de la spec (501→Unimplemented, 502→Unavailable, 401→Unauthenticated...); (b) round-trip de `ChatResponse` con `conversation_id="c-1"`, `served_by="profile"`, `usage={12,34,Some(0.5)}` ⇒ el cliente decode los tres campos; (c) `Unavailable` ≠ `Provider`. El test existente `api_integration.rs:690` (Validation→InvalidArgument) sigue verde.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --target-dir target-tests --test grpc_wire_tests`
Expected: FAIL — `grpc_code` y campos proto no existen.

- [ ] **Step 3: Implement**

En `grpc.rs`, reemplazar `to_status` por `fn grpc_code(error: &AppError) -> Code` (tabla del spec §D5) y `to_status` = `Status::new(grpc_code(&error), error.to_string())`, conservando `record_grpc_error()`. En `proto/llamar.proto:22-27`, conservar campos 1-4 y añadir `optional string conversation_id = 5; optional string served_by = 6; optional Usage usage = 7;` con `message Usage { uint64 input_tokens=1; uint64 output_tokens=2; optional double cost_usd=3; }`. Poblar en `grpc.rs:112-120` desde `event.conversation_id/served_by/usage` (map `Usage` → `pb::Usage`). `build.rs` regenera automáticamente.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --target-dir target-tests --test grpc_wire_tests && cargo test --target-dir target-tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/api/grpc.rs proto/llamar.proto tests/grpc_wire_tests.rs
git commit -m "feat(grpc): map all AppError codes and carry conversation metadata"
```

---

### Task 4: `connect_mcp` real + regresión final

**Files:**
- Modify: `src/api/grpc.rs` (`connect_mcp`)
- Test: `tests/grpc_wire_tests.rs` (ampliar)
- Docs: `AGENTS.md`, `README.md` si aplica (contrato gRPC)

**Interfaces:**
- Consumes: `mcp_api::handle_message(Arc<AppState>, Json<JsonRpcRequest>) -> axum::response::Response`; `JsonRpcRequest` (`models.rs:240-257`); `self.state` en `GrpcService`.
- Produces: streaming gRPC que despacha igual que HTTP.

- [ ] **Step 1: Write the failing tests**

Con el cliente generado: enviar `initialize` y `tools/list` por `connect_mcp`, asserts de que el JSON devuelto es equivalente al de `POST /api/mcp`; una notificación sin `id` ⇒ sin frame; JSON-RPC malformado ⇒ envelope `-32700`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --target-dir target-tests --test grpc_wire_tests connect_mcp`
Expected: FAIL — hoy hace eco.

- [ ] **Step 3: Implement**

En `grpc.rs:153-171`: para cada `msg`, `serde_json::from_str::<JsonRpcRequest>(&msg.json_rpc)` (fallback: construir desde `msg.method`/`msg.params` si `json_rpc` vacío); parseo inválido ⇒ `McpMessage { json_rpc: <envelope JSON-RPC con code -32700> }`; `let resp = mcp_api::handle_message(self.state.clone(), Json(req)).await;`; si `resp.status() == StatusCode::NO_CONTENT` ⇒ `continue`; `let bytes = axum::body::to_bytes(resp.into_body(), 2*1024*1024).await` (mapear error a `Status::internal`); `yield McpMessage { json_rpc: String::from_utf8_lossy(&bytes).into_owned(), method: String::new(), params: String::new() }`. Documentar en el código que `tools/call` con aprobación puede bloquear el stream (el park-and-ack de `handlers.rs:288-343` no aplica aquí) — es comportamiento heredado del dispatch HTTP, no regresión.

- [ ] **Step 4: Run full regression**

Run: `cargo test --target-dir target-tests && cargo fmt --check && cargo clippy --all-targets -- -D warnings && bash tests/ci_config_tests.sh`
Expected: todo verde; 1045+ tests.

- [ ] **Step 5: Commit**

```bash
git add src/api/grpc.rs tests/grpc_wire_tests.rs AGENTS.md README.md
git commit -m "feat(grpc): dispatch connect_mcp through the real MCP handler"
```
