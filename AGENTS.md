# AGENTS.md

## Purpose
Llama-R is a Rust-based personal AI gateway that sits in front of an LLM provider (Ollama by default) and exposes:

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
- On first run, when `DEFAULT_MODEL` is missing **and** a terminal is available, the app enters interactive provider setup — where `PROVIDER` and its credentials are chosen — and persists the result to `.env`.
- One `PROVIDER` picks the **boot** upstream for chat, the decision router and (where supported) embeddings: `ollama` (default), `openrouter`, `opencode`, `openai-compatible`, `anthropic` or `gemini`. An unknown value is a configuration error naming it and all six accepted spellings. An agent manifest can override the global choice per request: `agent.provider` > global `PROVIDER` > `ollama`, and `agent.model` > `DEFAULT_MODEL`; `agent.provider` accepts a canonical spelling or a `providers.toml` profile name. The global choice can be changed live from a TUI Settings modal over the Dashboard: Apply persists to `.env` and rebuilds the stack, its companions (agent engine, RAG) **and the `context_enricher` skill-ranking embedder** without a restart.
- Streaming and tool use are gated by the **resolved model's** `ModelCapabilities` (static catalog in `src/providers/capabilities.rs`), not by a provider-name check. Each `(provider, model-prefix)` pair declares `streaming`, `tool_calling`, `vision`, `embeddings`, …; an unknown or renamed model gets the conservative all-false answer, so a capability is never assumed. MCP tool discovery is skipped entirely for a model that cannot call tools (logged with the reason). A streaming-capable model with a Rig engine attached emits final tokens progressively (Nivel-A: final token + completed; tool call/result events are not mapped to SSE). The catalog currently marks Ollama prefixes as streaming + tools + embeddings, Anthropic/Gemini/OpenAI-compatible prefixes as streaming + tools + vision, OpenRouter as tools + embeddings but no streaming (501), and OpenCode as none. The direct adapters other than Ollama reject `chat_stream` with `501 Unavailable` before opening a connection, so a non-agent or engine-less stream still returns 501 even for a catalog-streamable model.
- Six direct `LLMProvider` adapters live in `src/providers/`. Embeddings: Ollama, OpenRouter, OpenAI-compatible and Gemini have native embedding adapters; Anthropic and OpenCode have none of their own — with no `OLLAMA_URL` or `OPENAI_BASE_URL`/`OPENROUTER_API_KEY` fallback RAG is disabled and `/api/rag/*` answers an explicit `501` naming embeddings (Ollama embeddings can back OpenCode/Anthropic when `OLLAMA_URL` is set; an OpenAI-compatible endpoint named by `OPENAI_BASE_URL` with `OPENROUTER_API_KEY` works too, with Ollama tried first when both are configured). Decision adapters: Ollama, OpenAI-compatible, Anthropic and Gemini use the separate llama.cpp d1 router, while OpenRouter and OpenCode have native d1 adapters (routing stays off by default). The Rig engine has native clients for Ollama, OpenRouter, OpenAI-compatible, Anthropic and Gemini, and the boot-time selector (`engine_target_for`) attaches one for **every** kind with a client. Each request then rebuilds its target from the resolved `(provider, model)` pair, so an agent's `provider` in TOML alone selects the upstream client — no code change and no restart. OpenCode is the only kind with no Rig client: no engine is attached and agent-scoped chats degrade to a direct answer with an explicit warning naming tools/RAG/MCP as unavailable.
- A failed chat is retried in a **bounded** ladder: the same provider with `DEFAULT_MODEL`, then **one** next *usable* `providers.toml` profile in deterministic name order (profiles with no credential in their `api_key_env` are skipped). It never walks a chain. A fallback answer sets `ChatResponse.served_by` to the secret-free profile name; `ChatResponse.usage` carries upstream token counts, with `cost_usd` computed only when both catalog prices are known (none are set today, so cost stays absent). Streaming failover happens only while opening a stream, and a deliberate 501 is preserved rather than retried against another profile.
- The provider being unreachable is **not** a startup failure. With `DEFAULT_MODEL` set, llama-r boots degraded and `GET /api/health` reports `"status":"degraded"` with the underlying cause; chat and RAG requests fail with a `502`/`500` naming the connection error. It recovers on its own once the provider is reachable.
- Without a TTY (systemd, Docker, CI) run `llama-r --server-only` (also accepted as `llama-r run --server-only`): the HTTP and gRPC servers start and stay up until Ctrl-C/SIGTERM, with no TUI. Without that flag the interactive default still starts the TUI — and because `main` aborts both servers when `tui.run()` returns, a headless host without the flag tears them down immediately. `tests/headless_server_tests.rs` pins this: it spawns the real binary with no TTY and polls the real API.
- **Graceful shutdown:** on Ctrl-C or SIGTERM the servers now drain in-flight requests before exiting (exit code 0). `HTTP_TIMEOUT_MS` and `CORS_ORIGINS` must be reviewed for any LAN deployment — the former bounds outbound calls and the latter is the only set of browser origins allowed to call the gateway.
- Agent routing is **off by default** (`ROUTER_ENABLED=false`): an update that only ships the router changes nothing. When it is enabled, a chat request carrying `X-Project` but no `X-Agent` is routed to that project's most appropriate agent by a Liquid AI d1 decision model. d1 generates no tokens — it evaluates typed questions in one forward pass and returns calibrated probabilities — so Ollama cannot serve it (its endpoints are generative); it runs in a separate `llama-server` process (`llama-server -hf LiquidAI/d1-3B-GGUF:Q8_0`, which serves `POST /v1/systemone`).
- A missing or unreachable router is **not** a startup failure, mirroring the provider rule above: with `DEFAULT_MODEL` set llama-r boots and logs a warning, `GET /api/health` reports the router's state, and the requests that would have been routed degrade to the project's general agent — a project with no general agent to degrade to keeps failing with `400`, as it did before routing existed.
- Routing lives inside the shared chat dispatch, so the HTTP chat endpoints and gRPC both inherit it (`src/api/grpc.rs` builds the same `AgentSelection` and calls the same `execute_chat`). The TUI does not route: it is a separate path, and there you pick an agent deliberately.
- Agent configs are loaded from `agents/` and `contextos/projects/<project_id>/agents/`.
- Project contexts are stored under `contextos/projects/<project_id>/`.
- Hot reload watches the base Llama-R directory, so agent and context changes are picked up without restarting.
- `LLAMA_R_DIR` can override the default base directory for agents and contexts.

## Environment
Primary environment variables:

- `PORT`: HTTP API port, default `3000`
- `PROVIDER`: boot upstream for chat, routing and (where supported) embeddings — `ollama` (default), `openrouter`, `opencode`, `openai-compatible`, `anthropic`, `gemini`. An unknown value is a configuration error naming it and all six accepted spellings; an agent's `provider` overrides it per request.
- `OPENROUTER_API_KEY`: Bearer key for OpenRouter, optional. Stored in `.env` (mode `0600` on Unix); never placed in a URL, log, health response or the TUI.
- `OPENAI_API_KEY`: Bearer key for `PROVIDER=openai-compatible` (and the matching Rig client). Optional; read from the env var named by the `openai-compatible` profile — never from a URL, log or health response.
- `ANTHROPIC_API_KEY`: Bearer key for `PROVIDER=anthropic`. Optional; the built-in profile names this variable, and a `providers.toml` profile can point at a different variable name with `api_key_env`. Same secret rule as other keys.
- `GEMINI_API_KEY`: key for `PROVIDER=gemini`, sent in an `x-goog-api-key` **header** (never `?key=` in the URL). Optional; same profile/secret rule as above.
- `OPENAI_BASE_URL`: OpenAI-compatible base URL for OpenRouter and `PROVIDER=openai-compatible`, default `https://openrouter.ai/api/v1` for OpenRouter (OpenAI-compatible has no fixed default when unset). The Rig engine also uses it as a compatible-gateway override for its OpenAI-compatible, Anthropic and Gemini targets; empty means rig's native endpoint. Together with `OPENROUTER_API_KEY` it is also the embeddings fallback for providers with no endpoint of their own (`opencode`, `anthropic`): both settings must be present, and the key is sent as a Bearer header, never in a URL or log.
- `OPENCODE_API_KEY`: OpenCode server password sent as HTTP basic auth, optional — same `.env`/no-URL/no-log rule as other keys.
- `OPENCODE_URL`: origin for OpenCode's generate/decision endpoints, default `http://127.0.0.1:4096`.
- `OLLAMA_URL`: Ollama base URL, default `http://localhost:11434`; also the preferred optional embeddings fallback for providers with no endpoint of their own (`opencode`, `anthropic`). The alternative `OPENAI_BASE_URL`/`OPENROUTER_API_KEY` fallback is used when this URL is empty; a provider without either fallback has embeddings disabled.
- Credentials live only in the environment, never in config files: `Config::from_env` reads `OPENROUTER_API_KEY`, `OPENCODE_API_KEY` and `OPENAI_BASE_URL` directly; `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` and `GEMINI_API_KEY` are read at client-build time through the built-in `providers.toml` profiles' `api_key_env`, which names the variable rather than holding the secret. No key is ever echoed in health, logs, URLs or the TUI (asserted by tests).
- `DEFAULT_MODEL`: default model used for direct requests and fallback routing
- `LLAMA_R_DIR`: optional override for the base data directory
- `EMBEDDING_MODEL`: Ollama embedding model for RAG, default `nomic-embed-text`
- `EMBEDDING_DIMENSIONS`: expected vector size, default `768`
- `RAG_ENABLED`: build/use RAG store at runtime, default `true`
- `HISTORY_RETENTION_DAYS`: purge conversations older than this many days, default `90`. `0` disables purging.
- `HTTP_TIMEOUT_MS`: timeout for outbound provider/health HTTP requests, default `30000`. `0` is rejected (a request with no deadline can hang a connection forever).
- `CORS_ORIGINS`: comma-separated allowlist of web origins permitted by CORS, default `http://localhost,http://127.0.0.1`. `*` is rejected (use an explicit origin). Required when exposing the gateway to a LAN browser; only listed origins receive `Access-Control-Allow-Origin`.
- `ROUTER_ENABLED`: enable agent routing, default `false`. Off by default so an update that only ships the router changes nothing.
- `ROUTER_URL`: decision-model origin (the separate `llama-server`), default `http://127.0.0.1:8080`. **Check the port against your build.** Recent llama.cpp defaults to **9931**, not 8080 — read the `listening on` line the server prints and set this to match.
- `ROUTER_MODEL`: model label for the decision router, default `d1-3b`. It reaches only the adapter's own log line: the body sent to `POST /v1/systemone` is `{state, questions}`, with no model field, because the model is chosen by which `llama-server` process is running.
- `ROUTER_TIMEOUT_MS`: budget for one decision in milliseconds, default `3000`. `0` is rejected. **Raise this on CPU-only inference.** Liquid's published latencies (8 ms on a 4090, 26 ms on a Jetson AGX Orin) are GPU figures; a d1-3B decision measured on an 8-core CPU takes **2.4–3.3 s**, so the default sits right at the edge and every decision degrades to the general agent as `router_unavailable`. Use `30000` for CPU. A decision past the budget is not fatal — it degrades — which is why this failure looks like "routing never picks a specialist" rather than an error.
- `MIN_CONFIDENCE`: below this `Choice` confidence the request stays with the project's general agent, default `0.60`. Must be greater than `0.0` and at most `1.0`; `0` is rejected rather than clamped.
- `MIN_SPECIALIZED`: below this specialisation score a specialist does not win over the general agent, default `0.50`. Same `(0.0, 1.0]` bounds as `MIN_CONFIDENCE`. The env name uses a `z`; the matching `router.toml` key is `min_specialised`.

