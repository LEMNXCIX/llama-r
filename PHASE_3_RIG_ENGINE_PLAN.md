# Fase 3 — Integración Rig.rs (Agent Engine)

> **Documento de ejecución para un agente de IA implementador**  
> Fuente: [`ARCHITECTURE_PLAN.md`](./ARCHITECTURE_PLAN.md) § Fase 3 + §6.6  
> Audiencia: agente **menos capaz** — seguir este plan **en orden**, sin saltarse pasos, sin “mejoras creativas” fuera de alcance.  
> Ubicación: raíz del repo (`PHASE_3_RIG_ENGINE_PLAN.md`)

---

## 0. Cómo usar este plan (léelo completo antes de tocar código)

### 0.1 Rol del implementador

Eres un implementador Rust en el repo **llama-r**. Tu trabajo es completar la **Fase 3: Integración Rig.rs** para que un request de chat con agente ejecute un **loop multi-step con tools MCP filtradas por scope**, usando Rig como motor.

### 0.2 Reglas obligatorias

1. **Trabaja solo en Fase 3.** No implementes RAG LanceDB (Fase 4), historial SQLite (Fase 5), ni sub-agentes completos salvo el stub opcional al final.
2. **No rompas el path legacy.** Sin feature `rig-engine`, o cuando el engine falle de forma controlada, el chat actual vía `OllamaProvider` debe seguir funcionando.
3. **No inventes APIs nuevas de producto** (nuevos endpoints HTTP) salvo lo indicado. Extiende lo existente.
4. **Defense in depth:** cada invocación de tool MCP **debe** re-chequear `scope.allows_tool(...)`.
5. **No pongas secretos en TOML.**
6. **Después de cada subtarea (3.x):** corre `cargo check` y, cuando haya tests, `cargo test --target-dir target-tests`.
7. **No edites** `target/`, `target-tests/`, logs, ni archivos generados.
8. Si la API de `rig-core` no coincide con los snippets de este plan, **adapta el código al crate real** leyendo docs del crate (`cargo doc --open` o docs.rs), pero **mantén el contrato de `AgentEngine`** en `src/ports/engine.rs` estable.
9. Commits: solo si el usuario lo pide. No hagas `git push`.
10. Idioma de comentarios de código: inglés. Mensajes de error al usuario pueden ser en inglés (consistente con el resto del repo).

### 0.3 Criterio de salida de toda la fase (Definition of Done)

La fase está **completa** solo si **todas** estas condiciones se cumplen:

| # | Condición |
|---|-----------|
| D1 | Feature Cargo `rig-engine` existe y activa `rig-core`. |
| D2 | Con feature ON, `RigAgentEngine::run` completa un chat con Ollama (sin tools) y devuelve texto. |
| D3 | Con feature ON y agente con `mcp_sources` + `tools_override`, el engine descubre tools, las expone a Rig, y puede invocar solo las permitidas por scope. |
| D4 | Tools denegadas por scope **nunca** se invocan (re-check en invoke). |
| D5 | Se respetan `max_iterations`, `max_tool_calls`, `timeout_secs` del scope. |
| D6 | System prompt del agente pasa por el enricher existente (context + skills + rules) antes del engine. |
| D7 | `POST /api/chat` y `POST /chat` con headers `X-Project` / `X-Agent` usan el engine cuando hay agente resuelto y feature ON. |
| D8 | Sin feature o con fallback, el path legacy (`provider.chat`) sigue funcionando. |
| D9 | Streaming: al menos tokens de respuesta (y si es viable, tool_call/tool_result) llegan como SSE o se documenta limitación con path no-stream. |
| D10 | Tests unitarios del bridge de tools + límites pasan con mock (sin Ollama real obligatorio para unit tests). |
| D11 | `cargo fmt`, `cargo check`, `cargo test --target-dir target-tests` pasan. |
| D12 | `AGENTS.md` y/o `ROADMAP.md` actualizados con estado de Fase 3. |

### 0.4 Qué YA existe (no reimplementar desde cero)

| Pieza | Ruta | Estado |
|-------|------|--------|
| Trait `AgentEngine` + tipos | `src/ports/engine.rs` | ✅ listo — **no cambiar el contrato público sin necesidad** |
| Stub `RigAgentEngine` + `ScopedMcpTool` + `prepare()` | `src/adapters/rig_engine/mod.rs` | ✅ stub — **completar** |
| Orquestador `AgentRuntime` | `src/services/agent_runtime.rs` | ✅ existe — **falta wiring a AppState/chat** |
| MCP client HTTP + registry | `src/adapters/mcp/` | ✅ usable |
| Scope + `allows_tool` | `src/domain/scope.rs` | ✅ listo |
| Chat HTTP actual | `src/api/chat_core.rs` | ✅ path legacy Ollama — **integrar engine** |
| `AppState` | `src/api/handlers.rs` | ✅ **no tiene** campo engine/runtime aún |
| Wiring runtime | `src/runtime.rs` | ✅ construye AppState — **agregar engine** |
| Feature `rig-engine` / dep `rig-core` | `Cargo.toml` | ❌ **no existe aún** |

### 0.5 Flujo objetivo (después de Fase 3)

```text
POST /api/chat  (+ X-Project / X-Agent opcionales)
  → chat_core::execute_chat
  → prepare_request (resuelve agente, enrich system prompt)   [ya existe]
  → SI hay agente resuelto Y engine disponible:
        AgentRuntime.chat / AgentEngine.run
          → RigAgentEngine.prepare (tools scoped + RAG opcional)
          → loop Rig (LLM + tools MCP)
          → AgentRunResult
  → SI NO: path legacy provider.chat (Ollama)
  → ChatResponse
```

### 0.6 Orden de implementación (NO reordenar)

