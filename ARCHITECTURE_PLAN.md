# Llama-R Architecture Plan

> **Agent Runtime headless + gateway central**  
> Versión del plan: `0.2.0` · Fecha: 2026-07-23  
> Alcance: plataforma extensible de agentes con Rig.rs, MCP, scopes aislados y RAG segmentado.

---

## 0. Resumen ejecutivo

Llama-R evoluciona de **gateway de chat/proxy a Ollama** hacia un **Agent Runtime headless** que:

1. Carga agentes especializados desde manifiestos TOML (globales y por proyecto).
2. Aísla cada agente con un **scope de mínimo privilegio** (`mcp_sources`, `tools_override`, `rag_sources`).
3. Usa **Rig.rs** como motor de razonamiento, tools y multi-step agents.
4. Conecta apps externas (Fudi, etc.) solo vía **MCP Servers** — sin acoplar el core a cada app.
5. Centraliza **RAG** (embeddings + LanceDB) en llama-r, **segmentado por agente/proyecto**.
6. Persiste historial de conversaciones e indexa resúmenes para aprendizaje continuo.

**Principio rector:** agregar un agente nuevo o una app externa **nunca debe requerir modificar el core**. Solo TOML + MCP server externo + (opcional) fuente RAG.

---

## 1. Estado actual vs. objetivo

### 1.1 Lo que ya existe (base sólida)

| Área | Estado | Ubicación |
|------|--------|-----------|
| HTTP / OpenAI-compatible / gRPC / health | ✅ | `src/api/`, `src/runtime.rs` |
| Carga de agentes TOML + hot reload | ✅ parcial | `src/services/agent_manager.rs`, `src/core/hot_reload.rs` |
| Scopes proyecto/agente (`X-Project`, `X-Agent`) | ✅ | `src/api/chat_core.rs` |
| Contextos de proyecto + analyze | ✅ | `src/context/` |
| Skills registry | ✅ | `src/services/skill_manager.rs` |
| MCP **server** (exponer agentes como tools) | ✅ parcial | `src/api/mcp_api.rs` |
| MCP **client** (consumir servers externos) | ❌ placeholder | `src/mcp/mod.rs` |
| Rig.rs agent engine | ❌ | — |
| Tools con allowlist por agente | ❌ | — |
| RAG / embeddings / LanceDB | ❌ | — |
| Historial de conversaciones persistente | ❌ | — |
| Indexación de resúmenes en RAG | ❌ | — |

### 1.2 Gap crítico

Hoy el flujo es:

```text
Request → AgentManager.resolve → system prompt enrich → OllamaProvider.chat
```

El flujo objetivo es:

```text
Request
  → AgentRegistry.resolve(project, agent)
  → AgentScope.build(mcp_sources ∩ tools_override, rag_sources)
  → ConversationStore.append(user)
  → RagStore.retrieve(scope, query)          // opcional
  → RigAgentRuntime.run(agent, tools, rag, history)
       ├─ MCP Client (tool discovery + call, filtrado)
       ├─ Built-in tools (si allowlist)
       └─ Provider (Ollama / futuros)
  → ConversationStore.append(assistant)
  → Summarizer.maybe_index(conversation) → RagStore
  → Response (HTTP / SSE / gRPC)
```

---

## 2. Arquitectura de alto nivel

```text
┌─────────────────────────────────────────────────────────────────────────┐
│                         External Consumers                               │
│   Fudi UI · CLI · IDE plugins · Other apps · OpenAI-compatible clients  │
└───────────────────────────────┬─────────────────────────────────────────┘
                                │ HTTP / gRPC / SSE
┌───────────────────────────────▼─────────────────────────────────────────┐
│                    llama-r  (Gateway + Agent Runtime)                    │
│  ┌────────────┐  ┌──────────────┐  ┌─────────────┐  ┌────────────────┐ │
│  │ API Layer  │  │ Agent Runtime│  │ MCP Client  │  │ RAG Engine     │ │
│  │ axum/tonic │──│ (Rig-based)  │──│ discovery + │──│ embeddings +   │ │
│  │            │  │ scopes       │  │ tool call   │  │ LanceDB        │ │
│  └────────────┘  └──────┬───────┘  └──────┬──────┘  └───────┬────────┘ │
│                         │                 │                  │          │
│  ┌──────────────────────▼─────────────────▼──────────────────▼────────┐ │
│  │ Domain: AgentManifest · AgentScope · ToolRef · Conversation        │ │
│  │ Persistence: TOML agents · SQLite history · LanceDB vectors        │ │
│  └────────────────────────────────────────────────────────────────────┘ │
└───────────────────────────────┬──────────────────┬──────────────────────┘
                                │                  │
              ┌─────────────────▼───┐    ┌─────────▼──────────┐
              │  LLM Providers      │    │  External MCP      │
              │  Ollama (now)       │    │  Fudi MCP Server   │
              │  + future adapters  │    │  Other app servers │
              └─────────────────────┘    └────────────────────┘
```

### 2.1 Capas (Clean Architecture adaptada a Rust)

| Capa | Responsabilidad | Depende de |
|------|-----------------|------------|
| **domain** | Entidades puras, scopes, manifiestos, errores de dominio | nada externo |
| **ports** | Traits: `LlmProvider`, `McpTransport`, `RagStore`, `ConversationStore`, `AgentEngine` | domain |
| **adapters** | Ollama, MCP HTTP/stdio, LanceDB, SQLite, Rig | ports + crates |
| **services** | Orquestación: registry, scope builder, runtime loop | ports |
| **api / cli / tui** | Entrada/salida | services |

Regla: el **core nunca importa un crate de app externa**. Solo habla MCP y traits.

### 2.2 Modelo de aislamiento (scope)

Cada invocación de agente construye un `AgentScope` **inmutable** para ese request:

```text
AgentScope {
  agent_id, project_id,
  allowed_mcp_servers: HashSet,   // de mcp_sources
  allowed_tools: Option<HashSet>, // tools_override (None = todas las descubiertas en sources)
  rag_collections: Vec,           // de rag_sources
  max_tool_calls, timeout, ...
}
```

**Reglas de seguridad en runtime:**

1. Solo se conecta a MCP servers listados en `mcp_sources`.
2. Si `tools_override` está vacío → **deny-all** de tools (safe default).  
   Si contiene `["*"]` → permite todas las tools de `mcp_sources` (explícito).  
   Si lista nombres → allowlist estricta.
3. RAG solo consulta collections en `rag_sources`.
4. Un agente **no puede** mutar el scope de otro ni escribir en collections ajenas (salvo policy `write` explícita).

---

## 3. Estructura de carpetas recomendada

