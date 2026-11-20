# Fase 5 (piezas 2 y 3) — Contexto activo y model routing

**Fecha:** 2026-11-20
**Estado:** aprobado en brainstorming (diseño de 3 secciones), pendiente de revisión del spec
**Alcance:** compaction por presupuesto real del modelo con reinyección de instrucciones, y
selección de modelo por complejidad con coste relativo opcional, en un solo spec con dos bloques
independientes.

## Contexto

La pieza 1 de Fase 5 cerró los ocho gaps de historial/memoria. Quedan dos piezas del roadmap
global, aquí combinadas por decisión del usuario en un solo spec (con tareas separadas por bloque):

1. **Compaction / contexto activo.** No existe hoy: `ContextEnricher::build_system_prompt`
   (`src/context/analyzer.rs:317-476`) solo descarta secciones enteras en un prune loop
   (`:454-473`); el historial se inyecta fijo con `get_history(cid, 20)`
   (`src/services/agent_runtime.rs:159,243`); RAG se añade después, fuera del presupuesto
   (`src/adapters/rig_engine/mod.rs:422-440`). `ModelCapabilities.max_context_tokens`
   (`src/providers/capabilities.rs:25,41,317`) está declarado pero **nunca se lee** fuera de tests.
2. **Model routing por complejidad/coste.** No existe lógica de "elegir modelo". El catálogo de
   precios está `None` a propósito (`:62-72`), `Usage::from_counts` (`src/domain/models.rs:34-58`)
   exige ambos precios, y el `DecisionRouter` (`src/ports/router.rs:91-101`) ya tiene varios
   adapters (llama.cpp `/v1/systemone`, OpenAI-compatible, OpenCode): **el modelo de decisión es
   el que esté configurado, no uno hardcodeado**.

Decisiones del brainstorming:

- **Compaction** dispara por `min(capabilities.max_context_tokens, agent.max_context_tokens)` y
  comprime solo el historial; las instrucciones nunca se compactan.
- **Precios:** contrato listo + `models.toml` opcional con `relative_cost` entero; sin tabla, el
  coste no participa. No se inventan precios ni se escriben en `ModelCapabilities`.
- **Selector:** el `DecisionRouter` inyectado clasifica complejidad con una pregunta `Choice`; los
  candidatos se declaran en el manifiesto; sin router, degrada al modelo actual.
- **Enfoque A:** dos puertos nuevos aditivos (`ContextCompactor`, `ModelSelector`) + `ActiveContext`;
  sin cambiar `AgentEngine`/`LLMProvider`.

## Goal

Que el prompt respete el presupuesto real del modelo sin perder reglas/skills/contexto, y que el
runtime pueda elegir modelo por complejidad usando el router configurado y un coste relativo
opcional — con degradación segura al comportamiento actual en ambos casos.

## Non-goals

- Precios reales de providers; escribir `cost_usd` fabricated; poblar `ModelCapabilities` con datos
  inventados.
- Compaction de prompts internos de herramientas o RAG (solo del historial de conversación).
- Reinyección de memoria en el prompt **del router** (solo en el del agente).
- OpenTelemetry, cambios de TUI, auth, o cambio de comportamiento de agentes sin `model_candidates`.
- Cambiar firmas de `AgentEngine`, `LLMProvider`, `DecisionRouter` o cualquier puerto sagrado.

## Decisiones de diseño

1. **`ActiveContext`** (`src/context/active.rs`): `{ system_prompt, memory: Option<String>,
   history: Vec<ChatMessage>, reserved_tokens }`. Es lo único que ve el engine como contexto. Las
   instrucciones, skills y reglas se construyen una sola vez y no se compactan.
2. **Presupuesto.** `budget = min(capabilities.max_context_tokens, agent.max_context_tokens)`; si el
   catálogo no declara el suyo, usa solo el del agente; si faltan ambos, **no hay compaction** (se
   mantiene el comportamiento actual). `reserved_tokens` es el espacio apartado para tools y RAG.
