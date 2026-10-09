# Agent router (decision-model orchestration) — design

**Date:** 2026-10-08
**Status:** proposed
**Scope:** a routing step for project-scoped chat requests that arrive without an
`X-Agent` header. Adds a decision-model call, one port, one adapter, two services and
one config file. Does not change the TUI, and does not add a new execution path.

> **Cite this document by section, not by line.**

## Problem

A chat request that carries `X-Project` but no `X-Agent` does not get an agent chosen.
Today `chat_core::engine_target` returns `None` for it, the request skips the agent
engine entirely, and it lands on the legacy path with `DEFAULT_MODEL` — no project
context, no rules, no RAG, no tools.

Meanwhile the specializations a project *does* have sit unused:

```
POST /api/chat   X-Project: fudi      (no X-Agent)
  → DEFAULT_MODEL, project context dropped
  → contextos/projects/fudi/agents/fudi-ops.toml never consulted
```

`AgentConfig.description` already exists (`src/domain/agent.rs:14`) and is written
for exactly this purpose — "Human-readable description for UIs and MCP tool listings" —
but nothing consumes it for selection. The registry already holds every agent for a
project in one place, already hot-reloads, and already carries a resolved `AgentScope`
per agent.

The fix is not to add a master agent that *talks*. It is to add a decision step that
*selects*, before the existing dispatch runs.

## Why a decision model

Liquid's d1 family (d1-3B, d1-omni-600M) does not generate tokens. It evaluates a
state against a fixed set of typed questions in one forward pass and returns calibrated
probabilities — no text parsing, no schema validation, no retry on malformed output.
Routing is one of the three documented use cases, and the one this feature is.

Two properties matter here beyond speed:

- **Calibrated confidence.** A generative router emits a label and nothing else. d1
  returns a probability distribution over every candidate, so "I am not sure enough to
  trust a specialist" is a decision the router is allowed to make.
- **Zero output tokens.** The routing cost is one forward pass, not a generation. On the
  hardware Liquid reports (RTX 4090: 8 ms; Jetson AGX Orin: 26 ms) this is noise against
  an LLM turn.

**d1 cannot be served by Ollama.** Ollama exposes generative `/api/chat` and
`/api/generate`; a non-generative model has nothing to call. Liquid's model matrix lists
`d1` as API-only, and the open d1-3B weights are published with GGUF builds for
**llama.cpp** day one. So the router runs against a separate `llama-server` process:

```bash
llama-server -hf LiquidAI/d1-3B-GGUF:Q8_0     # serves POST /v1/systemone on :8080
```

This is a second inference process. It is opt-in, off by default, and its absence is
never fatal (§ Errors).

## Decisions

| Decision | Choice | Why |
|---|---|---|
| Mechanism | d1 decision model over llama.cpp `/v1/systemone` | Single forward pass, calibrated probabilities, zero output tokens. Ollama cannot serve it, so a separate local process is unavoidable. |
| Hosted API (Liquid / OpenRouter / Vercel) | Rejected | Would add a network dependency and an API key to a repo whose current posture is fully local. Revisit if local inference proves too heavy. |
| Scope of v1 | Single-hop routing + confidence threshold | A real orchestrator (handoff chains, fan-out) is a different subsystem. v1 is one extra decision per request. |
| Candidates | The project's agents plus global agents | Scoped by the `X-Project` header, so routing can never cross projects. See § Candidates. |
| "Nothing fits" destination | The project's general agent | Preserves the strict validation rule in `AGENTS.md`. See § The strict validation consequence. |
| Candidate text | Derived from the manifest, with a curated override | Zero authoring cost for existing agents; a field for the ones whose derived text is ambiguous. |
| Config | Env for infrastructure, optional `router.toml` for per-project tuning | Matches the repo's two existing patterns: env for infrastructure, TOML for content. |
| Default | `ROUTER_ENABLED=false` | Updating llama-r must not change its behaviour. |

## Architecture

The router does not own the request. It fills the hole `X-Agent` would have filled, and
then everything downstream runs exactly as it does today.

```
handler
  │
  ├─ resolve_selection()          ← NEW. The only new step.
  │     X-Agent present      → use it (today's behaviour)
  │     X-Project, no X-Agent → ask the router
  │     neither header       → today's behaviour (bare DEFAULT_MODEL)
  │
  ↓ (project_id, agent_id), owned and concrete
  ↓
prepare_request → AgentRuntime (Rig) → RAG → history → SSE / plain
```