```text
llama-r/
├── ARCHITECTURE_PLAN.md          # este documento
├── AGENTS.md                     # workflows de desarrollo
├── Cargo.toml
├── .env.example
│
├── agents/                       # agentes globales (TOML)
│   └── default.toml
│
├── contextos/
│   └── projects/
│       └── <project_id>/
│           ├── agents/           # agentes del proyecto
│           │   └── <agent>.toml
│           ├── context/          # contexto generado (analyze)
│           ├── rag/              # (futuro) metadata local de sources
│           └── history/          # (opcional) export humano
│
├── data/                         # runtime data (gitignored)
│   ├── history.db                # SQLite: conversaciones
│   └── lancedb/                  # vector store segmentado
│       └── <project_or_global>/
│           └── <collection>/
│
├── mcp-servers/                  # (opcional) configs de conexión MCP
│   └── fudi.toml                 # no es código: solo endpoints/auth refs
│
├── proto/
│   └── llamar.proto
│
├── src/
│   ├── main.rs
│   ├── lib.rs
│   ├── config.rs
│   ├── error.rs
│   ├── runtime.rs                # wiring DI / AppState
│   │
│   ├── domain/                   # entidades puras
│   │   ├── mod.rs
│   │   ├── agent.rs              # AgentManifest + Agent (legacy + nuevo)
│   │   ├── scope.rs              # AgentScope, ToolRef, RagSourceRef
│   │   ├── conversation.rs       # Conversation, Message, Summary
│   │   └── models.rs             # DTOs API (ChatRequest, etc.)
│   │
│   ├── ports/                    # traits (hexagonal)
│   │   ├── mod.rs
│   │   ├── llm.rs
│   │   ├── mcp.rs
│   │   ├── rag.rs
│   │   ├── history.rs
│   │   └── engine.rs             # AgentEngine trait
│   │
│   ├── adapters/                 # implementaciones
│   │   ├── mod.rs
│   │   ├── ollama/
│   │   ├── mcp/
│   │   │   ├── mod.rs
│   │   │   ├── http.rs           # Streamable HTTP / SSE client
│   │   │   ├── stdio.rs          # subprocess MCP (fase 2)
│   │   │   └── registry.rs       # pool de conexiones por server id
│   │   ├── rag/
│   │   │   ├── mod.rs
│   │   │   ├── embeddings.rs     # Ollama embeddings u otros
│   │   │   └── lancedb.rs
│   │   ├── history/
│   │   │   └── sqlite.rs
│   │   └── rig_engine/
│   │       ├── mod.rs
│   │       ├── builder.rs        # AgentManifest → Rig agent
│   │       └── tools.rs          # bridge MCP tool → Rig tool
│   │
│   ├── services/                 # casos de uso
│   │   ├── agent_manager.rs      # (existente) evoluciona a registry
│   │   ├── agent_registry.rs     # carga dinámica + validación scope
│   │   ├── scope_builder.rs
│   │   ├── agent_runtime.rs      # orquestación chat+tools+rag
│   │   ├── conversation_service.rs
│   │   ├── skill_manager.rs
│   │   └── validation.rs
│   │
│   ├── api/                      # HTTP/gRPC (existente, se extiende)
│   ├── cli/
│   ├── tui/
│   ├── context/                  # analyze / enrich (existente)
│   ├── core/                     # paths, hot_reload
│   ├── optimizer/
│   └── providers/                # deprecar gradualmente → adapters/ollama
│
├── examples/
│   ├── agents/
│   │   ├── nutritionist.toml
│   │   └── fudi_ops.toml
│   └── mcp/
│       └── mock_server.rs
│
└── tests/
    ├── api_integration.rs
    ├── agent_scope_tests.rs
    ├── mcp_client_tests.rs
    └── rag_isolation_tests.rs
```

### 3.1 Migración desde la estructura actual

No hace falta big-bang:

1. Mantener `src/providers/` y `src/services/agent_manager.rs` como facade.
2. Introducir `domain/scope.rs`, `ports/`, `adapters/` de forma incremental.
3. Re-exportar desde `lib.rs` sin romper imports.
4. Feature flags Cargo: `rag`, `rig-engine`, `history` para builds ligeros.

---

## 4. Esquema detallado del manifiesto TOML

### 4.1 Esquema completo (v2)

```toml
# agents/example.toml  ó  contextos/projects/<id>/agents/example.toml
# Schema version: 2

[agent]
# Identidad (el id canónico es el stem del archivo; name es display)
name = "Fudi Ops Assistant"
description = "Opera pedidos y menú de Fudi vía MCP, con RAG de políticas."
version = "1.0.0"
# tags opcionales para discovery / UI
tags = ["ops", "fudi", "mcp"]

# Modelo. Vacío → DEFAULT_MODEL global.
model = "llama3.2"
# Provider hint (futuro multi-provider). Default: "ollama"
provider = "ollama"

# Prompt y plantillas
system_prompt = """
Eres el asistente operativo de Fudi.
Usa solo las tools permitidas. No inventes IDs de pedidos.
Si falta contexto, pregunta.
"""

# Variables de plantilla: {{var}} en system_prompt / rules
[agent.variables]
timezone = "America/Argentina/Buenos_Aires"
locale = "es-AR"

# Reglas inyectadas al system prompt (lista ordenada)
[[agent.rules]]
text = "Nunca exponer PII completa en logs."

[[agent.rules]]
text = "Confirmar acciones destructivas (cancelar pedido) con el usuario."

# --- Context / skills (compat con v1) ---
[context]
# Proyecto cuyo contexto generado se inyecta
project = "fudi"
# Archivos adicionales relativos a LLAMA_R_DIR
files = ["contextos/projects/fudi/context/rules.md"]
# Skills del registry compartido
skills = ["sql-safety", "json-apis"]
# Skills auto-seleccionadas por analyze (sistema)
auto_skills = []
max_context_tokens = 8192

[optimize]
enabled = true
rules = ["strip-redundant-whitespace"]

# --- SCOPE AISLADO (núcleo de la arquitectura v2) ---

[scope]
# Servidores MCP de los que este agente puede descubrir tools.
# IDs deben existir en mcp-servers/*.toml o en [mcp.servers] global.
mcp_sources = ["fudi", "filesystem-readonly"]

# Allowlist de tools (mínimo privilegio).
# - Lista vacía []  → deny-all (default seguro si se omite: ver serde)
# - ["*"]           → todas las tools de mcp_sources
# - ["tool.a", ...] → solo esas (nombre calificado: server/tool o tool)
tools_override = [
  "fudi/list_orders",
  "fudi/get_order",
  "fudi/update_order_status",
  "fudi/search_menu",
]

# Bases de conocimiento permitidas (no globales compartidas implícitamente)
rag_sources = [
  "fudi/policies",
  "fudi/menu_docs",
  "agent:fudi_ops/memory",   # memoria propia del agente
]

# Política de escritura RAG
rag_write = "own_memory_only"  # none | own_memory_only | listed

# Límites de ejecución
max_tool_calls = 12
max_iterations = 8
timeout_secs = 120
temperature = 0.2

# --- Sub-agentes (delegación controlada) ---
[subagents]
# Solo IDs de agentes del mismo project (o globales si allow_global = true)
allowed = ["fudi_support"]
allow_global = false
# Budget de profundidad para evitar recursión infinita
max_depth = 2

# --- Historial y aprendizaje ---
[memory]
persist_history = true
# Tras N turnos o al cerrar sesión, resumir e indexar en RAG
summarize_every_n_turns = 10
index_summaries = true
# Collection destino de resúmenes (debe estar en rag_sources o ser agent memory)
summary_collection = "agent:fudi_ops/memory"
retention_days = 90

# --- Observabilidad ---
[observability]
trace = true
log_tool_args = false   # nunca loguear args sensibles por defecto
log_tool_results = false
metrics_labels = { team = "ops" }
```

### 4.2 Formato plano (compat v1 + campos nuevos)

Para no romper agentes existentes, el loader acepta **ambos** layouts. El plano (actual) se mapea a v2:

```toml
name = "Nutricionista"
model = "llama3.2"
system_prompt = "Eres un nutricionista..."
context_project = "mi-proyecto"
context_files = []
rules = ["Prioriza evidencia científica."]
skills = []
auto_skills = []
variables = { locale = "es" }
max_context_tokens = 4096

# --- nuevos (opcionales, defaults seguros) ---
description = ""
mcp_sources = []
tools_override = []          # deny-all
rag_sources = []
rag_write = "none"
max_tool_calls = 8
max_iterations = 6
timeout_secs = 90
persist_history = true
index_summaries = false

[optimize]
enabled = false
rules = []
```

### 4.3 Config de servidor MCP (separado del agente)

```toml
# mcp-servers/fudi.toml
id = "fudi"
transport = "http"                 # http | stdio
url = "http://127.0.0.1:4100/mcp"
# headers / auth por referencia a env, nunca secretos en claro
auth_env = "FUDI_MCP_TOKEN"
timeout_secs = 30
# Opcional: prefix de tools al registrar (fudi/...)
tool_namespace = "fudi"
enabled = true
```

```toml
# mcp-servers/filesystem-readonly.toml
id = "filesystem-readonly"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/data/readonly"]
tool_namespace = "fs"
enabled = true
```

### 4.4 Tipos Rust del manifiesto

