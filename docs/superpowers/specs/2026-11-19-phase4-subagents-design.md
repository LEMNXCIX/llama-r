# Fase 4 — Subagents con aislamiento

**Fecha:** 2026-11-19
**Estado:** aprobado en brainstorming (diseño de 3 secciones), pendiente de revisión del spec
**Alcance:** sustituir el stub `[subagents]` por delegación real de un coordinador a
especialistas del mismo proyecto, con herencia cero y aprobaciones reenviadas al padre.

## Contexto

`AgentConfig.subagents` es hoy un `Option<toml::Table>` inerte (`src/domain/agent.rs:77-84`):
manifiestos antiguos declaran `allowed` / `allow_global` / `max_depth` y ningún código los lee.
No hay ningún test ni camino de delegación (`grep` sobre `src/` y `tests/` confirma que el stub
solo aparece en la definición, los defaults y dos tests de serialización).

El roadmap (`Fase 4`) pide: directorio por subagente con instrucciones/tools/connections/skills
propias, **herencia cero**, delegación como tool `{message, taskId?}`, anidamiento, aprobaciones
reenviadas al canal padre, y sustituir el stub. El mapeo de seams mostró cuatro hechos que
condicionan el diseño:

1. El bridge de tools locales (`src/adapters/rig_engine/local_tools.rs:233-380`) tiene
   `LocalToolDeps`, pero **no** `AppState`, ni selección de agente, ni el pipeline de chat.
2. Los puertos (`LLMProvider`, `AgentEngine`, `DecisionRouter`) son sagrados: no cambian de firma.
   `AgentRunRequest` (`src/ports/engine.rs:12-34`) no lleva contexto de padre.
3. El scope del subagente nace `DenyAll` por defecto (`src/domain/scope.rs:43-52`,
   `src/services/scope_builder.rs:44-76`): **la herencia cero ya es representable**, sin código nuevo
   de "no heredar".
4. No existe canal padre: `PendingApproval` (`src/services/approval.rs:63-84`) solo lleva
   `execution_id` y un oneshot local, y la única salida es el `event_tx` del run.

Decisiones confirmadas en brainstorming:

- **Caso de uso:** coordinador → especialistas del **mismo proyecto**, reutilizando el runtime actual.
- **Un solo nivel:** el especialista no delega. `max_depth` se acepta pero se ignora.
- **Allowlist de agentes ya registrados:** el coordinador declara ids; los especialistas viven en
  `contextos/projects/<p>/agents/` con su propio TOML y Scope. No se duplica el schema de `AgentConfig`.
- **Enfoque A + aprobaciones mapeadas:** tool local con un `Delegator` inyectado; reentrada a
  `execute_chat_with_id`; `ApprovalRequired` del hijo reenviado al SSE del padre con `task_id`.

Se descartaron dos alternativas: delegar directamente a `AgentRuntime::chat` (B: se salta
`finish_execution`, trazas y fallback, duplicando lógica) y un evento `AgentRunEvent::DelegateRequested`
(C: cambia un puerto y exige sintetizar el `ToolResult`).

## Goal

Que un agente coordinador delegue tareas a especialistas del mismo proyecto **solo con TOML**, que
el especialista arranque con herencia cero demostrable, y que el resultado, sus aprobaciones y sus
trazas vuelvan al padre reutilizando el pipeline y los puertos existentes.

## Non-goals

- Anidamiento (sub-subagentes), detección de ciclos y `max_depth` funcional.
- Agentes remotos o sandbox (Fase 8).
- *Steering* de mensajes al hijo en vuelo (Fase 7 lo deja diferido).
- Cambios en la TUI, en `AgentEngine`/`LLMProvider`/`DecisionRouter` ni en agentes que no declaren
  `[subagents]` (v1 intacto).
- Identidad de usuario en aprobaciones (ya diferida en Fase 3/7).

## Decisiones de diseño

1. **Manifest tipado y compatible.** `AgentConfig.subagents` pasa a
   `Option<SubagentsConfig { allowed: Vec<String>, allow_global: bool }>` (`#[serde(default)]`).
   Los manifests v1 con `[subagents]` siguen cargando; `max_depth` se acepta y se ignora. A diferencia
   del stub, el campo presente **sí se serializa**, para que `PUT /api/agents/:id` preserve la
   allowlist (igual que `routing` y `description`). Se actualiza
   `the_subagents_stub_never_serialises` para reflejarlo.