Because the router produces the same `(project_id, agent_id)` pair the header would
have produced, **nothing downstream changes**. No new execution path is introduced; an
existing one is selected. This is why the change is expected to be low-risk: the blast
radius is the selection step and the modules it newly depends on.

### Files

| File | Role |
|---|---|
| `src/ports/router.rs` | **New.** `DecisionRouter` trait and the request/answer types. |
| `src/adapters/router/llamacpp.rs` | **New.** The only place that knows llama.cpp's JSON. |
| `src/adapters/router/mod.rs` | **New.** Module root. |
| `src/services/route_candidates.rs` | **New.** Derives candidate set and per-agent routing text. |
| `src/services/agent_router.rs` | **New.** The decision ladder. Pure; no I/O. |
| `src/api/chat_core.rs` | **Changed.** `resolve_selection()` before `engine_target()`, in both `execute_chat` and `execute_chat_stream`. |
| `src/runtime.rs` | **Changed.** Constructs the router, probes it once at startup, and adds it to `AppState`. |
| `src/api/handlers.rs` | **Changed.** `AppState` gains `router: Option<Arc<dyn DecisionRouter>>` and `router_config: Arc<RouterConfig>`. |
| `src/api/observability.rs` | **Changed.** Four routing counters. |
| `src/domain/agent.rs` | **Changed.** Optional `routing` field on `AgentConfig`. |
| `src/api/route_api.rs` | **New.** `POST /api/route` dry-run endpoint. |
| `src/api/health.rs` | **Changed.** `HealthResponse` gains a `router` field. |

`src/services/agent_router.rs` holds no I/O: it maps `(candidates, DecisionResponse,
thresholds)` to a routing outcome. Every branch of the ladder is therefore testable
without a server, and the adapter's wire format is testable without a ladder.

## The decision ladder

```
X-Project present, X-Agent absent, router enabled
  │
  ├─ candidates = project agents + global agents
  │   (the project's general agent is a candidate, flagged)
  │
  ├─ empty candidate set ─────────────────────────────► 400
  │                                                      (today's behaviour)
  ├─ POST /v1/systemone { state, questions }
  │     ├─ error / timeout / unparseable ──────────────► ProjectGeneral
  │     ├─ returned key not in candidate set ──────────► ProjectGeneral
  │     │
  │     ├─ noul("specialized") < MIN_SPECIALIZED ──────► ProjectGeneral
  │     ├─ choice.confidence < MIN_CONFIDENCE ─────────► ProjectGeneral
  │     ├─ winner is the project's own general agent ──► ProjectGeneral
  │     └─ winner is a specialised agent ──────────────► that agent
  │
  └─ project has no general agent ─────────────────────► 400
```

Every rejection lands on `ProjectGeneral` rather than on an error, except the two 400s
that preserve today's contract. A router that is down, slow, or unsure degrades to
exactly what a request with an explicit `X-Agent: <project>` would have done.

### The key validation guard

The returned key is validated against the candidate set before use. A decision model
returns the highest-probability label; it does not return a certified member of the set
it was given. A key that is not in the candidates is discarded and the ladder falls to
`ProjectGeneral`.

This is the module's security-relevant invariant and it gets its own test. It is the
same defence-in-depth posture as `AgentScope`: the router's output is treated as
untrusted input to the dispatch, not as a decision that has already been made.

## The two questions

One request, one forward pass:

```json
{
  "state": "<recent conversation turns>",
  "questions": {
    "agent": {
      "type": "choice",
      "instructions": "Which agent should handle this request? Pick the general agent if no specialisation fits.",
      "criteria": {
        "fudi::fudi-ops": "Fudi Ops Assistant — Opera pedidos y menú de Fudi vía MCP…",
        "fudi::fudi":     "Fudi — Asistente general del proyecto Fudi"
      }
    },
    "specialized": {
      "type": "noul",
      "instructions": "Does this request need a specialised agent?"
    }
  }
}
```

The two are not the same question asked twice:

- **`choice.confidence`** — "how sure am I of my best candidate?" Governs *trust*.
- **`noul.specialized`** — "is any specialisation warranted at all?" Governs *strength*.

A `choice` can sit at 0.9 confidence on a specialised agent while `specialized` returns
0.2: the model recognises the topic but concedes the general agent handles it. A single
threshold cannot distinguish that from a case where it is 0.9 on both. And because d1
evaluates every question against the same state in one pass, the second question is
close to free — Liquid's own multi-question example makes three separate decisions in
what an LLM pipeline would spend three sequential calls on.