```rust
// domain/agent.rs (esquema v2 — campos nuevos con defaults)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    pub system_prompt: String,

    #[serde(default)]
    pub context_project: Option<String>,
    #[serde(default)]
    pub context_files: Vec<String>,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub auto_skills: Vec<String>,
    #[serde(default)]
    pub variables: HashMap<String, String>,
    #[serde(default = "default_context_budget")]
    pub max_context_tokens: usize,
    #[serde(default)]
    pub optimize: OptimizeConfig,

    // --- Scope (v2) ---
    #[serde(default)]
    pub mcp_sources: Vec<String>,
    /// Empty = deny-all. `["*"]` = all tools from mcp_sources.
    #[serde(default)]
    pub tools_override: Vec<String>,
    #[serde(default)]
    pub rag_sources: Vec<String>,
    #[serde(default)]
    pub rag_write: RagWritePolicy,
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub temperature: Option<f32>,

    // --- Subagents / memory ---
    #[serde(default)]
    pub subagents: SubagentsConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub observability: AgentObservabilityConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RagWritePolicy {
    #[default]
    None,
    OwnMemoryOnly,
    Listed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SubagentsConfig {
    #[serde(default)]
    pub allowed: Vec<String>,
    #[serde(default)]
    pub allow_global: bool,
    #[serde(default = "default_subagent_depth")]
    pub max_depth: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub persist_history: bool,
    #[serde(default = "default_summarize_every")]
    pub summarize_every_n_turns: u32,
    #[serde(default)]
    pub index_summaries: bool,
    #[serde(default)]
    pub summary_collection: Option<String>,
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            persist_history: true,
            summarize_every_n_turns: 10,
            index_summaries: false,
            summary_collection: None,
            retention_days: 90,
        }
    }
}
```

---

## 5. Plan de implementación por fases

Estimaciones en **días-persona** para un dev senior Rust a tiempo parcial-medio (~50–70% focus).  
Prioridad: **P0** bloqueante · **P1** alto valor · **P2** importante · **P3** nice-to-have.

### Fase 0 — Alineación y fundaciones de dominio  
**Duración: 2–3 días · Prioridad: P0**

| # | Tarea | Est. | Pri |
|---|-------|------|-----|
| 0.1 | Congelar este plan + ADR corto de decisiones (Rig, LanceDB, SQLite) | 0.5d | P0 |
| 0.2 | Extender `AgentConfig` con campos de scope (backward compatible) | 0.5d | P0 |
| 0.3 | Crear `domain/scope.rs` (`AgentScope`, `ToolRef`, `RagSourceRef`) | 0.5d | P0 |
| 0.4 | Introducir carpeta `ports/` con traits vacíos compilables | 0.5d | P0 |
| 0.5 | Tests unitarios de parsing TOML v1 + v2 | 0.5d | P0 |
| 0.6 | Ejemplo `examples/agents/*.toml` y `mcp-servers/*.toml.example` | 0.5d | P1 |

**Criterio de salida:** agentes actuales siguen cargando; nuevos campos se deserializan con defaults seguros.

---

### Fase 1 — Registry dinámico + Scope enforcement  
**Duración: 3–4 días · Prioridad: P0**

| # | Tarea | Est. | Pri |
|---|-------|------|-----|
| 1.1 | `AgentRegistry`: load/reload, validación de ids, métricas | 1d | P0 |
| 1.2 | `ScopeBuilder`: construir `AgentScope` desde manifiesto + MCP registry | 1d | P0 |
| 1.3 | Validar referencias: `mcp_sources` y `rag_sources` existen | 0.5d | P0 |
| 1.4 | Integrar hot-reload existente con invalidación de scopes cacheados | 0.5d | P0 |
| 1.5 | API `GET /api/agents/:id/scope` (debug, gated) | 0.5d | P2 |
| 1.6 | Tests de aislamiento (agente A no ve tools de B) | 0.5d | P0 |

**Criterio de salida:** cada request resuelve un scope inmutable y lo registra en tracing.

---

### Fase 2 — MCP Client (descubrimiento + ejecución)  
**Duración: 5–7 días · Prioridad: P0**

| # | Tarea | Est. | Pri |
|---|-------|------|-----|
| 2.1 | Trait `McpClient` + tipos `McpTool`, `McpCallResult` | 0.5d | P0 |
| 2.2 | Transport HTTP (JSON-RPC + SSE/streamable HTTP) con `reqwest` | 2d | P0 |
| 2.3 | `McpServerRegistry` (carga `mcp-servers/*.toml`, pool conexiones) | 1d | P0 |
| 2.4 | `tools/list` cache con TTL + invalidación | 0.5d | P1 |
| 2.5 | `tools/call` con timeout, cancelación, y filtrado por scope | 1d | P0 |
| 2.6 | Transport stdio (subprocess) | 1.5d | P1 |
| 2.7 | Namespacing `server/tool` y resolución de `tools_override` | 0.5d | P0 |
| 2.8 | Tests con mock MCP server | 1d | P0 |

**Criterio de salida:** un agente TOML con `mcp_sources` + `tools_override` puede listar y llamar solo sus tools.

**Nota:** el MCP **server** actual de llama-r (`/api/mcp`) se mantiene para exponer el gateway hacia afuera; el **client** es el camino inverso (apps → tools dentro del agente).

---

### Fase 3 — Integración Rig.rs (Agent Engine)  
**Duración: 6–8 días · Prioridad: P0**

| # | Tarea | Est. | Pri |
|---|-------|------|-----|
| 3.1 | Spike: `rig-core` + Ollama provider, agent mínimo | 1d | P0 |
| 3.2 | Trait `AgentEngine` + implementación `RigAgentEngine` | 1d | P0 |
| 3.3 | Bridge: MCP tools → Rig `Tool` (solo allowlist del scope) | 1.5d | P0 |
| 3.4 | Inyección de system prompt + context enricher existente | 1d | P0 |
| 3.5 | Límites: `max_iterations`, `max_tool_calls`, timeout | 0.5d | P0 |
| 3.6 | Streaming de eventos (token + tool_call + tool_result) hacia SSE | 1.5d | P1 |
| 3.7 | Fallback: si Rig no disponible / feature off → path legacy chat | 0.5d | P1 |
| 3.8 | Sub-agentes: tool `delegate_to_agent` con budget de profundidad | 1d | P2 |
| 3.9 | Tests de multi-step tool use con mock LLM | 1d | P0 |

**Criterio de salida:** `POST /api/chat` con `X-Agent` ejecuta un loop Rig con tools MCP scoped.

Feature Cargo:

```toml
[features]
default = ["rig-engine"]
rig-engine = ["dep:rig-core"]
```

---

### Fase 4 — RAG centralizado y segmentado  
**Duración: 6–8 días · Prioridad: P1**

| # | Tarea | Est. | Pri |
|---|-------|------|-----|
| 4.1 | Trait `RagStore` + `EmbeddingProvider` | 0.5d | P1 |
| 4.2 | Embeddings vía Ollama (`/api/embeddings`) | 1d | P1 |
| 4.3 | Adapter LanceDB con path `data/lancedb/<ns>/<collection>` | 2d | P1 |
| 4.4 | Namespaces: `project:<id>/<col>`, `agent:<id>/memory`, `global:<col>` | 0.5d | P1 |
| 4.5 | Enforce `rag_sources` en retrieve; `rag_write` en upsert | 1d | P1 |
| 4.6 | Ingest pipeline: archivos de `context_files` + analyze output | 1d | P1 |
| 4.7 | API admin: `POST /api/rag/ingest`, `POST /api/rag/query` (debug) | 1d | P2 |
| 4.8 | Tests de aislamiento entre collections de agentes | 1d | P1 |

**Criterio de salida:** dos agentes con `rag_sources` distintos no recuperan chunks cruzados.

---

### Fase 5 — Historial + aprendizaje continuo  
**Duración: 4–5 días · Prioridad: P1**

