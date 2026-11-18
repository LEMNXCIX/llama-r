# Fase 3 — Connections, Permission engine y Human-in-the-loop

**Fecha:** 2026-11-18
**Estado:** propuesto — aprobado en brainstorming, pendiente de revisión del spec
**Alcance:** los tres sub-sistemas de la Fase 3 del roadmap. HITL usa aprobación síncrona
(long-poll dentro del mismo request, enfoque B) con `approval_id` + args inmutables como
semilla para la ejecución durable de la Fase 7.

## Contexto

Tras las Fases 1 y 2, llama-r tiene seis providers, capacidades por modelo, un registry de
tools locales junto a MCP, y skills bajo demanda. Tres hechos del código actual condicionan
esta fase:

1. **Connections ya existen a medias.** `McpServerConfig` (`src/adapters/mcp/registry.rs:15`)
   ya tiene `transport`, `url`, `auth_env` (nombre de env var, nunca el secreto),
   `timeout_secs`, `tool_namespace` y `enabled`. Hay clientes HTTP, streaming, stdio y
   `NamespacedMcpClient`. Falta: headers custom, auth basic, y un generador OpenAPI→tools.
2. **No hay pausa/reanudación.** `McpCallRequest` tiene `CancellationToken` (patrón `watch`),
   pero no existe `ExecutionState`, checkpoints ni `WaitingApproval`. `AgentRuntime::chat`
   es síncrono: `prepare → engine.run → persistir`.
3. **Permisos hoy = headers + scope.** `X-Project`/`X-Agent` seleccionan el agente; los
   permisos viven en `AgentScope`, derivado del TOML. No hay permission engine explícito
   ni identidad de solicitante.

`ToolDyn::call` de rig-core 0.40 devuelve `WasmBoxedFuture` y puede hacer `.await` dentro del
bridge — eso hace viable la aprobación síncrona sin cambiar los puertos.

## Goal

Separar conexiones, permisos y aprobación en capacidades explícitas del runtime: que un
agente conecte servicios externos con credenciales que el modelo nunca ve, que sus permisos
sean una política declarativa derivada del TOML (no una convención de headers), y que una
herramienta sensible espere aprobación humana ligada a sus argumentos concretos.

## Non-goals

- Persistencia de aprobaciones / reanudación tras restart (Fase 7, ejecución durable).
- Política `Auto` con decision model real (Fase 6) — hoy se comporta como `Always`.
- Identidad de usuario real en el endpoint de aprobación (Fase 7 con auth).
- Sandbox, subagentes, HITL en la TUI (el endpoint es HTTP; la TUI puede consumirlo después).
- Cambiar el contrato de `/chat` (no `202` + polling): la aprobación ocurre en el mismo request.
- Cambiar los puertos `McpClient`, `McpServerRegistry`, `LLMProvider`, `AgentEngine`, `RagStore`.

## Decisiones de diseño

1. **Enfoque B — long-poll en el mismo request.** El bridge de la herramienta se bloquea en
   un `oneshot` hasta que `POST /api/approvals/:id` resuelve. El request de chat/SSE sigue
   abierto; no se cambia su contrato. Un timeout expira el pending con error nombrado.
2. **Semilla para durable:** cada aprobación lleva `approval_id: Uuid` y un `args_hash`
   (SHA-256 de los argumentos canónicos). `decide(id, approve, args_hash)` rechaza si el hash
   no coincide — la aprobación está ligada a herramienta + argumentos, no solo al id.
3. **Connections extienden, no reemplazan:** se añaden campos a `McpServerConfig` con
   `#[serde(default)]`; los perfiles existentes siguen cargando. Las credenciales siguen
   siendo nombres de env var (`auth_env`), nunca valores en TOML.
4. **Permission policy like código:** `PermissionPolicy` se compila desde el TOML del agente
   en `AgentScope` (nuevos campos con default). El permission engine es una función pura y
   testeable; los headers siguen seleccionando *qué agente*, no *qué permisos*.
5. **Sin sobreescritura de credenciales:** los headers que el modelo/request pudiera aportar
   nunca pueden reemplazar el `Authorization` ni los headers de autenticación de una
   conexión — se aplican después y son inmutables.

## Arquitectura

### Connections