2. **Puerto nuevo y aditivo.** `src/ports/delegation.rs` define
   `trait Delegator: Send + Sync { async fn delegate(&self, req: DelegateRequest) -> Result<DelegateResult, String> }`.
   Ningún puerto existente cambia de firma. Igual que `AutoDecider` en Fase 6.
3. **Handle inyectado.** `RuntimeDelegator { registry: Arc<AgentRegistry>, app: Weak<AppState> }`
   (`src/services/delegation.rs`). El `Weak` rompe el ciclo `AppState → engine → delegator → AppState`.
   Se inyecta en `LocalToolDeps` desde `CompanionHandles` con el patrón ya usado por `auto_decider` y
   `observability`: `mod.rs` → `builder.rs` → ambos bridges → spawned stream → boot y TUI Apply.
4. **Reentrada al pipeline.** `delegate_turn` en `src/api/chat_core.rs` reconstruye un
   `AgentSelection` con el `project_id` del padre y el id resuelto, y llama `execute_chat_with_id`
   con `stream: false`, `execution_id` hijo nuevo y `conversation_id = task_id` (o UUID nuevo).
   El hijo corre con **su** manifiesto, **su** scope y **su** budget; el resultado vuelve como
   `ToolResult` al bucle de Rig del padre.
5. **Herencia cero por construcción.** El hijo se resuelve únicamente por su id, validado contra
   `allowed` y el mismo proyecto (o `agents/` si `allow_global`). El padre no le pasa project context,
   skills, RAG sources ni tools: el `ScopeBuilder` del hijo ya produce `DenyAll`. Un id no permitido
   es un `ToolError` nombrado, nunca una delegación.
6. **Aprobaciones sin tocar el gate.** El hijo usa el mismo `ApprovalGate` global. En streaming, un
   `EventForwarder` del padre reenvía el `ApprovalRequired` del engine hijo al `ChatStreamEvent` del
   padre con un campo `task_id` opcional y aditivo en el payload de `approval`. En no-streaming no hay
   sender: el cliente descubre el pendiente con `GET /api/approvals` y decide con `POST`, sin cambios.
   `approval_id` sigue siendo el UUID global; no se prefija.
7. **Budget, idempotencia y deadline.** La delegación gasta **1 unidad del budget del padre** (como
   cualquier tool, cargada solo tras `authorize → Ok`) más las que gaste el hijo con el suyo. El
   `operation_id` de Fase 7 cachea el resultado del tool call, así que reintentar el turno del padre no
   re-ejecuta al hijo. `DelegateRequest` lleva el `deadline` del padre y el delegator usa
   `timeout_at(deadline - 5s)` para que el error del hijo llegue antes de que el bucle del padre expire.
8. **Un solo nivel, sin recursión.** El scope del hijo no incluye `llama_r__delegate`, así que el
   tool ni siquiera se le ofrece. No hay `max_depth` ni contador de profundidad.

## Arquitectura

### Manifest y resolución

- `src/domain/agent.rs`: `SubagentsConfig { allowed, allow_global }`, campo tipado, defaults y tests.
- `src/services/agent_registry.rs:178-232`: reutilizar `resolve_agent` / `get_project_agent` / `get_agent`;
  la validación de allowlist vive en el delegator, no en el registry.
- `src/api/route_resolution.rs:19-25`: construir `AgentSelection` nueva sin reutilizar la del padre
  (los headers ya se consumieron en `handlers.rs:180-189`).

### Componentes

- `src/ports/delegation.rs` (nuevo): `DelegateRequest { agent_id, message, task_id: Option<String>,
  deadline: Instant, event_tx: Option<EventForwarder> }`, `DelegateResult { answer, task_id }`, trait.
- `src/services/delegation.rs` (nuevo): validación, resolución, `delegate_turn`, timeout.
- `src/adapters/rig_engine/local_tools.rs`: id `llama_r__delegate`, `LocalToolKind::Delegate`,
  esquema JSON `{message, task_id?}`, dispatch al delegator; re-check de scope, aprobación, budget,
  idempotencia y eventos `ToolCall`/`ToolResult` igual que las demás tools.
- `src/runtime.rs`: `CompanionHandles.delegator`, construido tras el `AppState` con `Arc::downgrade`.