| # | Tarea | Est. | Pri |
|---|-------|------|-----|
| 5.1 | Schema SQLite: conversations, messages, tool_events | 1d | P1 |
| 5.2 | `ConversationStore` + wiring en runtime | 1d | P1 |
| 5.3 | Summarizer (LLM) cada N turnos / on session end | 1d | P1 |
| 5.4 | Indexar resumen en collection de memoria del agente | 0.5d | P1 |
| 5.5 | API: list/get conversation, export | 0.5d | P2 |
| 5.6 | Retención / purge por `retention_days` | 0.5d | P2 |
| 5.7 | Tests de round-trip history + summary index | 0.5d | P1 |

**Criterio de salida:** conversaciones persisten; resúmenes aparecen en retrieve del mismo agente.

---

### Fase 6 — Seguridad, observabilidad, hardening  
**Duración: 4–5 días · Prioridad: P1**

| # | Tarea | Est. | Pri |
|---|-------|------|-----|
| 6.1 | Deny-by-default tools; audit log de tool calls | 1d | P1 |
| 6.2 | Secrets solo por env / secret store; never in TOML | 0.5d | P1 |
| 6.3 | Rate limit por agent/project (tower layer) | 1d | P2 |
| 6.4 | Tracing spans: `agent.run`, `mcp.call`, `rag.query` | 1d | P1 |
| 6.5 | Métricas: tool success rate, latency p95, rag hit rate | 0.5d | P1 |
| 6.6 | Redact PII en logs (configurable) | 0.5d | P2 |
| 6.7 | Threat model doc + checklist de review de manifiestos | 0.5d | P2 |

---

### Fase 7 — Extensibilidad del ecosistema  
**Duración: 3–4 días · Prioridad: P2**

| # | Tarea | Est. | Pri |
|---|-------|------|-----|
| 7.1 | CLI: `init-agent` genera TOML v2 con scope skeleton | 0.5d | P2 |
| 7.2 | CLI: `mcp register` / validar conectividad | 0.5d | P2 |
| 7.3 | Plugin policy: documentar contrato MCP para apps (Fudi) | 0.5d | P2 |
| 7.4 | Multi-provider adapters (OpenAI-compatible) | 1.5d | P3 |
| 7.5 | Docker + volume mounts para data/ | 0.5d | P2 |
| 7.6 | Actualizar AGENTS.md / README / export-rules | 0.5d | P1 |

---

### Timeline consolidado

```text
Semana 1     Fase 0 + Fase 1 (dominio + registry + scope)
Semana 2–3   Fase 2 (MCP client)
Semana 3–4   Fase 3 (Rig engine)
Semana 5–6   Fase 4 (RAG)
Semana 6–7   Fase 5 (history + learning)
Semana 7–8   Fase 6 + 7 (hardening + ecosistema)

Total estimado: ~6–8 semanas (1 senior) · ~3–4 semanas (2 devs en paralelo tras Fase 1)
```

### Dependencias entre fases

```text
F0 ──► F1 ──► F2 ──► F3 ──► F5
              │       │
              │       └──► F6 (parcial desde F1)
              └──► F4 ──► F5 (index summaries)
F7 puede ir en paralelo desde F2 (docs/CLI)
```

---

## 6. Código inicial clave

> Los módulos siguientes están diseñados para **integrarse incrementalmente**.  
> Priorizan traits, defaults seguros y cero dependencia de apps externas en el core.

### 6.1 Domain — Scope

```rust
// src/domain/scope.rs
//! Scope aislado por agente: la frontera de seguridad del runtime.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Referencia calificada a una tool: `server_id/tool_name` o `tool_name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolRef {
    pub server_id: Option<String>,
    pub name: String,
}

impl ToolRef {
    pub fn parse(raw: &str) -> Self {
        if let Some((server, name)) = raw.split_once('/') {
            if !server.is_empty() && !name.is_empty() && server != "*" {
                return Self {
                    server_id: Some(server.to_string()),
                    name: name.to_string(),
                };
            }
        }
        Self {
            server_id: None,
            name: raw.to_string(),
        }
    }

