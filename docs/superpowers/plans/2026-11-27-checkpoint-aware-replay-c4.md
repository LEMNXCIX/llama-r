# Replay informado por checkpoints (Fase 10-C4) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Emitir checkpoints reales, avanzar `current_step` y hacer el retry checkpoint-aware con un resume note redactado, sin prometer replay byte-exacto del modelo.

**Architecture:** Los bridges (que llama-r controla) emiten `ToolCallRequested`/`CheckpointCreated` y avanzan `current_step` vía un método aditivo del store; `retry_turn` lee esos checkpoints + resultados idempotentes y construye un resume note que guía al modelo, con `from_step` opcional y `max_iterations` reducido. Rig sigue dueño del loop.

**Tech Stack:** Rust 1.97, SQLite/rusqlite, serde_json, tokio, axum admin endpoints, `redact_secrets`.

**Spec:** `docs/superpowers/specs/2026-11-27-checkpoint-aware-replay-c4-design.md`

## Global Constraints

- `CheckpointCreated` payload: `{step, next_step_index, tool, args_hash, replayed, status}` — **nunca** args crudos ni resultados en el evento.
- `ExecutionStore` gana método **aditivo** `set_current_step(id, step)` (SQLite + noop); best-effort.
- Resume note redactado (`redact_secrets`) y truncado (200 chars); resultados de `get_operation`.
- `from_step: Option<u32>` en el body de retry (0 = todo); X-Debug + admin sin cambios.
- `max_iterations` reducido vía campo aditivo en `AgentRunRequest` (no cambiar firma de `AgentEngine`).
- Sin cambio a `AgentEngine` trait, sin reimplementar Rig, sin nuevo endpoint, sin mensajes tool-call nativos.
- `cargo test --target-dir target-tests` MUST pasar (1027 tests); fmt/clippy limpios.

## Review Focus

- Checkpoint con tool que recibe `api_key` ⇒ solo `args_hash`, nunca el valor. Test en Task 1.
- Resume note con resultados de tools ⇒ redactado + truncado; `from_step=2` filtra paso 1. Test en Task 2.
- Tool NO se re-ejecuta en retry (contador sigue 1), `ToolCallCompleted{replayed:true}` aparece. Test en Task 3 E2E.
- Legacy row sin `user_message` sigue 403; comportamiento por defecto (sin `from_step`) compatible. Test en Task 3.
- `set_current_step` en noop y SQLite sin romper otros callers del puerto. Test en Task 1.

---

### Task 1: Checkpoints reales + `set_current_step`

**Files:**
- Modify: `src/domain/execution.rs` (helper de payload si aplica — o construir en bridges)
- Modify: `src/ports/execution.rs` (método aditivo)
- Modify: `src/adapters/execution/sqlite.rs`, `src/adapters/execution/noop.rs` (impl)
- Modify: `src/adapters/rig_engine/operations.rs` (emisión `ToolCallRequested`/`CheckpointCreated` + avanzar step)
- Test: `tests/checkpoint_replay_tests.rs` (nuevo)

**Interfaces:**
- Consumes: `EventKind::{ToolCallRequested, CheckpointCreated}` (definidos, nunca emitidos); `OperationContext { execution_id, store }` (`operations.rs:22-47`); `args_hash_hex` (`redaction.rs`).
- Produces: `set_current_step` en el puerto; bridges emiten checkpoint con payload `{step,next_step_index,tool,args_hash,replayed,status}`.

- [ ] **Step 1: Write the failing tests**

- Store: `set_current_step` en SQLite persiste y lee de vuelta (`get().current_step == N`); noop no-op.
- Bridge: ejecutar una tool via `run_idempotent` con un store fake, verificar que se emiten `ToolCallRequested` y `CheckpointCreated` con `args_hash` (no `arguments`), `replayed:false`, y que `current_step` avanzó. Repetir con resultado stored ⇒ `replayed:true`.
- Un tool con argumento `api_key` ⇒ el checkpoint NO contiene el valor.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --target-dir target-tests --test checkpoint_replay_tests`
Expected: FAIL — método/emisión no existen.

- [ ] **Step 3: Implement**

Añadir `async fn set_current_step(&self, execution_id: Uuid, step: u32) -> Result<(), String>;` al trait (default no-op no se puede en trait object — implementar en ambos adapters). En `operations.rs::run_idempotent`: antes de ejecutar, `append_event(ToolCallRequested{step, tool, args_hash})`; después (o en la rama replay), `append_event(CheckpointCreated{... status})` y `set_current_step(id, step+1)`. El `step` se obtiene de `get(id).current_step` (best-effort, default 0). Ningún payload incluye args/resultados. Los store errors son warnings.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --target-dir target-tests --test checkpoint_replay_tests && cargo test --target-dir target-tests`
Expected: PASS; suite verde.

- [ ] **Step 5: Commit**

```bash
git add src/domain/execution.rs src/ports/execution.rs src/adapters/execution src/adapters/rig_engine/operations.rs tests/checkpoint_replay_tests.rs
git commit -m "feat(execution): emit checkpoints and advance current_step"
```

---

### Task 2: Retry checkpoint-aware + resume note + `from_step`