A malformed router value (bad number, timeout `0`, threshold outside `(0.0, 1.0]`) is a configuration error naming the variable and the value received, not a silent clamp. At boot that error is logged as a warning and the router falls back to the inert defaults (routing disabled) — like a bad `ROUTER_URL`, it never stops startup.

An optional `router.toml` in the base directory overrides any of the values above (`enabled`, `url`, `model`, `timeout_ms`, `min_confidence`, `min_specialised`, and an `exclude` list of agent ids routing must never select): the environment provides the defaults and the file wins on the keys it actually sets. There is no env var for `exclude`. A missing file is fine; an unreadable or malformed one is a configuration error naming the path, and at boot it is logged and falls back to the inert defaults (routing disabled) rather than being silently ignored.

`exclude` matches on the bare agent id and drops the agent from that project's candidate set. It cannot drop a project's **own general agent** (`contextos/projects/<p>/agents/<p>.toml`): that agent is the one landing place every degrading rung of the ladder has, so excluding it would fail the request with `agent_not_found` instead of changing where it lands. The exclusion is ignored for that one agent and logged as a warning. A *global* whose id matches the project id is a different agent and stays excludable.

An optional `providers.toml` in the base directory is the profile registry (`src/providers/registry.rs`). Six built-in profiles — `ollama`, `openrouter`, `opencode`, `openai-compatible`, `anthropic`, `gemini` — always exist; a file overlays only the fields it sets, so a small edit never removes the others. Each profile accepts `type` (one of the six `PROVIDER` spellings), `base_url`, `api_key_env` and `timeout_ms` (default `30000`; `0` is rejected). **`api_key_env` is the NAME of an environment variable holding the key, never the secret itself** — built-ins use `OPENROUTER_API_KEY`, `OPENCODE_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` and `GEMINI_API_KEY`. A missing file is fine; an unreadable or malformed one is a configuration error naming the path and at stack build it is logged and falls back to the built-in profiles.

Selection is deliberately narrower than profile configuration. `PROVIDER` accepts **only** the six canonical kind spellings — a custom profile name is a configuration error. An agent's `provider` accepts a canonical spelling **or** a profile name, but a profile name selects only that profile's `kind` (`resolver.rs`); a custom-named profile's `base_url`/`api_key_env` reach the chat and embedding adapters **only on the one fallback rung**, never for primary or agent-selected chat. The agent/engine path reads the fixed built-in env vars (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`) and endpoints instead; overriding a built-in profile **by its canonical name** (e.g. `[providers.anthropic]`) does change that kind's direct adapters, because `profile_for` looks it up by canonical name. Profiles are iterated in deterministic name order, which is what makes the single fallback pick reproducible. Example:

```toml
[providers.my-openai]
type = "openai-compatible"
base_url = "https://api.example.com/v1"
api_key_env = "MY_OPENAI_KEY"   # name of the env var holding the key
timeout_ms = 30000
```

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

Headless (systemd, Docker, CI) — no TTY needed, servers stay up until Ctrl-C:

```powershell
cargo run -- --server-only
# or: cargo run -- run --server-only
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

The response carries a `router` field: `{"enabled": bool, "healthy": bool, "url": …}`. `healthy` is a **last-known value**, not a live probe: it starts with a **TCP reachability check of the configured origin** at boot (a connect, not a request to `/v1/systemone`) and is then updated by the outcome of every decision. It means "something was listening at `ROUTER_URL`", **not** that `/v1/systemone` answers. A router that is switched off reports `healthy: false` (nothing has checked it), and `status` is `"ok"` only when the provider is healthy and the router is either disabled or healthy — an enabled-but-unreachable router reports `"degraded"`, for the same reason an unreachable provider does.

It also carries a `provider` object: `{"name": …, "embeddings": bool, "reason": null|string, "agent_engine": bool, "agent_engine_reason": null|string}`. `name` is the selected `PROVIDER`; `reason` explains a missing embeddings capability (e.g. OpenCode with no `OLLAMA_URL` or `OPENAI_BASE_URL`/`OPENROUTER_API_KEY` fallback), and `agent_engine` / `agent_engine_reason` report whether a Rig engine is attached and, if not, name the lost capabilities (tools/RAG/MCP). These are the same strings the chat-time degradation warning uses; no field ever carries a key.

The response also carries an `observability` object (`ObservabilitySnapshot`, `src/api/observability.rs`) of atomic counters. Alongside the request/latency/routing counters it now exposes four Fase 6 fields: `auto_approved`, `auto_denied` and `auto_unavailable` count gate-time `Auto` decisions (`Allow`, `Deny`, and router error/no-decider respectively, including a missing decider as `auto_unavailable`), and `cost_usd_total_micros` accumulates real reported `cost_usd` in integer microdollars (saturating; no entry while catalog prices are `None`). Only an `Auto` rule reaches the `auto_*` counters — `Never`, `Always` and `Once` never touch them. The dashboard/TUI reads the same snapshot; no health contract change beyond these added fields, and no field carries a key.

### Metrics

```text
GET /metrics
```

Prometheus text format 0.0.4 rendering of the same `ObservabilitySnapshot` (`src/api/metrics.rs`, 16 `llamar_*` series). This endpoint is public: it requires neither `X-Debug` nor `X-Admin-Token`, and it carries no secrets — no key, token or secret appears in the output.

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

With routing enabled (`ROUTER_ENABLED=true`), one of the two behaviours above changes, and only then; with the router off (the default) both behave exactly as written:

- `X-Project` without `X-Agent` no longer always resolves to the general agent: it is routed among that project's agents plus the global agents, keyed by the registry's scheme (`project::agent` for project agents, the bare id for globals). `X-Agent` still pins the target and never spends a decision.
- The candidate set cannot cross projects — another project's agent is never offered, which is what keeps a routed request inside the authority its `X-Project` named. It *can* reach a **global**, and a global winner dispatches with `project_id: None`: answered outside the project, with none of that project's context, rules, `rag_sources` or `tools_override`. Precisely: never another project, sometimes no project at all.
- Routing is **per request, not per conversation**. Each turn is a fresh decision, so the chosen agent can change between two messages of the same thread; there is no stickiness, and `X-Conversation-Id` is not read by the router.
- A request whose answer is already known spends no decision: an empty `messages` array is refused before resolution, and a specialists-only project is refused before the router is consulted. Both `400`s are the ones that request already got.
- A project with specialised agents but **no general agent still returns `400`** — it never routes. Every rung of the ladder degrades to the general agent, so `route` checks that one exists *before* it reads any answer, and the check runs before the decision is spent: such a request fails with the same `agent_not_found` `400` as always, without the decision model being consulted at all.
- A project with no candidates left to route between (no project agents, and no global agents either after `exclude`) is not routed at all: resolution returns the request untouched and it fails validation exactly as it did before routing existed — also without spending a decision. Both cases answer the same `400` they always did; neither is a new kind of failure.

When nothing fits, the destination is the project's **general agent** (`contextos/projects/<p>/agents/<p>.toml`), never a bare `DEFAULT_MODEL`: a request that named a project must be answered inside that project with its context and rules, and falling through to an unscoped model would drop exactly what the request asked for. When the project has no general agent there is nothing to fall back to, so — as the bullet above says — the request gets that `400` instead, and still never a bare model.

Routing is not HTTP-only: it lives inside `execute_chat`, so gRPC inherits it because `src/api/grpc.rs` builds the same `AgentSelection` from the `x-project`/`x-agent` metadata and calls the same dispatch — there is no per-surface opt-out. The TUI does not route: it is a separate path, and there you pick an agent deliberately.

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

These endpoints are part of the **admin group** and require `X-Admin-Token` (breaking change vs. earlier releases, where they were public). A bare `GET /api/conversations` with no `project_id`/`agent_id` returns an empty page (`[]`) instead of every conversation; scope with `?project_id=...&agent_id=...`.

### MCP
```text
GET  /api/mcp
POST /api/mcp
```

### Webhooks (signed channels)
```text
POST /api/webhooks/:channel
```

A signed channel runs one agent turn. The signature is verified over the raw body **before** any JSON parsing: a missing/malformed/wrong signature, or a channel with no secret, is a generic `401`. Unknown or disabled channels are `404`. `mode = "sync"` answers `200` with the `ChatResponse`; `mode = "async"` (default) answers `202 {"execution_id": "..."}`, observable at `GET /api/executions/:id`. Scope and the debug gate come from `channels.toml`, never from the request. See "Webhook channels (Fase 8)".

### Approval (human-in-the-loop — no `X-Debug` required)
```text
GET  /api/approvals
POST /api/approvals/:id
```

`GET /api/approvals` lists undecided pending approvals (`id`, `tool_name`, `args`, `created_at`), with secret-bearing argument fields redacted. `POST /api/approvals/:id` takes `{"approve": bool, "args_hash"?: "<64-hex>"}` and unblocks the waiting tool call; omitting `args_hash` still binds the decision to the hash on file. Errors: `400` malformed/mismatched hash, `404` unknown id, `409` already decided, `410` expired. Unlike the RAG/Router admin endpoints these are **not** behind `X-Debug`: they are how a human answers a live approval, not an admin view. They are, however, gated by the shared admin token (`X-Admin-Token`) like the rest of the admin group, and still carry no per-user identity — the token is a shared secret, not a principal. Approvals now **do** persist across a restart: pendings and decisions are mirrored into the execution store and rehydrated at boot (a decision made after a crash is consumed by a retry). See "Connections, permissions and human approval" for the full flow.

### RAG (debug/admin — requires `X-Debug: true`)
```text
POST /api/rag/ingest
POST /api/rag/query
POST /api/rag/delete-document
```

Returns `403` without the header (checked before the body is parsed), and `501` when RAG is disabled (`RAG_ENABLED=false`).

### Router (debug/admin — requires `X-Debug: true`)
```text
POST /api/route
```

Body: `{"project_id": "...", "prompt": "..."}`. It runs the routing decision for that prompt — same candidates, same two questions, same thresholds as a chat would use — and returns the evidence (candidates, raw response, derived decision and reason, both scores, the probability distribution, decision latency) **without executing the chat**. This is the tool for calibrating `MIN_CONFIDENCE`: a preview costs one decision-model forward pass instead of a full agent turn.

Returns `403` without the header (checked before the body is parsed), `400` when the project has no candidates to route between or none to degrade to (the same `agent_not_found` the chat would answer), and `501` when routing is disabled. The same evidence is available on a real chat response as `debug_route` when `X-Debug: true` is sent.

### Execution observability and retry (debug/admin — requires `X-Debug: true`)
```text
GET  /api/executions          # most recent agent turns first (?limit, default 50, max 200)
GET  /api/executions/:id      # one turn plus its append-only event log
POST /api/executions/:id/retry
```

Agent turns are recorded as `AgentExecution` rows (see "Durable execution and recovery"). All three endpoints are behind `X-Debug: true` like RAG/Router, return `403` without it (checked before the id or body is parsed), and apply the shared secret redaction again at the DTO layer. `GET /api/executions/:id` returns `404` for an unknown id. `POST /api/executions/:id/retry` re-runs an `Interrupted` or `Failed` turn under its **original** id so recorded tool effects are replayed, not re-executed: `200` the updated turn, `400` malformed id, `403` no `X-Debug` or a legacy row with no stored `user_message`, `404` unknown id, `409` the turn is not in a retryable status. Retry is turn-level (see the caveat in "Durable execution and recovery"), uses `stream: false`, and does not reload prior conversation history.

## Developer Commands
```powershell
cargo fmt
cargo check
cargo test --target-dir target-tests
# Multi-provider eval smoke (fixture mode by default; no live key/network):
cargo test --test provider_eval_smoke
# Declarative eval runner (checks, strict mode, called_tool, judge):
cargo test --test evals_tests
```

CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo audit`, `cargo clippy --all-targets -- -D warnings`, `cargo test --target-dir target-tests`, `CARGO_TARGET_DIR=target-tests cargo llvm-cov --all-features --lcov --output-path lcov.info --fail-under-lines 40`, `cargo deny check` and `cargo build --release --target-dir target-release`. The pinned toolchain and required components (`clippy`, `rustfmt`, `llvm-tools-preview`) live in `rust-toolchain.toml`; `tests/ci_config_tests.sh` checks the CI and release workflow configuration locally.