```text
3.1 Spike cargo + Ollama mínimo
  → 3.2 Completar RigAgentEngine.run (sin tools primero)
    → 3.3 Bridge MCP → Rig Tool
      → 3.4 Wiring system prompt / enricher (vía chat_core + AgentRuntime)
        → 3.5 Límites (iterations, tool calls, timeout)
          → 3.6 Streaming (si viable; si no, fallback documentado)
            → 3.7 Feature flag + fallback legacy en chat_core
              → 3.8 Sub-agentes (OPCIONAL / P2 — solo si sobra tiempo)
                → 3.9 Tests
                  → 3.10 Docs + verificación final
```

---

## 1. Contexto de arquitectura (solo lo necesario)

### 1.1 Capas

- **ports** (`src/ports/engine.rs`): contrato estable `AgentEngine`.
- **adapters** (`src/adapters/rig_engine/`): única capa que puede depender de `rig-core`.
- **services** (`src/services/agent_runtime.rs`): orquesta scope + engine + history hooks.
- **api** (`src/api/chat_core.rs`): elige engine vs legacy.

**Regla:** ningún módulo fuera de `adapters/rig_engine` debe hacer `use rig::...`.

### 1.2 Tipos del port (referencia — ya en código)

Archivo: `src/ports/engine.rs`

- `AgentRunRequest`: agent, scope, model, system_prompt, user_message, history, conversation_id
- `AgentRunResult`: text, tool_calls, iterations, model
- `AgentRunEvent`: Token | ToolCall | ToolResult | Completed | Error
- `AgentEngine::run` y `AgentEngine::run_stream`

**No renombres estos tipos** salvo bug de compilación.

### 1.3 Scope de seguridad

Archivo: `src/domain/scope.rs`

- `tools_override` vacío → `ToolsAllow::DenyAll`
- `["*"]` → `AllFromSources`
- lista → allowlist
- `allows_tool(server_id, tool_name)` es la fuente de verdad

---

## 2. Tarea 3.1 — Spike: `rig-core` + Ollama (mínimo)

**Objetivo:** que el proyecto compile con `rig-core` y se pueda llamar a Ollama vía Rig en un path aislado.  
**Estimación:** ~1 día  
**Prioridad:** P0  
**Dependencias:** Ollama local (para prueba manual); unit tests pueden mockear después.

### 2.1 Cambios en `Cargo.toml`

Edita `Cargo.toml` y agrega:

```toml
[features]
default = ["rig-engine"]
rig-engine = ["dep:rig-core"]

[dependencies]
# ... existentes ...
rig-core = { version = "0.11", optional = true }
```

**Notas importantes para el implementador:**

