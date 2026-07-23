# Llama-R: Personal AI Gateway

Llama-R es un gateway AI personal escrito en Rust que se sitúa frente a Ollama y expone APIs unificadas para chatear con agentes especializados. Cada agente puede tener su propio prompt de sistema, contexto de proyecto, herramientas MCP, reglas de optimización y política de acceso RAG.

## Arquitectura

```
Cliente HTTP/gRPC/MCP → Llama-R → Ollama
                            ↓
                    AgentRegistry
                    ScopeBuilder
                    ContextEnricher
                    TokenOptimizer
```

El runtime levanta tres servidores simultáneamente:
- **HTTP API** (puerto `3000` por defecto)
- **gRPC** (puerto `50051`)
- **TUI** (terminal)

## Requisitos

- Rust y Cargo
- [Ollama](https://ollama.ai) ejecutándose localmente (por defecto `http://127.0.0.1:11434`)

## Configuración rápida

```bash
cp .env.example .env
# Editar .env con OLLAMA_URL y DEFAULT_MODEL, o dejar que el setup interactivo lo haga
cargo run
```

En el primer arranque, si falta `DEFAULT_MODEL`, Llama-R abre un setup interactivo y persiste la configuración en `.env`.

### Variables de entorno

| Variable | Default | Descripción |
|---|---|---|
| `PORT` | `3000` | Puerto HTTP |
| `OLLAMA_URL` | `http://localhost:11434` | URL del provider Ollama |
| `DEFAULT_MODEL` | — | Modelo por defecto (obligatorio) |
| `LLAMA_R_DIR` | — | Directorio base override para datos |

## CLI

### Iniciar todo (HTTP + gRPC + TUI)
```bash
cargo run
# o explícitamente:
cargo run -- run
```

### Crear agente base del proyecto
```bash
cargo run -- init
```
Crea `contextos/projects/<directorio-actual>/agents/<directorio-actual>.toml`.

### Crear agente especializado
```bash
cargo run -- init-agent nutricion
```
Crea un agente con nombre específico para el proyecto actual.

### Analizar proyecto y generar contexto
Requiere el servidor corriendo. Usa `--server` si el puerto no es el `3000` por defecto (respeta la variable `PORT` del entorno):

```bash
cargo run -- analyze /ruta/al/proyecto --id mi-proyecto --agent nutricion
cargo run -- analyze /ruta --id mi-proyecto --server http://localhost:8082
```

### Reanalizar contexto existente
```bash
cargo run -- reanalyze mi-proyecto
cargo run -- reanalyze mi-proyecto --server http://localhost:8082
```

### Exportar reglas
```bash
cargo run -- export-rules mi-proyecto ./destino --format all
```

Formatos: `cursor`, `gemini`, `claude`, `all`.

## HTTP API

### Salud
```
GET /health
GET /api/health
```

### Modelos
```
GET /models
GET /api/models
```

### Chat
```
POST /chat
POST /api/chat
POST /v1/chat/completions
```

**Headers:**
- `X-Project` — selecciona el proyecto
- `X-Agent` — selecciona un agente dentro del proyecto (si se omite, usa el agente general del proyecto)
- `X-Debug` — si se envía, incluye `debug_prompt` en la respuesta

**Validación estricta:** si se envía `X-Project`, Llama-R **requiere** que exista un agente válido dentro de ese proyecto. Si no se encuentra, responde con `400 Bad Request`.

### Agentes CRUD
```
GET    /api/agents
POST   /api/agents
GET    /api/agents/:id
PUT    /api/agents/:id
DELETE /api/agents/:id
```

### Scope de un agente
```
GET /api/agents/:id/scope
```
Devuelve el `AgentScope` inmutable que se aplica por request.

### Contextos CRUD
```
GET    /api/contexts
POST   /api/contexts
GET    /api/contexts/:id
PUT    /api/contexts/:id
DELETE /api/contexts/:id
POST   /api/contexts/:id/analyze
```

### MCP
```
GET  /api/mcp
POST /api/mcp
```
Endpoint JSON-RPC para interactuar con el gateway como servidor MCP.

## gRPC API

Puerto `50051`. Proto definido en `proto/llamar.proto`.

```protobuf
service LlamaGateway {
  rpc Chat (ChatRequest) returns (ChatResponse);
  rpc ChatStream (ChatRequest) returns (stream ChatStreamEvent);
  rpc ConnectMcp (stream McpMessage) returns (stream McpMessage);
}
```

Headers: `x-project`, `x-agent` en metadatos gRPC.

## Agentes

### Formato TOML

Los agentes se definen en archivos TOML editables. Campos principales:

```toml
name = "MiAgente"
model = "llama3.2"
system_prompt = "Eres un asistente experto en..."
description = "Descripción para UIs y MCP"
context_project = "mi-proyecto"
rules = ["Usa español siempre"]
skills = ["rust-best-practices"]
mcp_sources = ["filesystem", "fudi"]
tools_override = ["*"]
rag_sources = ["docs", "agent:mi-proyecto/miagente/memory"]
max_tool_calls = 8
max_iterations = 6
timeout_secs = 90

[optimize]
enabled = true
rules = ["compress_code", "minify_json"]

[memory]
persist_history = true
summarize_every_n_turns = 10
retention_days = 90

[observability]
trace = true
log_tool_args = false
```

### Almacenamiento

- `agents/` — agentes globales
- `contextos/projects/<project_id>/agents/` — agentes por proyecto

### Scope

Cada request resuelve un `AgentScope` inmutable que define el perímetro de seguridad:
- `mcp_sources` — servidores MCP permitidos
- `tools_allow` — `DenyAll`, `AllFromSources`, o `Allowlist` específica
- `rag_sources` — fuentes RAG legibles
- `rag_write` — política de escritura (`none`, `own_memory_only`, `listed`)
- `max_tool_calls`, `max_iterations`, `timeout_secs`

## Servidores MCP

Configuración en `mcp-servers/*.toml`:

```toml
id = "fudi"
transport = "http"
url = "http://127.0.0.1:4100/mcp"
auth_env = "FUDI_MCP_TOKEN"
timeout_secs = 30
tool_namespace = "fudi"
enabled = true
```

Solo servidores `enabled = true` se cargan. Si un agente referencia un servidor desconocido en `mcp_sources`, se emite una advertencia pero no se bloquea la carga.

## Contexto de proyecto

El `ProjectContext` se genera automáticamente via LLM:
1. Escanea la estructura del proyecto
2. Analiza tecnologías, dependencias, patrones
3. Selecciona skills compatibles
4. Genera un contexto markdown enriquecido

Se almacena en `contextos/projects/<project_id>/context/context.json`.

## Skills

Skills se descubren desde múltiples directorios:
- `~/.cursor/skills/`
- `~/.claude/skills/`
- `~/.agents/skills/`
- `./skills/`
- y otros

Cada skill es un subdirectorio con `SKILL.md` (frontmatter YAML: `name`, `description`, `tags`).

El auto-sync (tras `analyze`) selecciona hasta 5 skills relevantes por agente y las escribe en `auto_skills`.

## Optimización de tokens

El `TokenOptimizer` aplica reglas configurables por agente:
- `compress_code` — elimina saltos de línea redundantes en bloques de código
- `minify_json` — compacta objetos JSON

Métricas globales de tokens ahorrados vs procesados disponibles en `/health` y en la TUI.

## Hot reload

El directorio base se observa con `notify`. Los cambios en `agents/`, `contextos/projects/*/agents/` y `mcp-servers/` recargan automáticamente el registro de agentes sin reiniciar el servidor.

## TUI

Terminal UI con tres vistas:
- **Dashboard** — estado del servidor, métricas, agentes cargados
- **Projects** — lista de proyectos, agentes, formulario de edición
- **Agent Form** — edición inline de campos del agente

Navegación con teclas: `Tab` entre paneles, `Enter` para editar.

## Desarrollo

```bash
cargo check
cargo fmt
cargo test --target-dir target-tests
```

## Instalación

Compila e instala el binario `llama-r` en el sistema, permitiendo ejecutarlo desde cualquier directorio:

```bash
cargo install --path .
```

Una vez instalado, puedes usar el comando `llama-r` directamente:

```bash
llama-r                # Inicia servidor HTTP + gRPC + TUI
llama-r init           # Crea agente base del proyecto actual
llama-r init-agent mi-agente
llama-r analyze /ruta/al/proyecto --id mi-proyecto
```

También puedes ejecutarlo sin instalarlo, desde el directorio del repositorio:

```bash
cargo run -- <comando>
```

## Almacenamiento en disco

```
<base_dir>/
├── agents/                     # Agentes globales
│   └── *.toml
├── contextos/
│   └── projects/
│       └── <project_id>/
│           ├── agents/         # Agentes del proyecto
│           │   └── *.toml
│           └── context/        # Contexto generado
│               ├── context.json
│               └── context.md
├── mcp-servers/                # Config MCP
│   └── *.toml
└── logs/
    └── llama-r.log.YYYY-MM-DD
```

`<base_dir>` se resuelve por orden: `$LLAMA_R_DIR` → directorio del ejecutable → directorio actual.
