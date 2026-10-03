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
- On first run, when `DEFAULT_MODEL` is missing **and** a terminal is available, the app enters interactive provider setup and persists the result to `.env`.
- The provider being unreachable is **not** a startup failure. With `DEFAULT_MODEL` set, llama-r boots degraded and `GET /api/health` reports `"status":"degraded"` with the underlying cause; chat and RAG requests fail with a `502`/`500` naming the connection error. It recovers on its own once the provider is reachable.
- Without a TTY (systemd, Docker, CI) llama-r never blocks on an interactive prompt: it boots and logs a warning.
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

## TUI

- One chrome for every screen (`src/tui/chrome.rs`): a one-row context bar at the top, a key-hint footer pinned to the last row, and thin rules. `layout(area) -> Chrome { bar, body, footer }` reserves the two fixed rows, and every subtraction saturates, so a terminal too small for them gets an empty body rather than a wrapped-around one.
- Colour and style tokens live in `src/tui/theme.rs` with a fixed meaning: `ok` green, `warn` yellow, `error` red, `chrome` `DarkGray`, `action` cyan, `active` yellow + bold, `content` white. `active` is the only bold *token*: in a list it marks the row the next keystroke acts on, and its other bold users are the bar's current view name and the two dialog rows in `views/modals.rs` — the question and the warning the user has to read. The one place a view adds bold on top of another token is the analysis view's `Error: ` prefix. Views style themselves from these tokens rather than picking colours ad hoc.
- **Views take `body: Rect` as their second parameter and never call `f.area()`.** This is the rule the redesign exists to enforce: a view that lays out `f.area()` includes row 0 — the bar's row — in its own layout, so drawing it writes over the bar. (The bar was lost a second way at the same time: it was drawn into a one-row rect wrapped in `Borders::ALL`, so the border ate the only row it had.) The chrome takes those two rows for itself and hands each view only `body`, so a view cannot reach them; `TuiApp::run` is the one call site in shipped TUI code that still uses `f.area()`, to run `layout` on it.
- **Key hints live in `TuiApp::hints_for` and nowhere else.** It has no `&self` receiver, so `app.rs`'s own tests call it without a terminal or an `AppState`. Its argument is a `KeyTarget`, resolved by `key_target` from the current view plus two facts the view alone cannot supply: whether the agent list inside the projects view has focus *and* has a row to aim at, and whether the analysis view is showing skill proposals. That is why the proposal modal's keys are on the row only while that modal is up. No view may hard-code hint text, with one sanctioned exception: the delete confirmation is not a `CurrentView`, so it has no row on the bar and no footer of its own, and spells its answers inside the box where the question is — `views/modals.rs` is excluded from the hint scan for that reason.
- **No boxes.** Borders are permitted only in `views/modals.rs` (the delete confirmation and the skill-proposal dialog): a dialog is the one thing that has to read as separate from the page behind it. Everything else frames content with type, rules and the `chrome` token. `tests/tui_chrome.rs` is the guard, and it makes three passes:
  - rendered output: each view is drawn into `body` after the bar, the whole screen must be free of `┌┐└┘`, `│` is checked over the body alone (the bar's own `│` joiner is allowed), and the bar row is asserted to have survived
  - sources: every module under `src/tui/views/` except `modals`, plus `app.rs`, trimmed to the code before `#[cfg(test)]`, must not ask ratatui for a border (`Borders::`, `.borders(`, `.bordered(`, `.border_type(`)
  - sources: those same files, with `hints_for`'s own body removed, must not name a key and what it does (the two spellings caught are `Press 'a'` and `[Enter] approve`)
  - The module list is walked from disk at test time, so a view added tomorrow is guarded tomorrow. `modals` is that one exemption, named by a constant, and the border test asserts it still uses `Borders::ALL`, so the exemption never becomes vacuous. Two more tests prove each source detector can fire, and two more pin the bar: it never draws a border, and it names the product.
- The context bar leads with the product's name, then the six view names — `Dashboard │ Projects │ Agent │ Analysis │ Context │ Chat` — with the active one in `active`, then right-aligns the scope from `context_label` (`project/agent`, `project`, `agent`, or `direct`) and a liveness dot. Only the scope label gives way when the row is narrow: it truncates from the right and ends in `…`. Neither the product's name nor a view name is ever truncated by the bar itself, which fits only the label; a terminal narrow enough to clip the whole row will still cut the last name off.
- **The dot is the HTTP listener's liveness, not the provider's.** It is `AppState::api_running`, set immediately before `axum::serve` and cleared only when that call errors. No view reports whether the provider answers; the dashboard's status row reports `grpc_running` for the gRPC server and `api_running` — the same flag as the dot — for the API, plus counters, and `provider_healthy` is the health endpoint's to report.
- Renderers report what they drew: `render_projects` returns `ListBounds` and `render_chat` returns its scroll bound together with the messages rect its mouse handlers hit-test against, because a key that drives a list has to be able to measure against the rows that list actually got — which is also how the footer stops naming keys an empty list cannot answer.
- Modules: `src/tui/app.rs` (dispatch, `CurrentView`, `key_target`, `hints_for`), `src/tui/chrome.rs`, `src/tui/theme.rs`, `src/tui/views/`

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

## Skills

- A skill is a directory containing `SKILL.md` with YAML frontmatter (`name`, `description`, optional `tags`). **The id is the directory name**, not `name:`.
- Discovery uses one shared list (`SKILL_DIR_NAMES` in `src/services/skill_manager.rs`): `skills/`, `.agents/skills/`, `.claude/skills/`, `.cursor/skills/`, `.agent/skills/` — searched under `~` globally and under the project root per project.
- `SkillScope` is assigned by *how* a skill was found, not by a path table: `Project` > `LlamaR` > `Harness` (Claude/Cursor/Windsurf). Shadowing is recorded and logged (`SkillManager::shadowed`), never applied silently.
- Assignment in the agent TOML: `skills` (manual) and `auto_skills` (chosen by `sync_project_agent_skills` from the project profile).
- **Relevance ranking** (`SkillIndex`) narrows the declared skills using embeddings. It is deliberately not a `RagStore` collection: `query_scoped` only reads `scope.rag_sources`, so indexing skills there would force every agent manifest to list a shared catalog. Manual skills get a ranking boost, not a guarantee; anything dropped is logged with its score. Without embeddings (or on provider failure) every declared skill is used.
- **Generation**: `analyze` proposes new skills (`AnalysisState::Proposals`). Nothing is written until approved in the TUI. Approved proposals go to `<project>/skills/<id>/SKILL.md` with `generated_by: llama-r` in the frontmatter. Regeneration only replaces files carrying that marker, so a hand-written skill sharing an id is never clobbered.
- Modules: `src/services/skill_manager.rs`, `skill_index.rs`, `skill_generation.rs`, `agent_skill_sync.rs`

## RAG

- Disk-backed `FileRagStore` implements the `RagStore` port (LanceDB deferred: heavy arrow/datafusion stack).
- Embeddings: `OllamaEmbeddings` batches all texts into one `POST /api/embed` call (current Ollama API) and falls back to the legacy `POST /api/embeddings` when that endpoint returns 404.
- Build without RAG: there is no build-time flag — RAG pulls in no optional dependency, so it is controlled at runtime with `RAG_ENABLED=false`.
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

- Feature: `history` (default ON). Disable: `cargo build --no-default-features --features rig-engine`
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

