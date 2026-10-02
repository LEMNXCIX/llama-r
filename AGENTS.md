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
- `EMBEDDING_MODEL`: Ollama embedding model for RAG, default `nomic-embed-text`
- `EMBEDDING_DIMENSIONS`: expected vector size, default `768`
- `RAG_ENABLED`: build/use RAG store at runtime, default `true`
- `HISTORY_RETENTION_DAYS`: purge conversations older than this many days, default `90`. `0` disables purging.

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

`X-Project` selects the project scope, `X-Agent` selects a specific agent inside that project, and `X-Conversation-Id` optionally resumes an existing conversation history session. 

**Strict Validation:** If `X-Project` is provided, Llama-R **requires** a valid agent to be found within that project context. If the requested agent (or the project's default agent) is missing, the request will fail with a `400 Bad Request` error. This ensures that project-scoped requests never accidentally bypass the intended context and rules.

If `X-Project` is sent without `X-Agent`, Llama-R loads the project general agent whose id matches the project id (e.g., `contextos/projects/my-project/agents/my-project.toml`).

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

### Conversation History API
```text
GET    /api/conversations
GET    /api/conversations/:id
GET    /api/conversations/:id/messages
DELETE /api/conversations/:id
POST   /api/conversations/:id/export
```

### MCP
```text
GET  /api/mcp
POST /api/mcp
```

### RAG (debug/admin — requires `X-Debug: true`)
```text
POST /api/rag/ingest
POST /api/rag/query
POST /api/rag/delete-document
```

Returns `403` without the header (checked before the body is parsed), and `501` when RAG is disabled (`RAG_ENABLED=false`).

## Developer Commands
```powershell
cargo fmt
cargo check
cargo test --target-dir target-tests
```

## Storage Layout
- `agents/`: editable global agent TOML files
- `contextos/projects/<project_id>/agents/`: project-scoped agent TOML files
- `contextos/projects/<project_id>/context/`: saved generated context
- `data/lancedb/`: persistent RAG collections (gitignored; FileRagStore JSONL per `source_id`)
- `data/history.db`: SQLite conversation history (gitignored)
- `logs/llama-r.log`: rolling application logs

## Agent engine (Rig)

- Feature: `rig-engine` (default ON). Kept as a compile-time flag so light builds can disable `rig-core`; runtime always prefers Rig when the feature is on.
- Build without engine: `cargo build --no-default-features`
- Agent chat with tools uses Rig when:
  - `X-Project` / `X-Agent` resolve an agent, or
  - `model` matches a global agent id
- Multi-turn: prior user/assistant messages in the request body are forwarded to Rig as chat history
- Streaming (`stream: true` / SSE): agent path uses `AgentEngine::run_stream` (Nivel-A: final token + completed). Tool call/result events are not yet mapped to SSE.
- On engine failure, requests fall back to legacy Ollama chat
- Direct model requests (no agent resolved) always use the legacy provider path
- Source modules: `src/adapters/rig_engine/` (`builder.rs`, `tools.rs`, `limits.rs`)

## RAG

- Feature: `rag` (default ON). Disk-backed `FileRagStore` implements the `RagStore` port (LanceDB deferred: heavy arrow/datafusion stack).
- Embeddings: `OllamaEmbeddings` batches all texts into one `POST /api/embed` call (current Ollama API) and falls back to the legacy `POST /api/embeddings` when that endpoint returns 404.
- Build without RAG: `cargo build --no-default-features` or omit feature `rag`.
- Data path: `{LLAMA_R_DIR}/data/lancedb/<encoded_source_id>/docs.jsonl` (gitignored via `/data`).
- Env:
  - `EMBEDDING_MODEL` (default `nomic-embed-text`)
  - `EMBEDDING_DIMENSIONS` (default `768`)
  - `RAG_ENABLED` (default `true`)
- Agent TOML: `rag_sources`, `rag_write` (`none` | `own_memory_only` | `listed`)
- Isolation: agents only **read** listed `rag_sources`; **writes** follow `rag_write` (enforced in store + ingest).
- Memory ids: a project agent's own memory is `agent:{project}/{agent}/memory`; a global agent's is `agent:{agent}/memory`. `rag_write = "own_memory_only"` grants exactly that one id — never the unqualified form for a project agent.
- Changing `EMBEDDING_MODEL` or `EMBEDDING_DIMENSIONS` invalidates existing vectors: upsert rejects a dimension mismatch, so delete and re-ingest the affected collections.
- Re-ingest replaces a document's chunks (ids are `{doc_key}#chunk-{index}`), so a shorter document does not leave stale chunks behind. An empty/whitespace document is skipped and reported in `skipped`, never treated as a deletion.
- Document keys may not contain `#` (reserved for chunk ids), and must be unique within one ingest request.
- One ingest is atomic per collection: it goes through `replace_batch_scoped`, so a failure partway through leaves nothing behind. To drop a single document, re-ingest it shorter — an empty document is a no-op, not a deletion.
- Retracting a document is an explicit operation (`delete_document` / `POST /api/rag/delete-document`); it must never be inferred from empty content.
- `FileRagStore` holds a per-collection lock across snapshot → write → publish, so concurrent writers to one collection cannot lose each other's updates, while disk I/O runs on `spawn_blocking` so it never stalls the runtime.
- Chat path: when an agent has `rag_sources`, Rig `prepare()` queries the store and injects `## Retrieved knowledge` into the system prompt.
- Debug API (requires header `X-Debug: true`):
  - `POST /api/rag/ingest` — body: `project_id?`, `agent_id?`, `source_id`, `texts[]`, optional `files[]` under base dir. Response reports `chunks_written`, `files_read`, and `skipped[]` (paths not indexed, with a reason)
  - `POST /api/rag/query` — body: `project_id?`, `agent_id?`, `query`, `top_k?`
  - `POST /api/rag/delete-document` — body: `project_id?`, `agent_id?`, `source_id`, `doc_key`. Retracts one document (distinct from re-ingesting it empty, which is a no-op)
- Example agent: `examples/agents/rag-demo.toml`
- Modules: `src/adapters/rag/` (`file_store`, `embeddings`, `chunker`, `namespace`, `store`), `src/services/rag_ingest.rs`, `src/api/rag_api.rs`

### Manual RAG smoke (Ollama + embed model)

```bash
ollama pull nomic-embed-text
cargo run
# Ingest (adjust agent/project and source_id to match own_memory / rag_write)
curl -s http://127.0.0.1:3000/api/rag/ingest \
  -H 'Content-Type: application/json' \
  -H 'X-Debug: true' \
  -d '{"agent_id":"demo","source_id":"agent:demo/memory","texts":["Refunds within 24h."]}'
curl -s http://127.0.0.1:3000/api/rag/query \
  -H 'Content-Type: application/json' \
  -H 'X-Debug: true' \
  -d '{"agent_id":"demo","query":"refunds","top_k":3}'
```

## Conversation history

- Feature: `history` (default ON). Disable: `cargo build --no-default-features --features rig-engine,rag`
- DB: `{LLAMA_R_DIR}/data/history.db`
- Continue a thread: request header `X-Conversation-Id` (gRPC metadata with the same name)
- Persist follows the agent `[memory] persist_history` flag
- Summaries: set `index_summaries = true`; chunks go to `summary_collection` or `agent.memory_source_id()` if RAG write policy allows
- Admin API: `GET/DELETE /api/conversations`, `GET /api/conversations/:id/messages`, `POST /api/conversations/:id/export`
- Retention: `HISTORY_RETENTION_DAYS` (default 90); purge on boot and daily. `0` disables purging entirely.
- Execution plan (verify remaining gaps): [`PHASE_5_HISTORY_PLAN.md`](./PHASE_5_HISTORY_PLAN.md)

## Notes For Contributors
- Prefer documenting commands that exist in `src/cli/commands.rs`.
- Keep `AGENTS.md`, `README.md`, and generated rule exports aligned when workflows change.
- If you add a new CLI command or endpoint, update this file with the command, whether it requires the server, and what it reads or writes on disk.

