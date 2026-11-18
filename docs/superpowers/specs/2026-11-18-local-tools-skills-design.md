# Fase 2 (pieza 1) — Tools locales declarativas y skills bajo demanda

**Fecha:** 2026-11-18
**Estado:** propuesto — aprobado en brainstorming, pendiente de revisión del spec
**Alcance:** primer sub-proyecto de la Fase 2 del roadmap. Cubre 2c (registry de tools locales)
y 2a (skills bajo demanda como primer consumidor). La pieza 2b (prompt como archivo y roles
system/user) se especifica y ejecuta por separado.

## Contexto

`llama-r` tiene hoy **dos sistemas distintos llamados "tools"** que no deben confundirse:

- `src/optimizer/tools.rs` (`TOOLS`): transformadores de texto del prompt (minify_json,
  compress_code…). El modelo **no** los invoca; los aplica el optimizer.
- `McpToolBridge` (`src/adapters/rig_engine/tools.rs`): el único mecanismo de herramientas
  **invocables por el modelo**, hoy limitado a servidores MCP. No existe tool local declarativa.

Las skills se descubren y se inyectan **completas** como texto en
`ContextEnricher::build_system_prompt` (`src/context/analyzer.rs:398-402`), con un ranking
que descarta las no relevantes. No hay carga bajo demanda: el modelo no puede pedir el
contenido de una skill.

La Fase 1 dejó el camino de agente con tools funcionando por proveedor (el engine reconstruye
su target desde el `(provider, model)` resuelto por request). Ese es el cimiento sobre el que
se apoya esta pieza.

## Goal

Que un agente pueda declarar tools locales en su configuración y que el modelo pueda invocarlas
con schemas tipados, y que las skills pesadas se carguen solo cuando el modelo las pide — sin
tocar el formato `SKILL.md`, sin mezclar con el optimizer y sin duplicar la disciplina de
scope/budget que ya tienen las tools MCP.

## Non-goals

- Ejecutor `http`: pertenece a la Fase 3 (Connections).
- Campo `deferred` en el frontmatter de `SKILL.md`: el formato queda intacto.
- Roles `system`/`user`, `instructions.md`, prompt caching: pieza 2b.
- Subagentes, HITL, ejecución durable: fases posteriores.
- Cambiar los puertos `ToolDyn` (Rig), `RagStore`, `LLMProvider` o `AgentEngine`.

## Decisiones de diseño

1. **Enfoque A — registry tipado, un `ToolDyn` por herramienta.** `LocalToolRegistry` produce
   `Box<dyn ToolDyn>` con `parameters()` desde el JSON Schema declarado. Se descartaron una
   tool "meta" `llama_r__invoke` (pierde schemas tipados) y extender `McpToolBridge` (acopla
   el puerto MCP con tools que no lo son).
2. **Ejecutores de esta entrega:** `builtin:skill` y `rag_query`. Sin `http`.
3. **La skill bajo demanda es una `LocalTool` más**, no un caso especial: `llama_r__skill`
   usa el ejecutor `builtin:skill` sobre `SkillManager::get_skill`.
4. **`deferred` se decide en el manifiesto del agente**, no en el `SKILL.md`: `AgentConfig`
   gana `deferred_skills: Vec<String>` (subconjunto de `skills` ∪ `auto_skills`). Las skills en
   esa lista se anuncian como `nombre — descripción`; las demás siguen inyectándose completas
   como hoy. Un id en `deferred_skills` que no esté declarado produce un warning y se ignora.
   Esto resuelve la ambigüedad "¿cómo se marca deferred sin tocar SKILL.md?" de forma explícita
   y declarativa, coherente con el principio de agentes declarativos del roadmap.
5. **Deny-all por defecto:** `AgentConfig.tools_local: Vec<String>` funciona como
   `tools_override` — vacío significa que el agente ve **ninguna** tool local. Para usar skills
   bajo demanda, el manifiesto debe incluir `llama_r__skill`. Es seguridad por capability, no
   una convención.

## Arquitectura

### Archivos nuevos

- `src/adapters/rig_engine/local_tools.rs`:
  - `LocalTool { id: String, description: String, input_schema: Value, kind: LocalToolKind }`
  - `LocalToolKind { BuiltinSkill, RagQuery { source_id: Option<String> } }`
  - `LocalToolRegistry::load() -> Result<Self, AppError>`: lee `tools/*.toml` del base dir
    (`get_base_dir()`, mismo patrón que `agents/`, `skills/`, `router.toml`), y **siempre**
    registra las dos built-ins `llama_r__skill` y `llama_r__rag_query`. Un archivo ausente es
    válido; uno malformado es un error de configuración que nombra la ruta, se registra como
    warning y cae a las built-ins (no detiene el arranque, igual que `router.toml`).
  - `LocalToolBridge`: implementa `rig_core::tool::ToolDyn` con los mismos cuatro métodos que
    `McpToolBridge`. Re-check de scope, `ToolCallBudget` y `parameters()` desde `input_schema`.

### Archivos modificados

- `src/domain/agent.rs`: `AgentConfig` gana `tools_local: Vec<String>` y
  `deferred_skills: Vec<String>` (ambos `#[serde(default)]`, manifests v1 siguen cargando).
- `src/services/scope_builder.rs`: propagar `tools_local` a `AgentScope`, con el mismo patrón
  con el que `tools_override` llega hoy al scope.
