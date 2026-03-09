# AGENTS.md

## Purpose
Llama-R is a Rust-based personal AI gateway that sits in front of Ollama and exposes:

- A native chat API at `/chat` and `/api/chat`
- An OpenAI-compatible endpoint at `/v1/chat/completions`
- Agent management APIs under `/api/agents`
- Context management APIs under `/api/contexts`
- Health endpoints at `/health` and `/api/health`
- MCP over `/api/mcp`
- A local TUI plus HTTP and gRPC servers started from the same runtime

This file documents the current developer workflows and commands for working on the repo.

## Runtime Model
- `cargo run` starts the full application: TUI, HTTP API, and gRPC server.
- On first run, or when `DEFAULT_MODEL` is missing, the app enters interactive provider setup and persists the result to `.env`.
- Agent configs are loaded from `agents/`.
- Project contexts are stored under `contextos/projects/<project_id>/`.
- Hot reload watches the base Llama-R directory, so agent and context changes are picked up without restarting.
- `LLAMA_R_DIR` can override the default base directory for agents and contexts.

## Environment
Primary environment variables:

- `PORT`: HTTP API port, default `3000`
- `OLLAMA_URL`: provider base URL, default `http://localhost:11434`
- `DEFAULT_MODEL`: default model used for direct requests and fallback routing
- `LLAMA_R_DIR`: optional override for the base data directory

Recommended setup:

```powershell
Copy-Item .env.example .env
```

## Core Commands
Use these commands from the repository root.

### Run The App
```powershell
cargo run
```

Explicit subcommand form:

```powershell
cargo run -- run
```

### Show CLI Help
```powershell
cargo run -- --help
```

### Create A Global Agent
```powershell
cargo run -- init-agent nutricion
```

### Create A Project MCP Agent
```powershell
cargo run -- init-mcp C:\ruta\al\proyecto
```

### Analyze A Project And Generate Context
Requires the server to be running. This calls `POST /api/contexts`.

```powershell
cargo run -- analyze C:\ruta\al\proyecto --id mi-proyecto --agent mi-proyecto_mcp
```

### Refresh An Existing Context
Requires the server to be running. This calls `POST /api/contexts/:id/analyze`.

```powershell
cargo run -- reanalyze mi-proyecto
```

### Export Rules For Other AI Tools
```powershell
cargo run -- export-rules mi-proyecto .
```

Formats:

```powershell
cargo run -- export-rules mi-proyecto . --format cursor
cargo run -- export-rules mi-proyecto . --format gemini
cargo run -- export-rules mi-proyecto . --format claude
cargo run -- export-rules mi-proyecto . --format all
```

## Recommended Workflows

### First Run
1. Ensure Ollama is running locally.
2. Start Llama-R with `cargo run`.
3. Complete the interactive setup if prompted.
4. Confirm `.env` now contains `OLLAMA_URL` and `DEFAULT_MODEL`.

### Agent Creation Workflow
1. Create a base agent with `cargo run -- init-agent <name>`.
2. Edit the generated TOML in `agents/`.
3. Keep the server running so hot reload picks up changes.
4. Test with `POST /api/chat` or `POST /v1/chat/completions`.

### Project Context Workflow
1. Create a project MCP agent with `cargo run -- init-mcp <path>`.
2. Start the server with `cargo run`.
3. Generate the project context with `cargo run -- analyze <path> --id <project_id> --agent <project_id>_mcp`.
4. Refresh it later with `cargo run -- reanalyze <project_id>`.
5. Optionally export the generated rules with `cargo run -- export-rules <project_id> <target_dir>`.

## HTTP API Quick Reference

### Health Endpoints
```text
GET /health
GET /api/health
```

### Basic Endpoints
```text
GET  /
GET  /api
GET  /models
GET  /api/models
```

### Chat Endpoints
```text
POST /chat
POST /api/chat
POST /v1/chat/completions
```

`X-Agent` is supported and takes priority over `payload.model`.

### Agent API
```text
GET    /api/agents
POST   /api/agents
GET    /api/agents/:id
PUT    /api/agents/:id
DELETE /api/agents/:id
```

### Context API
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

## Developer Commands
```powershell
cargo fmt
cargo check
cargo test
```

## Storage Layout
- `agents/`: global agent TOML files
- `contextos/projects/<project_id>/agents/`: project-scoped MCP agents
- `contextos/projects/<project_id>/context/`: saved generated context
- `logs/llama-r.log`: rolling application logs

## Notes For Contributors
- Prefer documenting commands that exist in `src/cli/commands.rs`.
- Keep `AGENTS.md`, `README.md`, and generated rule exports aligned when workflows change.
- If you add a new CLI command or endpoint, update this file with the command, whether it requires the server, and what it reads or writes on disk.