1. Verifica en [crates.io/crates/rig-core](https://crates.io/crates/rig-core) la versión más reciente **estable** compatible. Si `0.11` no existe o falla, usa la última `0.x` documentada y anota la versión elegida en un comentario en `Cargo.toml` o en la sección “Decisiones” al final de este archivo.
2. Si `rig-core` trae features opcionales (p.ej. providers), habilita las necesarias para Ollama según docs del crate.
3. Si hay conflicto de versiones de `reqwest` / `tokio` / `serde`, resuelve con versiones compatibles **sin bajar** features críticas del proyecto (`json`, `stream` en reqwest).

### 2.2 Estructura de archivos a crear

```text
src/adapters/rig_engine/
  mod.rs          # ya existe — reorganizar exports
  builder.rs      # NUEVO — construcción del agent Rig
  tools.rs        # NUEVO — bridge ScopedMcpTool → Rig Tool
  limits.rs       # NUEVO — contadores max_tool_calls / timeout
  legacy_fallback.rs  # OPCIONAL — helper si prefieres separar
```

Actualiza `src/adapters/rig_engine/mod.rs` para:

```rust
pub mod builder;
pub mod limits;
pub mod tools;

// re-export públicos útiles
pub use builder::RigAgentEngine; // o mantener struct en mod.rs y builder solo helpers
```

**Decisión de diseño recomendada (elige UNA y sé consistente):**

- **Opción A (recomendada):** dejar `RigAgentEngine` en `mod.rs` y poner helpers en `builder.rs` / `tools.rs` / `limits.rs`.
- **Opción B:** mover `RigAgentEngine` a `builder.rs` y re-exportar desde `mod.rs`.

### 2.3 Spike compilable (sin tools)

Implementa primero un path mínimo en `RigAgentEngine::run` **detrás de** `#[cfg(feature = "rig-engine")]`:

Pseudocódigo (API real de Rig puede variar — adaptar):

```rust
#[cfg(feature = "rig-engine")]
async fn run_with_rig(&self, req: AgentRunRequest, system: String) -> Result<AgentRunResult, String> {
    use rig::completion::Prompt;
    use rig::providers::ollama;

    // Ajusta constructor según docs de la versión instalada:
    // Client::new() | Client::from_url(&self.ollama_url) | etc.
    let client = ollama::Client::from_url(&self.ollama_url);

    let agent = client
        .agent(&req.model)
        .preamble(&system)
        .build();

    let text = agent
        .prompt(&req.user_message)
        .await
        .map_err(|e| e.to_string())?;

    Ok(AgentRunResult {
        text,
        tool_calls: 0,
        iterations: 1,
        model: req.model,
    })
}
```

### 2.4 cfg dual

```rust
#[async_trait]
impl AgentEngine for RigAgentEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        let (_tools, system) = self.prepare(&req).await?;

        #[cfg(feature = "rig-engine")]
        {
            return self.run_with_rig(req, system /*, tools más adelante */).await;
        }

        #[cfg(not(feature = "rig-engine"))]
        {
            let _ = system;
            Err("feature `rig-engine` disabled".into())
        }
    }
    // ...
}
```

### 2.5 Verificación 3.1

```bash
cargo check
cargo check --no-default-features
cargo check --features rig-engine
```

**Prueba manual opcional** (si Ollama corre):

1. Temporalmente, desde un test bin o `cargo test` con `#[ignore]`, llama `RigAgentEngine::run` con un agent sin tools.
2. Confirma respuesta no vacía.

### 2.6 Checklist 3.1

- [ ] `Cargo.toml` tiene feature `rig-engine` y dep opcional `rig-core`
- [ ] `cargo check --features rig-engine` compila
- [ ] `cargo check --no-default-features` compila (path sin rig)
- [ ] No se importa `rig` fuera de `adapters/rig_engine`
- [ ] `prepare()` existente sigue funcionando

**STOP:** no avances a 3.3 hasta que 3.1 + un `run` sin tools compile.

---

## 3. Tarea 3.2 — Completar `AgentEngine` + `RigAgentEngine`

**Objetivo:** `run` funcional end-to-end **sin tools** (chat simple con preamble).  
**Estimación:** ~1 día  
**Prioridad:** P0

### 3.1 Archivos a tocar

| Archivo | Acción |
|---------|--------|
| `src/adapters/rig_engine/mod.rs` | Completar `run` |
| `src/adapters/rig_engine/builder.rs` | Extraer construcción client/agent |
| `src/ports/engine.rs` | Solo si falta un campo crítico (evitar cambios) |

### 3.2 Comportamiento de `run` (sin tools)

1. Llamar `self.prepare(&req).await?` → obtiene tools (pueden usarse o no) + `system` enriquecido con RAG si hay store.
2. Si `req.history` no está vacío, incluir historial en el prompt/chat de Rig **si la API lo permite**. Si no, concatenar un resumen simple al user message o usar API de chat multi-turn de Rig.
3. Ejecutar prompt.
4. Devolver `AgentRunResult { text, tool_calls: 0, iterations: 1, model }`.

### 3.3 Temperature y model

- Modelo: `req.model` (ya resuelto por runtime/chat).
- Si `req.agent.config.temperature` es `Some(t)`, pásalo al builder Rig **si la API lo soporta**. Si no, ignóralo con un `tracing::debug!` y sigue.

### 3.4 Errores

- Errores de red Ollama → `Err(String)` con mensaje claro (`"ollama chat failed: ..."`).
- Nunca hagas `unwrap()` en paths de producción.

### 3.5 Verificación 3.2

```bash
cargo check --features rig-engine
```

Test unitario mínimo (puede ir en `src/adapters/rig_engine/mod.rs` bajo `#[cfg(test)]`):

- Con un mock **no es obligatorio** aún si Rig no se mockea fácil; al menos compilar.
- Opcional: test `#[ignore]` de integración llamado `rig_ollama_smoke` que se saltea en CI.

### 3.6 Checklist 3.2

- [ ] `RigAgentEngine::run` no devuelve el error stub actual cuando feature ON
- [ ] Usa `prepare()` para system prompt
- [ ] Manejo de errores sin panic
- [ ] Compila con y sin feature

---

## 4. Tarea 3.3 — Bridge MCP tools → Rig `Tool`

**Objetivo:** cada `ScopedMcpTool` se registra como tool del agent Rig; al llamarse, ejecuta MCP con re-check de scope.  
**Estimación:** ~1.5 días  
**Prioridad:** P0  
**Crítico para seguridad.**

### 4.1 Archivo principal

Crear/editar: `src/adapters/rig_engine/tools.rs`

### 4.2 Problema de diseño (léelo)

En Rig, el trait `Tool` suele requerir:

- `const NAME: &'static str` **o** nombre dinámico según versión
- Tipos asociados `Args`, `Output`, `Error`
- `definition()` y `call()`

Las tools MCP son **dinámicas** (nombre + JSON Schema en runtime). Por eso **no** puedes hacer un struct estático por cada tool del universo.

**Estrategias aceptables (elige la que funcione con la versión de rig-core):**

| Estrategia | Descripción | Preferencia |
|------------|-------------|-------------|
| **A. Tool dyn / closure** | Si Rig expone `tool_fn`, `ToolDyn`, o registro por `ToolDefinition` + callback | **Preferida** |
| **B. Wrapper genérico con nombre runtime** | Un struct `DynamicMcpTool` que implementa `Tool` con nombre en runtime (si la API lo permite) | OK |
| **C. Loop manual sin Rig agent tools** | Implementar multi-step tú mismo: completion → parse tool_calls → MCP → re-prompt | **Fallback** si A/B fallan |

**Si usas C:** todavía cumples Fase 3 si el loop vive dentro de `RigAgentEngine` y el resto del sistema solo ve `AgentEngine`. Documenta la decisión en la sección final.

### 4.3 Contrato del bridge

Para cada `ScopedMcpTool`:

1. **Nombre expuesto al LLM:** preferir calificado estable:

   ```text
   {server_id}__{tool_name}
   ```

   Ejemplo: `fudi__list_orders`

   Razón: evita colisiones entre servers. Documenta el mapeo inverso al invocar MCP (`server_id` + `tool_name` reales de `def`).

2. **Description:** `def.description` (si vacía, usar `MCP tool {qualified}`).

3. **Parameters schema:** `def.input_schema` (JSON Schema object). Si no es object, envolver o usar `{"type":"object","properties":{}}`.

4. **call(args: Value o Args deserializados):**
   - Re-check: `scope.allows_tool(&def.server_id, &def.name)` → si false, error `"tool denied by agent scope"`.
   - Incrementar contador de tool calls (ver 3.5).
   - `client.call_tool(McpCallRequest::new(...))`.
   - Si `is_error`, devolver error o string de error al modelo (preferible: `Err` o output con marca de error para que el LLM reintente).
   - Log: `tracing::info!(server_id, tool, "scoped MCP tool invoke")` — **no** loguear args por defecto (pueden ser sensibles).

### 4.4 Código de referencia (adaptar a Rig real)

```rust
// src/adapters/rig_engine/tools.rs
use crate::adapters::rig_engine::ScopedMcpTool;
use crate::adapters::rig_engine::limits::ToolCallBudget;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Nombre seguro para el LLM (sin '/').
pub fn rig_tool_name(server_id: &str, tool_name: &str) -> String {
    format!("{server_id}__{tool_name}")
}

pub struct McpToolBridge {
    pub inner: ScopedMcpTool,
    pub budget: Arc<Mutex<ToolCallBudget>>,
}

impl McpToolBridge {
    pub async fn invoke_json(&self, args: Value) -> Result<String, String> {
        // 1) budget
        {
            let mut b = self.budget.lock().await;
            b.try_consume()?;
        }
        // 2) scope + MCP
        self.inner.invoke(args).await
    }
}
```

`ScopedMcpTool::invoke` **ya existe** en `mod.rs` y ya re-chequea scope — **reutilízalo**, no dupliques la lógica MCP.

### 4.5 Registro en el agent

En `run_with_rig`, después de `prepare`:

```text
for tool in tools {
  registrar bridge en el agent builder
}
```

Si `tools` está vacío (DenyAll), el agent corre **sin tools** (chat normal). Eso es correcto y esperado.

### 4.6 Tests de bridge (obligatorios en 3.9, esqueleto aquí)

Crear mock de `McpClient` en tests:

```rust
struct MockMcp {
    // records calls
}
```

Casos:

1. Tool permitida → `invoke` llama MCP una vez.
2. Tool no permitida (scope DenyAll o allowlist sin ella) → error, 0 calls al MCP real.
3. Nombre `server__tool` se mapea bien.

### 4.7 Checklist 3.3

- [ ] Bridge en `tools.rs`
- [ ] Re-check de scope en cada invoke (vía `ScopedMcpTool` o doble check)
- [ ] Tools denegadas no se registran **o** se registran pero fallan al invocar (preferible: **no registrar** las denegadas; `prepare` ya devuelve solo scoped)
- [ ] `prepare().discover_scoped_tools` es la única fuente de tools registradas
- [ ] Sin `unwrap` en invoke

---

## 5. Tarea 3.4 — System prompt + ContextEnricher

**Objetivo:** el preamble de Rig es el mismo system prompt enriquecido que hoy usa el chat legacy.  
**Estimación:** ~1 día  
**Prioridad:** P0

### 5.1 Qué NO hacer

- **No** reimplementes enricher dentro de Rig.
- **No** ignores `ContextEnricher`.

### 5.2 Flujo correcto

Hoy `chat_core::prepare_request` ya:

1. Resuelve agente vía `agent_registry`
2. Llama `context_enricher.build_system_prompt(&agent)`
3. Optimiza tokens
4. Inserta mensaje `role=system` al inicio
5. Resuelve model

Debes **reutilizar** ese system prompt al llamar al engine.

### 5.3 Cambios en `AgentRuntime` (`src/services/agent_runtime.rs`)

El runtime ya acepta `system_prompt: Option<String>`. Asegura:

1. Si viene `Some(prompt)` → usarlo.
2. Si viene `None` → `agent.config.system_prompt` (fallback).
3. Extraer `user_message` del último mensaje user del request HTTP (en chat_core).
4. Pasar `history` (mensajes previos sin el system inyectado, o sin el último user) en `AgentRunRequest.history`.

### 5.4 Cambios en `chat_core.rs`

En `execute_chat` (y stream análogo):

```text
1. prepare_request(...) como ahora
2. Determinar si hay agente seleccionado (project/agent headers o model=agent id)
3. Si hay agente Y state.agent_runtime / engine está presente:
     - system = primer mensaje role=system de prepared.messages (si existe)
     - user = último mensaje role=user
     - history = resto
     - llamar runtime.chat o engine.run
     - mapear AgentRunResult → ChatResponse
4. Si no: path legacy run_with_fallback
```

### 5.5 Extender `AppState`

Archivo: `src/api/handlers.rs`

Agregar campo opcional o Arc:

```rust
pub struct AppState {
    // ... campos existentes ...
    /// When set, agent-scoped chat uses the multi-step engine (Rig).
    pub agent_runtime: Option<Arc<crate::services::agent_runtime::AgentRuntime>>,
}
```

O bien:

```rust
pub agent_engine: Option<Arc<dyn AgentEngine>>,
```

**Recomendación:** guardar `Option<Arc<AgentRuntime>>` para no duplicar wiring de history/rag después.

Actualiza **todos** los sitios que construyen `AppState`:

- `src/runtime.rs` → `build_app_state` / `build_runtime`
- tests en `src/api/chat_core.rs` (`test_state`)
- cualquier otro test de integración en `tests/`

Si omites un sitio, **no compila** — búscalo con:

```bash
rg "AppState \{" -n
```

### 5.6 Wiring en `build_runtime` (`src/runtime.rs`)

Después de crear `mcp_registry` y `agent_registry`:

```rust
use crate::adapters::rig_engine::RigAgentEngine;
use crate::services::agent_runtime::AgentRuntime;
use crate::ports::engine::AgentEngine;

let engine: Arc<dyn AgentEngine> = Arc::new(RigAgentEngine::new(
    config.ollama_url.clone(), // o el campo real de Config — verificar nombre en config.rs
    mcp_registry.clone(),
    None, // rag: None en Fase 3 (Fase 4 lo conecta)
));

let agent_runtime = Arc::new(AgentRuntime {
    registry: agent_registry.clone(),
    engine,
    history: None, // Fase 5
    rag: None,
});
```

Pásalo a `AppState { agent_runtime: Some(agent_runtime), ... }`.

**Verifica nombres reales en `src/config.rs`** (`ollama_url` vs `OLLAMA_URL` mapping). Usa el campo correcto.

### 5.7 Mapeo a `ChatResponse`

```rust
ChatResponse {
    model: result.model,
    created_at: chrono::Utc::now().to_rfc3339(), // o el formato que use el legacy
    message: ChatMessage {
        role: "assistant".to_string(),
        content: result.text,
    },
    done: true,
    debug_prompt,
}
```

Mantén consistencia con el formato actual de `created_at` del provider Ollama si los clientes dependen de él.

### 5.8 Checklist 3.4

- [ ] `AppState` tiene runtime/engine
- [ ] `build_runtime` lo instancia
- [ ] `execute_chat` usa enricher + engine cuando hay agente
- [ ] Requests **sin** agente siguen en legacy
- [ ] Tests de `chat_core` compilan (añadir `agent_runtime: None` en fakes)

---

## 6. Tarea 3.5 — Límites: iterations, tool calls, timeout

**Objetivo:** el scope del agente acota el loop.  
**Estimación:** ~0.5–1 día  
**Prioridad:** P0

### 6.1 Archivo

Crear: `src/adapters/rig_engine/limits.rs`

```rust
use std::time::{Duration, Instant};

pub struct ToolCallBudget {
    pub max: u32,
    pub used: u32,
}

impl ToolCallBudget {
    pub fn new(max: u32) -> Self {
        Self { max, used: 0 }
    }

    pub fn try_consume(&mut self) -> Result<(), String> {
        if self.used >= self.max {
            return Err(format!(
                "max_tool_calls exceeded ({}/{})",
                self.used, self.max
            ));
        }
        self.used += 1;
        Ok(())
    }
}

pub struct RunDeadline {
    pub started: Instant,
    pub limit: Duration,
}

impl RunDeadline {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            started: Instant::now(),
            limit: Duration::from_secs(timeout_secs.max(1)),
        }
    }

    pub fn check(&self) -> Result<(), String> {
        if self.started.elapsed() > self.limit {
            Err(format!("agent run timeout after {}s", self.limit.as_secs()))
        } else {
            Ok(())
        }
    }

    pub fn remaining(&self) -> Duration {
        self.limit.saturating_sub(self.started.elapsed())
    }
}
```

### 6.2 Aplicación de límites

| Límite | Campo | Cómo aplicar |
|--------|-------|--------------|
| max iterations | `scope.max_iterations` | Pasar a Rig `.max_loops(n)` / equivalente; o cortar loop manual |
| max tool calls | `scope.max_tool_calls` | `ToolCallBudget` compartido por todos los bridges del run |
| timeout | `scope.timeout_secs` | `tokio::time::timeout(deadline.remaining(), run_future)` envolviendo todo el `run` |

### 6.3 Implementación recomendada del timeout

```rust
let timeout = std::time::Duration::from_secs(req.scope.timeout_secs.max(1));
let result = tokio::time::timeout(timeout, self.run_inner(req)).await;
match result {
    Ok(inner) => inner,
    Err(_) => Err(format!("agent run timed out after {}s", timeout.as_secs())),
}
```

### 6.4 Contadores en `AgentRunResult`

- `tool_calls`: valor final de `budget.used`
- `iterations`: loops reales del agent (si Rig no lo expone, aproximar 1 + tool_calls o el max alcanzado)

### 6.5 Comportamiento al exceder

- **max_tool_calls:** la tool devuelve error al modelo *o* el engine aborta el run con error claro. Preferible: error en la tool call para que el LLM pueda responder sin tool; si el modelo insiste, el siguiente call también falla.
- **timeout:** error de run (HTTP 500/runtime error mapeado por `AppError::Runtime`).
- **max_iterations:** el agent termina y devuelve el mejor texto parcial si Rig lo permite; si no, error controlado.

### 6.6 Checklist 3.5

- [ ] `limits.rs` existe y se usa
- [ ] Timeout envuelve el run completo
- [ ] Budget compartido entre tools del mismo run
- [ ] Tests unitarios de `ToolCallBudget` (sin red)

---

## 7. Tarea 3.6 — Streaming de eventos → SSE

**Objetivo:** emitir progreso al cliente SSE.  
**Estimación:** ~1.5 días  
**Prioridad:** P1 (importante, pero no bloquea un MVP de `run` no-stream)

### 7.1 Tipos existentes

- Engine: `AgentRunEvent` en `src/ports/engine.rs`
- HTTP stream actual: `ChatStreamEvent` en `src/domain/models.rs` (solo token-like: model, message delta, done)

### 7.2 Estrategia en dos niveles

#### Nivel A — MVP streaming (mínimo aceptable)

1. Implementar `AgentEngine::run_stream` que:
   - Ejecuta el run (aunque sea internamente no-stream al principio)
   - Envía un único `AgentRunEvent::Token` con el texto final
   - Envía `AgentRunEvent::Completed`

2. En `execute_chat_stream`, si se usa engine:
   - Mapear `Token` → `ChatStreamEvent` con `message.content = text`, `done = false`
   - Al completar, evento con `done = true`

#### Nivel B — Streaming real (si Rig lo soporta)

1. Usar API streaming de Rig (`stream_prompt` o similar).
2. Por cada chunk de texto → `AgentRunEvent::Token`.
3. Por cada tool call → `AgentRunEvent::ToolCall { server_id, tool_name, arguments }`.
4. Por cada tool result → `AgentRunEvent::ToolResult { ..., preview }` donde `preview` es un truncado seguro (p.ej. 200 chars), **sin** volcar PII completa.

### 7.3 Compatibilidad del wire format SSE

Hoy el handler serializa `ChatStreamEvent` a JSON en SSE. Los clientes OpenAI-compat pueden esperar otro formato en `/v1/chat/completions`.

**Reglas:**

- Para `/chat` y `/api/chat`: puedes seguir emitiendo `ChatStreamEvent` para tokens.
- Tool events: **opción 1** (simple): no romper el schema; solo stream de texto. Tool calls solo en logs/tracing.  
  **opción 2**: extender `ChatStreamEvent` con campos opcionales:

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub event_type: Option<String>, // "token" | "tool_call" | "tool_result"
```

Si extiendes el schema, mantén backward compatible (`event_type` omitido = token clásico).

### 7.4 Implementación de `run_stream` en stub actual

Hoy devuelve error. Reemplazar:

```rust
async fn run_stream(&self, req: AgentRunRequest) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
    let (tx, rx) = mpsc::channel(32);
    // spawn task that runs and sends events
    let this = /* clone needed fields */;
    tokio::spawn(async move {
        match this.run(req).await {
            Ok(result) => {
                let _ = tx.send(AgentRunEvent::Token { text: result.text.clone() }).await;
                let _ = tx.send(AgentRunEvent::Completed { result }).await;
            }
            Err(message) => {
                let _ = tx.send(AgentRunEvent::Error { message }).await;
            }
        }
    });
    Ok(rx)
}
```

**Nota:** `RigAgentEngine` debe ser `Clone` o clonar `Arc` internos para el spawn. Prefiere que los campos pesados ya sean `Arc`.

### 7.5 Checklist 3.6

- [ ] `run_stream` no es stub de error
- [ ] `execute_chat_stream` puede consumir engine stream cuando hay agente
- [ ] Clientes que solo leen `message.content` siguen funcionando
- [ ] Errores se propagan como evento Error o fin de stream con mensaje

---

## 8. Tarea 3.7 — Feature flag + fallback legacy

**Objetivo:** builds sin Rig y fallos del engine no rompen el gateway.  
**Estimación:** ~0.5 día  
**Prioridad:** P1

### 8.1 Features en Cargo (repaso)

```toml
[features]
default = ["rig-engine"]
rig-engine = ["dep:rig-core"]
```

### 8.2 Política de fallback en `execute_chat`

```text
IF agent resolved AND agent_runtime is Some:
    match runtime.chat(...).await {
        Ok(text) => return ChatResponse from text
        Err(e) => {
            tracing::warn!(error=%e, "agent engine failed; falling back to legacy chat");
            // SOLO fallback si el error es de engine/provider, no de validación de agente
            return run_with_fallback(legacy prepared request)
        }
    }