On low confidence this ladder falls back to the **general** agent. Liquid's routing
example falls back to the **most capable** option instead; that is a deliberate
difference, not an oversight. Here "most capable" would be a specialist, so deferring to
one on low confidence would route to a specialised agent *because the router was
unconfident* — which is precisely what the threshold exists to prevent. The general agent
is the correct destination for uncertainty here: it is unspecialised by definition.

`Choice.probabilities` also gives the runner-up, so "close call" (top two within a small
margin) is detectable rather than invisible. `debug_route` reports it.

### Conversation state

`state` carries the conversation, not only the last message. d1-3B has a 32K context, and
this avoids persisting a routing decision anywhere: routing stays stateless, and a thread
that continues coherently does so because the router can see the thread. No stickiness
table, no new storage.

Concretely, `state` is built from the request's own `messages` array — the same messages
the rest of the pipeline uses, not a separate fetch. Stored history reached via
`X-Conversation-Id` is **not** read by the router: the router does not depend on
`ConversationStore`, and a client that sends history in the body gets it either way.
Turns are truncated to a token budget if the conversation is long, keeping the most
recent.

The consequence to document: **the router runs per request, not per conversation.** The
chosen agent can change between turns. That is deliberate for v1 and is the main thing
a future stickiness mechanism would address.

## Candidates

Candidate set for project `p`:

- every registered agent with `project_id == Some(p)`
- plus every registered agent with `project_id == None` (globals)

The project's general agent (`contextos/projects/p/agents/p.toml`) is **a candidate**,
flagged so the ladder can recognise it as its own winner.

### Routing keys

A `Choice` question's `criteria` is a map, so keys must be unique. A global `ops` agent
and a project `fudi/ops` agent would collide under a bare id.

Routing keys are therefore **the registry's own keys** (`registry_key` in
`src/services/agent_registry.rs`): `project::agent` for project agents, bare `agent` for
globals. No second key scheme is introduced, and `get_project_agent` / `get_agent` both
remain the way to resolve a chosen key back to an agent.

### Derived text

Each candidate's `criteria` description is derived from fields that already exist:

```
{name} — {description}
Skills: {skills}, {auto_skills}
Tools: {tools_override}
Sources: {mcp_sources}
```

Size is not a constraint — d1-3B takes 32K — and tools plus skills are frequently the
sharpest routing signal, since they name the actual capability surface ("facturas"
versus "pedidos").

Sections with no content are omitted.

When an agent has no usable description — `description` defaults to empty, and nothing
writes it — the agent's **id stands in for them as the header** and the `Skills:` /
`Tools:` / `Sources:` sections still follow, and a warning is logged at
candidate-build time naming each such agent.

The id replaces only the missing `name` and `description`, not the sections.
Discarding the sections too would hand the model `"fudi::ops"` and nothing else —
no capability signal at all, for precisely the agents that are already the weakest
— while a sibling agent missing only its `description` keeps them. Consistency
with that sibling arm is the rule: the header degrades, the signal survives.

The alternative (dropping the candidate) was rejected because it turns a request that
succeeds today into a `400`: a project whose agents all lack `description` would have
its candidates emptied, hit the empty-set rung, and fail — while the same request works
today by resolving the general agent directly. An agent with a weak description routes
badly. An agent that cannot be routed at all is worse, and is silent. The warning makes
the weakness visible without removing the capability.

### Curated override

New optional field on `AgentConfig`:

```toml
[agent.routing]
hint = "Cancela pedidos, gestiona Menú y consulta el ERP de Fudi. No habla de nutritional."
```

When present, `hint` **replaces** the derived text for that agent. One field, not a
schema: the derived text is a floor, and `hint` exists only for the agents where that
floor is not enough.

## Ports

```rust
#[async_trait]
pub trait DecisionRouter: Send + Sync {
    async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, AppError>;
}

pub struct DecisionRequest  { state: String, questions: BTreeMap<String, Question> }
pub enum    Question        { Choice { instructions: String, criteria: BTreeMap<String, String> },
                              Noul   { instructions: String } }
pub struct DecisionResponse { answers: BTreeMap<String, Answer> }
pub enum    Answer          { Choice { key: String, confidence: f32,
                                        probabilities: BTreeMap<String, f32> },
                              Noul(f32) }
```

This mirrors the repo's existing `ports` / `adapters` split (`RagStore`,
`ConversationStore`, `AgentEngine`). It carries all three d1 primitives' *shape* but
only the two the ladder uses; adding a third later does not change the trait.