    pub fn qualified(&self) -> String {
        match &self.server_id {
            Some(s) => format!("{s}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RagSourceRef {
    /// Ej: "fudi/policies", "agent:nutricion/memory", "global/docs"
    pub id: String,
}

/// Scope inmutable construido por request (o cacheado por agent version).
#[derive(Debug, Clone)]
pub struct AgentScope {
    pub agent_id: String,
    pub project_id: Option<String>,
    pub mcp_sources: HashSet<String>,
    /// `None` = deny-all; `Some(set)` con `"*"` especial → all from sources
    pub tools_allow: ToolsAllow,
    pub rag_sources: HashSet<String>,
    pub rag_write: crate::domain::agent::RagWritePolicy,
    pub max_tool_calls: u32,
    pub max_iterations: u32,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub enum ToolsAllow {
    /// Ninguna tool (default seguro).
    DenyAll,
    /// Todas las tools descubiertas en `mcp_sources`.
    AllFromSources,
    /// Solo las listadas (tras normalizar a ToolRef).
    Allowlist(HashSet<ToolRef>),
}

impl AgentScope {
    /// ¿Puede este scope usar la tool descubierta en `server_id`?
    pub fn allows_tool(&self, server_id: &str, tool_name: &str) -> bool {
        if !self.mcp_sources.contains(server_id) {
            return false;
        }
        match &self.tools_allow {
            ToolsAllow::DenyAll => false,
            ToolsAllow::AllFromSources => true,
            ToolsAllow::Allowlist(set) => {
                let exact = ToolRef {
                    server_id: Some(server_id.to_string()),
                    name: tool_name.to_string(),
                };
                let bare = ToolRef {
                    server_id: None,
                    name: tool_name.to_string(),
                };
                set.contains(&exact) || set.contains(&bare)
            }
        }
    }

    pub fn allows_rag_read(&self, source_id: &str) -> bool {
        self.rag_sources.contains(source_id)
    }

    pub fn allows_rag_write(&self, source_id: &str) -> bool {
        use crate::domain::agent::RagWritePolicy;
        match self.rag_write {
            RagWritePolicy::None => false,
            RagWritePolicy::OwnMemoryOnly => {
                let own = match &self.project_id {
                    Some(p) => format!("agent:{p}/{}/memory", self.agent_id),
                    None => format!("agent:{}/memory", self.agent_id),
                };
                // también forma corta agent:<id>/memory
                source_id == own
                    || source_id == format!("agent:{}/memory", self.agent_id)
                    || self.rag_sources.contains(source_id)
                        && source_id.contains("/memory")
            }
            RagWritePolicy::Listed => self.rag_sources.contains(source_id),
        }
    }
}
```

### 6.2 ScopeBuilder

```rust
// src/services/scope_builder.rs
use crate::domain::agent::{Agent, AgentConfig};
use crate::domain::scope::{AgentScope, ToolRef, ToolsAllow};
use crate::error::AppError;
use std::collections::HashSet;

pub struct ScopeBuilder;

impl ScopeBuilder {
    pub fn build(agent: &Agent) -> Result<AgentScope, AppError> {
        let cfg = &agent.config;
        let tools_allow = Self::parse_tools_override(&cfg.tools_override);

        // Validación básica: tools_override con server prefix debe estar en mcp_sources
        if let ToolsAllow::Allowlist(ref set) = tools_allow {
            for tool in set {
                if let Some(ref server) = tool.server_id {
                    if !cfg.mcp_sources.iter().any(|s| s == server) {
                        return Err(AppError::Validation(format!(
                            "tools_override references server '{}' not in mcp_sources for agent '{}'",
                            server, agent.id
                        )));
                    }
                }
            }
        }

        Ok(AgentScope {
            agent_id: agent.id.clone(),
            project_id: agent.project_id.clone(),
            mcp_sources: cfg.mcp_sources.iter().cloned().collect(),
            tools_allow,
            rag_sources: cfg.rag_sources.iter().cloned().collect(),
            rag_write: cfg.rag_write.clone(),
            max_tool_calls: cfg.max_tool_calls,
            max_iterations: cfg.max_iterations,
            timeout_secs: cfg.timeout_secs,
        })
    }

    fn parse_tools_override(list: &[String]) -> ToolsAllow {
        if list.is_empty() {
            return ToolsAllow::DenyAll;
        }
        if list.iter().any(|t| t == "*") {
            return ToolsAllow::AllFromSources;
        }
        let set = list.iter().map(|t| ToolRef::parse(t)).collect::<HashSet<_>>();
        ToolsAllow::Allowlist(set)
    }
}
```

### 6.3 Ports — MCP

```rust
// src/ports/mcp.rs
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub server_id: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct McpCallRequest {
    pub server_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct McpCallResult {
    pub content: Value,
    pub is_error: bool,
}

#[async_trait]
pub trait McpClient: Send + Sync {
    async fn list_tools(&self, server_id: &str) -> Result<Vec<McpToolDef>, String>;
    async fn call_tool(&self, req: McpCallRequest) -> Result<McpCallResult, String>;
    async fn health(&self, server_id: &str) -> Result<(), String>;
}

/// Filtra tools según scope. Usar SIEMPRE antes de exponer tools al engine.
pub fn filter_tools_for_scope(
    tools: Vec<McpToolDef>,
    scope: &crate::domain::scope::AgentScope,
) -> Vec<McpToolDef> {
    tools
        .into_iter()
        .filter(|t| scope.allows_tool(&t.server_id, &t.name))
        .collect()
}

#[async_trait]
pub trait McpServerRegistry: Send + Sync {
    async fn list_server_ids(&self) -> Vec<String>;
    async fn client_for(&self, server_id: &str) -> Result<std::sync::Arc<dyn McpClient>, String>;
    /// Descubre y filtra tools para un scope completo.
    async fn discover_scoped_tools(
        &self,
        scope: &crate::domain::scope::AgentScope,
    ) -> Result<Vec<McpToolDef>, String> {
        let mut out = Vec::new();
        for server_id in &scope.mcp_sources {
            let client = self.client_for(server_id).await?;
            let tools = client.list_tools(server_id).await?;
            out.extend(filter_tools_for_scope(tools, scope));
        }
        Ok(out)
    }
}
```

### 6.4 Adapter — MCP HTTP Client básico

```rust
// src/adapters/mcp/http.rs
//! Cliente MCP sobre HTTP JSON-RPC (compatible con servers streamable-http / legacy POST).

use crate::ports::mcp::{McpCallRequest, McpCallResult, McpClient, McpToolDef};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct HttpMcpClient {
    http: Client,
    server_id: String,
    base_url: String,
    auth_token: Option<String>,
    timeout: Duration,
}

impl HttpMcpClient {
    pub fn new(server_id: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            server_id: server_id.into(),
            base_url: base_url.into(),
            auth_token: None,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_auth(mut self, token: Option<String>) -> Self {
        self.auth_token = token;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": uuid_v4_lite(),
            "method": method,
            "params": params,
        });

        let mut req = self
            .http
            .post(&self.base_url)
            .timeout(self.timeout)
            .header("Content-Type", "application/json")
            .json(&body);

        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }

        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let val: Value = resp.json().await.map_err(|e| e.to_string())?;

        if !status.is_success() {
            return Err(format!("MCP HTTP {status}: {val}"));
        }
        if let Some(err) = val.get("error") {
            return Err(format!("MCP RPC error: {err}"));
        }
        val.get("result")
            .cloned()
            .ok_or_else(|| "MCP response missing result".into())
    }
}

#[async_trait]
impl McpClient for HttpMcpClient {
    async fn list_tools(&self, server_id: &str) -> Result<Vec<McpToolDef>, String> {
        if server_id != self.server_id {
            return Err(format!(
                "client bound to '{}', requested '{}'",
                self.server_id, server_id
            ));
        }
        let result = self.rpc("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .ok_or("invalid tools/list result")?;

        Ok(tools
            .iter()
            .filter_map(|t| {
                Some(McpToolDef {
                    server_id: self.server_id.clone(),
                    name: t.get("name")?.as_str()?.to_string(),
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                    input_schema: t.get("inputSchema").cloned().unwrap_or(json!({})),
                })
            })
            .collect())
    }

    async fn call_tool(&self, req: McpCallRequest) -> Result<McpCallResult, String> {
        if req.server_id != self.server_id {
            return Err("server_id mismatch".into());
        }
        let result = self
            .rpc(
                "tools/call",
                json!({
                    "name": req.tool_name,
                    "arguments": req.arguments,
                }),
            )
            .await?;

        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(McpCallResult {
            content: result.get("content").cloned().unwrap_or(result),
            is_error,
        })
    }

    async fn health(&self, server_id: &str) -> Result<(), String> {
        // initialize es el handshake mínimo MCP
        let _ = self
            .rpc(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "llama-r", "version": "0.1.0" }
                }),
            )
            .await?;
        if server_id != self.server_id {
            return Err("server_id mismatch".into());
        }
        Ok(())
    }
}

/// UUID-ish sin dependencia extra (suficiente para id JSON-RPC).
fn uuid_v4_lite() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("llama-r-{t}")
}
```

### 6.5 Agent Registry (carga dinámica mejorada)

```rust
// src/services/agent_registry.rs
//! Carga y registro dinámico de agentes TOML con validación de scope.

use crate::domain::agent::{Agent, AgentConfig};
use crate::domain::scope::AgentScope;
use crate::error::AppError;
use crate::services::scope_builder::ScopeBuilder;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

#[derive(Debug, Clone)]
pub struct RegisteredAgent {
    pub agent: Agent,
    pub scope: AgentScope,
    pub path: PathBuf,
    pub version_hash: u64,
}

pub struct AgentRegistry {
    agents: RwLock<HashMap<String, RegisteredAgent>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
        }
    }

    pub fn reload_all(&self) -> Result<usize, AppError> {
        let mut loaded = HashMap::new();

        self.load_dir(&crate::core::paths::get_agents_dir(), None, &mut loaded)?;

        let projects = crate::core::paths::get_contexts_dir();
        if projects.exists() {
            for entry in fs::read_dir(&projects)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let project_id = entry.file_name().to_string_lossy().to_string();
                let agents_dir = entry.path().join("agents");
                self.load_dir(&agents_dir, Some(&project_id), &mut loaded)?;
            }
        }

        let count = loaded.len();
        let mut guard = self
            .agents
            .write()
            .map_err(|_| AppError::Runtime("AgentRegistry lock poisoned".into()))?;
        *guard = loaded;
        tracing::info!(agent_count = count, "AgentRegistry reloaded");
        Ok(count)
    }

    fn load_dir(
        &self,
        dir: &Path,
        project_id: Option<&str>,
        out: &mut HashMap<String, RegisteredAgent>,
    ) -> Result<(), AppError> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            match self.load_one(&path, project_id) {
                Ok(reg) => {
                    let key = registry_key(project_id, &reg.agent.id);
                    out.insert(key, reg);
                }
                Err(err) => {
                    tracing::error!(path = %path.display(), error = %err, "skip agent");
                }
            }
        }
        Ok(())
    }

    fn load_one(&self, path: &Path, project_id: Option<&str>) -> Result<RegisteredAgent, AppError> {
        let content = fs::read_to_string(path)?;
        let config: AgentConfig = toml::from_str(&content)?;
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| AppError::Validation("invalid agent filename".into()))?
            .to_string();

        let agent = Agent {
            id: id.clone(),
            project_id: project_id.map(str::to_string),
            config,
        };
        let scope = ScopeBuilder::build(&agent)?;
        let version_hash = seahash_lite(content.as_bytes());

        Ok(RegisteredAgent {
            agent,
            scope,
            path: path.to_path_buf(),
            version_hash,
        })
    }

    pub fn resolve(&self, project_id: Option<&str>, agent_id: Option<&str>) -> Option<RegisteredAgent> {
        let key = match (project_id, agent_id) {
            (Some(p), Some(a)) => registry_key(Some(p), a),
            (Some(p), None) => registry_key(Some(p), p),
            (None, Some(a)) => registry_key(None, a),
            (None, None) => return None,
        };
        self.agents
            .read()
            .ok()
            .and_then(|g| g.get(&key).cloned())
    }
}