3. **Puerto `ContextCompactor`** (`src/ports/compaction.rs`):
   `async fn compact(&self, req: CompactRequest) -> Result<CompactResult, String>`, con
   `CompactRequest { system_prompt, history, budget_tokens, reserved_tokens }` y
   `CompactResult { memory: Option<String>, history: Vec<ChatMessage> }`. Adapters:
   `SummarizingCompactor` (usa el `Summarizer` inyectable de la pieza 1 sobre los turnos más
   antiguos) y `NoopCompactor` (sin presión de presupuesto). Con `NoopSummarizer`, se descartan los
   turnos antiguos con warning, **nunca** se truncan las instrucciones.
4. **Puerto `ModelSelector`** (`src/ports/model_selection.rs`):
   `async fn select(&self, req: ModelSelectionRequest) -> ModelSelection`, con
   `ModelSelectionRequest { candidates: Vec<ModelCandidate>, prompt_hint: String, current_model:
   String }`, `ModelCandidate { model: String, provider_hint: Option<String>, relative_cost: u32 }`,
   y `ModelSelection { model: String, reason: SelectionReason }`. Adapters: `RouterModelSelector`
   (envuelve `Arc<dyn DecisionRouter>`, pregunta `Choice` de complejidad sobre etiquetas de
   candidatos) y `CurrentModelSelector` (devuelve `current_model`).
5. **El router es configurable, no un modelo fijo.** La pregunta de complejidad viaja por el mismo
   `DecisionRequest` que ya usan el routing de agentes y `Auto`; el adapter/modelo lo decide
   `ROUTER_URL` / `router.toml`, igual que hoy. `complex` elige el candidato complejo; `simple` el
   primero; `Noul`, error, baja confianza o router ausente → modelo actual (misma disciplina
   fail-closed que Fase 6a).
6. **Candidatos y coste.** `AgentConfig.model_candidates: Vec<String>` (`#[serde(default)]`, vacío =
   sin cambio). `models.toml` opcional mapea `model → relative_cost: u32` entero (p. ej. 1 barato,
   3 caro). El coste solo desempata entre candidatos de igual complejidad; sin tabla, todos valen 1 y
   gana el primero declarado. Ningún número se reporta como `cost_usd` ni se escribe en el catálogo.
7. **`ContextAssembler`** (`src/services/context_assembler.rs`) orquesta: construye el system prompt
   con el `ContextEnricher` existente, calcula presupuesto, carga historial, compacta si hace falta y
   consulta al selector solo si hay candidatos. Devuelve `AssembledContext { context: ActiveContext,
   model: String }` al `AgentRuntime`.
8. **Reinyección.** El resumen se inyecta como sección `## Conversation memory` **delante del
   historial pero detrás de las instrucciones del agente**. Las secciones de reglas/skills/contexto
   del prompt con compaction son byte-for-byte idénticas a las del prompt sin ella.
9. **Degradación best-effort.** Fallo de compactor o selector → warning + contexto/modelo originales;
   el chat nunca falla. Un turno delegado (Fase 4) hereda el budget del root pero **no** su
   `ActiveContext`: ensambla el suyo desde su manifiesto, igual que hoy hereda scope cero.
10. **Wiring.** `compactor` y `selector` viajan como companions desde `CompanionHandles`
    (`src/runtime.rs`) al `AgentRuntime`, sobreviviendo a un Apply como `auto_decider` y
    `observability`. Sin router, el selector es `CurrentModelSelector`; sin compactor configurado,
    `NoopCompactor`.

## Arquitectura

### Puertos y adapters

- `src/ports/compaction.rs`, `src/ports/model_selection.rs` (nuevos, aditivos).
- `src/adapters/context/`: `summarizing_compactor.rs`, `router_model_selector.rs`,
  `current_model_selector.rs`.
- `src/providers/model_costs.rs`: carga opcional de `models.toml` (`relative_cost`), sin secretos;
  archivo ausente = tabla vacía.