One asymmetry to preserve if `Score` is ever added: `Choice.criteria` is a **map** of
label → description, and the returned `choice` is one of its keys. `Score.criteria` is an
**ordered list**, and its levels are indexed from 0 in the order given — a score over a
four-level rubric reads 0..3. They are not the same shape, and `Score` must not be added
as `criteria: BTreeMap<..>`.

The port takes questions rather than a routing-specific signature so that the adapter's
job stays "translate to `/v1/systemone`" and the service's job stays "decide what to
ask, and what to do with the answer".

## Adapter

`LlamaCppRouter` issues `POST {ROUTER_URL}/v1/systemone` with `reqwest`, under
`ROUTER_TIMEOUT_MS`. It deserialises into the `DecisionResponse` types and converts to
the port's `Answer` enum. Nothing above it sees HTTP, JSON keys, or status codes.

## Configuration

Infrastructure by environment, alongside `PORT` / `OLLAMA_URL` / `DEFAULT_MODEL`:

| Variable | Default | Meaning |
|---|---|---|
| `ROUTER_ENABLED` | `false` | Master switch. Off by default. |
| `ROUTER_URL` | `http://127.0.0.1:8080` | `llama-server` base URL. Note the port differs from Ollama's. |
| `ROUTER_MODEL` | `d1-3b` | Model label sent to the decision endpoint. |
| `ROUTER_TIMEOUT_MS` | `3000` | Budget for one decision. Exceeding it falls to `ProjectGeneral`. |
| `MIN_CONFIDENCE` | `0.60` | Below this, the winner is not trusted. |
| `MIN_SPECIALIZED` | `0.50` | Below this, no specialisation is warranted. |

Plus an optional `router.toml` at the base directory (`LLAMA_R_DIR`, else cwd), holding
the two thresholds and an optional `exclude` list of agent ids to keep out of routing.
Env values are defaults; `router.toml` overrides them when present.

`router.toml` is read at startup, alongside the other base-directory config, and is not
hot-reloaded in v1.

## Errors

**A missing router is not a startup failure.** This follows the rule already in
`AGENTS.md` for the provider: *"The provider being unreachable is not a startup
failure."* With `DEFAULT_MODEL` set, llama-r boots, logs a warning, and serves. Chat
requests that would have been routed degrade to the project's general agent.

- `GET /api/health` gains a `router` field: `{ "enabled": bool, "healthy": bool, "url": … }`.
  `status` becomes `"ok"` only when the provider is healthy **and** either the router is
  disabled or the router is healthy; an enabled-but-unreachable router reports
  `"degraded"`, for the same reason an unreachable provider does.
- Router health is **last-known-value**, not a live probe. `health()` already makes a
  live provider call on every request, and a second HTTP round trip per health check
  would be a poor trade. A `AtomicBool router_healthy` is set by the ladder on every
  decision and probed once at startup, mirroring how `api_running` and `grpc_running`
  are already handled. It reports what routing has actually been doing, which a probe
  cannot.
- Timeout, connection refused, non-2xx, unparseable body, and a missing expected answer
  key are all one outcome: `ProjectGeneral`. They are counted and logged distinctly, so a
  silently broken router is still diagnosable.
- No client-visible failure mode is added. A request that succeeds today still succeeds.

## Observability

- **`AppObservability`**, following the existing `AtomicU64` pattern:
  `routed_to_agent`, `routed_to_general`, `router_errors`, `router_latency_ms_total`.
  All four appear in the existing snapshot.
- **`tracing::info!`** per decision: project, chosen key, confidence, specialised score,
  candidate count, router latency.
- **`X-Debug: true`** adds a `debug_route` field to `ChatResponse`, next to the existing
  `debug_prompt`, carrying the full distribution rather than only the winner. This is what
  makes a bad threshold diagnosable after the fact.
- **`POST /api/route`** (behind `X-Debug: true`, like the RAG endpoints): body
  `{ project_id, prompt }`, returns the candidate set, the raw answers and the derived
  decision, **without executing anything**. Calibrating `MIN_CONFIDENCE` against live
  traffic is guesswork; this is the tool that makes it a measurement. It is the first
  thing to cut if it proves excessive.

## Testing

| Layer | What is pinned |
|---|---|
| `route_candidates` | Keys unique across globals and project agents; general agent present and flagged; `routing.hint` beats derived text; a description-less agent falls back to its id and is still a candidate; empty set is empty. |
| `agent_router` (pure) | Each ladder rung separately, with a fake `DecisionRouter`. Including: unknown key rejected; low confidence → general; low specialised → general; general wins → general; router error → general. |
| Adapter | A fixture captured from a real `llama-server` response pins the wire format. See § Open verification. |
| Integration | `execute_chat` routes to the expected agent through a fake router, on both the plain and streaming paths. |
| Regression | With the router down, a project-scoped request produces the same result as before this change. This is the test that justifies the claim that nothing downstream changed. |
| Health | Configured-but-unreachable router reports degraded. |

