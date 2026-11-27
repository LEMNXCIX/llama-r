# Eventos de herramientas en SSE y gRPC (Fase 10-C2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Exponer `ToolCall`/`ToolResult` en el stream SSE y gRPC como campo `tool` aditivo, sin argumentos crudos ni secretos, manteniendo byte-for-byte el payload de Token/Completed/approval.

**Architecture:** `ChatStreamEvent` gana `tool: Option<ToolStreamInfo>` con `skip_serializing_if`. `engine_events_to_chat_stream` completa los dos arms vacíos. El proto amplía su mensaje con `approval` (gap existente) y `tool`, y `grpc.rs` mapea ambos.

**Tech Stack:** Rust 1.97, axum SSE, tonic 0.14 + tonic-prost-build (build.rs), serde skip-if-none, utoipa.

**Spec:** `docs/superpowers/specs/2026-11-27-sse-tool-events-c2-design.md`

## Global Constraints

- `ToolStreamInfo { kind: "call"|"result", tool_name, is_error: Option<bool>, args_hash: Option<String> }`; campo `tool: Option<ToolStreamInfo>` en `ChatStreamEvent` con `#[serde(skip_serializing_if = "Option::is_none")]`.
- `tool_name` cualificado: si `server_id` vacío ⇒ `tool_name` tal cual (local); si no ⇒ `rig_tool_name(&server_id, &tool_name)` (`server__tool`).
- `args_hash` = `args_hash_hex(&arguments)` (canon SHA-256); **NUNCA** serializar `arguments` ni `preview`.
- Eventos tool: `message.content = ""`, `done: false`. Token/Completed/approval/Error byte-for-byte iguales.
- Proto conserva campos 1-4 y añade `optional approval = 5; optional tool = 6;`; `build.rs` ya corre `tonic_prost_build::compile_protos`.
- `cargo test --target-dir target-tests` MUST pasar (998 tests); fmt/clippy limpios; gates B verdes.

## Review Focus

- Argumentos con `api_key` => solo `args_hash`, nunca el valor; ningún `"arguments"`/`"preview"` en JSON. Test en Task 2.
- Evento Token sigue sin claves `approval` ni `tool` (extensión de `approval_tests.rs:239-242`). Test en Task 2.
- Local tool (server_id vacío) => `tool_name` bare, no `"__id"`. Test en Task 2.
- gRPC recibe `tool` y `approval` (hoy pierde approval). Test en Task 3.
- Forwarder de hijos no reenvía tool events (D7) — tests de subagentes siguen verdes; NO cambiar `local_tools.rs:548-562`.

---

### Task 1: DTO `ToolStreamInfo` + campo `tool` y literales

**Files:**
- Modify: `src/domain/models.rs` (junto a `ApprovalInfo` ~170-190 y `ChatStreamEvent` ~192-202)
- Modify: todos los literales `ChatStreamEvent { ... }` en `src/` y `tests/` (~20)
- Test: compilación + un test de serialización en `src/domain/models.rs` o `tests/sse_tool_tests.rs`

