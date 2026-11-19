# Fase 4 — Subagents con aislamiento

**Fecha:** 2026-11-19
**Estado:** revisado tras petición del usuario (anidamiento acotado); pendiente de aprobación del spec
**Alcance:** sustituir el stub `[subagents]` por delegación real con **anidamiento limitado**,
herencia cero, budget compartido, trazas jerárquicas y aprobaciones reenviadas al padre.

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
- **Anidamiento acotado** (revisión del usuario): un subagente puede delegar a su vez, hasta un
  `max_depth` configurable. Se revive el campo `max_depth` del stub, antes ignorado. Límite duro
  global de 8 niveles y detección de ciclos.
- **Allowlist de agentes ya registrados:** el coordinador declara ids; los especialistas viven en
  `contextos/projects/<p>/agents/` con su propio TOML y Scope. No se duplica el schema de `AgentConfig`.
- **Enfoque A + aprobaciones mapeadas:** tool local con un `Delegator` inyectado; reentrada a
  `execute_chat_with_id`; `ApprovalRequired` de cualquier nivel reenviado al SSE del coordinador
  raíz con `task_id` y `depth`.

Se descartaron dos alternativas: delegar directamente a `AgentRuntime::chat` (B: se salta
`finish_execution`, trazas y fallback, duplicando lógica) y un evento `AgentRunEvent::DelegateRequested`
(C: cambia un puerto y exige sintetizar el `ToolResult`).

## Goal

Que un agente coordinador delegue tareas a especialistas del mismo proyecto **solo con TOML**, con
anidamiento limitado y sin ciclos; que cada especialista arranque con herencia cero demostrable; y que
resultado, aprobaciones y trazas de todo el árbol vuelvan al coordinador reutilizando el pipeline y
los puertos existentes.

## Non-goals

- Anidamiento **ilimitado** (techo duro de 8) y agentes con `max_depth` fuera de rango.
- Agentes remotos o sandbox (Fase 8).
- *Steering* de mensajes a un hijo en vuelo (Fase 7 lo deja diferido).
- Cambios en la TUI, en `AgentEngine`/`LLMProvider`/`DecisionRouter` ni en agentes que no declaren
  `[subagents]` (v1 intacto).
- Identidad de usuario en aprobaciones (ya diferida en Fase 3/7).

## Decisiones de diseño

1. **Manifest tipado y compatible.** `AgentConfig.subagents` pasa a
   `Option<SubagentsConfig { allowed: Vec<String>, allow_global: bool, max_depth: Option<u32> }>`
   (`#[serde(default)]`). Los manifests v1 con `[subagents]` siguen cargando. El campo presente **sí
   se serializa**, para que `PUT /api/agents/:id` preserve la allowlist y la profundidad (igual que
   `routing` y `description`). Se actualiza `the_subagents_stub_never_serialises`.
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
   El hijo corre con **su** manifiesto y **su** scope. El resultado vuelve como `ToolResult` al bucle
   de Rig del padre, que sigue; el `task_id` efectivo se devuelve para reanudar.
5. **Herencia cero por construcción.** El hijo se resuelve únicamente por su id, validado contra
   `allowed` y el mismo proyecto (o `agents/` si `allow_global`). El padre no le pasa project context,
   skills, RAG sources ni tools: el `ScopeBuilder` del hijo ya produce `DenyAll`. Un id no permitido
   es un `ToolError` nombrado, nunca una delegación.
6. **Profundidad acotada y sin ciclos.** `max_depth` = número máximo de niveles de delegación desde
   el coordinador raíz. Default `1` (comportamiento de un solo nivel de la primera versión del diseño),
   rango válido `1..=8`. El **límite efectivo** es el mínimo entre el `max_depth` del raíz y el de cada
   agente del chain, con techo global 8; se propaga en `DelegateRequest`. `DelegateRequest.chain` lleva
   los ids cualificados de los ancestros; si el id destino ya está en el chain → `ToolError` nombrado,
   cero ejecución. Un valor fuera de rango se ignora con un warning que nombra ruta y valor, y cae al
   default 1 (misma disciplina que los manifests malformados).
