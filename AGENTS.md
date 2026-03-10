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
- Agent configs are loaded from `agents/` and `contextos/projects/<project_id>/agents/`.
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

### Create The Project Base Agent
```powershell
cargo run -- init
```

This creates `contextos/projects/<current-directory>/agents/<current-directory>.toml` as the editable default agent for the current project.

### Create A Custom Project Agent
```powershell
cargo run -- init-agent nutricion
```

Run `init-agent <name>` as many times as you need to create more project agents. A name is always required.

### Analyze A Project And Generate Context
Requires the server to be running. This calls `POST /api/contexts`.

```powershell
cargo run -- analyze C:\ruta\al\proyecto --id mi-proyecto --agent nutricion
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
1. Create the base project agent with `cargo run -- init`; it uses the current directory name.
2. Create specialized agents with `cargo run -- init-agent <name>`; the name is mandatory.
3. Edit the generated TOML files in `contextos/projects/<project_id>/agents/`.
4. Keep the server running so hot reload picks up changes.
5. Test with `POST /api/chat` or `POST /v1/chat/completions`.

### Project Context Workflow
1. Start the server with `cargo run`.
2. Generate the project context with `cargo run -- analyze <path> --id <project_id> --agent <agent_id>`.
3. Refresh it later with `cargo run -- reanalyze <project_id>`.
4. Optionally export the generated rules with `cargo run -- export-rules <project_id> <target_dir>`.

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

`X-Project` selects the project scope and `X-Agent` selects a specific agent inside that project. If `X-Project` is sent without `X-Agent`, Llama-R loads the project general agent whose id matches the project id.

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
cargo test --target-dir target-tests
```

## Storage Layout
- `agents/`: editable global agent TOML files
- `contextos/projects/<project_id>/agents/`: project-scoped agent TOML files`r`n- `contextos/projects/<project_id>/context/`: saved generated context
- `logs/llama-r.log`: rolling application logs

## Notes For Contributors
- Prefer documenting commands that exist in `src/cli/commands.rs`.
- Keep `AGENTS.md`, `README.md`, and generated rule exports aligned when workflows change.
- If you add a new CLI command or endpoint, update this file with the command, whether it requires the server, and what it reads or writes on disk.