- `src/adapters/rig_engine/mod.rs`:
  - `ScopedTool` enum con variantes `Mcp(ScopedMcpTool)` y `Local(LocalTool)`.
  - `prepare()` devuelve `Vec<ScopedTool>` en vez de `Vec<ScopedMcpTool>`; mantiene el gate de
    `capabilities.tool_calling` exactamente donde está (`mod.rs:267`) — si el modelo no puede
    llamar tools, no se descubren ni MCP ni locales, con el mismo warning.
  - `RigAgentEngine` gana `skill_manager: Arc<SkillManager>` y `local_tools: Arc<LocalToolRegistry>`;
    se construyen en `build_companions` (`src/runtime.rs`) desde los handles existentes. Igual
    disciplina que cuando se añadió `config` en la Fase 1: actualizar todos los constructores.
- `src/adapters/rig_engine/builder.rs`: `build_agent` recibe `Vec<ScopedTool>` y hace el match
  para construir `McpToolBridge` o `LocalToolBridge` antes de `.tools(rig_tools)`. El
  `ToolCallBudget` se comparte entre ambos bridges.
- `src/context/analyzer.rs`: `build_system_prompt` separa las skills declaradas en
  `deferred_skills` (solo `nombre — descripción` + una línea invitando a llamar
  `llama_r__skill`) de las que se inyectan completas (comportamiento actual). El ranking
  (`SkillIndex`) sigue aplicándose a todas las candidatas antes de la separación.
- `src/api/agent_api.rs`, `src/cli/commands.rs`, `src/tui/app.rs`: emitir los dos campos nuevos
  en los TOML que generan, para que un agente creado desde la API/CLI/TUI pueda usarlos.

### Esquema del manifiesto de tool local (`tools/*.toml`)

```toml
# tools/rag-policies.toml
description = "Busca en las políticas cargadas en el RAG del agente."
type = "rag_query"
input_schema = { type = "object", properties = { query = { type = "string" } }, required = ["query"] }
source_id = "politicas"   # opcional; si falta, se usa el primer rag_source del scope
```

Las dos built-ins no necesitan archivo: `llama_r__skill` (`{name: string}`) y
`llama_r__rag_query` (`{query: string, top_k?: integer}`).

## Flujo de datos

1. **Boot:** `LocalToolRegistry::load()` registra las dos built-ins y lee `tools/*.toml`.
2. **Request:** `prepare()` filtra `registry.tools()` por `scope.tools_local` (y por
   `capabilities.tool_calling`, gate existente). Las tools resultantes se concatenan con las
   MCP y llegan a `build_agent` como `Vec<ScopedTool>`.
3. **El modelo llama `llama_r__skill({name})`:** el bridge verifica scope, llama a
   `SkillManager::get_skill(name)` y devuelve `skill.content` (el `SKILL.md` completo).
4. **El modelo llama `llama_r__rag_query({query, top_k})`:** el bridge llama a
   `rag.query_scoped(scope, query, top_k)` y devuelve los chunks como JSON. Sin `rag_sources`
   en el scope, devuelve un error nombrado; nunca consulta una colección global.
5. **Ambos bridges** descuentan el mismo `ToolCallBudget` y re-chequean scope antes de ejecutar.

## Manejo de errores

- Skill inexistente o no permitida → tool result con error nombrado (id), sin panic.
- `LocalTool` desconocida declarada en `tools_local` → warning al resolver el agente, con el
  id y los ids conocidos; el tool no se ofrece (patrón de `optimizer.tools::unknown`).
- `rag_query` sin `rag_sources` → error nombrado; nunca acceso global.
- Manifiesto `tools/*.toml` malformado → `AppError::Config` con la ruta, warning en boot,
  fallback a built-ins.
- Ningún archivo de esta pieza introduce credenciales en prompts, logs, health ni respuestas.

## Testing

- `tests/local_tools_tests.rs` (nuevo): el modelo ve el schema de `llama_r__skill` y su llamada
  devuelve el `SKILL.md` correcto; una skill de otro agente es rechazada por scope.
- `rag_query` devuelve solo chunks del `rag_source` permitido; con scope vacío, error nombrado.
- Budget: N+1 llamadas a una tool local agotan el presupuesto igual que una MCP (mismo bridge).
- `tests/context_enricher_tests.rs` o equivalente: `build_system_prompt` con una skill en
  `deferred_skills` lista nombre/descripción y **no** el cuerpo; el cuerpo aparece solo tras la
  llamada a la tool. Sin `deferred_skills`, el comportamiento es byte-a-byte el de hoy.
- Manifiesto v1 sin los campos nuevos carga sin warning (`#[serde(default)]`).
- Gates: fmt, clippy `-D warnings`, audit, `cargo test --target-dir target-tests` (617+),
  `cargo check --no-default-features`.

## Riesgos

- `deferred_skills` y `tools_local` son campos nuevos en un TOML muy usado por tests y por la
  API: hay que actualizar todos los literales de `AgentConfig` y los generadores de TOML.
  Mitigación: `#[serde(default)]` + grep de literales en la implementación.
- `RigAgentEngine` gana dos dependencias más (`SkillManager`, registry): mismo riesgo de
  constructores olvidados que la Fase 1 con `config`. Mitigación: actualizar todos los call
  sites y correr `--no-default-features` (donde el engine no se compila).
- Las skills "deferred" solo funcionan en el camino de agente con tools; el chat directo sin
  agente no ve la tool. Es coherente con el diseño y se documenta en AGENTS.md.

## Exit criteria

- Un agente con `tools_local = ["llama_r__skill"]` y `deferred_skills = ["x"]` ve el nombre
  y la descripción de `x` en el prompt, y obtiene su contenido al llamar `llama_r__skill`.
- `rag_query` respeta `rag_sources` del scope y falla con error nombrado sin ellos.
- Ni `SKILL.md`, ni los puertos, ni el formato de skills cambiaron.
- Tests nuevos + gates verdes; AGENTS.md y README reflejan los dos campos nuevos y las tools.