ELSE:
    run_with_fallback(...)
```

**Importante:** si el usuario pidió un agente de proyecto y el agente **no existe**, eso ya es `400 Validation` — **no** hagas fallback silencioso a chat sin contexto. El fallback es solo para fallos del **motor** (Rig/Ollama/tools), no para resolución de agente.

### 8.3 Variable de entorno opcional (nice-to-have)

```text
LLAMA_R_AGENT_ENGINE=on|off   # default on si feature compilada
```

Si implementas esto, léelo en `Config` y decide si `agent_runtime` se pone en `None`.

### 8.4 Compilación sin feature

Cuando `rig-engine` está off:

- `RigAgentEngine::run` devuelve error claro **o** ni siquiera se construye runtime.
- Preferible: en `build_runtime`,

```rust
#[cfg(feature = "rig-engine")]
let agent_runtime = Some(...);

#[cfg(not(feature = "rig-engine"))]
let agent_runtime = None;
```

Así el binario sin feature es más liviano y el path legacy es el único.

### 8.5 Checklist 3.7

- [ ] `--no-default-features` compila y sirve chat legacy
- [ ] Fallo de engine con feature ON hace fallback con log warn
- [ ] Agente inexistente sigue siendo error de validación
- [ ] Documentado en AGENTS.md

---

## 9. Tarea 3.8 — Sub-agentes (OPCIONAL / P2)

**Objetivo:** tool virtual `delegate_to_agent` con budget de profundidad.  
**Prioridad:** P2 — **hazlo solo si 3.1–3.7 y 3.9 están verdes.**

### 9.1 Diseño mínimo

1. Nueva tool built-in (no MCP): `delegate_to_agent`.
2. Args: `{ "agent_id": "...", "message": "..." }`.
3. Validar `agent.config.subagents.allowed` y `max_depth`.
4. Construir nuevo `AgentRunRequest` con `depth + 1` (necesitarás agregar `depth: u8` a `AgentRunRequest` **solo si implementas esto**).
5. Misma `AgentRuntime` / engine recursivo.
6. Si `depth >= max_depth` → error.

### 9.2 Si no implementas 3.8

Deja un comentario `// TODO(phase-3.8): sub-agent delegation` en `agent_runtime.rs` y marca la tarea como skipped en el checklist final.