## Open verification

**The response shape of llama.cpp's `POST /v1/systemone` is not documented.** The
*request* shape is confirmed — Liquid's d1-3B page gives a complete `curl` against
`/v1/systemone` with `{state, questions}`, keyed question names, and `type` +
`instructions` (+ `criteria`) per question, which is what this design sends. But no
documented example shows a **response body** from the local server: none of the published
`curl` examples print one.

The hosted API's answer field names are known from the TypeSafe SDK examples —
`answers.<name>.choice`, `.confidence`, `.probabilities` for a Choice, and `.noul` for a
Noul — and the adapter's structs are modelled on them. That is corroboration, not proof:
the hosted API and `llama-server` are two implementations and the local one's envelope
may differ.

Mitigation: response parsing is confined to serde structs in the adapter. A mismatch
changes that one file and nothing above it. The adapter test uses a fixture captured
from a live `llama-server`, so the format is pinned by a real observation rather than by
an assumption carried into code. The deserialiser should also tolerate an unknown
envelope by failing into the `ProjectGeneral` rung rather than erroring the request.

The first implementation step is therefore: start `llama-server`, capture a real
response, and fit the structs to it.

## Non-goals

- **The TUI does not route.** In the TUI you pick an agent deliberately; a router would
  compete with a deliberate choice. `src/tui/` is a separate path and is untouched.
- **Multi-hop orchestration.** No agent hands off to another, no fan-out, no synthesis
  across agents. v1 is one decision, then the existing single-agent execution.
- **The dead `subagents` config is not revived.** `AgentConfig.subagents`
  (`allowed`, `allow_global`, `max_depth`) is declared in `src/domain/agent.rs` and read
  nowhere in `src/` — it is dead schema for a feature that was never built. This design
  does not implement it. It is flagged as debt: either delete it or implement it, but
  neither belongs to the routing step.
- **No stickiness across turns.** See § Conversation state.

## API surfaces affected

Routing lives inside `execute_chat`, so every surface that calls it inherits the
behaviour. This is worth stating because gRPC was not in the original request:

| Surface | Effect |
|---|---|
| `POST /chat`, `POST /api/chat`, `POST /v1/chat/completions` | Routes when `X-Project` is present and `X-Agent` is absent. |
| gRPC `chat` / `chat_stream` | **Same**, via `x-project` / `x-agent` metadata (`src/api/grpc.rs` builds the identical `AgentSelection`). Not opt-out per surface. |
| SSE streaming | Routing completes before the first token; the stream itself is unchanged. The extra decision adds latency to time-to-first-byte and nothing else. |
| `GET /health`, `GET /api/health` | Gains the `router` field; `status` can become `degraded` (§ Errors). |

`debug_route` is HTTP-only: `pb::ChatResponse` in the gRPC proto has no debug fields
today, and adding one is outside this design.

`AGENTS.md` must be updated to describe the routing behaviour on all four chat
endpoints, the new env variables, and the new `POST /api/route` endpoint.

## The strict validation consequence

This is the one place where the design changes a rule the repo already states.

`AGENTS.md` says: *"If `X-Project` is provided, Llama-R **requires** a valid agent to be
found within that project context... the request will fail with a `400 Bad Request`
error. This ensures that project-scoped requests never accidentally bypass the intended
context and rules."*

The original intent — that a request which names a project must be answered by that
project — is preserved. But the *destination* when nothing fits changes:

- It is **the project's general agent** (`contextos/projects/<p>/agents/<p>.toml`), which
  carries the project's context, rules and skills but no specialisation.
- It is **not** `DEFAULT_MODEL` with no agent.

The reason is that a project-scoped request falling through to a bare model is exactly
the bypass the rule exists to prevent — it would silently drop the project's context and
rules on a request that explicitly asked for them. The general agent is the
"unspecialised" answer within the project's scope, and it keeps the invariant intact.

Two behaviours change as a result:

1. **`X-Project` without `X-Agent` no longer always resolves to the general agent.** It
   now routes among the project's agents. That is the feature.
2. **A project with specialised agents but no general agent can now route**, where
   previously `X-Project` alone returned `400`. If routing selects no specialised agent,
   the request returns `400` — the same as today.

`AGENTS.md` must be updated with both changes when this ships.