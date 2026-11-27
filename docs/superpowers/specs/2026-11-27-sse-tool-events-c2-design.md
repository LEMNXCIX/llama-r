# Diseño: eventos de herramientas en SSE y gRPC (Fase 10-C2)

- **Fecha:** 2026-11-27
- **Estado:** Diseño aprobado en brainstorming; pendiente de revisión del spec
- **Tipo:** Arquitectural — añade campos al DTO de streaming y al wire de gRPC
- **Perfil:** LAN / localhost, uso personal (hereda A/B/C1)
- **Parte:** 2 de 4 de la Fase 10-C (C1 embeddings ✅ → C2 SSE → C3 compactación → C4 step-replay)

## 1. Contexto y problema

`engine_events_to_chat_stream` (`src/api/chat_core.rs:467-554`) mapea `Token`, `Completed`, `Error` y **`ApprovalRequired`** a `ChatStreamEvent`. Los dos brazos `ToolCall`/`ToolResult` están explícitamente vacíos (`:537-539`, comentario "Nivel-A streaming"), aunque los bridges ya emiten esos eventos al canal (`src/adapters/rig_engine/tools.rs:134-164`, `local_tools.rs:415-455`) y llegan al receiver. Falta solo mapearlos.

Además, `src/api/grpc.rs:110-121` convierte `ChatStreamEvent` al proto `pb::ChatStreamEvent`, que solo tiene 4 campos (`proto/llamar.proto:29-34`) — **ni siquiera `approval` se envía hoy** por gRPC. El usuario pidió C2 con HTTP SSE **y gRPC**, así que esta spec corrige ese gap existente.

`ChatStreamEvent` es un struct (`src/domain/models.rs:192-202`) con `approval: Option<ApprovalInfo>` (`skip_serializing_if = "Option::is_none"`). Agregar campos opcionales con el mismo patrón mantiene el payload de clientes actuales byte-for-byte.

Objetivo: que un cliente SSE/gRPC pueda ver el progreso de herramientas (llamada y resultado) sin romper el stream de tokens, sin filtrar argumentos crudos ni secretos, y con el contrato gRPC alineado al HTTP.

## 2. Decisiones

| # | Decisión | Justificación |
|---|---|---|
| D1 | Añadir un campo `tool: Option<ToolStreamInfo>` a `ChatStreamEvent`, con `skip_serializing_if = "Option::is_none"`. | Un solo campo con `kind` discriminado: más simple que dos campos y suficiente para call/result. |
| D2 | `ToolStreamInfo { kind: "call"\|"result", server_id, tool_name, is_error: Option<bool>, args_hash: Option<String> }`. | Para `call`: kind + nombre cualificado (`server__tool`, vía `rig_tool_name`) + `args_hash` del JSON canónico. Para `result`: kind + nombre + `is_error`. **Nunca `arguments` ni `preview` crudos** — el preview hoy es `String::new()` y los arguments pueden traer secretos. |
| D3 | `args_hash` se calcula con `args_hash_hex(&arguments)` (el canon de `src/services/redaction.rs:86-98`), el mismo que usa aprobación. | No expone valores, solo un hash verificable. |
| D4 | Los eventos `ToolCall`/`ToolResult` mantienen `message.content = ""` y `done: false`. | Clientes que solo leen `message.content` no ven cambios; el progreso va en `tool`. |
| D5 | Token/Completed/approval/Error quedan byte-for-byte iguales. `approval_tests.rs:239-242` (token JSON sin clave `approval`) se extiende a "sin clave `tool`". | Compatibilidad hacia atrás. |
| D6 | gRPC: ampliar `proto/llamar.proto` `ChatStreamEvent` con `approval` y `tool` (mensajes opcionales) y mapearlos en `src/api/grpc.rs`. | El usuario eligió HTTP+gRPC; gRPC hoy pierde `approval`, se corrige en el mismo cambio. |
| D7 | `ToolCall`/`ToolResult` de **turnos hijos** (delegación) NO se reenvían al SSE root: el forwarder `local_tools.rs:548-562` solo reenvía `ApprovalRequired` y los tests lo pinnean (`subagent_tests.rs:1812-1833`). | Cambiar eso alteraría subagentes (comportamiento C4), no es mapear el stream. Se documenta como deferido. |
| D8 | Sin cambios en `capabilities.streaming`, proveedores no-Ollama ni endpoints nuevos. | El mapper es compartido por todos los targets con engine y ya está dentro del gate de resolved-model. |

RFC 2119: **MUST**, **SHALL**, **SHOULD**, **MAY**, **MUST NOT**.

## 3. Modelo de datos

### 3.1 `src/domain/models.rs` (junto a `ApprovalInfo`)

```rust
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ToolStreamInfo {
    /// "call" | "result"
    pub kind: String,
    /// Qualified tool name via `rig_tool_name` (`server__tool` or bare id).
    pub tool_name: String,
    /// Present on "result": whether the tool failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// Present on "call": SHA-256 of the canonical arguments — never raw args.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args_hash: Option<String>,
}
```

`ChatStreamEvent` gana:

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub tool: Option<ToolStreamInfo>,
```

**Nota de implementación:** todos los literales de `ChatStreamEvent` en `src/` y `tests/` (~20; p. ej. `ollama.rs:205`, `chat_core.rs:482/500/520/544`, `approval_tests.rs`, y los ~15 tests con `approval: None`) deben añadir `tool: None`. Use `..Default::default()` solo si el struct deriva `Default`; si no, edite cada literal. El cambio es mecánico y no altera aserciones existentes.

### 3.2 `proto/llamar.proto`

```proto
message ToolStreamInfo {
  string kind = 1;
  string tool_name = 2;
  optional bool is_error = 3;
  optional string args_hash = 4;
}

