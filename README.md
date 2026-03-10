# Llama-R: High-Performance Personal AI Gateway

Llama-R es un gateway AI ligero y ultrarrápido escrito en Rust. Actúa como un proxy unificado y personal para Ollama, permitiendo gestionar múltiples agentes especializados con sus propios prompts de sistema, contextos y reglas de optimización de tokens.

Diseñado para uso personal y proyectos de desarrollo, prioriza la latencia baja mediante streaming, hot reload y una superficie unificada para HTTP, OpenAI-compatible, gRPC y MCP.

## Características principales

- Low latency con streaming en tiempo real.
- Gestor de agentes por archivos TOML editables.
- Contextos de proyecto persistidos y reanalizables.
- Hot reload para agentes y contextos locales.
- Compatibilidad OpenAI vía `/v1/chat/completions`.
- Endpoints de salud en `/health` y `/api/health`.
- TUI nativa para monitoreo local.

## Requisitos

1. Rust y Cargo.
2. Ollama ejecutándose localmente, normalmente en `http://127.0.0.1:11434`.

## Configuración rápida

```powershell
Copy-Item .env.example .env
cargo run
```

En el primer arranque, si falta `DEFAULT_MODEL` o el provider no responde, Llama-R abre un setup interactivo y persiste la configuración en `.env`.

## CLI actual

### Iniciar servidor + TUI
```powershell
cargo run
```

### Crear el agente base del proyecto
```powershell
cargo run -- init
```

Esto crea `contextos/projects/<nombre-del-directorio>/agents/<nombre-del-directorio>.toml` como agente general del proyecto actual.

### Crear un agente especializado del proyecto
```powershell
cargo run -- init-agent nutricion
```

`init-agent` exige un nombre y crea `contextos/projects/<nombre-del-directorio>/agents/<nombre>.toml`.

### Analizar un proyecto y generar contexto
Requiere que el servidor ya esté corriendo.

```powershell
cargo run -- analyze C:\Ruta\A\Tu\Proyecto --id mi-proyecto --agent nutricion
```

### Reanalizar un contexto existente
También requiere que el servidor ya esté corriendo.

```powershell
cargo run -- reanalyze mi-proyecto
```

### Exportar reglas para otras herramientas
```powershell
cargo run -- export-rules mi-proyecto . --format all
```

## Flujo recomendado

1. Ejecuta `cargo run` y completa el setup inicial.
2. Crea el agente base del proyecto con `init`; usará el nombre del directorio actual.
3. Crea agentes especializados con `init-agent <nombre>`; el nombre es obligatorio.
4. Edita los TOML dentro de `contextos/projects/<project_id>/agents/` para personalizarlos.
5. Si quieres contexto de proyecto, genera el contexto con `analyze` y vincúlalo al agente con `--agent` o editando el TOML.
6. Refresca contexto con `reanalyze` cuando el proyecto cambie.
7. Consume el gateway por `/api/chat` o `/v1/chat/completions`.

## API rápida

### Salud
```text
GET /health
GET /api/health
```

### Modelos
```text
GET /models
GET /api/models
```

### Chat
```text
POST /chat
POST /api/chat
POST /v1/chat/completions
```

Usa `X-Project` para indicar el proyecto y `X-Agent` para elegir un agente específico de ese proyecto. Si envías `X-Project` sin `X-Agent`, se carga el agente general del proyecto, cuyo id es el nombre del proyecto.

### Agentes
```text
GET    /api/agents
POST   /api/agents
GET    /api/agents/:id
PUT    /api/agents/:id
DELETE /api/agents/:id
```

### Contextos
```text
GET    /api/contexts
POST   /api/contexts
GET    /api/contexts/:id
PUT    /api/contexts/:id
DELETE /api/contexts/:id
POST   /api/contexts/:id/analyze
```

### MCP
```text
GET  /api/mcp
POST /api/mcp
```

## Verificación local

```powershell
cargo check
cargo test --target-dir target-tests
```