## Declarative evals

A reusable eval runner lives in `src/evals/` (`cases`, `check`, `runner`, `report`,
`fixture` and, under `rig-engine`, `agent_fixture`). Suites are TOML files under
`evals/`; the shipped one is `evals/providers/smoke.eval.toml`. Both test targets
above are consumers of the runner, not separate harnesses. Everything runs
against **recorded local fixtures** — a one-shot HTTP listener per provider case
and a fake Ollama socket for agent cases — so CI needs no live key, network or
decision model. Report details are redacted through the shared `redact_secrets`
before they reach a table or a `FAIL` detail.

### Suite and case format

- `[[cases]]` — each has `id`, `kind`, `prompt`, optional `expect`, `checks` and
  optional `agent`.
  - `kind` is `plain`, `tool` or `stream` (provider-class cases, run against every
    entry in `[[providers]]`) or `agent` (a case run once through a real tool loop).
  - `expect = "text"` is v1 sugar for a `contains` check: it is folded into
    `checks` as a `Contains` appended after the explicit checks, so it is a final
    backstop. A case with no `checks` (and no `expect`) becomes a `FAIL` row, not
    a silent pass.
  - `agent = "<id>"` names the in-memory agent for an `agent` case (defaults to
    `eval-agent` when omitted).
  - `[[providers]]` entries carry `name` and `kind` (one of the six
    `PROVIDER` spellings). A top-level `mode` field (`strict` | `lenient`,
    default `strict`) is parsed for compatibility, but `run_suite` does not read
    it: the gate is decided by the caller through `RunOptions.mode` and
    `EvalReport::all_passed(strict)`.
- `checks` is an array of tagged objects (`type = ...`). The four types are:
  - `contains { text }` — the answer must contain `text`.
  - `not_contains { text }` — the answer must not contain `text`.
  - `called_tool { name }` — the turn must have emitted a tool call with that
    qualified name (`server__tool` for an MCP tool, the bare id for a local
    tool). Evidence comes from **real** `AgentRunEvent::ToolCall` events of an
    `agent` case; a tool that never ran is a `FAIL`, not a green skip.
  - `judge { question }` — the existing decision router (`DecisionRouter`, d1)
    grades the answer as a `yes`/`no` `Choice`. This is grading only, **not** the
    tool-approval `Auto` policy, which is a separate gate-time approval feature
    (see "Connections, permissions and human approval") and has no eval check here.

### Outcomes, strict mode and judge skips

Each row aggregates its case's checks: a `FAIL` wins over a `SKIP`, which wins
over a `PASS`. Default mode is **strict**: `EvalReport::all_passed(strict)` is
false if any row is `FAIL`; lenient mode reports failures but passes. A `SKIP`
never fails in either mode. A `judge` check **skips with a reason** instead of
passing whenever there is no injected router (`router_unavailable`), its
confidence is below the threshold (default `0.6`, `low_confidence`), or the
router returns an unusable answer — so in fixture-only CI all judge rows abstain
and strict stays green. A malformed case is caught into a `FAIL` row naming its
id; the rest of the suite still runs.

The eval runner has no check for the `Auto` approval policy: `judge` grades
answers, it does not decide tool approvals. The `Auto` policy does exist now, at
the approval gate (see "Connections, permissions and human approval"), and turn
traces plus cost tracking do exist now in the execution log and observability
snapshot (see "Durable execution and recovery" and the health endpoints) — but
none of that is an eval check. There is **no OpenTelemetry crate**: the opt-in
exporter posts the already-redacted trace fields as OTLP/HTTP JSON with the
existing `reqwest` (see "Admin auth, approval parking and OTLP telemetry").

## Storage Layout
- `evals/`: declarative eval suites in TOML (`evals/providers/smoke.eval.toml` is the shipped smoke suite); loaded by `src/evals/` and run by the two eval test targets
- `agents/`: editable global agent TOML files
- `mcp-servers/` (optional): one `*.toml` per external connection. The classic fields are `id`, `transport` (`http` | `sse`/`streaming` | `stdio` | `openapi`), `url`, `command`/`args` (stdio), `timeout_secs`, `tool_namespace`, `enabled`, and `auth_env` (the **name** of the env var holding the credential, never the secret). Typed-connection fields: `headers` (array of `[name, value]` pairs; a `$VAR` value is resolved from the environment at build time), `auth` (`bearer` | `basic` | `none`, defaults to `bearer` so a v1 config with only `auth_env` keeps working; `none` must be explicit), and `auth_user_env` (env var name for the Basic username; the password still comes from `auth_env`). All new fields are `#[serde(default)]`, so v1 manifests load unchanged. With `transport = "openapi"`, `url` is a path to a JSON OpenAPI 3.x / Swagger 2.0 spec (see "Connections, permissions and human approval" below).
- `tools/` (optional): local tool manifests, one `*.toml` per file (`type` = `builtin:skill` or `rag_query`, plus `id`, `description`, `input_schema`, `source_id`); overlays the two built-ins, malformed files are warned about and ignored at boot
- `contextos/projects/<project_id>/agents/`: project-scoped agent TOML files
- `contextos/projects/<project_id>/context/`: saved generated context
- `data/lancedb/`: persistent RAG collections (gitignored; FileRagStore JSONL per `source_id`)
- `data/history.db`: SQLite conversation history (gitignored). The durable-execution store adds four tables to this **same** file with an additive, `IF NOT EXISTS` schema (no destructive migration, so an old `history.db` opens unchanged): `executions` (one agent turn), `execution_events` (append-only event log), `tool_operations` (idempotency cache keyed by `operation_id`) and `pending_approvals` (persisted human decisions). WAL mode is enabled. See "Durable execution and recovery".
- `router.toml` (optional): router tuning — the two thresholds, `exclude`, and any of the `ROUTER_*` values; overrides the environment defaults
- `providers.toml` (optional): provider profile registry — `type`, `base_url`, `api_key_env` (env var name, never a key) and `timeout_ms`; overlays the six built-in profiles
- `channels.toml` (optional): inbound webhook channels — `[[channels]]` entries with `id`, `agent_id`, `project_id`, `mode`, `template`, `signature_header`, `auth_env` (the **name** of the env var holding the shared secret, never the secret) and `enabled`; loaded per request (see "Webhook channels (Fase 8)")
- `logs/llama-r.log`: rolling application logs

## TUI