message ApprovalInfoMsg { ... }  // id, tool_name, args_hash, task_id, depth
```

`ChatStreamEvent` proto gana `optional ApprovalInfoMsg approval = 5; optional ToolStreamInfo tool = 6;` (los números 1-4 existentes se conservan). Regenerar con el mismo mecanismo `tonic-build`/`tonic-prost` que usa el repo (buscar `build.rs`), sin cambiar la versión de tonic.

## 4. Mapeo en `chat_core.rs`

Completar los brazos vacíos de `engine_events_to_chat_stream` (`:537-539`):

- `AgentRunEvent::ToolCall { server_id, tool_name, arguments }` ⇒ `tool: Some(ToolStreamInfo { kind: "call", tool_name: rig_tool_name(&server_id, &tool_name), is_error: None, args_hash: Some(args_hash_hex(&arguments)) })`, `message.content: ""`, `done: false`.
- `AgentRunEvent::ToolResult { server_id, tool_name, is_error, .. }` ⇒ `tool: Some(ToolStreamInfo { kind: "result", tool_name: rig_tool_name(...), is_error: Some(is_error), args_hash: None })`, `message.content: ""`, `done: false`. El `preview` se descarta (hoy siempre vacío y podría contener datos sensibles).

`rig_tool_name` está en `src/adapters/rig_engine/tools.rs:13` — verificar visibilidad; si es privado, exponer un helper equivalente en `chat_core` o re-exportar. No se toca el builder: los bridges ya emiten ambos eventos al canal.

## 5. gRPC (`src/api/grpc.rs:110-121`)

Rellenar los nuevos campos del proto desde `ChatStreamEvent`:
- `approval`: mapear `ApprovalInfo` → `ApprovalInfoMsg` (id, tool_name, args_hash, task_id, depth).
- `tool`: mapear `ToolStreamInfo` → mensaje proto.

Sin secretos en ningún campo. Los tests de `tests/grpc_wire_tests.rs` que construyen eventos proto deben actualizarse con los campos opcionales.

## 6. Seguridad

- `arguments` NUNCA se serializa en el stream; solo `args_hash` (hash canónico, como en aprobación).
- `preview` se descarta.
- `approval` ya cumplía esto; se mantiene.
- Ningún campo nuevo lleva ids de conversación, keys ni URLs.

## 7. Testing

### 7.1 Unit (`src/api/chat_core/tests.rs` o `tests/approval_tests.rs` patrón :195-262)

Alimentar `engine_events_to_chat_stream` con una secuencia `ToolCall → ToolResult → Token → Completed` sobre un `mpsc`:

1. El evento `ToolCall` serializado contiene `"tool":{"kind":"call","tool_name":"srv__t","args_hash":"<64 hex>"}`, `message.content == ""`, `done == false`, y **no contiene** `"arguments"` ni `"preview"`.
2. El evento `ToolResult` contiene `"kind":"result"`, `is_error`, y **no** `args_hash`.
3. El evento `Token` sigue sin claves `approval` ni `tool` (extensión de `approval_tests.rs:239-242`).
4. El evento `Completed` sigue con `done: true` y sin `tool`.
5. Un argumento con un campo `api_key` produce un `args_hash`, nunca el valor.

### 7.2 E2E SSE

Arrancar con Ollama local y un agente con una herramienta local que el modelo invoque; `POST /api/chat` con `stream: true` y `Accept: text/event-stream`; verificar que aparecen líneas `data:` con `"tool"` y que el texto final y `done:true` siguen llegando. Si el modelo no llama herramientas de forma determinista, usar un fake-Ollama que emita NDJSON con tool call (patrón `tests/agent_stream_tests.rs`).

### 7.3 gRPC wire

Actualizar/añadir test en `tests/grpc_wire_tests.rs`: un `ChatStreamEvent` con `tool` y `approval` viaja por gRPC y el cliente recibe ambos campos.

### 7.4 Regresión

`cargo test --target-dir target-tests` (998+ verdes), fmt, clippy, gates de Fase 10-B. Los tests de subagentes que pinnean "forwarder ignora otros eventos" siguen verdes (D7).

## 8. Riesgos

| Riesgo | Mitigación |
|---|---|
| Romper ~20 literales de `ChatStreamEvent` | Campo `Option` con `skip`; error de compilación fuerza actualizar todos; cambio mecánico. |
| Filtrar argumentos crudos | Solo `args_hash`; test que verifica ausencia de `"arguments"` y de un valor `api_key`. |
| Regenerar proto cambie el wire | Conservar números de campo 1-4; solo añadir 5-6 opcionales; tests de grpc_wire. |
| Clientes viejo confundidos por campo `tool` | `skip_serializing_if` ⇒ ausente en eventos sin herramienta; byte-for-byte igual. |

## 9. Fuera de alcance

C3/C4; reenviar `ToolCall`/`ToolResult` de hijos al SSE root (D7); cambiar `capabilities.streaming`; streaming en OpenRouter/OpenCode (sigue 501); tool events en historial de conversación.

## 10. Plan de implementación (siguiente paso)

Tras revisión, invocar `writing-plans`:

1. `ToolStreamInfo` + campo `tool` en `models.rs`; actualizar literales.
2. Mapear `ToolCall`/`ToolResult` en `engine_events_to_chat_stream` + tests unit.
3. Proto + `grpc.rs` + test wire.
4. Docs (AGENTS/README) + E2E SSE + regresión.

Criterio de aceptación: SSE y gRPC exponen `tool` para llamadas/resultados sin argumentos crudos, y los eventos Token/Completed/approval siguen idénticos para clientes existentes.