7. **Budget compartido (jerárquico).** `build_tool_bridges` ya crea un `Arc<ToolCallBudget>` por run
   (`src/adapters/rig_engine/limits.rs:3-27`). El `DelegateRequest` lo lleva; cada turno del árbol
   usa **el mismo** budget en vez de crear uno nuevo. Así hay una sola asignación para todo el árbol,
   sin importar cuántos niveles se deleguen. Agotarlo da un `ToolError` nombrado que sube por la cadena.
   La carga sigue ocurriendo solo después de `authorize → Ok`.
8. **Aprobaciones sin tocar el gate.** El hijo usa el mismo `ApprovalGate` global. Un `EventForwarder`
   del coordinador raíz reenvía el `ApprovalRequired` de **cualquier** nivel al SSE del raíz con
   `task_id` y `depth` en el payload de `approval` (campos opcionales y aditivos en el DTO, sin cambiar
   el puerto). En no-streaming no hay sender: el cliente descubre el pendiente con
   `GET /api/approvals` y decide con `POST`, sin cambios. `approval_id` sigue siendo el UUID global.
9. **Trazas jerárquicas.** Cada turno del árbol emite su `TraceRecorded` con su propio
   `execution_id`. El payload gana campos opcionales `parent_execution_id`, `root_execution_id` y
   `depth`, de modo que el log de ejecución reconstruye el árbol sin un backend nuevo. Se redacta con
   el mismo `redact_trace_payload` de Fase 6.
10. **Idempotencia y deadline.** `operation_id` de Fase 7 cachea el resultado de cada tool call, así
    que reintentar un turno no re-ejecuta a sus hijos. `DelegateRequest.deadline` se deriva del padre
    (`min(restante − 5s, timeout del scope del hijo)`) recursivamente, para que el error del hijo llegue
    antes de que expire el bucle del padre.

## Arquitectura

### Manifest y resolución

- `src/domain/agent.rs`: `SubagentsConfig`, campo tipado, defaults, validación de `max_depth` y tests.
- `src/services/agent_registry.rs:178-232`: reutilizar `resolve_agent` / `get_project_agent` / `get_agent`;
  la validación de allowlist y profundidad vive en el delegator, no en el registry.
- `src/api/route_resolution.rs:19-25`: construir `AgentSelection` nueva sin reutilizar la del padre
  (los headers ya se consumieron en `handlers.rs:180-189`).

### Componentes

- `src/ports/delegation.rs` (nuevo): `DelegateRequest { agent_id, message, task_id: Option<String>,
  chain: Vec<String>, depth: u32, effective_max_depth: u32, deadline: Instant,
  budget: Arc<ToolCallBudget>, event_tx: Option<EventForwarder> }`,
  `DelegateResult { answer, task_id }`, trait `Delegator`.
- `src/services/delegation.rs` (nuevo): validación de allowlist/proyecto/profundidad/ciclo,
  resolución, `delegate_turn`, timeout, propagación de budget y eventos.
- `src/adapters/rig_engine/local_tools.rs`: id `llama_r__delegate`, `LocalToolKind::Delegate`,
  esquema JSON `{message, task_id?}`, dispatch al delegator; re-check de scope, aprobación, budget,
  idempotencia y eventos `ToolCall`/`ToolResult` igual que las demás tools.
- `src/runtime.rs`: `CompanionHandles.delegator`, construido tras el `AppState` con `Arc::downgrade`.

### Threading

Igual que Fase 6: `delegator` viaja en `LocalToolDeps` y se inyecta en el engine; boot y Apply
comparten `build_companions`, de modo que un hot swap de provider no pierde el delegator. El budget
se comparte explícitamente por `DelegateRequest`, sin tocar el puerto del engine.

## Flujo de datos

1. Un agente emite `llama_r__delegate({message, task_id?})`.
2. El bridge re-checka `allows_local_tool`, resuelve aprobación, carga 1 unidad del budget compartido,
   calcula `operation_id` y llama `delegator.delegate(...)` con `depth+1`, el chain actualizado y el
   `effective_max_depth` recalculado.
3. El delegator valida id (allowlist + mismo proyecto), profundidad (`depth <= max`) y ciclo
   (`id ∉ chain`), y llama `delegate_turn`.
4. `delegate_turn` crea `execution_id` hijo, usa `task_id` como `conversation_id` y ejecuta
   `execute_chat_with_id(stream: false)` con el budget compartido. El hijo emite su `TraceRecorded`
   con `parent_execution_id`, `root_execution_id` y `depth`.
5. Los `ApprovalRequired` de cualquier nivel se reenvían al SSE del raíz con `task_id` y `depth`;
   en no-streaming el cliente usa el polling global existente.