---

## 10. Tarea 3.9 — Tests

**Objetivo:** cobertura confiable sin depender de Ollama en CI.  
**Estimación:** ~1 día  
**Prioridad:** P0

### 10.1 Archivos de test sugeridos

| Test | Ubicación |
|------|-----------|
| Budget límites | `src/adapters/rig_engine/limits.rs` `#[cfg(test)]` |
| Scope re-check en invoke | `src/adapters/rig_engine/mod.rs` o `tools.rs` |
| Mock engine + AgentRuntime | `src/services/agent_runtime.rs` tests |
| chat_core elige engine | `src/api/chat_core.rs` tests con `FakeEngine` |
| Integración API (opcional) | `tests/api_integration.rs` |

### 10.2 FakeEngine

```rust
struct FakeEngine {
    response: String,
    fail: bool,
}

#[async_trait]
impl AgentEngine for FakeEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        if self.fail {
            return Err("engine down".into());
        }
        Ok(AgentRunResult {
            text: format!("{}|{}", self.response, req.user_message),
            tool_calls: 0,
            iterations: 1,
            model: req.model,
        })
    }
    async fn run_stream(&self, req: AgentRunRequest) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
        // similar or Err("not used")
        let (tx, rx) = mpsc::channel(1);
        let result = self.run(req).await?;
        tx.send(AgentRunEvent::Completed { result }).await.ok();
        Ok(rx)
    }
}
```