**Interfaces:**
- Consumes: `ApprovalInfo` existente como patrón de `skip_serializing_if`.
- Produces: `pub struct ToolStreamInfo` y `ChatStreamEvent.tool: Option<ToolStreamInfo>`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn chat_stream_event_without_tool_serializes_no_tool_key() {
    let ev = ChatStreamEvent {
        model: "m".into(), created_at: "t".into(),
        message: ChatMessage { role: "assistant".into(), content: "hi".into() },
        done: false, approval: None, tool: None,
    };
    let json = serde_json::to_string(&ev).unwrap();
    assert!(!json.contains("\"tool\""), "{json}");
    assert!(!json.contains("\"approval\""), "{json}");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --target-dir target-tests --test sse_tool_tests` (o el módulo donde lo ponga)
Expected: FAIL — campo `tool` no existe.

- [ ] **Step 3: Implement**

En `models.rs`, definir `ToolStreamInfo` con los 4 campos del Global Constraints y `#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]`, opcionales con `skip_serializing_if`. Añadir `pub tool: Option<ToolStreamInfo>` a `ChatStreamEvent` con el mismo atributo. Actualizar **todos** los literales de `ChatStreamEvent` en `src/` y `tests/` añadiendo `tool: None` (grep `ChatStreamEvent {`). El cambio es mecánico; no altere aserciones existentes.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --target-dir target-tests --test sse_tool_tests && cargo test --target-dir target-tests`
Expected: PASS; suite verde (todos los literales compilan).

- [ ] **Step 5: Commit**

```bash
git add src/domain/models.rs tests/sse_tool_tests.rs
git commit -m "feat(sse): add additive tool field to ChatStreamEvent"
```

---

### Task 2: Mapear ToolCall/ToolResult en `engine_events_to_chat_stream`

**Files:**
- Modify: `src/api/chat_core.rs:537-539` (y posiblemente un helper de nombre cerca)
- Test: `tests/sse_tool_tests.rs` (ampliar) o `src/api/chat_core/tests.rs`

**Interfaces:**
- Consumes: `AgentRunEvent::{ToolCall,ToolResult}` de `src/ports/engine.rs:110-120`; `rig_tool_name` (`pub`, `src/adapters/rig_engine/tools.rs:13`); `args_hash_hex` (`src/services/redaction.rs:86-98`).
- Produces: eventos SSE con `tool` poblado.

- [ ] **Step 1: Write the failing tests**

Alimente `engine_events_to_chat_stream(model, rx)` con un `mpsc` y la secuencia:

```rust
tx.send(AgentRunEvent::ToolCall {
    server_id: "srv".into(), tool_name: "search".into(),
    arguments: serde_json::json!({"q":"x","api_key":"SECRET_VALUE"}),
}).await.unwrap();
tx.send(AgentRunEvent::ToolResult {
    server_id: "srv".into(), tool_name: "search".into(),
    is_error: false, preview: "hola".into(),
}).await.unwrap();
tx.send(AgentRunEvent::ToolCall {
    server_id: String::new(), tool_name: "local_echo".into(),
    arguments: serde_json::json!({"x":1}),
}).await.unwrap();
tx.send(AgentRunEvent::Token { text: "final".into() }).await.unwrap();
drop(tx);
// Recoja los eventos del stream
```

Asserts:
1. Evento 0 (call): `json.contains("\"kind\":\"call\"")`, `"tool_name":"srv__search"`, un `"args_hash":"<64hex>"`, **NO** `SECRET_VALUE`, NO `"arguments"`, `message.content == ""`, `done == false`.
2. Evento 1 (result): `"kind":"result"`, `"is_error":false`, NO `args_hash`, NO `"preview"`, `message.content == ""`.
3. Evento 2 (local call): `"tool_name":"local_echo"` (bare, NO `"__local_echo"`).
4. Evento 3 (Token): NO `"tool"` y NO `"approval"`, `content == "final"` ( backward-compat).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --target-dir target-tests --test sse_tool_tests`
Expected: FAIL — eventos tool llegan con `tool: None`.

- [ ] **Step 3: Implement**

En `chat_core.rs:537-539`, reemplazar el arm combinado por dos arms:

```rust
AgentRunEvent::ToolCall { server_id, tool_name, arguments } => {
    let qualified = if server_id.is_empty() { tool_name.clone() }
                    else { rig_tool_name(&server_id, &tool_name) };
    yield Ok(ChatStreamEvent {
        model: model.clone(), created_at: Utc::now().to_rfc3339(),
        message: ChatMessage { role: "assistant".into(), content: String::new() },
        done: false, approval: None,
        tool: Some(ToolStreamInfo {
            kind: "call".into(), tool_name: qualified,
            is_error: None, args_hash: Some(args_hash_hex(&arguments)),
        }),
    });
}
AgentRunEvent::ToolResult { server_id, tool_name, is_error, .. } => { /* análogo, kind "result", is_error Some, args_hash None */ }
```

Importe `rig_tool_name` y `args_hash_hex` con la ruta correcta (o re-export si son privados — `rig_tool_name` ya es `pub`). Nunca use `arguments` ni `preview` en el contenido del evento.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --target-dir target-tests --test sse_tool_tests && cargo test --target-dir target-tests`
Expected: PASS; suite verde.

- [ ] **Step 5: Commit**

```bash
git add src/api/chat_core.rs tests/sse_tool_tests.rs
git commit -m "feat(sse): map tool call and result events into the stream"
```

---

### Task 3: Proto y mapeo gRPC

**Files:**
- Modify: `proto/llamar.proto:29-34` (mensajes `ToolStreamInfo`, `ApprovalInfoMsg`, campos 5-6 en `ChatStreamEvent`)
- Modify: `src/api/grpc.rs:110-121`
- Test: `tests/grpc_wire_tests.rs` (ampliar o nuevo)

**Interfaces:**
- Consumes: `ChatStreamEvent` con `approval`/`tool` (Tasks 1-2).
- Produces: `pb::ChatStreamEvent` con ambos campos.

- [ ] **Step 1: Write the failing test**

En `tests/grpc_wire_tests.rs`, construya un `ChatStreamEvent` con `tool: Some(ToolStreamInfo { kind: "call", tool_name: "srv__t", is_error: None, args_hash: Some("abc".into()) })` y `approval: Some(ApprovalInfo{...})`, mapéelo con la misma función que usa `grpc.rs` (extraiga el mapping a una `pub fn` si está inline, o testee el stream extremo a extremo), y asserts de que el proto resultante tiene `tool.kind == "call"`, `tool.args_hash == "abc"` y `approval.id` presente.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --target-dir target-tests --test grpc_wire_tests`
Expected: FAIL — proto no tiene los campos.

- [ ] **Step 3: Implement**

En `proto/llamar.proto`, conservar campos 1-4 y añadir:

```proto
message ToolStreamInfo {
  string kind = 1;
  string tool_name = 2;
  optional bool is_error = 3;
  optional string args_hash = 4;
}
message ApprovalStreamInfo {
  string id = 1;
  string tool_name = 2;
  string args_hash = 3;
  optional string task_id = 4;
  optional uint32 depth = 5;
}
message ChatStreamEvent {
  string model = 1;
  string created_at = 2;
  ChatMessage message = 3;
  bool done = 4;
  optional ApprovalStreamInfo approval = 5;
  optional ToolStreamInfo tool = 6;
}
```

`build.rs` ya compila el proto con `protoc_bin_vendored` — no añadir dependencias. En `src/api/grpc.rs:110-121`, mapear ambos campos:
`approval: event.approval.map(|a| pb::ApprovalStreamInfo { id: a.id, tool_name: a.tool_name, args_hash: a.args_hash, task_id: a.task_id, depth: a.depth })`, `tool: event.tool.map(|t| pb::ToolStreamInfo { kind: t.kind, tool_name: t.tool_name, is_error: t.is_error, args_hash: t.args_hash })`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --target-dir target-tests --test grpc_wire_tests && cargo test --target-dir target-tests`
Expected: PASS; suite verde (build.rs regenera el proto automáticamente).

- [ ] **Step 5: Commit**

```bash
git add proto/llamar.proto src/api/grpc.rs tests/grpc_wire_tests.rs
git commit -m "feat(grpc): carry approval and tool fields in stream events"
```

---

### Task 4: E2E SSE, docs y regresión final

**Files:**
- Modify: `AGENTS.md` (líneas 20, 431, 461, 512), `README.md` (109, 433)
- Test: script temporal `/tmp/opencode/llama-r-c2-e2e.sh`

**Interfaces:** Tasks 1-3 completos.

- [ ] **Step 1: Update docs**

AGENTS.md: quitar "tool call/result events are not mapped to SSE" y describir el campo `tool` con `kind`/`tool_name`/`args_hash`/`is_error`, argumentos crudos nunca en el stream; actualizar la nota de que gRPC ahora también lleva `approval` y `tool`. README.md: equivalente en español. No tocar capability table ni puertos.

- [ ] **Step 2: E2E SSE**

Use un fake-Ollama que emita NDJSON con una tool call (patrón `tests/agent_stream_tests.rs`) o un agente con `llama_r__skill`. Arrancar llama-r con `PROVIDER=ollama`, puerto libre, `LLAMA_R_DIR` aislado; `POST /api/chat` con `{"stream":true,"model":"<agente>"}` y un prompt que dispare la herramienta. Verificar en el cuerpo SSE: una línea `data:` con `"kind":"call"`, otra con `"kind":"result"`, tokens y final `done:true`; ninguna con `SECRET`/argumentos crudos. Mate el servidor.

- [ ] **Step 3: Full regression**

Run: `cargo test --target-dir target-tests && cargo fmt --check && cargo clippy --all-targets -- -D warnings && bash tests/ci_config_tests.sh`
Expected: todo verde; 998+ tests; sin servidores vivos.

- [ ] **Step 4: Commit**

```bash
git add AGENTS.md README.md
git commit -m "docs: document tool events in SSE and gRPC streams"
```
