# Diseño: replay informado por checkpoints (Fase 10-C4)

- **Fecha:** 2026-11-27
- **Estado:** Diseño aprobado en brainstorming; pendiente de revisión del spec
- **Tipo:** Arquitectural — emite checkpoints, avanza `current_step` y hace el retry checkpoint-aware
- **Perfil:** LAN / localhost, uso personal (hereda A/B/C1/C2/C3)
- **Parte:** 4 de 4 de la Fase 10-C (C1 ✅ → C2 ✅ → C3 ✅ → C4) — cierre de la serie 10

## 1. Contexto y problema

Hoy `POST /api/executions/:id/retry` re-ejecuta **todo el turno** desde el `user_message` redactado bajo el mismo `execution_id` (`src/api/execution_api.rs:228-270`). La idempotencia de herramientas (`operation_id = <execution_id>:<tool>:<args_hash>`, `operations.rs:53`) garantiza que los efectos de tools ya completados se **replayan** en vez de repetirse — pero el modelo regenera su respuesta y puede tomar otro camino. `current_step` se fija en 0 y nunca se incrementa; `CheckpointCreated` está definido (`execution.rs:73`) pero nunca se emite y "performs no replay" (`AGENTS.md:470`).

**Hallazgo decisivo (factibilidad):** el replay **exacto** de pasos no es viable sin reimplementar el tool loop. `run_agent_inner` (`builder.rs:227-250`) hace `agent.prompt(...).history(...).await` y Rig ejecuta el bucle multi-turn internamente (`default_max_turns`, tool bridging, re-llamadas). llama-r solo observa eventos vía `event_tx`; no hay API para decirle "reanuda desde el paso N". Ya estaba registrado como non-goal en `docs/superpowers/specs/2026-11-18-phase7-durable-execution-design.md`.

El usuario eligió el **middle ground honesto**: checkpoints reales + replay lógico (tool effects replayados, modelo guiado para no repetir pasos), sin prometer replay byte-exacto.

Objetivo: hacer el retry **checkpoint-aware** — emitir checkpoints observacionales, avanzar `current_step`, y alimentar al modelo un "resume note" con los pasos ya completados para que no los repita, manteniendo la idempotencia de tools como mecanismo de replay de efectos.

## 2. Decisiones

| # | Decisión | Justificación |
|---|---|---|
| D1 | Emitir `ToolCallRequested`, `CheckpointCreated` (y `ModelRequested` donde aplique) en los **únicos hooks que llama-r posee**: los bridges MCP/local (`tools.rs`, `local_tools.rs`) y `AgentRuntime`. Payload: `{step, next_step_index, tool, args_hash, replayed}`. | Esos bridges ya emiten `ToolCall`/`ToolResult`; añadir el checkpoint es incremental, sin tocar Rig. |
| D2 | Añadir un método **aditivo** `set_current_step(&self, id, step)` al puerto `ExecutionStore` (y a sus 2 adapters: SQLite + noop) e incrementarlo desde los bridges. | `current_step` ya existe en el schema; solo falta escribirlo. Aditivo = no rompe otros callers. |
| D3 | En el retry, leer el último checkpoint + `get_operation` de cada `operation_id` y construir un **"resume note" redactado** que se antepone al `user_message` (o se inyecta en `system_prompt`) con: pasos completados, herramientas ya ejecutadas y sus resultados almacenados truncados. | Guía al modelo para no repetir pasos; los resultados son de tools ya autorizados y se redactan con `redact_secrets`. |
| D4 | Campo opcional `from_step: Option<u32>` en el body de `POST /api/executions/:id/retry`. Si se da, se usan checkpoints con `step >= from_step`; si no, comportamiento actual (todo el turno) + resume note igualmente. | Permite al operador elegir desde dónde reanudar sin cambiar el contrato por defecto. `X-Debug`/admin sin cambios. |
| D5 | Reducir `max_iterations` del turno de reintento en el número de pasos ya completados (mínimo 1). | Evita que el modelo gaste iteraciones en pasos que ya están hechos. Se pasa por `AgentRunRequest`, no por un env. |
| D6 | Nombres honestos: llamar al resultado **"checkpoint-informed deterministic resume"**; documentar explícitamente que el replay es de **efectos de tools**, no de tokens del modelo. Reemplazar "Exact model/tool step replay is future work" por la descripción real. | No prometer algo que el motor no puede hacer. |
| D7 | **Sin** cambio a `AgentEngine` trait, sin reimplementar el loop de Rig, sin nuevo endpoint, sin mensajes tool-call nativos en el puerto, sin secretos en payloads de checkpoint. | Alcance acotado; multi-proveedor intacto. |
| D8 | Todo best-effort: si el store falla al leer checkpoints o escribir `current_step`, warning y degradar al retry actual (sin resume note), nunca fallar el chat. | Convención de durabilidad del proyecto. |

RFC 2119: **MUST**, **SHALL**, **SHOULD**, **MAY**, **MUST NOT**.

## 3. Modelo de datos y store

### 3.1 Payload de `CheckpointCreated`

```json
{
  "step": 3,
  "next_step_index": 4,
  "tool": "server__tool",       // o bare id para local
  "args_hash": "<64 hex>",      // canon, nunca args crudos
  "replayed": false,
  "status": "completed"         // completed | failed
}
```

`ToolCallRequested` comparte payload sin `replayed`; `ModelRequested` lleva `{step, model}`. Ningún payload incluye `arguments`, `preview` ni claves.