### 10.3 Casos de prueba obligatorios

1. **`tool_budget_blocks_after_max`** — tercer call falla si max=2.
2. **`scoped_tool_denied`** — `allows_tool` false → invoke error.
3. **`agent_runtime_returns_engine_text`** — FakeEngine devuelve texto esperado.
4. **`chat_uses_engine_when_agent_present`** — con FakeEngine en AppState, `execute_chat` no usa FakeProvider (puedes hacer que FakeProvider paniquee o falle si se llama).
5. **`chat_falls_back_when_engine_fails`** — engine fail → provider responde.
6. **`project_missing_agent_still_400`** — sin cambios de comportamiento.

### 10.4 Mock MCP para multi-step (si implementaste loop manual o tools)

- Mock `list_tools` devuelve 1 tool.
- Mock `call_tool` devuelve JSON fijo.
- Idealmente un test de integración con un `AgentEngine` de prueba que invoque el bridge (sin Rig real).

### 10.5 Comando de tests

```bash
cargo test --target-dir target-tests
cargo test --target-dir target-tests --features rig-engine
cargo test --target-dir target-tests --no-default-features
```

### 10.6 Checklist 3.9

- [ ] ≥ 4 tests nuevos pasando
- [ ] No requieren Ollama en el path default de CI
- [ ] Tests existentes de chat_core siguen verdes