- One chrome for every screen (`src/tui/chrome.rs`): a one-row context bar at the top, a key-hint footer pinned to the last row, and thin rules. `layout(area) -> Chrome { bar, body, footer }` reserves the two fixed rows, and every subtraction saturates, so a terminal too small for them gets an empty body rather than a wrapped-around one.
- A provider Settings modal opens over the Dashboard: it edits `PROVIDER`, its key and the model, and every key goes to the dialog before the view underneath. The dialog cycles the three boot-engine providers — Ollama / OpenRouter / OpenCode (`PROVIDERS` in `src/tui/views/settings.rs`); `openai-compatible`, `anthropic` and `gemini` are selected through `PROVIDER`/`providers.toml` and normalize to the first slot if the dialog is opened on one. Apply persists to `.env` and rebuilds the provider stack, its companions **and the skill-ranking `context_enricher`** in place; keys are masked in the UI (`Settings::masked_key`) and never rendered.
- Colour and style tokens live in `src/tui/theme.rs` with a fixed meaning: `ok` green, `warn` yellow, `error` red, `chrome` `DarkGray`, `action` cyan, `active` yellow + bold, `content` white. `active` is the only bold *token*: in a list it marks the row the next keystroke acts on, and its other bold users are the bar's current view name and the two dialog rows in `views/modals.rs` — the question and the warning the user has to read. The one place a view adds bold on top of another token is the analysis view's `Error: ` prefix. Views style themselves from these tokens rather than picking colours ad hoc.
- **Views take `body: Rect` as their second parameter and never call `f.area()`.** This is the rule the redesign exists to enforce: a view that lays out `f.area()` includes row 0 — the bar's row — in its own layout, so drawing it writes over the bar. (The bar was lost a second way at the same time: it was drawn into a one-row rect wrapped in `Borders::ALL`, so the border ate the only row it had.) The chrome takes those two rows for itself and hands each view only `body`, so a view cannot reach them; `TuiApp::run` is the one call site in shipped TUI code that still uses `f.area()`, to run `layout` on it.
- **Key hints live in `hints_for` (`src/tui/keys.rs`) and nowhere else.** It is a free function with no `&self` receiver, so its tests call it without a terminal or an `AppState`. Its argument is a `KeyTarget`, resolved by `key_target` from the current view plus the four facts the view alone cannot supply: whether the agent list inside the projects view has focus *and* has a row to aim at, whether the analysis view is showing skill proposals, whether the delete confirmation is up, and which field the agent form's caret is in. Each is a screen whose keys belong to no `CurrentView`, and each was added after the footer was found naming keys that did something else entirely — so a fifth such screen arrives with no footer written for it unless `key_target` is given an input for it too. No view may hard-code hint text; there is no exception, and `views/modals.rs` is in the scan like every other view.
- **A dialog's keys are routed by a function, and its guard goes above every view guard.** `skill_picker::picker_action(picker, code) -> PickerAction` holds the picker's whole key map, `rule_editor::rule_action(editor, key)` holds the rule editor's, and `chat_input_action` does the same for the chat input — both for the same reason: key maps written inline in the event loop were untestable; as free functions each map has its own unit tests, and `dispatch` itself is now testable by injecting keys. That is not hypothetical. The picker shipped completely inert (no cursor, no toggle, no `Esc`) because its arm sat *below* the agent form's guard, and it only ever opens from inside that form, so the form's guard consumed every key first. `TuiApp::dispatch` is the one place keys are handled; `tests/tui/app/tests.rs` injects keys into a `TuiApp` built from a stub `AppState` and asserts that each open dialog consumes the key before the view underneath.
- **In the picker, `Space` toggles and every other character filters.** `j` and `k` therefore type rather than move, and the arrows are the only cursor keys — a filter you cannot type a `k` into is useless, since a skill id is a directory name. The filter changes only what is on screen: `chosen_ids` never consults it, so a pick hidden behind a mistyped needle survives `Enter` rather than being silently dropped. The panel sizes itself with `chrome::hints_width` rather than a fixed width, because a legend too wide for its panel loses its tail — and inside a dialog the tail is `esc`.
- **`form_rules` is a `Vec<String>`, and the one line the form draws is a rendering.** It used to be a `String` joined with commas and split again on save, which made a rule containing a comma silently become two rules — the same defect the picker had, in a field the picker could not help with. `Ctrl+R` opens `views::rule_editor::RuleEditor` over the list; typing or backspacing on the rules row opens it too and lands the character inside, because a row that looks editable, swallows every keypress and shows nothing is the picker-inert bug in another field. The focused rule grows open over as many rows as it needs and wraps (`modals::wrap`), because a rule of prose is longer than one line by definition.
- **The rule editor's structural keys all take a modifier; nothing bare is stolen.** `⇥`/`⏎`/`^n`/`^d`/`esc` — every other key types. The picker's `Space: toggle` does not transfer: a skill id contains no spaces and a rule in Spanish is mostly `a`, `d`, `n` and `s`. `Esc` and `^s` both close and keep. There is no cancel, because the form behind the dialog is discarded whole by its own `Esc`.
- **The picker's keys are drawn inside the panel, and the chrome footer goes silent while it is up.** The panel is centred in the screen while the footer is pinned to the last row, and a legend nine lines below the list it describes is one nobody reads. So `app.rs` hands the chrome footer an empty slice and the panel draws the very same pairs. `modals::render_skill_picker` therefore builds a `chrome::hints_line` rather than calling `chrome::render_footer`: the guard counts `render_footer` call sites, and a dialog that reaches for the footer renderer *is* the second footer it exists to catch. One list (`hints_for`), one formatter (`hints_line`), two places it can be drawn, never both.
- **No boxes.** Borders are permitted only in `views/modals.rs` (the delete confirmation and the skill-proposal dialog): a dialog is the one thing that has to read as separate from the page behind it. Everything else frames content with type, rules and the `chrome` token. `tests/tui_chrome.rs` is the guard, and it makes four passes:
  - rendered output: each view is drawn into `body` after the bar, the whole screen must be free of `┌┐└┘`, `│` is checked over the body alone (the bar's own `│` joiner is allowed), and the bar row is asserted to have survived
  - sources: every module under `src/tui/views/` except `modals`, plus `app.rs`, trimmed to the code before `#[cfg(test)]`, must not ask ratatui for a border (`Borders::`, `.borders(`, `.bordered(`, `.border_type(`)
  - sources: `shipped(app.rs)` with no exception, plus `shipped(keys.rs)` minus `hints_for`'s own body, must not name a key and what it does (the two spellings this codebase has used are `Press 'a'` and `[Enter] approve`; `names_a_key` also matches `press "`, and any short whitespace-free alphanumeric token in brackets followed by a word)
  - sources: every `.rs` under `src/tui` must contain exactly one call to `chrome::render_footer`, in `app.rs` — the two checks above are heuristics over prose, and a hint written the idiomatic way arrives as data (`&[("a", "analyze")]`), which neither spelling catches
  - The module list is walked from disk at test time, so a view added tomorrow is guarded tomorrow. `modals` is exempt from the border pass alone, named by a constant, and that test asserts it still uses `Borders::ALL`, so the exemption never becomes vacuous. Two tests prove each source detector can fire, and two more pin the bar: it never draws a border, and it names the product.
- The context bar leads with the product's name, then the six view names — `Dashboard │ Projects │ Agent │ Analysis │ Context │ Chat` — with the active one in `active`, then right-aligns the scope from `context_label` (`project/agent`, `project`, `agent`, or `direct`) and a liveness dot. Only the scope *label* is fitted: it truncates from the right and ends in `…`. The rest of the row is clipped rather than chosen — the line is built left to right and cut at the row's width, so with the six shipped names the fixed part is 66 columns and the dot needs 69 before the label has a column of its own. The product's name is the last thing that cut reaches, not a thing the cut never reaches.
- **The dot is the HTTP listener's liveness, not the provider's.** It is `AppState::api_running`, set immediately before `axum::serve` and cleared only when that call errors. No view reports whether the provider answers; the dashboard's status row reports `grpc_running` for the gRPC server and `api_running` — the same flag as the dot — for the API, plus counters, and `provider_healthy` is the health endpoint's to report.
- Renderers report what they drew: `render_projects` returns `ListBounds` and `render_chat` returns its scroll bound together with the messages rect its mouse handlers hit-test against, because a key that drives a list has to be able to measure against the rows that list actually got — which is also how the footer stops naming keys an empty list cannot answer.
- Modules: `src/tui/app.rs` (dispatch), `src/tui/keys.rs` (`CurrentView`, `KeyTarget`, `key_target`, `hints_for`), `src/tui/chrome.rs`, `src/tui/theme.rs`, `src/tui/views/`

## Agent engine (Rig)

- Feature: `rig-engine` (default ON). Kept as a compile-time flag so light builds can disable `rig-core`; runtime always prefers Rig when the feature is on.
- Build without engine: `cargo build --no-default-features`
- An engine is attached at boot for every kind with a Rig completion client (all six except OpenCode). `RigAgentEngine` keeps the boot target and a clone of `Config`, but `run_inner`/`stream_inner` rebuild the target from the *request's* resolved `(provider, model, capabilities)` via `engine_target_for_resolved`. That is what lets an agent switch upstream with TOML alone; the boot target only decides whether an engine exists. An OpenCode-resolved request has no client and fails with a named error instead of silently using another provider's client. `tests/agent_provider_wire_tests.rs` proves the Anthropic/Gemini wire is hit from an Ollama boot.
- Agent chat with tools uses Rig when:
  - `X-Project` / `X-Agent` resolve an agent, or
  - `model` matches a global agent id
- Multi-turn: prior user/assistant messages in the request body are forwarded to Rig as chat history
- **Local tools live alongside MCP in `prepare()`.** `RigAgentEngine::prepare` gathers MCP tools and `LocalToolRegistry` entries into one `Vec<ScopedTool>`; `builder::build_tool_bridges` wraps both kinds as `ToolDyn` bridges that share a single `ToolCallBudget`, so one run has one allowance no matter which kind spends it. `RigAgentEngine` owns the two executors' companions: a `SkillManager` (for `llama_r__skill`) and the optional `RagStore` (for `llama_r__rag_query`). The catalog is loaded once at boot from `tools/*.toml` overlays, with the two built-ins always present; a malformed manifest is logged and falls back to the built-ins, never stopping startup. The catalog is registered, not offered: scope decides what reaches the model.
- Streaming (`stream: true` / SSE): agent path uses `AgentEngine::run_stream` (Nivel-A: final token + completed). Tool call/result events are not yet mapped to SSE.
- **`AgentRuntime` holds a `ContextEnricher` and a `TokenOptimizer`.** Two `None`s it used to mean something narrower than "build it": a `system_prompt` of `None` fell back to the manifest's `system_prompt` alone, dropping the agent's `rules`, its project context and its selected skills; and the agent's optimization tools were applied only by `chat_core`, so from the TUI they were configuration that appeared to work and did not. Both paths now agree. A prompt or message the caller *does* bring is still used as given, because `chat_core` has already run it through the metrics. `tests/agent_runtime_tests.rs` pins it.
  - `chat_core` still optimizes, but only when `state.agent_runtime.is_none()` — the same field its own dispatch branches on, so the two cannot disagree about which engine will run. Doing it in both places ran every tool twice, which is invisible until a tool turns out not to be idempotent.
- On engine failure, requests fall back to legacy Ollama chat
- Direct model requests (no agent resolved) always use the legacy provider path
- Source modules: `src/adapters/rig_engine/` (`builder.rs`, `tools.rs`, `limits.rs`, `local_tools.rs`)

## Connections, permissions and human approval

Three capabilities added around the agent engine; all three are opt-in through config and change nothing for a v1 manifest that does not use them.

### Connections (`mcp-servers/*.toml`)

- `McpServerConfig` (`src/adapters/mcp/registry.rs`) gained `headers`, `auth` and `auth_user_env`, all `#[serde(default)]` so existing profiles load unchanged. Credentials stay **env var names** (`auth_env`, `auth_user_env`, or `$VAR` inside a header value), never values in TOML. `auth` is `bearer` (the default), `basic` (username from `auth_user_env`, password from `auth_env`) or `none` (explicit only). A missing `$VAR` is skipped with a warning naming only the variable, never a secret; boot continues.
- **Auth cannot be spoofed.** `apply_connection_headers` (`src/adapters/mcp/http.rs`) inserts custom headers first and the connection's `Authorization` last with `HeaderMap::insert`, leaving exactly one value: a header supplied by a caller (e.g. an OpenAPI `Authorization` header parameter coming from tool arguments) can never override or append to the configured credential. `OpenApiConnection`'s `Debug` redacts resolved header values, so an API key from `$VAR` is not printed in logs.
- **`transport = "openapi"`** turns a JSON OpenAPI 3.x or Swagger 2.0 spec into MCP tools (`src/adapters/mcp/openapi.rs`): `paths` with `get`/`post`/`put`/`patch`/`delete`, path/query/header parameters and an optional JSON request body. It is a small parser, not a validator. An unreadable, invalid or unsupported spec is a **warning naming the path** and the connection is skipped — boot is never blocked by one bad spec.

### Permission policy

- An agent's `[policy]` table is compiled into `AgentScope.permission` by `ScopeBuilder` (`src/domain/policy.rs`, `src/services/scope_builder.rs`). It has `connections_allow` (declarative list of connection ids; actual tool visibility is still enforced by `mcp_sources`/`tools_override`/`allows_tool`, so a v1 manifest keeps exactly its old behavior) and ordered `[[policy.approvals]]` rules `{ tool_pattern, policy }`.
- `tool_pattern` is a simple glob over the qualified tool name — `server__tool` for an MCP tool (`rig_tool_name`), or the bare id for a local tool. `*` matches any run of characters; everything else is literal; the whole string must match, case-sensitively. **First matching rule wins**, not the most specific; with no match the default is `Never`, which allows. `PermissionEngine::authorize` is a pure function (no I/O, no runtime), tested in `tests/permission_tests.rs`. **No tool requires approval by default.**
- `ApprovalPolicy` is `Never` (execute without asking; the default), `Once`, `Always` or `Auto`. `PermissionEngine::authorize` stays pure: `Never` → `Allow`, and `Once`/`Always`/`Auto` → `Approve`, so all three block on the first call in that pure function. The behavior difference is at the **gate** (`src/adapters/rig_engine/approval.rs`), not in the pure engine:
  - `Once` and `Always` take the unchanged human path (pending + long-poll/SSE). There is still no "first use only" suppression for `Once`, so `Once` behaves like `Always` today.
  - `Auto` consults the injected `AutoDecider` (`src/ports/auto.rs`, `RouterAutoDecider` in `src/adapters/auto/router.rs`), built from the same `DecisionRouter` (d1) as agent routing and only when `ROUTER_ENABLED=true`. It auto-approves with **no human pending** only when the model answers `Choice{key:"yes"}` with `confidence >= threshold`; a `no`, low confidence, `Noul`, unusable answer, router error or absent decider all degrade to the human path (never an automatic approval without an affirmative decision). Because the d1 router is absent in most deployments, `Auto` ≈ `Always` in practice even though the seam is implemented and tested with a fake router. The question sent to the model carries the tool name, the canonical `args_hash` and the **redacted** args JSON — args are redacted with `redact_secrets` first, so no raw secret reaches the model.
  - `[policy]` / `[[policy.approvals]]` accepts an optional `auto_threshold` (`Option<f32>`, serde default). When present and valid in `(0.0, 1.0]` it overrides the decider's default threshold (the router's `MIN_CONFIDENCE`, 0.60); an out-of-range or non-finite value is ignored with a warning naming only the number and the tool, never any argument. The pure `PermissionEngine` still treats `Auto` as `Approve`.