6. La respuesta del hijo vuelve como `ToolResult`; el bucle del padre continúa y el `task_id` efectivo
   se devuelve en el resultado para reanudar.

## Manejo de errores

- Id no permitido, de otro proyecto o global sin `allow_global` → `ToolError` nombrado, cero ejecución.
- Profundidad excedida o id ya presente en el chain (ciclo) → `ToolError` nombrado, cero ejecución.
- Hijo ausente, provider caído, budget agotado o timeout → `ToolError` con el motivo; el turno del
  padre **continúa** (el modelo decide cómo responder). Nunca se propaga como error del chat raíz.
- Fallo al persistir la traza o el pending → warning best-effort, misma disciplina que Fase 7.
- `Weak::upgrade` fallido (AppState ya liberado) → `ToolError` nombrado, no panic.

## Testing

`tests/subagent_tests.rs` (nuevo, sin red ni modelo real):

- id permitido del mismo proyecto → delegación ejecuta, `ToolResult` vuelve, se emite `ToolCall`.
- Fuera de allowlist, otro proyecto, global sin `allow_global` → `ToolError`, sin ejecutar nada.
- **Herencia cero**: scope del hijo `DenyAll` en tools locales y MCP, sin `rag_sources` ni skills del
  padre, y su prompt no contiene las secciones del padre.
- **Anidamiento**: cadena de 2–3 niveles funciona; `max_depth = 1` impide el segundo nivel; un valor
  fuera de `1..=8` cae al default 1 con warning; el límite efectivo es el mínimo del chain; un ciclo
  (A → B → A) se rechaza nombrando el id, sin ejecutar.
- **Budget compartido**: un solo allowance para todo el árbol; agotarlo en un hijo produce el
  `ToolError` nombrado en el padre.
- **Trazas**: cada nivel emite un `TraceRecorded` con su `execution_id` y los campos de jerarquía;
  el árbol se reconstruye desde `GET /api/executions/:id`.
- `task_id` → `conversation_id` del hijo; sin él se genera y se devuelve; `task_id` existente reanuda.
- Aprobación de cualquier nivel: en streaming llega al SSE del raíz con `task_id` y `depth`; en
  no-streaming aparece en `GET /api/approvals` y se decide con `POST`.
- Fallo del hijo no tumba al padre; reintento cacheado por `operation_id` sin re-ejecutar.
- Manifiestos v1 con `[subagents]` cargan; `PUT /api/agents` preserva allowlist y `max_depth`.

Gates: fmt / clippy `-D warnings` / audit / suite completa (813+) / build `--no-default-features`.
Verificación explícita de que ninguna firma de `pub trait` existente cambió.

## Riesgos

- **Ciclo AppState ↔ engine:** mitigado con `Weak<AppState>` y construcción del delegator después del
  AppState, reutilizando el patrón de companions de Fase 6.
- **Explosión combinatoria por anidamiento:** mitigada con techo duro 8, `max_depth` efectivo = mínimo
  del chain, detección de ciclos y **un solo budget compartido** para todo el árbol.
- **Timeout del padre antes que el del hijo:** mitigado con `deadline` recursivo (`restante − 5s`).
- **Aprobar de más por un hijo:** el subagente hereda la política de **su** manifiesto, nunca la del
  padre; con scope vacío no tiene tools que aprobar salvo las declaradas explícitamente.
- **La allowlist se vuelva un agujero:** `allow_global` es opt-in (default `false`) y la validación
  de proyecto ocurre en el delegator, no solo en el registry.
- **El `task_id` reanude una conversación ajena:** se usa como `conversation_id` del hijo; la
  persistencia sigue la flag `[memory] persist_history` del subagente. Sin identity de usuario (diferido),
  cualquier llamador que vea el id puede reanudarlo — mismo modelo de confianza que el resto de la API.

## Exit criteria

- El coordinador delega a especialistas del mismo proyecto solo con TOML, con anidamiento hasta
  `max_depth`, sin ciclos y con un budget compartido; cada especialista arranca con herencia cero
  demostrable; resultado, aprobaciones y trazas jerárquicas vuelven al raíz.
- Ningún puerto existente cambia de firma; agentes sin `[subagents]` y manifests v1 siguen igual.
- Sin agentes remotos y sin cambios en la TUI; gates verdes.