- `src/adapters/mcp/registry.rs`: extender `McpServerConfig` con
  `headers: Vec<(String, String)>` (valor `$VAR` → `env::var`), `auth: AuthKind { Bearer, Basic, None }`
  (default Bearer si `auth_env` existe), `auth_user_env: Option<String>` (para Basic). El
  `build_client` inyecta auth + headers en el cliente HTTP/streaming. Campos `#[serde(default)]`.
- `src/adapters/mcp/openapi.rs` (nuevo): `OpenApiConnection { spec_path, base_url, auth }`
  implementa `McpClient`: al `list_tools`, parsea el spec (rutas + operaciones) y produce
  `McpToolDef`s; al `call_tool`, construye la HTTP request con los args como query/body según
  el spec. Spec inválido → `AppError::Config` con la ruta, warning en boot, conexión omitida.
- `mcp-servers/*.toml` sigue siendo el origen; se documenta el nuevo esquema.

### Permission engine

- `src/domain/policy.rs` (nuevo):
  - `ApprovalPolicy { Never, Once, Always, Auto }` — `Auto` hoy ≡ `Always` (documentado).
  - `ApprovalRule { tool_pattern: String, policy: ApprovalPolicy }` (glob simple sobre el
    nombre calificado `server__tool` o id de tool local).
  - `PermissionPolicy { connections_allow: Vec<String>, approvals: Vec<ApprovalRule> }`.
  - `fn approval_for(&self, tool_name: &str) -> ApprovalPolicy` — primera regla que hace match;
    default `Never`.
- `src/domain/agent.rs` + `src/domain/scope.rs` + `src/services/scope_builder.rs`: `AgentConfig`
  gana `connections: Vec<String>` (o reutiliza `mcp_sources` — ver Ruling) y `[policy]` con
  las reglas de aprobación. `AgentScope` gana `permission: PermissionPolicy`, compilada por
  `ScopeBuilder`. `#[serde(default)]` en todo; manifests v1 cargan igual.
- Un `PermissionEngine::authorize(scope, tool_name) -> Decision { Allow, Approve }` —
  función pura, testeable sin runtime.

### HITL

- `src/services/approval.rs` (nuevo):
  - `ApprovalGate { pending: RwLock<HashMap<Uuid, PendingApproval>>, timeout: Duration }`.
  - `PendingApproval { tool_name: String, args_hash: [u8;32], created_at, tx: oneshot::Sender<ApprovalDecision> }`.
  - `ApprovalDecision { Approved, Denied { reason } }`.
  - `request(tool_name, args) -> (Uuid, Receiver<ApprovalDecision>)`; registra el pending.
  - `decide(id, approve, args_hash) -> Result<(), ApprovalError>`: verifica existencia,
    expiración y hash; envía la decisión; elimina el pending. `args` canónicos = JSON
    serializado con claves ordenadas (`serde_json::Map` ya es BTreeMap por defecto).
  - Reaper: `prune_expired()` llamado en `request` y en cada `decide`; sin task de fondo.
- `src/api/approval_api.rs` (nuevo): `GET /api/approvals` (lista pendientes: id, tool, args,
  created_at — sin secretos) y `POST /api/approvals/:id` con body `{approve: bool, args_hash?:
  String}`; valida el hash si se envía. Erige AppError apropiado (404/409/400). Registradas
  en el router en `src/api/mod.rs`/`handlers.rs`.
- `src/ports/engine.rs`: `AgentRunEvent` gana `ApprovalRequired { id: Uuid, tool_name: String,
  args: Value }`. Es un cambio de enum, no de firma del trait — compatible.
- `src/adapters/rig_engine/mod.rs`/`tools.rs`: `RigAgentEngine` lleva `Arc<ApprovalGate>`;
  los bridges (`McpToolBridge`, `LocalToolBridge`) reciben el gate. En `invoke_json`, antes de
  despachar: `engine.authorize(scope, tool)` → si `Approve`, `gate.request(...)`, emitir
  evento, `rx.await`, y solo entonces cargar budget + ejecutar. El budget **no** se carga
  mientras espera. Renegar/expirar devuelve `ToolError` nombrado.