### Human-in-the-loop approval

- A single `ApprovalGate` (`src/services/approval.rs`) lives on `AppState`. A Settings Apply rebuilds the engine from the same `Arc`, so a pending decision survives a hot provider swap. It is **also persisted across restart** (Fase 7 durable execution): the gate mirrors pendings and decisions into the `ExecutionStore`, and boot rehydrates them **without a oneshot** (there is no live request to unblock after a crash). A decision recorded after the original run stopped is consumed by the retry path via `decision_for`. The oneshot remains live-request only; the store futures are immediately ready (`Mutex<Connection>`/rusqlite or the no-op store), so the synchronous gate API drives them with `block_on` without parking on a tokio primitive. The gate's deadline comes from `Config::approval_timeout()`: `None` by default (`APPROVAL_PARK=true`), so a pending waits for a human rather than expiring at 120 s. A periodic reaper (`src/services/approval_reaper.rs`, `REAPER_INTERVAL_SECS`, default 5) resumes *decided orphans* — `WaitingApproval` turns absent from `parked_live` after a restart — under their original execution id, so recorded tool effects replay instead of repeating.
- Flow: before dispatching a tool, the bridge calls `approval::authorize` (`src/adapters/rig_engine/approval.rs`). For an approval-ruled tool it registers a pending with a UUID `approval_id` and computes `args_hash` = SHA-256 over the canonical JSON of the exact args (`serde_json` sorts object keys), then waits for the human. The decision is bound to what the human saw: a changed hash is rejected and the tool never runs.
- Discovery differs by path: in a streaming chat the `ApprovalRequired` engine event maps to an SSE `ChatStreamEvent` whose optional `approval` field carries `{ id, tool_name, args_hash }` — raw args are deliberately absent from the stream. In a non-streaming chat there is no SSE channel: the handler resolves up front whether the turn has approval rules and, if so, runs it in a task and answers `202 {"execution_id", "approval_id"?}` once `PARK_GRACE_MS` (default 1000) expires while the turn is still parked. The client then **polls `GET /api/approvals`** and decides via `POST /api/approvals/:id`. `args_hash` may be omitted on the POST; the gate then uses its stored hash, still binding the decision to the exact args. A gated turn that finishes inside the grace answers the ordinary `200 ChatResponse`; a turn with no approval rules stays synchronous and unchanged.
- The wait is **unbounded by default** (park); `APPROVAL_PARK=false` restores a bounded deadline (`APPROVAL_TIMEOUT_SECS`, default 120, `0` = no deadline) and an undecided pending then times out with a named tool error. `400` malformed/mismatched hash, `404` unknown id, `409` already decided, `410` expired. The shared tool-call budget is charged **only after approval succeeds** — a denial, expiry, hash mismatch or timeout spends no allowance.
- The engine-level timeout at the `AgentEngine` boundary (`src/adapters/rig_engine/mod.rs`) is `max(scope.timeout_secs, gate.timeout() + 5s)` **when the request has approval rules and the gate is bounded**; a parked gated turn has **no** outer deadline, so no timer can drop the bridge's pending oneshot on a late decision. With no approval rules the short scope deadline is unchanged, so ordinary runs still time out as before.
- `GET /api/approvals` redacts secret-bearing fields recursively (names containing `key`, `token`, `password`, `secret` or `authorization`, case-insensitively, including inside arrays) to `[REDACTED]`. No credential appears in the endpoints, logs, health or stream. The approval endpoints are **admin-token gated** (`X-Admin-Token`) and still carry no user identity/principal — the token is a shared secret, not a per-user identity. Source modules: `src/services/approval.rs`, `src/services/approval_reaper.rs`, `src/api/approval_api.rs`, `src/adapters/rig_engine/approval.rs`; end-to-end tests in `tests/approval_tests.rs` and `tests/approval_parking_tests.rs`.

## Durable execution and recovery (Fase 7)

An `AgentExecution` (`src/domain/execution.rs`) is one agent **turn**, not one tool-loop step — Rig still owns the loop. The `ExecutionStore` port (`src/ports/execution.rs`) has a SQLite adapter (`src/adapters/execution/sqlite.rs`) over the same `data/history.db`, and a no-op adapter (`src/adapters/execution/noop.rs`, also used under `not(feature = "history")` or when the store cannot be opened). Durability is best effort everywhere: a missing/failed store never fails boot or chat, and observability/idempotency calls that error are logged and skipped.