### 3.2 `ExecutionStore` (src/ports/execution.rs)

Añadir:

```rust
async fn set_current_step(&self, execution_id: Uuid, step: u32) -> Result<(), String>;
```

SQLite: `UPDATE executions SET current_step = ?, updated_at = ? WHERE id = ?`. Noop: `Ok(())`. Best-effort en los callers.

### 3.3 Resume note

Construida en `retry_turn` (`execution_api.rs`):

```
[Resume] Este turno se está reanudando desde el paso N.
Ya se completaron los siguientes pasos (sus efectos están guardados y NO deben ejecutarse de nuevo):
- paso 1: herramienta 'server__tool' (hash abc123…) — resultado: <redactado, truncado a 200 chars>
- paso 2: herramienta 'local_echo' (hash def456…) — resultado: <redactado>
Continúa desde el paso N+1. No repitas los pasos anteriores.
```

El contenido se pasa como prefijo del `user_message` (o `system_prompt` — decisión del implementador, documentar) y pasa por `redact_secrets` antes de usarse. Los resultados vienen de `get_operation(operation_id)`, que ya están redactados en el store.

## 4. Flujo de retry checkpoint-aware

1. Cliente: `POST /api/executions/:id/retry` con `{"from_step": N}` opcional (X-Debug + admin tokens como hoy).
2. `retry_turn`: (a) validaciones actuales (legacy row, decisiones post-crash vía `note_decided_approvals`); (b) `get_events(id)` → último `CheckpointCreated` con `step >= from_step.unwrap_or(0)`; (c) para cada checkpoint, `get_operation(operation_id)` → resultado redactado; (d) construir resume note; (e) calcular `max_iterations` reducido; (f) `execute_chat_with_id` con el mensaje enriquecido y el mismo `id`.
3. Durante la ejecución, los bridges emiten `CheckpointCreated` y llaman `set_current_step`, incrementando `current_step`.
4. La idempotencia (`run_idempotent`) sigue siendo el mecanismo de replay: si el modelo vuelve a llamar una tool ya hecha, `get_operation` devuelve el resultado stored y se emite `ToolCallCompleted{replayed:true}`.

## 5. Seguridad

- Checkpoint payloads: `args_hash` (canon SHA-256), **nunca** args crudos ni resultados de herramientas en los eventos.
- Resume note: resultados pasan por `redact_secrets` (Fase 9) y se truncan; los store ya redactan en escritura.
- `from_step` es un entero validado, no una cadena; `0` = desde el inicio. Sin inyección en prompts.
- Admin/Debug gates sin cambios.

## 6. Testing

### 6.1 Unit / integración

- `set_current_step` en SQLite (lee de vuelta) y noop.
- Bridges: al ejecutar una tool, emiten `ToolCallRequested` + `CheckpointCreated` con payload correcto y avanzan `current_step`; en replay (`replayed:true`) el checkpoint lo refleja.
- `retry_turn` con 2 checkpoints y resultados stored: resume note contiene ambos pasos y resultados redactados; `from_step=2` filtra el paso 1; sin checkpoints cae al retry actual.
- Payload de checkpoint con un tool que recibe `api_key` ⇒ solo `args_hash`, nunca el valor (extensión de los tests de redacción).

### 6.2 E2E

- Turno con un fake tool que se ejecuta una vez, forzar `Interrupted`, retry con fake engine que registra el `step` solicitado: verificar que el resume note llega al modelo, que la tool NO se re-ejecuta (contador sigue en 1), que `ToolCallCompleted{replayed:true}` aparece, y que `current_step` avanzó.
- `from_step` filtra checkpoints; sin `from_step` comportamiento por defecto.
- Legacy row sin `user_message` sigue 403.

### 6.3 Regresión

`cargo test --target-dir target-tests` (1027+ verdes), fmt, clippy, gates B. Tests existentes de turn-level retry (`execution_api_tests.rs:463-538`) siguen verdes (comportamiento por defecto compatible).

## 7. Riesgos

| Riesgo | Mitigación |
|---|---|
| El modelo ignore la resume note y repita pasos | La idempotencia evita re-ejecutar tools; `max_iterations` reducido; documentado como replay lógico. |
| Resume note filtre secretos de resultados de tools | `redact_secrets` + truncamiento; test con api_key. |
| `set_current_step` rompa adapters custom | Método aditivo; noop y SQLite implementados; otros adapters del repo solo esos 2. |
| Checkpoints addan ruido al event log | Solo se emiten en bridges con tool calls; payload pequeño; best-effort. |

## 8. Fuera de alcance

Replay byte-exacto del modelo (imposible sin reimplementar Rig), cambio a `AgentEngine` trait, nuevo endpoint, streaming en replay, mensajes tool-call nativos en el puerto, C1/C2/C3.

## 9. Plan de implementación (siguiente paso)

Tras revisión, invocar `writing-plans`:

1. Payload de checkpoints + emisión en bridges + `set_current_step` en puerto/adapters.
2. `retry_turn` checkpoint-aware + resume note + `from_step` + `max_iterations` reducido.
3. Tests unit/E2E (incluye redacción y filtrado por step).
4. Docs (AGENTS 308/470/473/476, README) + regresión.

Criterio de aceptación: el retry usa checkpoints para guiar al modelo, los efectos de tools se replayan (no re-ejecutan), `current_step` avanza, y la documentación describe honestamente el replay como lógico — sin prometer replay exacto.