fn registry_key(project_id: Option<&str>, agent_id: &str) -> String {
    match project_id {
        Some(p) => format!("{p}::{agent_id}"),
        None => agent_id.to_string(),
    }
}

fn seahash_lite(bytes: &[u8]) -> u64 {
    // FNV-1a 64 — sin dependencia extra
    let mut hash = 0xcbf29ce484222325u64;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
```

### 6.6 Integración Rig (builder con scope limitado)

```rust
// src/adapters/rig_engine/builder.rs
//! Construye un agente Rig a partir de AgentManifest + tools ya filtradas por scope.
//!
//! NOTA: la API exacta de rig-core puede variar entre versiones.
//! Este módulo encapsula el acoplamiento: el resto del código solo usa AgentEngine.

use crate::domain::agent::Agent;
use crate::domain::scope::AgentScope;
use crate::ports::engine::{AgentEngine, AgentRunRequest, AgentRunEvent, AgentRunResult};
use crate::ports::mcp::{McpCallRequest, McpClient, McpToolDef};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// Tool bridge: cada tool MCP permitida se expone al loop del agent engine.
pub struct ScopedMcpTool {
    pub def: McpToolDef,
    pub client: Arc<dyn McpClient>,
    pub scope: AgentScope,
}

impl ScopedMcpTool {
    pub async fn invoke(&self, args: Value) -> Result<String, String> {
        // Defense in depth: re-check scope en cada call
        if !self.scope.allows_tool(&self.def.server_id, &self.def.name) {
            return Err(format!(
                "tool '{} / {}' denied by agent scope",
                self.def.server_id, self.def.name
            ));
        }

        let result = self
            .client
            .call_tool(McpCallRequest {
                server_id: self.def.server_id.clone(),
                tool_name: self.def.name.clone(),
                arguments: args,
            })
            .await?;

        if result.is_error {
            return Err(result.content.to_string());
        }
        Ok(result.content.to_string())
    }
}

/// Implementación del engine. Internamente usará rig-core cuando el feature esté activo.
pub struct RigAgentEngine {
    // provider_url, default_model, etc. se inyectan en el wiring
    pub ollama_url: String,
    pub mcp_registry: Arc<dyn crate::ports::mcp::McpServerRegistry>,
    pub rag: Option<Arc<dyn crate::ports::rag::RagStore>>,
}

#[async_trait]
impl AgentEngine for RigAgentEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        let scope = &req.scope;
        let tools = self.mcp_registry.discover_scoped_tools(scope).await?;

        // 1) RAG retrieve (solo sources del scope)
        let mut rag_context = String::new();
        if let Some(rag) = &self.rag {
            if !scope.rag_sources.is_empty() {
                let hits = rag
                    .query_scoped(scope, &req.user_message, 6)
                    .await
                    .unwrap_or_default();
                if !hits.is_empty() {
                    rag_context.push_str("\n\n## Retrieved knowledge\n");
                    for (i, h) in hits.iter().enumerate() {
                        rag_context.push_str(&format!("\n### Chunk {}\n{}\n", i + 1, h.text));
                    }
                }
            }
        }

        // 2) System prompt = agent prompt + rules + rag
        let system = format!("{}{}", req.system_prompt, rag_context);

        // 3) Aquí se construye el agente Rig:
        //
        // #[cfg(feature = "rig-engine")]
        // {
        //   use rig::providers::ollama;
        //   use rig::completion::Prompt;
        //   let client = ollama::Client::new(&self.ollama_url);
        //   let mut agent = client.agent(&req.model)
        //       .preamble(&system)
        //       .max_loops(scope.max_iterations as usize);
        //   for t in tools {
        //       let client = self.mcp_registry.client_for(&t.server_id).await?;
        //       agent = agent.tool(McpRigToolAdapter::new(t, client, scope.clone()));
        //   }
        //   let response = agent.prompt(&req.user_message).await?;
        //   return Ok(AgentRunResult { text: response, ... });
        // }

        // Stub de integración (compila sin rig): documenta el contrato.
        let _ = (system, tools);
        Err(
            "RigAgentEngine: enable feature `rig-engine` and complete rig-core wiring (see ARCHITECTURE_PLAN §6.6)"
                .into(),
        )
    }

    async fn run_stream(
        &self,
        _req: AgentRunRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<AgentRunEvent>, String> {
        Err("streaming not implemented yet".into())
    }
}