- **Lifecycle**: a turn is created `Running` when an agent resolves, then finalized to `Completed` or `Failed` on every outcome (engine success, fallback answer, or error). `Cancelled` is a defined status reserved for callers — there is no cancel endpoint today. At boot, one recovery pass (`mark_interrupted`, no background task) finds every turn left `Running`/`WaitingApproval` by a crash, marks it `Interrupted` with a warning, and leaves it retryable. The event log is append-only; shipped code currently appends `Started`, `ToolCallCompleted` (with a `replayed` flag), `ExecutionResumed`, `ExecutionCompleted`, `ExecutionFailed` and `TraceRecorded`. `TraceRecorded` is a unit variant emitted once per finished turn (engine success, fallback answer, or error × stream/non-stream); its data lives in the redacted `ExecutionEvent.payload` as `{trace_id (= execution_id), agent_qualified_id, provider, model, input_tokens, output_tokens, cost_usd, latency_ms}` — plus optional `parent_execution_id`, `root_execution_id` and `depth` for delegated turns (see "Subagent delegation"). `cost_usd` comes straight from the response's `Usage::from_counts` and is `null` while catalog prices are `None` (the calculation exists but no cost is fabricated). `redact_trace_payload` keeps the numeric counters while redacting secrets, and persistence is best effort — a store error is a warning, never a failed chat. Traces are read through the existing `GET /api/executions/:id`; there is no new endpoint. With `OTEL_ENABLED=true` the same redacted fields are also POSTed as one OTLP/HTTP span by a small `reqwest` adapter — there is still no OpenTelemetry crate, and with the default `false` the exporter is a noop. The `EventKind` enum also defines `ModelRequested`, `ToolCallRequested`, `WaitingApproval`, `ApprovalGranted`, `Interrupted` and an explicit `CheckpointCreated` marker for future step-level observability — they are not emitted today and `CheckpointCreated` performs no replay. `seq` is assigned atomically by the store; payloads are redacted on write.
- **Idempotency**: `operation_id = <execution_id>:<tool>:<args_hash_hex>` uses the same canonical-argument SHA-256 as approval binding, so a reordered JSON key hashes identically. Both tool bridges (`src/adapters/rig_engine/operations.rs`) replay a stored result before executing the tool, and record it after a first success; a no-op store is a transparent pass-through.
- **Approvals persist across restart**: the gate mirrors pendings and decisions into the store and rehydrates them at boot without a oneshot; a decision made while no live request existed is consumed by a retry via `decision_for`. The `block_on` in the synchronous gate API is safe only because all current stores are immediately ready — revisit if a real async backend is added. Hash binding still rejects a decision for arguments the human never saw.
- **Recovery is at TURN level — not step replay**: `POST /api/executions/:id/retry` re-runs the whole turn from the stored (redacted) `user_message` under the **original** execution id, with `stream: false` and no reload of prior conversation history. Reusing the id is what makes tool effects replay instead of repeating, but the model regenerates its answer and may take a different path; `current_step` and `CheckpointCreated` are observation only. Exact model/tool step replay is future work. A legacy row with no `user_message` is refused rather than retried with an empty prompt.
- **Endpoints**: `GET /api/executions`, `GET /api/executions/:id`, `POST /api/executions/:id/retry` (X-Debug gated, redacted — see "Execution observability and retry" above).

**Deferred / not done**: no Postgres or other async backend (the port is ready, SQLite is the only adapter); no per-user identity/principal — the approval and execution endpoints are gated by one shared admin token, not by who the caller is; no "first use only" suppression for `Once` (it still behaves like `Always`); no OpenTelemetry crate (there is an opt-in raw OTLP/HTTP JSON exporter); no step-level replay. The `Auto` decision model itself **is** done (gate-time, router-backed, safe degradation). Rig still owns the tool loop, and the HTTP/gRPC/API ports are unchanged.

Source modules: `src/domain/execution.rs`, `src/ports/execution.rs`, `src/adapters/execution/`, `src/adapters/rig_engine/operations.rs`, `src/api/execution_api.rs`, `src/services/approval.rs`, `src/runtime.rs` (boot recovery). End-to-end coverage: `tests/execution_store_tests.rs`, `tests/execution_api_tests.rs`, `tests/execution_recovery_tests.rs`, `tests/approval_tests.rs`.

## Webhook channels (Fase 8)

A signed inbound webhook becomes one agent turn. The sole endpoint is `POST /api/webhooks/:channel`; there is no other new surface. Channels live in the optional `channels.toml` in the base directory, loaded **per request** (like agents and contexts), so an edit takes effect without a restart. A missing file is fine (empty channel set); an unreadable or malformed one is a warning naming the path and an empty set — it never blocks boot.

`channels.toml` is a list of `[[channels]]` entries:

- `id` — the name in the URL path; an unknown or disabled channel is `404`.
- `agent_id` — the required target agent; optional `project_id` scopes it to a project. Both come from config, never from the emitter's headers. The turn is dispatched with a config-only `AgentSelection` whose `debug` is **always `false`**, so a caller cannot widen scope or turn on debug evidence (`debug_prompt`/`debug_route`) through the request.
- `mode` — `"sync"` or `"async"` (default `async`). `sync` runs the turn inline and answers `200` with the `ChatResponse`. `async` mints the execution id **before** spawning the in-process task, answers `202 {"execution_id": "<uuid>"}`, and the turn is observable through `GET /api/executions/:id` (the Fase 7 execution log).
- `template` — optional marker template (see below); omitted or blank means the raw body is the user message.
- `signature_header` — header carrying the signature, default `X-Signature`.
- `auth_env` — the **NAME** of the environment variable holding the shared secret, never the secret itself. `ChannelConfig` has no secret field, so it is safe to derive `Debug` on and to log; the value is resolved from the environment at request time.
- `enabled` — `bool`, default `true`.

**Signature before parse.** The `ChannelSignatureVerifier` port has one adapter, `HmacSha256Verifier` (`src/adapters/channel/hmac.rs`): HMAC-SHA256 over the **raw request bytes** — no parsing, trimming or re-serialising first — accepting `sha256=<hex>` and bare `<hex>`, decoded to 32 bytes and compared in constant time. It is fail-closed: a missing/empty secret, a missing/malformed header or a mismatch all produce the same generic `401 invalid signature`. No body, signature or secret is echoed into a response or a log line; the only warning names the env var, never its value. The port is pure (body, header, resolved secret → `bool`) and checked against RFC 4231 vectors.

**Marker templates, not Liquid.** A template is plain text with `{{path.to.field}}` markers replaced by scalars from the parsed JSON: strings as-is, numbers/booleans as their text form. There is **no Liquid engine, no expression language, no HTML escaping and no new dependency**. A missing/unknown path, a non-scalar (object/array/null) value or an unclosed `{{` is a named `400` error mentioning only the path from the template — never the payload body — so a secret in the payload cannot be echoed back. `message_from_template` keeps the "raw body when no template" rule in one place shared by production and tests.

**Non-claims / not done:** no delivery queue, retries or background reaper — `async` is one in-process spawned turn; no user identity/auth on the endpoint — the signature **is** the auth; no provider-specific signature schemes (one HMAC-SHA256 scheme, one configurable header); no secret or body in logs/errors; no new dependency (HMAC, hex decode and constant-time compare are built on the existing `sha2` crate, `hmac`/`hex`/`subtle` are not added); no sacred-port change; no new endpoint beyond `POST /api/webhooks/:channel`. Remote agents and a sandbox remain undone (see "Subagent delegation (Fase 4)").

Source modules: `src/domain/channel.rs`, `src/ports/channel.rs`, `src/adapters/channel/`, `src/services/marker_template.rs`, `src/api/webhook_api.rs`, `src/runtime.rs` (route + verifier). Coverage: `tests/webhook_tests.rs`.

## Subagent delegation (Fase 4)

An agent can delegate a turn to other agents from TOML alone. The `[subagents]` table is no longer an inert stub; it is a real, typed policy on `AgentConfig` (`Option<SubagentsConfig>`).

- **Manifest** — `[subagents]` takes `allowed` (`Vec<String>`, empty = deny-all), `allow_global` (`bool`, default `false`) and `max_depth` (`Option<u32>`). `max_depth` is the number of delegation levels from the root coordinator: `1` when omitted, valid range `1..=8` (hard ceiling `8`). An out-of-range value is ignored with a warning naming `subagents.max_depth` and the value, then falls back to `1` — never silently clamped. All fields are `#[serde(default)]`, so a v1 manifest that declares `[subagents]` still loads; the table **is** serialized on `PUT /api/agents/:id` so a curated allowlist and depth survive a round-trip (like `routing` and `description`).
- **Effective depth** — the bound that actually applies is the **minimum** of `max_depth` across the whole chain (root plus every ancestor), capped at `8`. It travels in `DelegationContext.effective_max_depth`; each hop tightens it with the target agent's own limit, so no child can widen its own allowance.
- **The tool** — `llama_r__delegate` has schema `{message (required), agent_id?, task_id?}`. `message` must be a non-empty string; `agent_id` and `task_id`, when present, must be non-empty strings too. An explicit `agent_id` is validated against the caller's allowlist; when omitted, the first entry of `allowed` is the fallback target, and an empty allowlist is a named refusal. Delegation is a local tool, so an agent must list `llama_r__delegate` in its `tools_local` to use it — local tools are deny-all by default.
- **Zero inheritance** — the child is resolved only by its id, validated against the allowlist and the same project (or `agents/` when `allow_global = true`). It carries none of the parent's project context, skills, RAG sources or tools: its own manifest and `ScopeBuilder` produce a `DenyAll` scope. A disallowed id is a named `ToolError`, never a delegation.
- **Depth and cycles** — a hop whose `depth > effective_max_depth`, or whose target qualified id is already in the ancestor `chain`, is rejected with a named `ToolError` and zero child turns executed.
- **One shared budget** — every turn in the tree spends from the same `Arc<dyn ToolBudget>` (an `Arc<Mutex<ToolCallBudget>>` behind the trait object) carried in the `DelegateRequest`; there is one allowance for the whole tree, no matter how deep. Exhausting it is a named `ToolError` that bubbles up; charging still happens only after `authorize → Ok`.
- **Approvals** — the child uses the same global `ApprovalGate`. From any level, an `ApprovalRequired` event is forwarded to the root coordinator's SSE with `task_id` and `depth` in the `approval` payload (additive optional fields), origin preserved across deeper hops. With no streaming there is no sender: the pending still appears in `GET /api/approvals` and is decided via `POST`, unchanged.
- **Hierarchical traces** — each turn emits its own `TraceRecorded`; the payload gains optional `parent_execution_id`, `root_execution_id` and `depth` so `GET /api/executions/:id` reconstructs the tree. Redaction is the same `redact_trace_payload` as Fase 6; no new backend.
- **Failure isolation** — a missing child, down provider, exhausted budget or timeout is a named `ToolError`; the parent's loop continues and the model decides how to answer, never a root chat error. A failed `Weak::upgrade` (state already dropped) is a named error, not a panic. Idempotency via `operation_id` means a retry replays child tool effects instead of re-running them, and the recursive deadline (`remaining − 5s`) makes the child's named timeout arrive before the parent loop is cancelled.