---

## 11. Tarea 3.10 — Documentación y cierre

### 11.1 Actualizar `ROADMAP.md`

Cambiar Fase 3 de stub a completado/parcial según realidad:

```text
| 3 | Integración Rig.rs | P0 | ✅ completado |   # o 🔄 parcial si stream/subagents faltan
```

### 11.2 Actualizar `AGENTS.md`

Agregar sección breve:

```markdown
## Agent engine (Rig)

- Feature: `rig-engine` (default ON)
- Build without engine: `cargo build --no-default-features`
- Agent chat with tools uses Rig when X-Project/X-Agent resolve an agent
- Legacy Ollama chat remains for direct model requests and fallback
```

### 11.3 No reescribir todo `ARCHITECTURE_PLAN.md`

Solo corrige si hay inconsistencia grave. El plan de arquitectura es histórico + diseño.

### 11.4 Verificación final (obligatoria)

```bash
cargo fmt
cargo check
cargo check --no-default-features
cargo check --features rig-engine
cargo test --target-dir target-tests
```

Prueba manual recomendada (si hay Ollama + un mcp mock):

```bash
# terminal 1
cargo run

# terminal 2 — chat sin agente (legacy)
curl -s http://127.0.0.1:3000/api/chat \
  -H 'Content-Type: application/json' \
  -d '{"model":"llama3.2","messages":[{"role":"user","content":"hola"}],"stream":false}'

# chat con agente (engine)
curl -s http://127.0.0.1:3000/api/chat \
  -H 'Content-Type: application/json' \
  -H 'X-Project: demo' \
  -H 'X-Agent: demo' \
  -d '{"model":"llama3.2","messages":[{"role":"user","content":"hola"}],"stream":false}'
```

---

## 12. Guía archivo por archivo (mapa de edición)

| Archivo | Crear/Editar | Qué hacer |
|---------|--------------|-----------|
| `Cargo.toml` | Editar | features + rig-core opcional |
| `src/adapters/rig_engine/mod.rs` | Editar | `run` / `run_stream` reales; exports |
| `src/adapters/rig_engine/builder.rs` | Crear | helpers client Ollama + agent build |
| `src/adapters/rig_engine/tools.rs` | Crear | bridge MCP ↔ Rig |
| `src/adapters/rig_engine/limits.rs` | Crear | budget + deadline |
| `src/ports/engine.rs` | Editar solo si hace falta | preferible no tocar contrato |
| `src/services/agent_runtime.rs` | Editar | history messages, depth opcional, errores claros |
| `src/api/handlers.rs` | Editar | campo `agent_runtime` en `AppState` |
| `src/api/chat_core.rs` | Editar | branch engine vs legacy; tests |
| `src/runtime.rs` | Editar | wiring DI del engine |
| `src/config.rs` | Editar si env flag | opcional `LLAMA_R_AGENT_ENGINE` |
| `tests/api_integration.rs` | Editar opcional | smoke con FakeEngine |
| `ROADMAP.md` | Editar | estado fase 3 |
| `AGENTS.md` | Editar | feature + comportamiento |
| `ARCHITECTURE_PLAN.md` | No tocar salvo error | — |

---

## 13. Pseudocódigo de integración en `execute_chat`

```rust
pub async fn execute_chat(
    state: &AppState,
    payload: ChatRequest,
    selection: AgentSelection<'_>,
) -> Result<ChatResponse, AppError> {
    let requested_model = selection.requested_target(&payload.model);
    let had_agent_headers = selection.project_id.is_some() || selection.agent_id.is_some();
    // También podrías detectar agente si prepare resolvió uno; usa la misma lógica que prepare_request.

    let prepared = prepare_request(state, payload, selection)?;
    let debug_prompt = /* igual que ahora */;

    // Detectar si prepare inyectó system de agente: heurística simple =
    // selection tenía project/agent O el registry resolvió agent por model.
    let use_engine = state.agent_runtime.is_some() && agent_was_resolved(...);

    if use_engine {
        if let Some(runtime) = &state.agent_runtime {
            let system = prepared.messages.iter()
                .find(|m| m.role == "system")
                .map(|m| m.content.clone());
            let user_message = prepared.messages.iter()
                .rev()
                .find(|m| m.role == "user")
                .map(|m| m.content.clone())
                .ok_or_else(|| AppError::Validation("missing user message".into()))?;

            let rt_req = RuntimeChatRequest {
                project_id: selection.project_id.map(str::to_string),
                agent_id: selection.agent_id.map(str::to_string),
                conversation_id: None,
                user_message,
                model_override: Some(prepared.model.clone()),
                system_prompt: system,
                default_model: state.default_model.clone(),
            };

            match runtime.chat(rt_req).await {
                Ok(text) => {
                    // metrics + return ChatResponse
                    return Ok(ChatResponse { /* ... text ... */ });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "engine failed; legacy fallback");
                    // fall through to legacy ONLY for runtime/provider failures
                }
            }
        }
    }

    let mut response = run_with_fallback(state, prepared, &requested_model).await?;
    response.debug_prompt = debug_prompt;
    Ok(response)
}
```