### Threading

Igual que Fase 6: `delegator` viaja en `LocalToolDeps` y se inyecta en el engine; boot y Apply
comparten `build_companions`, de modo que un hot swap de provider no pierde el delegator.

## Flujo de datos

1. El modelo del coordinador emite `llama_r__delegate({message, task_id?})`.
2. El bridge re-checka `allows_local_tool`, resuelve aprobación del padre, carga 1 unidad del budget,
   calcula `operation_id` y llama `delegator.delegate(...)`.
3. El delegator valida el id (allowlist + mismo proyecto), resuelve el agente y llama `delegate_turn`.
4. `delegate_turn` crea `execution_id` hijo, usa `task_id` como `conversation_id` y ejecuta
   `execute_chat_with_id(stream: false)`. El hijo emite su propio `TraceRecorded`.
5. Los `ApprovalRequired` del hijo se reenvían al SSE del padre con `task_id`; en no-streaming el
   cliente usa el polling global existente.
6. La respuesta del hijo vuelve como `ToolResult`; el bucle de Rig del padre continúa y el `task_id`
   efectivo se devuelve en el resultado para reanudar.

## Manejo de errores

- Id no permitido, de otro proyecto o global sin `allow_global` → `ToolError` nombrado, cero ejecución.
- Hijo ausente, provider caído o timeout → `ToolError` con el motivo; el turno del padre **continúa**
  (el modelo decide cómo responder). Nunca se propaga como error del chat padre.
- Fallo al persistir la traza o el pending del hijo → warning best-effort, mismo discipline que Fase 7.
- `Weak::upgrade` fallido (AppState ya liberado) → `ToolError` nombrado, no panic.

## Testing

`tests/subagent_tests.rs` (nuevo, sin red ni modelo real):

- id permitido del mismo proyecto → delegación ejecuta, `ToolResult` vuelve, se emite `ToolCall`.
- Fuera de allowlist, otro proyecto, global sin `allow_global` → `ToolError`, sin ejecutar nada.
- **Herencia cero**: scope del hijo `DenyAll` en tools locales y MCP, sin `rag_sources` ni skills del
  padre, y su prompt no contiene las secciones del padre.
- `task_id` → `conversation_id` del hijo; sin él se genera y se devuelve; `task_id` existente reanuda.
- Aprobación del hijo: en streaming llega al SSE del padre con `task_id`; en no-streaming aparece en
  `GET /api/approvals` y se decide con `POST`.
- Budget (1 del padre + el suyo), fallo del hijo no tumba al padre, reintento cacheado por
  `operation_id` sin re-ejecutar, y el hijo **no** recibe `llama_r__delegate` ofrecido.
- Manifiestos v1 con `[subagents]` cargan; `max_depth` ignorado; `PUT /api/agents` preserva la allowlist.

Gates: fmt / clippy `-D warnings` / audit / suite completa (813+) / build `--no-default-features`.
Verificación explícita de que ninguna firma de `pub trait` existente cambió.

## Riesgos

- **Ciclo AppState ↔ engine:** mitigado con `Weak<AppState>` y construcción del delegator después del
  AppState, reutilizando el patrón de companions de Fase 6.
- **Timeout del padre antes que el del hijo:** mitigado con `deadline - 5s`.
- **Aprobar de más por el hijo:** el subagente hereda la política de **su** manifiesto, nunca la del
  padre; con scope vacío no tiene tools que aprobar salvo las declaradas explícitamente.
- **La allowlist se vuelva un agujero:** `allow_global` es opt-in (default `false`) y la validación
  de proyecto ocurre en el delegator, no solo en el registry.
- **El `task_id` reanude una conversación ajena:** se usa como `conversation_id` del hijo; la
  persistencia sigue la flag `[memory] persist_history` del subagente. Sin identity de usuario (diferido),
  cualquier llamador que vea el id puede reanudarlo — mismo modelo de confianza que el resto de la API.

## Exit criteria

- El coordinador delega a un especialista del mismo proyecto solo con TOML; el especialista arranca
  con herencia cero demostrable; resultado, aprobaciones y trazas vuelven al padre.
- Ningún puerto existente cambia de firma; agentes sin `[subagents]` y manifests v1 siguen igual.
- Sin anidamiento, sin agentes remotos y sin cambios en la TUI; gates verdes.