- `src/runtime.rs`: construir el `ApprovalGate` y pasarlo a `build_companions` y a ambos
  bridges; el mismo gate sobrevive hot-swap (no está en `ProviderStack`, está en `AppState`
  o se crea una vez y se clona).

## Flujo de datos

1. Boot: `mcp-servers/*.toml` + TOML de agente → `McpServerConfig`s y `PermissionPolicy` en
   `AgentScope`.
2. Request con herramienta sensible: el bridge calcula `args_hash`, crea el pending en
   `ApprovalGate`, emite `AgentRunEvent::ApprovalRequired { id, tool_name, args }` por el canal
   de eventos del engine, y hace `.await` en el `oneshot`.
3. El adaptador SSE (`chat_core::engine_events_to_chat_stream`) mapea el nuevo evento a un
   evento de stream (campo `approval` en `ChatStreamEvent`, con `#[serde(skip_serializing_if)]`
   para no romper clientes existentes). El request sigue abierto (long-poll).
4. El humano/cliente llama `GET /api/approvals` para ver el pending y `POST
   /api/approvals/:id` con `{approve: true, args_hash: "<hex>"}`.
5. `decide` valida id + hash + expiración, resuelve el `oneshot`; el bridge continúa: si
   aprobado, carga budget y ejecuta la tool; si denegado/expirado/hash mismatch, devuelve un
   error nombrado como tool result y el modelo decide cómo responder.

## Manejo de errores

- `args_hash` mismatch → `ApprovalError::ArgsChanged` → rechazo, nunca ejecuta.
- Pending inexistente → 404; ya decidido → 409; expirado → 410 (o error nombrado en el bridge).
- Timeout esperando aprobación (configurable, default 120 s) → error "approval timed out" como
  tool result; el turno continúa.
- Spec OpenAPI inválido → config error nombrando la ruta, warning en boot, conexión omitida.
- Ningún secreto en `GET /api/approvals`, logs, health ni eventos de stream.

## Testing

- `tests/connection_wire_tests.rs`: path + Bearer/basic + headers custom; OpenAPI genera las
  tools correctas; spec inválido no detiene boot.
- `tests/permission_tests.rs`: `PermissionEngine::authorize` como función pura sobre el TOML;
  agente sin `connections_allow` no ve tools de esa conexión.
- `tests/approval_tests.rs`: `ApprovalGate` real — el bridge bloquea, `decide(approve)` desbloquea
  y ejecuta; hash distinto rechaza; timeout expira; budget no cargado mientras espera; el
  evento `ApprovalRequired` se emite con id/tool/args.
- Integración: un turno de agente SSE contra un fake Ollama que pide una tool sensible, el
  test consume el evento de aprobación, llama al endpoint y verifica que el tool-loop continúa.

## Riesgos

- **Long-poll y timeouts del cliente HTTP:** si el cliente cierra la conexión antes de la
  aprobación, el `oneshot` sigue vivo en el gate (el bridge todavía espera) hasta el timeout.
  Mitigación: timeout + reaper; documentar que el approval_id se puede resolver aunque el
  request original haya muerto, pero la respuesta se perderá (semilla para Fase 7).
- **Arg hash canónico:** si los args incluyen números float o mapas, la serialización debe ser
  determinista. `serde_json` con `preserve_order` desactivado (default) ordena las claves —
  verificar en el test.
- **Rig event mapping:** `AgentRunEvent` es consumido por `run_stream` y el adaptador SSE;
  añadir una variante requiere actualizar todos los `match` exhaustivos — el mismo riesgo
  cross-cutting de Fase 1/2; queuear en el plan.
- **Concurrencia:** `ApprovalGate` usa `RwLock<HashMap>` + `oneshot`; `decide` debe ser
  atómico (doble verificación bajo write lock) para que dos respuestas no resuelvan el mismo
  pending.

## Exit criteria

- Un `mcp-servers/*.toml` con headers/basic/OpenAPI conecta y expone tools sin secretos en TOML.
- Un agente con política `Always` bloquea su tool sensible hasta `POST /api/approvals/:id`;
  aprobar ejecuta, denegar/arg-mismatch/timeout no ejecuta y da error nombrado.
- El budget no se carga durante la espera; el evento de aprobación llega por SSE.
- Puertos sin cambios; manifests v1 cargan; tests nuevos + gates verdes; docs alineadas.