### Servicio y uso

- `src/services/context_assembler.rs` produce `AssembledContext`; `AgentRuntime` lo consume antes de
  `build_run_request`, pasando `system_prompt`/`history` ya ensamblados y el modelo elegido como
  `model_override`.
- `src/domain/agent.rs`: `model_candidates` con serde default; `PUT /api/agents/:id` lo preserva como
  `routing`/`description`/`subagents`.

## Flujo de datos

1. `AgentRuntime` recibe un turno con capabilities y manifiesto resueltos.
2. `ContextAssembler` construye el system prompt (enricher), calcula el presupuesto y la reserva,
   y carga el historial.
3. Si `prompt + historial + reserva > budget`, el compactor resume los turnos antiguos con el
   `Summarizer` inyectado y devuelve memoria + historial recortado; si no, `NoopCompactor`.
4. Si hay `model_candidates`, el selector consulta el `DecisionRouter` configurado (Choice de
   complejidad) y desempata con `relative_cost`; si no, usa el modelo actual.
5. El engine recibe el `ActiveContext` ensamblado y el modelo elegido; el hijo delegado ensambla el
   suyo propio.

## Manejo de errores

- Router ausente/error/`Noul`/baja confianza → `ModelSelection { model: current, reason:
  RouterUnavailable }` con warning; nunca un modelo inventado.
- Summarizer `Noop` o error → se descartan los turnos antiguos con warning; instrucciones intactas.
- Presupuesto sin catálogo → solo el del agente; sin ambos → sin compaction, warning debug.
- `models.toml` mal formado → warning nombrando la ruta, tabla vacía; boot continúa.
- Fallo del assembler → warning y se usa el prompt/modelo originales; el chat sigue.

## Testing

`tests/context_pipeline_tests.rs` (nuevo) + extensiones:

- Presupuesto: catálogo < agente gana el menor; sin catálogo usa el del agente; sin ambos no hay
  compaction; `prompt + memoria + historial + reserva <= budget`.
- Compaction: historial largo con reglas/skills/RAG → reglas y skills byte-for-byte, resumen en
  `## Conversation memory`, solo turnos antiguos resumidos (FakeSummarizer captura qué mensajes).
- Selector: `simple` → primer candidato; `complex` → candidato complejo; sin router/error/`Noul` →
  modelo actual; `relative_cost` desempata a igual complejidad; sin `models.toml` coste no participa;
  hijo delegado no hereda `ActiveContext`.
- Best-effort: compactor y selector fallando → warning, chat Ok con contexto/modelo originales.

Gates: fmt / clippy `-D warnings` / audit / suite completa (886+) / build `--no-default-features`.

## Riesgos

- **Perder instrucciones al compactar:** mitigado construyendo el prompt después de la compaction y
  testeando byte-for-byte las secciones de reglas/skills.
- **Atarse a un modelo de decisión:** mitigado usando el puerto `DecisionRouter` y su configuración
  existente; ningún nombre de modelo en el código del selector.
- **Precios inventados:** mitigado con `relative_cost` entero opcional en `models.toml` y sin
  writing a `ModelCapabilities` ni a `cost_usd`.
- **Doble threading de companions:** mitigado con el patrón ya probado en Fases 4 y 5
  (`CompanionHandles` → engine/runtime), con boot y Apply compartiendo la ruta.
- **Compaction en un turno delegado:** mitigado ensamblando el contexto del hijo desde su propio
  manifiesto, sin herencia del padre.

## Exit criteria

- El prompt respeta el presupuesto real del modelo sin perder reglas/skills/contexto; el resumen se
  reinyecta como memoria sin tocar instrucciones.
- El modelo puede cambiar por complejidad con el router configurado, `relative_cost` opcional y
  degradación segura al modelo actual.
- Dos puertos nuevos aditivos, ningún puerto sagrado cambiado, sin precios inventados ni OTel;
  gates verdes.