**Cuidado:** `selection` se mueve/copia — clona los `&str` a `String` antes si hace falta. Ajusta el código para el borrow checker sin hacks inseguros.

**Mejora de `AgentRuntime`:** hoy hace `registry.resolve` otra vez. Está bien (doble resolve). Alternativa: pasar `RegisteredAgent` ya resuelto — no es obligatorio.

---

## 14. Errores comunes y cómo evitarlos

| Error | Causa | Solución |
|-------|-------|----------|
| `rig` not found | Feature off o dep mal configurada | `optional = true` + feature gate |
| Conflicto reqwest | rig-core vs proyecto | unificar versiones o features |
| Tools se llaman sin scope | Olvidaste re-check | siempre `ScopedMcpTool::invoke` |
| Fallback oculta agent missing | catch-all demasiado amplio | separar Validation vs Runtime |
| Tests AppState no compilan | Falta campo nuevo | actualizar **todos** los constructores |
| Deadlock tokio Mutex en tool | lock sostenido durante await MCP | liberar budget lock antes del await MCP si no es necesario, o no await con lock de algo compartido global |
| Prompt sin contexto de proyecto | saltaste enricher | system prompt solo desde prepare_request/enricher |
| Nombre de tool con `/` rompe JSON schema | modelos confunden | usar `server__tool` |
| CI falla por Ollama | test de red en default | `#[ignore]` en smoke tests de red |

---

## 15. Qué está FUERA de alcance (no implementar)

- LanceDB / embeddings productivos (Fase 4) — RAG in-memory opcional ya en `prepare` puede quedarse.
- SQLite history real (Fase 5) — deja `history: None`.
- Rate limiting tower (Fase 6).
- CLI `mcp register` (Fase 7).
- Multi-provider OpenAI real en Rig (solo Ollama).
- Reescribir TUI para tool events (nice-to-have, no requerido).
- Cambiar el protocolo MCP server de llama-r (`/api/mcp`) salvo bugs bloqueantes.

---

## 16. Checklist maestro (marcar al terminar)

### Fundaciones
- [ ] 3.1 Feature + dep + spike Ollama
- [ ] 3.2 `run` sin tools funcional

### Tools y seguridad
- [ ] 3.3 Bridge MCP → Rig / loop con tools
- [ ] Re-check scope en invoke
- [ ] 3.5 max_iterations / max_tool_calls / timeout

### Integración HTTP
- [ ] 3.4 Enricher + AppState + chat_core wiring
- [ ] 3.6 Streaming mínimo o real
- [ ] 3.7 Fallback legacy + build sin feature

### Calidad
- [ ] 3.9 Tests (≥4 nuevos)
- [ ] 3.8 Sub-agentes o explícitamente skipped
- [ ] 3.10 Docs ROADMAP + AGENTS
- [ ] `cargo fmt && cargo check && cargo test --target-dir target-tests`

### DoD global
- [ ] D1–D12 de la sección 0.3 cumplidos

---

## 17. Orden de commits sugerido (si el usuario pide commits)

1. `feat(cargo): add optional rig-core feature flag`
2. `feat(rig): implement RigAgentEngine run without tools`
3. `feat(rig): bridge scoped MCP tools into agent loop`
4. `feat(rig): enforce tool call budget and run timeout`
5. `feat(api): wire AgentRuntime into chat with legacy fallback`
6. `feat(api): stream agent run events over SSE`
7. `test(rig): add engine, budget, and chat fallback tests`
8. `docs: mark phase 3 status in ROADMAP and AGENTS`

Cada commit debe compilar.

---

## 18. Decisiones abiertas (resolver durante implementación y anotar aquí)

Rellena al terminar:

| Decisión | Elección | Notas |
|----------|----------|-------|
| Versión exacta de `rig-core` | _TBD_ | |
| Estrategia tools A/B/C | _TBD_ | |
| Streaming nivel A o B | _TBD_ | |
| Sub-agentes 3.8 | hecho / skipped | |
| Env `LLAMA_R_AGENT_ENGINE` | sí / no | |

---

## 19. Referencias internas

- Plan general: `ARCHITECTURE_PLAN.md` (Fase 3, §6.6)
- Roadmap: `ROADMAP.md`
- Workflows dev: `AGENTS.md`
- Port engine: `src/ports/engine.rs`
- Stub actual: `src/adapters/rig_engine/mod.rs`
- Runtime orquestador: `src/services/agent_runtime.rs`
- Chat: `src/api/chat_core.rs`
- Scope: `src/domain/scope.rs`
- MCP ports: `src/ports/mcp.rs`
- MCP adapters: `src/adapters/mcp/`

---

## 20. Mensaje de arranque para el agente implementador

Copia y pega esto al iniciar la sesión de implementación:

```text
Implementa la Fase 3 de llama-r siguiendo estrictamente PHASE_3_RIG_ENGINE_PLAN.md
en la raíz del repo. Trabaja en orden 3.1 → 3.10. No implementes Fase 4–7.
No rompas el path legacy de chat. Tras cada subtarea corre cargo check.
Al final: cargo fmt, cargo check (con y sin features), cargo test --target-dir target-tests.
Marca checklists en el plan o reporta estado por tarea.
Si la API de rig-core difiere de los snippets, adáptala pero mantén el trait AgentEngine.
```

---

*Fin del plan de ejecución — Fase 3 Rig.rs Agent Engine.*