Source modules: `src/ports/delegation.rs`, `src/services/delegation.rs`, `src/adapters/rig_engine/local_tools.rs`, `src/domain/agent.rs`, `src/api/chat_core.rs` (`delegate_turn`), `src/runtime.rs` (`RuntimeDelegator` in `CompanionHandles`, injected via `Weak<AppState>`). Coverage: `tests/subagent_tests.rs`.

**Not done / non-goals:** no remote agents or sandbox (Fase 8); no message steering into a running child; no TUI changes; no OpenTelemetry dependency (the opt-in exporter is raw OTLP/HTTP JSON over `reqwest`); no change to agents that do not declare `[subagents]` — v1 manifests keep their exact behaviour; no new endpoint.

## Admin auth, approval parking and OTLP telemetry (Fase 9 closure)

Three opt-in capabilities share one rule: `Config` (`src/config.rs`) is the
single source of truth for their environment variables. `from_env` parses them,
`save_to_env` persists them to `.env` (mode `0600`, so the admin token stays a
secret), `CONFIG_ENV_VARS` guards the test isolation, and `AppState.config` is
what the middleware, chat handler, gate, reaper and exporter read at runtime —
no helper re-reads the environment per request. Malformed numbers are a config
error naming the variable (the reaper interval is the one exception: a numeric
but non-positive value falls back to `5`, matching its old helper).

1. **Admin token.** One `from_fn_with_state` middleware (`src/api/admin_auth.rs`)
   wraps the admin sub-router in `build_router`: `/api/approvals*`,
   `/api/executions*`, `/api/rag/*`, `/api/route`, `/api/contexts*`,
   `/api/agents*`. A caller presents `X-Admin-Token`; the value is compared in
   constant time with `state.config.admin_token`. Missing or empty token fails
   **every** admin route closed with a fixed, generic `401` that never echoes
   the value. `X-Debug` is additional, not replaced. `/chat`, `/api/chat`,
   `/v1/chat/completions`, `/api/webhooks/:channel`, health, models, docs stay
   public. Conversation-history endpoints are **admin-gated** (`X-Admin-Token`)
   as of Fase 10-A (breaking change).
2. **Approval parking.** `Config::approval_timeout()` returns `None` when
   `APPROVAL_PARK` is on or `APPROVAL_TIMEOUT_SECS` is `0`, so a pending human
   decision never expires (`ApprovalGate::parked()`); a non-streaming gated
   turn is answered `202 {"execution_id", "approval_id"?}` after
   `PARK_GRACE_MS`. The `WaitingApproval` status, `parked_live` index and boot
   reaper (interval `REAPER_INTERVAL_SECS`) make a decided orphan resumable
   under its original execution id with no double resume. See "Human-in-the-loop
   approval" above for the full flow.
3. **OTLP telemetry.** `exporter_from_config` (`src/adapters/telemetry/mod.rs`)
   returns a noop unless `OTEL_ENABLED` is truthy. When enabled it POSTs the
   already-redacted `TraceRecorded` value as one OTLP/HTTP JSON span over the
   existing `reqwest` dependency. It is best effort: a collector failure never
   fails boot or a chat, and no credential, argument or body is sent.

| Variable | Default | Meaning |
|---|---|---|
| `ADMIN_TOKEN` | — (fail closed) | Shared secret for `X-Admin-Token`; empty/absent = no admin access. In `.env` mode `0600`, never logged or echoed. |
| `APPROVAL_PARK` | `true` | Truthy (`1`/`true`/`yes`/`on`) = no approval deadline; `false` restores the bounded timeout. |
| `APPROVAL_TIMEOUT_SECS` | `120` | Gate deadline when parking is off; `0` = no deadline. Malformed = config error. |
| `PARK_GRACE_MS` | `1000` | Milliseconds before a parked non-streaming turn answers `202`. Malformed = config error. |
| `REAPER_INTERVAL_SECS` | `5` | Orphan-reaper period; numeric non-positive falls back to `5`, malformed = config error. |
| `OTEL_ENABLED` | `false` | Opt-in OTLP/HTTP span export; no requests when off. |
| `OTEL_ENDPOINT` | `http://127.0.0.1:4318/v1/traces` | OTLP/HTTP traces URL, read only when enabled. |
| `OTEL_SERVICE_NAME` | `llama-r` | `service.name` resource attribute on each span. |

Source modules: `src/config.rs`, `src/api/admin_auth.rs`, `src/api/handlers.rs`,
`src/services/approval.rs`, `src/services/approval_reaper.rs`,
`src/adapters/telemetry/`; coverage in the `src/config.rs` unit tests,
`tests/admin_auth_tests.rs`, `tests/approval_parking_tests.rs`,
`tests/otlp_exporter_tests.rs`.

**Non-claims:** no OpenTelemetry crate or SDK (raw OTLP/HTTP JSON via `reqwest`);
no user identity/principal (one shared token, not per-user auth); no sandbox or
remote agents; no external workflow engine (the reaper is one in-process task);
no new endpoint beyond the existing admin group and no change to the chat,
webhook, gRPC or OpenAI-compatible contracts; no new dependency; no sacred-port
change.

## Optimization tools

- `src/optimizer/tools.rs` holds the catalog (`TOOLS`) and every implementation. The picker offers that list and nothing else, so what is offered and what is implemented cannot drift.
- `apply_rule`'s old AST round-trip is gone. A tool is handed the text and returns the text; it locates what it cares about with `fenced_blocks` and splices the **original** bytes. Re-emitting every node re-tagged untagged JSON blocks as ```` ```json ````, and made the text outside blocks one opaque node that no tool could touch.
- `compress_code` strips comments per language family. `#` is a comment in Python and a preprocessor directive in C, so a block whose language is not in the table has **nothing** stripped — which is what stopped it deleting `#!/bin/bash`. A shebang is never stripped.
- `dedent_code` skips languages where indentation is meaning (`python`, `yaml`, `toml`, …) and skips untagged blocks: an untagged block of Python is still Python.
- `[subagents]` in an agent TOML used to be an inert stub; it is now real, bounded delegation — see "Subagent delegation (Fase 4)" above.
- A tool name that matches nothing used to be a silent no-op. `AgentRegistry` now warns with the unknown and the known names when it loads an agent, and the picker drops unknown names on save after showing them.
- `optimize.enabled` follows the selection: the form used to write `true` unconditionally, producing configs that claimed to be optimising while running nothing.

## Routing hints

- `[routing]` in an agent TOML takes one field, `hint` (`AgentConfig.routing: Option<RoutingConfig>`). When non-blank it **replaces** the whole criteria text the router scores that agent against (`derive_criteria_text`) — one curated sentence instead of the derived `name — description` plus `Skills:`/`Tools:`/`Sources:` sections. It is the only curated lever on routing quality; nothing else in the manifest is read by the router except the id, the name and those sections.
- `CreateAgentRequest` carries `routing` and `description`, and `build_agent_toml` emits both, so **`PUT /api/agents/:id` preserves a curated hint and description**. `auto_skills` is deliberately absent from the request: it is system-managed and a client cannot set it.

## Skills

- A skill is a directory containing `SKILL.md` with YAML frontmatter (`name`, `description`, optional `tags`). **The id is the directory name**, not `name:`.
- Discovery uses one shared list (`SKILL_DIR_NAMES` in `src/services/skill_manager.rs`): `skills/`, `.agents/skills/`, `.claude/skills/`, `.cursor/skills/`, `.agent/skills/` — searched under `~` globally and under the project root per project.
- `SkillScope` is assigned by *how* a skill was found, not by a path table: `Project` > `LlamaR` > `Harness` (Claude/Cursor/Windsurf). Shadowing is recorded and logged (`SkillManager::shadowed`), never applied silently.
- Assignment in the agent TOML: `skills` (manual) and `auto_skills` (chosen by `sync_project_agent_skills` from the project profile).
- **Local tools and on-demand skills** are two more agent TOML fields, both `Vec<String>` with a serde default: `tools_local` names the local tool ids the agent may call, and `deferred_skills` names declared skills whose body the model must load on demand. They are ordinary manifest settings — an agent's TOML alone opts in; no code change or restart is needed.
- Two built-in local tools always exist: `llama_r__skill` (`{name}`) returns a skill's full `SKILL.md` via `SkillManager::get_skill`, and `llama_r__rag_query` (`{query}`, optional `top_k`) runs `RagStore::query_scoped`. For `rag_query` the request's `AgentScope` is the authoritative isolation boundary — a `source_id` from a `tools/*.toml` manifest is informational and can never widen the query beyond `scope.rag_sources`; `top_k` is clamped to `1..=6`. A missing skill, an empty RAG scope or a missing store is a named refusal, never a silent empty success.
- **Deny-all by default.** `tools_local` is compiled by `ScopeBuilder` into `AgentScope.local_tools_allow`; empty means `DenyAll`, `*` means all, anything else is an allowlist of bare ids. `prepare()` only offers a registered tool whose id the scope allows, and `LocalToolBridge::invoke_json` re-checks that same predicate on every call before charging the **shared** `ToolCallBudget` (the one MCP bridges spend) and dispatching the executor. A registered tool is not an offered tool, and an offer is not a standing grant.
- **Deferred skills** are announced in the system prompt as `id — description` with an instruction to call `llama_r__skill({name})`; the `SKILL.md` body is absent until the model actually calls the tool. Non-deferred skills keep their full injection byte-for-byte, and relevance ranking is unchanged: it still narrows the candidate set first, and a deferred id that was not declared is warned about and ignored. The tools themselves exist only on the **agent-with-tools path**: a direct model request with no agent resolved uses the legacy provider path, and `prepare()` skips discovery for a resolved model whose capabilities say it cannot call tools (so no local tool is offered and the body cannot be loaded). The announcement text itself is rendered by the enricher from the manifest and is not capability-gated. `tests/agent_runtime_tests.rs` drives the whole seam end to end against a fake Ollama socket.
- **Relevance ranking** (`SkillIndex`) narrows the declared skills using embeddings. It is deliberately not a `RagStore` collection: `query_scoped` only reads `scope.rag_sources`, so indexing skills there would force every agent manifest to list a shared catalog. Manual skills get a ranking boost, not a guarantee; anything dropped is logged with its score. Without embeddings (or on provider failure) every declared skill is used.
- **Generation**: `analyze` proposes new skills (`AnalysisState::Proposals`). Nothing is written until approved in the TUI. Approved proposals go to `<project>/skills/<id>/SKILL.md` with `generated_by: llama-r` in the frontmatter. Regeneration only replaces files carrying that marker, so a hand-written skill sharing an id is never clobbered.
- Modules: `src/services/skill_manager.rs`, `skill_index.rs`, `skill_generation.rs`, `agent_skill_sync.rs`