**Files:**
- Modify: `src/api/execution_api.rs` (`retry_turn`, `retry_execution` body DTO)
- Modify: `src/ports/engine.rs` (campo aditivo `max_iterations_override: Option<usize>` en `AgentRunRequest`)
- Modify: `src/services/agent_runtime.rs` (usar el override al construir `AgentRunRequest`)
- Test: `tests/checkpoint_replay_tests.rs` (ampliar) o `tests/execution_api_tests.rs`

**Interfaces:**
- Consumes: `get_events`, `get_operation`, `set_current_step` (Task 1); `redact_secrets` (`services/redaction.rs`).
- Produces: `retry_turn` con resume note + `from_step` + override de iteraciones.

- [ ] **Step 1: Write the failing tests**

- Con 2 checkpoints + resultados stored: el `ChatRequest` que `retry_turn` pasa a `execute_chat_with_id` contiene un user_message con el resume note (pasos 1 y 2, resultados redactados/truncados).
- `from_step=2` ⇒ el note solo incluye el paso 2 (o ninguno si step < 2). Verificar filtro.
- Un resultado con `api_key` en el tool ⇒ el note contiene `[REDACTED]`, nunca el valor.
- Sin checkpoints ⇒ note vacío, comportamiento igual al retry actual (test existente `execution_api_tests.rs:463-538` sigue verde).
- `max_iterations_override` reducido en el `AgentRunRequest` resultante.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --target-dir target-tests --test execution_api_tests`
Expected: FAIL — no hay resume note / from_step.

- [ ] **Step 3: Implement**

En `retry_execution`, aceptar body opcional `{"from_step": u32}` (si el handler no toma body hoy, añadir `Option<Json<...>>` con `#[serde(default)]`; verificar compile). Pasar `from_step` a `retry_turn`. En `retry_turn`: tras las validaciones actuales, `get_events(id)` → filtrar `CheckpointCreated` con `step >= from_step`; para cada uno, `get_operation(operation_id)` → resultado; construir resume note con `redact_secrets(...)` y truncar a 200 chars por resultado; prefijarlo al `user_message` (decisión del implementador: user_message vs system_prompt — documentar). Calcular `iterations_override = max_iterations.saturating_sub(completed_steps).max(1)` y pasarlo vía el nuevo campo `max_iterations_override` en `AgentRunRequest` (en `agent_runtime.rs`, al construir el request, usar `override.unwrap_or(scope.max_iterations)`). Todo best-effort: si get falla, note vacío.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --target-dir target-tests --test execution_api_tests && cargo test --target-dir target-tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/api/execution_api.rs src/ports/engine.rs src/services/agent_runtime.rs tests/execution_api_tests.rs
git commit -m "feat(execution): make retry checkpoint-aware with a redacted resume note"
```

---

### Task 3: E2E y regresión de idempotencia

**Files:**
- Test: script temporal `/tmp/opencode/llama-r-c4-e2e.sh` + `tests/checkpoint_replay_tests.rs` (ampliar)

**Interfaces:** Tasks 1-2 completos.

- [ ] **Step 1: Write the failing test**

Test E2E con fake engine: turno ejecuta una tool (contador=1), forzar `Interrupted`, retry con `from_step` y un fake que registra el mensaje recibido. Asserts: el mensaje contiene el resume note, la tool NO se re-ejecuta (contador sigue 1), `ToolCallCompleted{replayed:true}` en el event log, `current_step > 0`. Legacy row sin `user_message` ⇒ 403 (test existente).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --target-dir target-tests --test checkpoint_replay_tests`
Expected: FAIL si el replay no está cableado al note.

- [ ] **Step 3: Implement/verify**

Si el fake engine no permite inspeccionar el mensaje, añadir un hook de test o verificar vía el store el resume note persistido. No cambiar producción salvo lo ya hecho en Task 2.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --target-dir target-tests && cargo fmt --check && cargo clippy --all-targets -- -D warnings && bash tests/ci_config_tests.sh`
Expected: todo verde; 1027+ tests.

- [ ] **Step 5: Commit**

```bash
git add tests/checkpoint_replay_tests.rs
git commit -m "test(execution): verify checkpoint-aware retry replays without re-executing tools"
```

---

### Task 4: Docs honestos y cierre

**Files:**
- Modify: `AGENTS.md` (308, 470, 473, 476), `README.md` (sección durable execution)

**Interfaces:** Tasks 1-3 completos.

- [ ] **Step 1: Update docs**

Reemplazar en `AGENTS.md:473` "Recovery is at TURN level — not step replay … Exact model/tool step replay is future work" por: "Recovery is checkpoint-informed: checkpoints (`step`, `tool`, `args_hash`, `replayed`) are emitted after each tool call; `current_step` advances; retry reads checkpoints + idempotent tool results and injects a redacted resume note so the model is steered past completed steps. Tool effects replay via `operation_id`; the model re-decides its answer — this is NOT byte-exact model replay (Rig owns the loop)." Actualizar `:470` (CheckpointCreated ya se emite), `:308` (body `from_step` opcional), `:476` (quitar "no step-level replay", reemplazar por la descripción real). README equivalente en español, sin prometer replay exacto.

- [ ] **Step 2: Full regression**

Run: `cargo test --target-dir target-tests && cargo fmt --check && cargo clippy --all-targets -- -D warnings && bash tests/ci_config_tests.sh`
Expected: verde; sin servidores vivos.

- [ ] **Step 3: Commit**

```bash
git add AGENTS.md README.md
git commit -m "docs: describe checkpoint-informed replay honestly"
```