// --- Contratos del port engine ---
// src/ports/engine.rs (referencia)
/*
#[async_trait]
pub trait AgentEngine: Send + Sync {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String>;
    async fn run_stream(&self, req: AgentRunRequest)
        -> Result<mpsc::Receiver<AgentRunEvent>, String>;
}

pub struct AgentRunRequest {
    pub agent: Agent,
    pub scope: AgentScope,
    pub model: String,
    pub system_prompt: String,
    pub user_message: String,
    pub history: Vec<ChatMessage>,
    pub conversation_id: Option<String>,
}
*/
```

### 6.7 Módulo RAG por agente

```rust
// src/ports/rag.rs
use crate::domain::scope::AgentScope;
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct RagChunk {
    pub id: String,
    pub source_id: String,
    pub text: String,
    pub score: f32,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct RagUpsert {
    pub source_id: String,
    pub id: String,
    pub text: String,
    pub metadata: serde_json::Value,
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
    fn dimensions(&self) -> usize;
}

#[async_trait]
pub trait RagStore: Send + Sync {
    async fn query_scoped(
        &self,
        scope: &AgentScope,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<RagChunk>, String>;

    async fn upsert_scoped(
        &self,
        scope: &AgentScope,
        docs: Vec<RagUpsert>,
    ) -> Result<usize, String>;

    async fn delete_collection(&self, source_id: &str) -> Result<(), String>;
}
```

```rust
// src/adapters/rag/lancedb_store.rs
//! RAG segmentado: un directorio LanceDB por source_id.
//! Path: {base}/data/lancedb/<encoded_source_id>/

use crate::domain::scope::AgentScope;
use crate::ports::rag::{EmbeddingProvider, RagChunk, RagStore, RagUpsert};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct LanceRagStore {
    base_dir: PathBuf,
    embeddings: Arc<dyn EmbeddingProvider>,
}

impl LanceRagStore {
    pub fn new(base_dir: impl Into<PathBuf>, embeddings: Arc<dyn EmbeddingProvider>) -> Self {
        Self {
            base_dir: base_dir.into(),
            embeddings,
        }
    }

    fn collection_path(&self, source_id: &str) -> PathBuf {
        // Encode path-safe: "fudi/policies" → "fudi__policies"
        let safe = source_id.replace(':', "_").replace('/', "__");
        self.base_dir.join(safe)
    }

    fn assert_readable(scope: &AgentScope, source_id: &str) -> Result<(), String> {
        if scope.allows_rag_read(source_id) {
            Ok(())
        } else {
            Err(format!(
                "RAG read denied for source '{source_id}' on agent '{}'",
                scope.agent_id
            ))
        }
    }

    fn assert_writable(scope: &AgentScope, source_id: &str) -> Result<(), String> {
        if scope.allows_rag_write(source_id) {
            Ok(())
        } else {
            Err(format!(
                "RAG write denied for source '{source_id}' on agent '{}'",
                scope.agent_id
            ))
        }
    }
}

#[async_trait]
impl RagStore for LanceRagStore {
    async fn query_scoped(
        &self,
        scope: &AgentScope,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<RagChunk>, String> {
        if scope.rag_sources.is_empty() {
            return Ok(vec![]);
        }

        let vectors = self.embeddings.embed(&[query.to_string()]).await?;
        let q = vectors
            .into_iter()
            .next()
            .ok_or("empty embedding for query")?;

        let mut all = Vec::new();
        for source_id in &scope.rag_sources {
            Self::assert_readable(scope, source_id)?;
            let path = self.collection_path(source_id);
            if !path.exists() {
                continue;
            }
            // PSEUDOCÓDIGO LanceDB:
            // let db = lancedb::connect(path).execute().await?;
            // let tbl = db.open_table("chunks").execute().await?;
            // let rows = tbl.vector_search(&q).limit(top_k).execute().await?;
            // map rows → RagChunk { source_id, ... }
            let _ = (&path, &q, top_k);
            tracing::debug!(%source_id, "RAG query scoped (lancedb wiring pending)");
        }

        // Ordenar por score y truncar
        all.sort_by(|a: &RagChunk, b: &RagChunk| {
            b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)
        });
        all.truncate(top_k);
        Ok(all)
    }

    async fn upsert_scoped(
        &self,
        scope: &AgentScope,
        docs: Vec<RagUpsert>,
    ) -> Result<usize, String> {
        let mut n = 0;
        for doc in docs {
            Self::assert_writable(scope, &doc.source_id)?;
            let _path = self.collection_path(&doc.source_id);
            let _emb = self.embeddings.embed(&[doc.text.clone()]).await?;
            // lancedb upsert...
            n += 1;
        }
        Ok(n)
    }

    async fn delete_collection(&self, source_id: &str) -> Result<(), String> {
        let path = self.collection_path(source_id);
        if path.exists() {
            std::fs::remove_dir_all(path).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// Embeddings vía Ollama (reutiliza el provider local).
pub struct OllamaEmbeddings {
    pub base_url: String,
    pub model: String,
    pub dimensions: usize,
    http: reqwest::Client,
}

#[async_trait]
impl EmbeddingProvider for OllamaEmbeddings {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let resp = self
                .http
                .post(format!("{}/api/embeddings", self.base_url.trim_end_matches('/')))
                .json(&serde_json::json!({ "model": self.model, "prompt": text }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
            let emb = val
                .get("embedding")
                .and_then(|e| e.as_array())
                .ok_or("missing embedding field")?
                .iter()
                .filter_map(|v| v.as_f64().map(|f| f as f32))
                .collect::<Vec<_>>();
            out.push(emb);
        }
        Ok(out)
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}
```

### 6.8 Orquestación del runtime

```rust
// src/services/agent_runtime.rs
//! Caso de uso principal: ejecutar un turno de agente con scope, tools, rag e history.

use crate::domain::scope::AgentScope;
use crate::error::AppError;
use crate::ports::engine::{AgentEngine, AgentRunRequest};
use crate::ports::history::ConversationStore;
use crate::ports::rag::RagStore;
use crate::services::agent_registry::AgentRegistry;
use std::sync::Arc;

pub struct AgentRuntime {
    pub registry: Arc<AgentRegistry>,
    pub engine: Arc<dyn AgentEngine>,
    pub history: Option<Arc<dyn ConversationStore>>,
    pub rag: Option<Arc<dyn RagStore>>,
    // context_enricher existente se inyecta para system prompt
}

pub struct RuntimeChatRequest {
    pub project_id: Option<String>,
    pub agent_id: Option<String>,
    pub conversation_id: Option<String>,
    pub user_message: String,
    pub model_override: Option<String>,
}

impl AgentRuntime {
    pub async fn chat(&self, req: RuntimeChatRequest) -> Result<String, AppError> {
        let registered = self
            .registry
            .resolve(req.project_id.as_deref(), req.agent_id.as_deref())
            .ok_or_else(|| AppError::NotFound("agent not found".into()))?;

        let scope: AgentScope = registered.scope.clone();
        let agent = registered.agent.clone();

        let span = tracing::info_span!(
            "agent.run",
            agent_id = %agent.qualified_id(),
            mcp_sources = scope.mcp_sources.len(),
            rag_sources = scope.rag_sources.len(),
        );
        let _guard = span.enter();

        // Persist user turn
        if let Some(history) = &self.history {
            if agent.config.memory.persist_history {
                history
                    .append_user(req.conversation_id.as_deref(), &agent, &req.user_message)
                    .await
                    .map_err(|e| AppError::Runtime(e))?;
            }
        }

        // system_prompt: reutilizar ContextEnricher actual en el wiring real
        let system_prompt = agent.config.system_prompt.clone();
        let model = req
            .model_override
            .unwrap_or_else(|| {
                if agent.config.model.is_empty() {
                    String::new() // runtime rellena DEFAULT_MODEL
                } else {
                    agent.config.model.clone()
                }
            });

        let run_req = AgentRunRequest {
            agent: agent.clone(),
            scope: scope.clone(),
            model,
            system_prompt,
            user_message: req.user_message,
            history: vec![],
            conversation_id: req.conversation_id.clone(),
        };

        let result = self
            .engine
            .run(run_req)
            .await
            .map_err(AppError::Runtime)?;

        if let Some(history) = &self.history {
            if agent.config.memory.persist_history {
                let _ = history
                    .append_assistant(req.conversation_id.as_deref(), &agent, &result.text)
                    .await;
                if agent.config.memory.index_summaries {
                    let _ = history.maybe_summarize_and_index(&agent, &scope, self.rag.clone()).await;
                }
            }
        }

        Ok(result.text)
    }
}
```

---

## 7. Seguridad

### 7.1 Principios

| Principio | Aplicación en llama-r |
|-----------|------------------------|
| **Least privilege** | `tools_override` deny-all por default; `mcp_sources` explícitos |
| **Defense in depth** | Filtro al descubrir tools **y** al invocar (`ScopedMcpTool`) |
| **No secrets in TOML** | Solo `auth_env`; valores en entorno / secret manager |
| **Namespace isolation** | RAG paths por `source_id`; keys de agente `project::id` |
| **Fail closed** | Referencias inválidas → error de validación al load, no ignore silencioso en prod |
| **Auditability** | Span + audit log de cada `tools/call` (sin args si `log_tool_args=false`) |

### 7.2 Controles concretos

1. **Validación al cargar manifiesto**
   - IDs de agente/proyecto: charset restringido (`[a-zA-Z0-9_-]`).
   - `mcp_sources` ⊆ servers registrados.
   - `tools_override` con prefijo de server ⊆ `mcp_sources`.
   - `rag_sources` con formato válido.

2. **Runtime**
   - Timeout por tool y por agent run.
   - Cap de `max_tool_calls` / `max_iterations`.
   - Cancelación con `tokio::time::timeout` + drop de futures.
   - Sub-agentes: allowlist + `max_depth`.

3. **Red**
   - MCP servers: preferir localhost / private network en default.
   - Allowlist de hosts MCP en config global (opcional `MCP_HOST_ALLOWLIST`).

4. **Datos**
   - History DB con permisos de archivo 0600.
   - Redacción de PII en logs.
   - `rag_write` impide contaminación de knowledge bases compartidas.

5. **Supply chain**
   - `cargo audit` en CI.
   - Pin de versiones de `rig-core`, `lancedb`, `rmcp` cuando se adopten.

### 7.3 Threat model (resumen)

| Amenaza | Mitigación |
|---------|------------|
| Agente A llama tools de app B | `mcp_sources` + allowlist |
| Prompt injection → tool abuse | allowlist estrecha + confirmación en rules + max calls |
| Data exfil vía RAG | segmentación de collections + write policy |
| TOML malicioso en hot-reload | validación + no ejecución de código en TOML |
| SSRF vía MCP URL | host allowlist + no redirects abiertos |
| Secret leak en logs | `log_tool_args=false`, redaction layer |

---

## 8. Extensibilidad

### 8.1 Agregar un agente nuevo (sin tocar core)

```bash
cargo run -- init-agent inventario
# editar contextos/projects/<proj>/agents/inventario.toml
# definir mcp_sources, tools_override, rag_sources
# hot-reload lo registra
```

### 8.2 Agregar una app externa (Fudi, etc.)

1. La app expone un **MCP Server** (`tools/list`, `tools/call`).
2. Se añade `mcp-servers/fudi.toml` (URL + auth_env).
3. Los agentes que deban usarla listan `mcp_sources = ["fudi"]` y su allowlist.
4. **Cero cambios en Rust del core.**

### 8.3 Agregar un provider LLM

1. Implementar `ports::llm::LlmProvider` (o el trait Rig correspondiente).
2. Registrar en el DI de `runtime.rs`.
3. El manifiesto usa `provider = "..."`.

### 8.4 Agregar una fuente RAG

1. Ingest a `source_id` nuevo vía API/CLI.
2. Referenciar en `rag_sources` del agente.
3. Sin migraciones globales ni índices compartidos obligatorios.

### 8.5 Puntos de extensión formales

```text
ports/llm.rs          → nuevos providers
ports/mcp.rs          → nuevos transports (WebSocket, Unix socket)
ports/rag.rs          → Qdrant/Chroma además de LanceDB
ports/engine.rs       → motor alternativo a Rig
ports/history.rs      → Postgres en vez de SQLite
```

---

## 9. Observabilidad

### 9.1 Tracing (OpenTelemetry-ready)

Spans obligatorios:

| Span | Atributos clave |
|------|-----------------|
| `http.request` | route, project, agent |
| `agent.run` | agent_id, model, iterations |
| `agent.tool_call` | server, tool, ok, latency_ms |
| `rag.query` | sources, top_k, hits |
| `rag.upsert` | source, n |
| `mcp.rpc` | server, method, status |
| `history.append` | conversation_id |

Usar `tracing` + `tracing-subscriber` (ya en el proyecto). Capa JSON en prod:

```rust
tracing_subscriber::fmt()
    .json()
    .with_current_span(true)
    .with_span_list(true)
    .init();
```

### 9.2 Métricas

| Métrica | Tipo | Labels |
|---------|------|--------|
| `llamar_agent_runs_total` | counter | agent, status |
| `llamar_tool_calls_total` | counter | agent, server, tool, status |
| `llamar_tool_latency_ms` | histogram | server, tool |
| `llamar_rag_hits` | histogram | source |
| `llamar_tokens_in/out` | counter | agent, model |
| `llamar_mcp_errors_total` | counter | server |

Exposición futura: `GET /metrics` (Prometheus) — Fase 6.

### 9.3 Audit log

Canal separado (archivo o tabla SQLite `audit_events`) para:

- carga/reload de agentes
- tool calls (sin payloads sensibles)
- cambios de scope
- ingest RAG

### 9.4 TUI

Extender dashboard existente:

- agentes cargados + version_hash
- MCP servers health
- últimas tool calls
- rag collections size

---

## 10. Decisiones de diseño (ADR light)

| Decisión | Elección | Alternativas | Razón |
|----------|----------|--------------|-------|
| Agent engine | **Rig.rs** | LangChain-like propio, AutoAgents | Ecosistema Rust nativo, tools first-class |
| Tool bus externo | **MCP** | plugins dylib, gRPC ad-hoc | Estándar abierto; apps no tocan core |
| Vector DB | **LanceDB** | Qdrant, sqlite-vss | Embedded, sin ops extra, buen fit local-first |
| History | **SQLite** | JSONL, Postgres | Simple, portable, suficiente personal/self-host |
| Default tools policy | **Deny-all** | Allow-all | Seguridad por defecto |
| Config agentes | **TOML files** | solo API/DB | Editable, git-friendly, hot-reload |
| Multi-tenancy | project + agent keys | org/tenant full | Alineado al producto actual |

---

## 11. Compatibilidad y migración

1. **v1 TOML** sigue válido; campos nuevos tienen default.
2. Path de chat legacy (`OllamaProvider` directo) se mantiene detrás de:
   - feature off de `rig-engine`, o
   - agente sin `mcp_sources` y sin flag `use_agent_engine` → short-circuit al provider (rápido, sin tools).
3. `AgentManager` puede delegar internamente a `AgentRegistry` sin romper `AppState`.
4. Tests de integración actuales deben seguir verdes en cada fase.

### Política de feature flags

```toml
[features]
default = []
rig-engine = ["dep:rig-core"]
rag = ["dep:lancedb"]          # nombre crate a confirmar en spike
history = ["dep:sqlx"]         # o rusqlite
full = ["rig-engine", "rag", "history"]
```

Recomendación: empezar `default = []` para no romper CI; activar `full` en desarrollo de la plataforma.

---

## 12. Criterios de aceptación globales (Definition of Done)

- [ ] Crear agente nuevo = solo TOML (+ opcional mcp-server.toml).
- [ ] Conectar Fudi = solo MCP server + referencias en manifiestos.
- [ ] Agente sin `tools_override` **no** puede invocar tools.
- [ ] Agente A no lee RAG de agente B.
- [ ] Historial se guarda y resúmenes aparecen en retrieve del mismo agente.
- [ ] Hot-reload actualiza scope sin restart.
- [ ] `cargo test --target-dir target-tests` verde.
- [ ] Documentación AGENTS.md / README alineada.
- [ ] Spans de tracing en agent.run y mcp.call.

---

## 13. Primer sprint recomendado (acción inmediata)

**Objetivo del Sprint 1 (1 semana):** cerrar Fase 0 + Fase 1 y dejar MCP client en stub testeable.

1. Extender `AgentConfig` con campos de scope (serde defaults).
2. Añadir `domain/scope.rs` + tests de `allows_tool` / `allows_rag_*`.
3. Añadir `services/scope_builder.rs` + validación.
4. Evolucionar load path para construir scope en memoria.
5. Crear `ports/mcp.rs` + `adapters/mcp/http.rs` (list/call).
6. Ejemplo de manifiesto v2 en `examples/agents/`.
7. Actualizar este plan con aprendizajes del spike Rig (Fase 3.1).

---

## 14. Referencias internas

- Runtime actual: `src/runtime.rs`, `src/api/chat_core.rs`
- Agentes: `src/domain/agent.rs`, `src/services/agent_manager.rs`
- MCP server (gateway): `src/api/mcp_api.rs`
- Paths: `src/core/paths.rs`
- Workflows: `AGENTS.md`
- Roadmap legado: `ROADMAP.md` (este plan lo supersede para la plataforma de agentes)

---

## 15. Apéndice — Diagrama de secuencia (request con tools)

```text
Client          API           AgentRuntime      Scope      MCP Registry     Rig Engine      RAG
  │              │                 │              │              │               │           │
  │ POST /chat   │                 │              │              │               │           │
  │ X-Agent: ops │                 │              │              │               │           │
  │─────────────►│                 │              │              │               │           │
  │              │ resolve+run     │              │              │               │           │
  │              │────────────────►│              │              │               │           │
  │              │                 │ build/get    │              │               │           │
  │              │                 │─────────────►│              │               │           │
  │              │                 │◄─────────────│              │               │           │
  │              │                 │ discover_scoped_tools       │               │           │
  │              │                 │─────────────────────────────►               │           │
  │              │                 │ tools[]                     │               │           │
  │              │                 │◄─────────────────────────────               │           │
  │              │                 │ query_scoped                                │           │
  │              │                 │─────────────────────────────────────────────────────────►│
  │              │                 │ chunks[]                                    │           │
  │              │                 │◄─────────────────────────────────────────────────────────│
  │              │                 │ run(agent, tools, rag, history)             │           │
  │              │                 │────────────────────────────────────────────►│           │
  │              │                 │              │     tools/call (loop)        │           │
  │              │                 │              │◄────────────────────────────│           │
  │              │                 │              │─────────────────────────────►│           │
  │              │                 │ final text                                  │           │
  │              │                 │◄────────────────────────────────────────────│           │
  │              │                 │ persist + maybe summarize                   │           │
  │              │ response        │              │              │               │           │
  │◄─────────────│◄────────────────│              │              │               │           │
```

---

*Fin del plan. Próximo paso operativo: ejecutar Sprint 1 (§13) sobre la base de código actual sin romper el path de chat legacy.*