## RAG

- Disk-backed `FileRagStore` implements the `RagStore` port (LanceDB deferred: heavy arrow/datafusion stack).
- Embeddings: `OllamaEmbeddings` batches all texts into one `POST /api/embed` call (current Ollama API) and falls back to the legacy `POST /api/embeddings` when that endpoint returns 404. Providers with no native embeddings adapter (`opencode`, `anthropic`) get a fallback: `OLLAMA_URL` when set (preferred), otherwise an OpenAI-compatible endpoint from `OPENAI_BASE_URL` authenticated with `OPENROUTER_API_KEY` (`POST /v1/embeddings`; OpenRouter-hosted or `/api/v1` origins use that adapter's path). Neither fallback is configured → embeddings are disabled and `/api/rag/*` reports `501`. The key is sent in a header, never in a URL, log or health response.
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
- Debug API (requires both `X-Debug: true` and the shared `X-Admin-Token`):
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
  -H 'X-Admin-Token: <token>' \
  -d '{"agent_id":"demo","source_id":"agent:demo/memory","texts":["Refunds within 24h."]}'
curl -s http://127.0.0.1:3000/api/rag/query \
  -H 'Content-Type: application/json' \
  -H 'X-Debug: true' \
  -H 'X-Admin-Token: <token>' \
  -d '{"agent_id":"demo","query":"refunds","top_k":3}'
```

## Conversation history

All eight known gaps from the Fase 5 plan (§0.7) are closed — seven in code, gap 8 by documented decision plus a concurrency test. The store architecture is unchanged.

- Feature: `history` (default ON). Disable: `cargo build --no-default-features --features rig-engine`
- DB: `{LLAMA_R_DIR}/data/history.db` (gitignored). The schema is additive / `IF NOT EXISTS`: no destructive migration, and an old `history.db` opens unchanged.
- Continue a thread: request header `X-Conversation-Id` (gRPC metadata with the same name). When a conversation is actually persisted and no id was supplied, the server generates a UUID.
- Return id: `ChatResponse.conversation_id: Option<String>` (`skip_serializing_if = "Option::is_none"`) **and** an `X-Conversation-Id` response header on both the JSON and SSE chat paths, so a client can send the id back. Absent when persistence is off or nothing was written. Every write — and the emitted id — is gated on the id `append_user` actually returned (`persisted_conv_id`), so a failed store never yields a phantom id. An old client that omits the field still parses.
- Persist follows the agent `[memory] persist_history` flag. Best-effort everywhere: a summarize, append, tool-event or purge failure logs a warning and never fails the chat.
- Identity: the persisted `agent_id` is the agent's file id (`agent.id`, filename without `.toml`), never the display `name`; two agents sharing a display name keep separate histories.
- Summaries: set `index_summaries = true`; chunks go to `summary_collection` or `agent.memory_source_id()` if RAG write policy allows. `maybe_summarize_and_index` takes the current turn's `conversation_id`, so a summary targets **that** conversation (not the agent's most recent one) and is idempotent per `turn_range_end`. The summarizer is an injected `Summarizer` trait (`src/ports/history.rs`): the Ollama boot provider gets `OllamaSummarizer` (Ollama-wire `/api/chat`), every other provider gets `NoopSummarizer` with a warning naming the lost capability. Conversations still persist with no summarizer; `FakeSummarizer` covers the happy path in tests without a network.
- Tool events: `append_tool_event(conversation_id, tool_name, success)` stores the tool name and success flag **only** — never arguments or output, so no secret can reach the table. An additive `AgentRunRequest.tool_event_tx` sink is shared by the MCP and local tool bridges; streaming and non-streaming turns use the same mechanism.
- Retention: `HISTORY_RETENTION_DAYS` (default 90); purge on boot and daily. An agent's `[memory] retention_days` overrides the global value for its own conversations; the per-agent map is rebuilt from `AgentRegistry::list()` at boot and on every daily tick, so a hot reload is honoured without a restart. `HISTORY_RETENTION_DAYS=0` is an **absolute kill-switch**: purging is disabled entirely and per-agent entries cannot re-enable it.
- Concurrency (gap 8 — decision, not refactor): the store keeps one `std::sync::Mutex<Connection>` in WAL mode; short synchronous work holds no lock across `.await`. There is deliberately no `tokio::sync::Mutex`, no per-query `spawn_blocking` and no async refactor — those are reconsidered only with **measured** contention. `concurrent_appends_keep_both_rows` pins that two independent callers both land their rows.
- Admin API: `GET/DELETE /api/conversations`, `GET /api/conversations/:id/messages`, `POST /api/conversations/:id/export`
- Not done (deferred, unchanged): no re-injection of conversation-store summaries into prompts (the active-context `## Conversation memory` block is a separate compaction summary — see below), no OpenTelemetry crate (the opt-in exporter is raw OTLP/HTTP JSON), no admin token on the conversation-history endpoints (they stay public — only the approval/execution/RAG/route/agent/context admin group is gated).
- Execution plan: [`PHASE_5_HISTORY_PLAN.md`](./PHASE_5_HISTORY_PLAN.md)

## Active context and model routing (Fase 5)

Two additive ports — `ContextCompactor` (`src/ports/compaction.rs`) and `ModelSelector` (`src/ports/model_selection.rs`) — plus `ActiveContext` (`src/context/active.rs`) and the `ContextAssembler` (`src/services/context_assembler.rs`) let a turn respect the model's real context window and pick a model by complexity, both with fail-closed degradation to the previous behaviour. No sacred port (`AgentEngine`, `LLMProvider`, `DecisionRouter`) changed.

- **`model_candidates`** — an agent TOML field (`Vec<String>`, `#[serde(default)]`, empty = no change). When it is empty the turn runs on `model` (or `DEFAULT_MODEL`) exactly as before and the selector is never consulted; `PUT /api/agents/:id` preserves it like `routing`/`description`/`subagents`.
- **Optional `models.toml`** in the base directory maps `model = <integer rank>` (`relative_cost: u32`, e.g. `1` cheap, `3` expensive). The rank is an **ordinal**, used only to break ties inside the tier the router chose (`simple` → lowest rank, `complex` → highest; a tie keeps the first declared candidate). A candidate absent from the table ranks `1`. A missing file means no table; an unreadable or malformed one warns naming its path and leaves an empty table — boot continues. The file holds only model names and integers, no credentials and no prices.
- **Budget** — `min(capabilities.max_context_tokens, agent.max_context_tokens)`: a missing (or zero) side does not constrain it, and with neither known there is **no compaction** (the original history passes through, the compactor is not even consulted). `reserved_tokens` (`DEFAULT_RESERVED_TOKENS`) is set aside for tool schemas and retrieved RAG documents. Only conversation **history** is compacted — the oldest turns are summarized while the newest suffix is kept, and the newest turn is never dropped.
- **Memory placement** — a compactor summary is appended to the system prompt as a `## Conversation memory` section **after** the instruction sections and **before** the history handed to the engine. Instruction/rule/skill/project/RAG sections are built once and re-injected byte-for-byte, so they are identical with or without compaction; a caller-supplied (already optimized) prompt is likewise kept verbatim.
- **Degradation** — the selector uses the same injected `DecisionRouter` Arc as agent routing and `Auto` (no second router instance), configured through `ROUTER_URL`/`router.toml`. With routing off or no router available it is `CurrentModelSelector` and keeps the current model; a router error, `Noul`, missing answer, non-finite or low confidence, or a choice naming an undeclared model likewise keep the current model with a warning. A compactor error logs a warning and falls back to the original prompt and history — the chat never fails. Boot installs `NoopCompactor` by default; deployed summarization awaits a real strategy.

**Non-claims:** no real provider prices — `relative_cost` is an ordinal and never becomes `cost_usd`, and nothing is written into `ModelCapabilities`; no OpenTelemetry crate (the opt-in exporter is raw OTLP/HTTP JSON); no compaction of internal tool or RAG prompts (history only); no hardcoded decision model — the question uses the tier keys `simple`/`complex` and the model is whatever the configured router is; no change to agents that do not declare `model_candidates`; no new endpoint. A delegated child (Fase 4) inherits the budget but assembles **its own** `ActiveContext` from its own manifest — it never inherits the parent's context, just as it inherits zero scope. The HTTP chat path supplies its already-optimized instructions via `assemble_with_instructions`, so metrics stay truthful and optimization tools are not run twice.

Source: `src/context/active.rs`, `src/services/context_assembler.rs`, `src/ports/{compaction,model_selection}.rs`, `src/adapters/context/`, `src/providers/model_costs.rs`, `src/runtime.rs` (`build_model_selector`, companions), `src/api/agent_api.rs`. Coverage: `tests/context_pipeline_tests.rs`.

## Notes For Contributors
- Prefer documenting commands that exist in `src/cli/commands.rs`.
- Keep `AGENTS.md`, `README.md`, and generated rule exports aligned when workflows change.
- If you add a new CLI command or endpoint, update this file with the command, whether it requires the server, and what it reads or writes on disk.

