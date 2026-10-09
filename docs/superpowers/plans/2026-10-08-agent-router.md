# Agent Router Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A chat request carrying `X-Project` but no `X-Agent` is routed to the project's most appropriate agent by a d1 decision model, falling back to the project's general agent when the router is unavailable or unsure.

**Architecture:** A new `DecisionRouter` port with one llama.cpp adapter. A pure service derives the candidate set from `AgentRegistry` and applies a decision ladder; `chat_core::resolve_selection` consults it before the existing dispatch, producing the same `(project_id, agent_id)` pair the header would have produced, so nothing downstream changes.

**Tech Stack:** Rust 2021, axum 0.7, tokio, reqwest 0.11 (`json` feature), serde + serde_json, async-trait, utoipa, toml.

**Spec:** `docs/superpowers/specs/2026-10-08-agent-router-design.md` — read it before starting. The plan argues from it; where the two differ, the spec wins and the plan is wrong.

## Global Constraints

- **No new dependencies.** `reqwest` (with `json`), `tokio`, `serde`, `async-trait`, `axum`, `utoipa` and `toml` are already in `Cargo.toml`. This feature adds zero crates.
- **`ROUTER_ENABLED` defaults to `false`.** Every task must leave the feature inert when it is unset. A test that only passes with the router on has not proved the default is safe.
- **The router never owns a request.** It returns `(project_id, agent_id)` or an error. It does not call the LLM provider, does not touch RAG, and does not alter messages.
- **`X-Agent` present means the router is not consulted.** Not "consulted and overridden" — not consulted.
- **Booleans from env follow the existing `RAG_ENABLED` idiom:** `false` for `"0"`, `"false"`, `"no"`, `"off"` (case-insensitive, trimmed); any other present value is `true`.
- **Numeric env vars are validated, not defaulted past.** A malformed or out-of-range value is `AppError::Config`, matching how `PORT` and `EMBEDDING_DIMENSIONS` are already handled in `src/config.rs`.
- **`src/services/agent_router.rs` performs no I/O and holds no `Arc`.** It takes a `Result<DecisionResponse, AppError>` and returns a decision. Every ladder branch is tested without a server.
- **Error message and commit style match the repo.** Lower-case imperative subject, no trailing period; commits are Conventional Commits with a `Co-Authored-By: opencode <opencode@local>` trailer.
- **Tests mutate `LLAMA_R_DIR` or the environment only while holding `core::paths::lock_env_for_tests()`.** This guard already exists for exactly this purpose.

## Review Focus

Five input classes the spec implies but no task's own tests would otherwise exercise. Each has a test assigned below, in the task that owns the code.

1. **Router disabled — the request must be byte-identical to today.** `X-Project` with no `X-Agent` and `ROUTER_ENABLED` unset must still resolve to the project's general agent. A field rename in the dispatch path silently reroutes every request in the product; this is the regression that would ship unnoticed.
2. **An agent id containing `::`.** Routing keys are `project::agent`, so a global agent whose id contains `::` cannot be split back unambiguously. It must be skipped with a warning, never mis-resolved to a different agent.
3. **A conversation too long for the model's context.** The d1 `state` is built from the request's messages and must be truncated to a token budget keeping the most recent turns — by truncation, never by failing the request.
4. **Hot reload between validating a key and resolving it.** The registry reloads while a decision is in flight, so a validated key can vanish. Resolution must re-check the registry and degrade, not error or panic.
5. **A `Choice` with zero options.** `exclude` can empty the candidate set. A `Choice` with no criteria is not answerable; the ladder must short-circuit before calling the router.

---

### Task 1: The `DecisionRouter` port

**Files:**
- Create: `src/ports/router.rs`
- Modify: `src/ports/mod.rs` (add `pub mod router;`)

**Interfaces:**
- Consumes: nothing.
- Produces — used by every later task:
  - `pub struct DecisionRequest { pub state: String, pub questions: BTreeMap<String, Question> }`
  - `pub enum Question { Choice { instructions: String, criteria: BTreeMap<String, String> }, Noul { instructions: String } }`
  - `pub struct DecisionResponse { pub answers: BTreeMap<String, Answer> }`
  - `pub enum Answer { Choice { key: String, confidence: f32, probabilities: BTreeMap<String, f32> }, Noul { value: f32 } }`
  - `#[async_trait] pub trait DecisionRouter: Send + Sync { async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, AppError>; }`
  - Derives on all four types: `Debug, Clone, PartialEq`, plus `Serialize` on `DecisionResponse` and `Answer` (needed by `debug_route` in Task 8). **No `Deserialize`** — the wire format is the adapter's business, not the port's.

- [ ] **Step 1: Write the failing test**

In `src/ports/router.rs`, under `#[cfg(test)]`:

```rust
#[test]
fn answers_serialize_with_their_answer_kind() {
    let mut answers = BTreeMap::new();
    answers.insert(
        "agent".to_string(),
        Answer::Choice {
            key: "fudi::ops".into(),
            confidence: 0.91,
            probabilities: BTreeMap::from([("fudi::ops".to_string(), 0.91)]),
        },
    );
    let response = DecisionResponse { answers };
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["answers"]["agent"]["answer"], "choice");
    assert_eq!(json["answers"]["agent"]["key"], "fudi::ops");
    assert_eq!(json["answers"]["agent"]["confidence"], 0.91);
}

#[test]
fn a_noul_is_a_probability_not_a_label() {
    let noul = Answer::Noul { value: 0.42 };
    let json = serde_json::to_value(&noul).unwrap();
    assert_eq!(json["answer"], "noul");
    assert_eq!(json["value"], 0.42);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --target-dir target-tests ports::router`
Expected: FAIL — `cannot find DecisionResponse in this scope`.

- [ ] **Step 3: Write the four types and the trait in `src/ports/router.rs`**

Use the signatures from the Interfaces block. `Answer` is internally tagged: `#[serde(tag = "answer", rename_all = "snake_case")]`. `Question` gets **no** serde attribute — Task 2 owns the wire encoding, and giving the port type a wire tag would leak the adapter's format into the core, which is the boundary this port exists to hold.

Above the `Question` enum, a comment recording the asymmetry so `Score` is not added wrongly later:

```
/// `Choice.criteria` is a map of label → description, and the returned `key`
/// is one of its keys. `Score.criteria` (unused here) is an *ordered list* whose
/// levels are indexed from 0 — not a map. Do not add `Score` with a map.
```

- [ ] **Step 4: Register the module and run the tests**

Add `pub mod router;` to `src/ports/mod.rs` (alphabetical: after `mcp`, before `rag`).

Run: `cargo test --target-dir target-tests ports::router`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add src/ports/router.rs src/ports/mod.rs
git commit -m "feat(router): add the DecisionRouter port

d1 evaluates a state against typed questions and returns probabilities.
The port mirrors that shape for the two primitives routing needs, so the
adapter owns the wire format and the core owns none of it.

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 2: The llama.cpp adapter

**Files:**
- Create: `src/adapters/router/mod.rs`, `src/adapters/router/llamacpp.rs`
- Modify: `src/adapters/mod.rs` (add `pub mod router;`)
- Create: `src/adapters/router/fixtures/systemone_response.json`

**Interfaces:**
- Consumes: Task 1's `DecisionRouter`, `DecisionRequest`, `Question`, `DecisionResponse`, `Answer`.
- Produces:
  - `pub struct LlamaCppRouter { base_url: String, model: String, http: reqwest::Client }`
  - `pub fn new(base_url: impl Into<String>, model: impl Into<String>, timeout_ms: u64) -> Self`
  - `impl DecisionRouter for LlamaCppRouter`
  - `fn encode_question(question: &Question) -> serde_json::Value` — module-private, tested directly
  - `fn decode_response(body: &serde_json::Value) -> Result<DecisionResponse, AppError>` — module-private, tested directly

- [ ] **Step 1: Capture a real response, or record that you could not**

```bash
llama-server -hf LiquidAI/d1-3B-GGUF:Q8_0 &
curl -s http://127.0.0.1:8080/v1/systemone -H 'Content-Type: application/json' -d '{
  "state": "me cobraron dos veces este mes",
  "questions": {
    "agent": {"type":"choice","instructions":"Que agente atiende esto?",
              "criteria":{"fudi::ops":"Gestiona pedidos y menu","fudi::fudi":"Asistente general"}},
    "specialized": {"type":"noul","instructions":"Hace falta un agente especializado?"}
  }
}' | tee src/adapters/router/fixtures/systemone_response.json
```

**If `llama-server` is unavailable in your environment**, do not skip this step and do not invent a fixture silently. Write the fixture from the hosted API's documented field names (`answers.<name>.choice`, `.confidence`, `.probabilities`, `.noul`), keeping it **valid JSON** — `include_str!` plus `serde_json::from_str` will reject a `//` comment — and record its provenance in a sibling `src/adapters/router/fixtures/README.md` instead:

```markdown
# systemone_response.json

NOT CAPTURED. `llama-server` was unavailable when this fixture was written.
This is the hosted API's documented shape (docs.liquid.ai, decision-model
guide), not an observation of the local server.

Task 2's decode test passes either way, but only a captured fixture proves
the local envelope. Re-capture with the `curl` in Task 2 Step 1 before
trusting this adapter against a real `llama-server`.
```

Report which happened in your summary. Everything else in this task is identical either way — which is the point of confining the wire format to this file.

- [ ] **Step 2: Write the failing tests**

Three tests in `#[cfg(test)]` in `llamacpp.rs`:

```rust
#[test]
fn encode_choice_uses_the_documented_wire_shape() {
    let q = Question::Choice {
        instructions: "Which agent?".into(),
        criteria: BTreeMap::from([("fudi::ops".to_string(), "Pedidos".to_string())]),
    };
    let json = encode_question(&q);
    assert_eq!(json["type"], "choice");
    assert_eq!(json["instructions"], "Which agent?");
    assert_eq!(json["criteria"]["fudi::ops"], "Pedidos");
}

#[test]
fn encode_noul_has_no_criteria_field() {
    let json = encode_question(&Question::Noul { instructions: "Specialised?".into() });
    assert_eq!(json["type"], "noul");
    assert_eq!(json["instructions"], "Specialised?");
    assert!(json.get("criteria").is_none());
}

#[test]
fn decode_reads_the_fixture_into_answers() {
    let body: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/systemone_response.json")).unwrap();
    let decoded = decode_response(&body).unwrap();
    // Assert the two answers exist with their documented field names:
    // the Choice carries `key`, `confidence`, `probabilities`; the Noul a float.
}
```

Plus one for the envelope this adapter must survive, from the spec's Open verification:

```rust
#[test]
fn decode_rejects_an_unexpected_envelope_instead_of_panicking() {
    let body = serde_json::json!({ "something": "else" });
    assert!(decode_response(&body).is_err());
}
```

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo test --target-dir target-tests adapters::router`
Expected: FAIL — `cannot find encode_question in this scope`.

- [ ] **Step 4: Implement `LlamaCppRouter`**

- `encode_question(&Question) -> serde_json::Value`: `Choice` → `{"type":"choice","instructions":…,"criteria":{…}}`; `Noul` → `{"type":"noul","instructions":…}` with **no** `criteria` key.
- `decode_response(&serde_json::Value) -> Result<DecisionResponse, AppError>`: read `answers` as an object; for each named answer, branch on its kind and pull `choice`/`confidence`/`probabilities` or `noul`. **Any** structural surprise — missing `answers`, an answer that is not an object, an unknown kind — returns `Err(AppError::Provider(..))`. The spec requires the deserialiser to fail into the ladder's `ProjectGeneral` rung rather than erroring the request, so this function must never panic and never return a partially-filled `DecisionResponse`.
- `decide()`: `POST {base_url}/v1/systemone` with body `{"state":…, "questions":{…}}` built by mapping each `Question` through `encode_question`. On non-2xx, capture status and body text into the error. Send the body, then hand the parsed JSON to `decode_response`.
- Constructor builds the `reqwest::Client` with `Duration::from_millis(timeout_ms)`, mirroring `OllamaEmbeddings::new`'s `.unwrap_or_else(|_| Client::new())` fallback.
- Trim trailing `/` from `base_url` before appending the path, as `OllamaEmbeddings::endpoint` does.

- [ ] **Step 5: Register the module and run the tests**

Add `pub mod router;` to `src/adapters/mod.rs` (alphabetical: after `rig_engine`).

Run: `cargo test --target-dir target-tests adapters::router`
Expected: PASS, 4 tests.

- [ ] **Step 6: Commit**

```bash
git add src/adapters/router src/adapters/mod.rs
git commit -m "feat(router): talk to llama.cpp /v1/systemone

d1 generates no tokens, so it cannot be reached through Ollama's
generative endpoints. It answers on a llama-server instead.

The wire format lives entirely in this module, so a shape difference
between the hosted API and llama-server costs this file and nothing
above it.

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 3: Router configuration

**Files:**
- Create: `src/services/router_config.rs`
- Modify: `src/services/mod.rs` (add `pub mod router_config;`)
- Modify: `src/core/paths.rs` (add `pub fn get_router_config_path() -> PathBuf`)

**Interfaces:**
- Consumes: nothing.
- Produces — used by Tasks 6, 7, 8:
  - `pub struct RouterConfig { pub enabled: bool, pub url: String, pub model: String, pub timeout_ms: u64, pub min_confidence: f32, pub min_specialised: f32, pub exclude: Vec<String> }`
  - `impl RouterConfig { pub fn from_env() -> Result<Self, AppError>; pub fn overlay_toml(&mut self, path: &Path) -> Result<(), AppError>; pub fn load() -> Result<Self, AppError> }`
  - `impl Default for RouterConfig` — `enabled: false`, `url: "http://127.0.0.1:8080"`, `model: "d1-3b"`, `timeout_ms: 3000`, `min_confidence: 0.60`, `min_specialised: 0.50`, `exclude: vec![]`
  - `core::paths::get_router_config_path() -> PathBuf` — `get_base_dir().join("router.toml")`

- [ ] **Step 1: Write the failing tests**

In `src/services/router_config.rs`, `#[cfg(test)]`. Every test that touches the environment holds the guard:

```rust
#[test]
fn router_is_disabled_when_the_env_is_silent() {
    let _guard = crate::core::paths::lock_env_for_tests();
    // The process may already have these set; clear what matters first.
    std::env::remove_var("ROUTER_ENABLED");
    let config = RouterConfig::from_env().unwrap();
    assert!(!config.enabled, "the default must be inert");
}

#[test]
fn defaults_match_the_documented_table() {
    let config = RouterConfig::default();
    assert_eq!(config.url, "http://127.0.0.1:8080");
    assert_eq!(config.model, "d1-3b");
    assert_eq!(config.timeout_ms, 3000);
    assert_eq!(config.min_confidence, 0.60);
    assert_eq!(config.min_specialised, 0.50);
}

#[test]
fn enabled_reads_the_false_idiom() {
    let _guard = crate::core::paths::lock_env_for_tests();
    for value in ["0", "false", "no", "off", "OFF", " Off "] {
        std::env::set_var("ROUTER_ENABLED", value);
        assert!(!RouterConfig::from_env().unwrap().enabled, "value {value:?}");
    }
    std::env::set_var("ROUTER_ENABLED", "true");
    assert!(RouterConfig::from_env().unwrap().enabled);
}

#[test]
fn a_threshold_outside_the_unit_interval_is_a_config_error() {
    let _guard = crate::core::paths::lock_env_for_tests();
    std::env::set_var("MIN_CONFIDENCE", "1.4");
    assert!(matches!(RouterConfig::from_env(), Err(AppError::Config(_))));
    std::env::set_var("MIN_CONFIDENCE", "not-a-number");
    assert!(matches!(RouterConfig::from_env(), Err(AppError::Config(_))));
    std::env::set_var("MIN_CONFIDENCE", "0");
    assert!(matches!(RouterConfig::from_env(), Err(AppError::Config(_))), "0 means trust nothing");
}

#[test]
fn router_toml_overrides_the_env_but_only_the_keys_it_sets() {
    let _guard = crate::core::paths::lock_env_for_tests();
    let dir = tempdir();
    let path = dir.path().join("router.toml");
    std::fs::write(&path, "min_confidence = 0.8\nexclude = [\"legacy\"]\n").unwrap();

    let mut config = RouterConfig { min_confidence: 0.6, ..RouterConfig::default() };
    config.overlay_toml(&path).unwrap();
    assert_eq!(config.min_confidence, 0.8);
    assert_eq!(config.exclude, vec!["legacy".to_string()]);
    // Untouched by the file, so still the default:
    assert_eq!(config.min_specialised, 0.50);
}

#[test]
fn a_missing_router_toml_is_not_an_error() {
    let mut config = RouterConfig::default();
    assert!(config.overlay_toml(Path::new("/nonexistent/router.toml")).is_ok());
}
```

The tempdir helper already exists in the repo's test code — reuse it rather than adding a dependency. If no shared helper exists, use `std::env::temp_dir().join(format!("llama-r-router-config-{}", std::process::id()))`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --target-dir target-tests services::router_config`
Expected: FAIL — `cannot find RouterConfig in this scope`.

- [ ] **Step 3: Implement**

`RouterConfig::from_env()` reads, in order: `ROUTER_ENABLED` (the `RAG_ENABLED` idiom from `src/config.rs:72`), `ROUTER_URL` (default `http://127.0.0.1:8080`, rejected when empty), `ROUTER_MODEL` (default `d1-3b`, rejected when empty), `ROUTER_TIMEOUT_MS` (default `3000`, rejected at 0 — a zero timeout would fail every decision), `MIN_CONFIDENCE` and `MIN_SPECIALIZED` (defaults `0.60` / `0.50`, each rejected outside `0.0 < v <= 1.0`, each error naming the variable and the value received, in the style of the `PORT` error at `src/config.rs:26`).

`overlay_toml` deserialises into a private `RouterOverrides` whose every field is `Option`, so a key that is absent leaves the current value alone. A malformed file is `AppError::Config` naming the path. A missing file returns `Ok(())`.

`load()` is `from_env()?` then `overlay_toml(&get_router_config_path())?`.

Note the env name is `MIN_SPECIALIZED` (z) while the struct field is `min_specialised` (s), matching the repo's existing `Summarize_every` / `synchronise` mixed convention — add a comment so the mismatch reads as deliberate.

- [ ] **Step 4: Add `get_router_config_path` and run the tests**

```rust
/// Optional per-project router tuning: `{base}/router.toml`.
pub fn get_router_config_path() -> PathBuf {
    get_base_dir().join("router.toml")
}
```

Add `pub mod router_config;` to `src/services/mod.rs` (alphabetical: after `rag_ingest`, before `scope_builder`).

Run: `cargo test --target-dir target-tests services::router_config`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add src/services/router_config.rs src/services/mod.rs src/core/paths.rs
git commit -m "feat(router): configure the router by env, tuned by router.toml

Defaults keep the feature off, so an update alone changes nothing.
Thresholds are validated rather than clamped: a silent 1.4 would look
configured and route everything to the general agent.

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 4: Candidate derivation

**Files:**
- Create: `src/services/route_candidates.rs`
- Modify: `src/domain/agent.rs` (add the optional `routing` field)
- Modify: `src/services/mod.rs`

**Interfaces:**
- Consumes: Task 3's `RouterConfig.exclude`.
- Produces — used by Tasks 5, 7, 8:
  - `pub struct RouteCandidate { pub key: String, pub agent_id: String, pub project_id: Option<String>, pub label: String, pub criteria: String, pub is_project_general: bool }`
  - `pub fn build_candidates(registry: &AgentRegistry, project_id: &str, exclude: &[String]) -> Vec<RouteCandidate>`
  - `pub fn derive_criteria_text(agent: &Agent) -> String`
  - `pub fn is_project_general(project_id: &str, agent_id: &str) -> bool` — `agent_id == project_id`
  - On `AgentConfig`: `pub routing: Option<RoutingConfig>` where `RoutingConfig { pub hint: Option<String> }`, both `#[serde(default)]`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn keys_are_unique_across_project_and_global_agents() {
    // Register a global "ops" and a project "fudi" agent named "ops".
    let candidates = build_candidates(&registry, "fudi", &[]);
    let keys: Vec<&str> = candidates.iter().map(|c| c.key.as_str()).collect();
    let mut sorted = keys.clone(); sorted.sort(); sorted.dedup();
    assert_eq!(sorted.len(), keys.len(), "duplicate routing key in {keys:?}");
    assert!(keys.contains(&"fudi::ops"));
    assert!(keys.contains(&"ops"));
}

#[test]
fn the_projects_own_agent_id_is_flagged_as_general() {
    let candidates = build_candidates(&registry, "fudi", &[]);
    let general: Vec<&RouteCandidate> =
        candidates.iter().filter(|c| c.is_project_general).collect();
    assert_eq!(general.len(), 1);
    assert_eq!(general[0].key, "fudi::fudi");
}

#[test]
fn an_agent_with_no_description_falls_back_to_its_id_and_stays_a_candidate() {
    // The spec rejects excluding it: a project whose agents all lack a
    // description would otherwise 400 on requests that succeed today.
    let candidates = build_candidates(&registry, "fudi", &[]);
    let ops = candidates.iter().find(|c| c.agent_id == "ops").unwrap();
    assert!(!ops.criteria.trim().is_empty());
}

#[test]
fn a_routing_hint_replaces_the_derived_text() {
    let agent = agent_with(RoutingConfig { hint: Some("Solo facturacion.".into()) },
                           "descripcion original", &[]);
    assert_eq!(derive_criteria_text(&agent), "Solo facturacion.");
}

#[test]
fn derived_text_mentions_skills_and_tools_when_present() {
    let agent = agent_with(RoutingConfig::default(), "Opera pedidos",
                           &["facturacion"], /* tools */ &["fudi/list_orders"]);
    let text = derive_criteria_text(&agent);
    assert!(text.contains("Opera pedidos"));
    assert!(text.contains("facturacion"));
    assert!(text.contains("fudi/list_orders"));
}

#[test]
fn exclude_removes_agents_from_the_candidate_set() {
    let candidates = build_candidates(&registry, "fudi", &["ops".to_string()]);
    assert!(!candidates.iter().any(|c| c.agent_id == "ops"));
}

#[test]
fn a_global_agent_id_containing_the_separator_is_skipped() {
    // Review Focus #2. `ops::night` would split back ambiguously.
    let candidates = build_candidates(&registry_with_global("ops::night"), "fudi", &[]);
    assert!(!candidates.iter().any(|c| c.agent_id == "ops::night"));
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --target-dir target-tests services::route_candidates`
Expected: FAIL — `cannot find build_candidates in this scope`.

- [ ] **Step 3: Add the optional `routing` field to `AgentConfig`**

In `src/domain/agent.rs`, add `pub routing: Option<RoutingConfig>` with `#[serde(default)]` so existing TOML deserializes unchanged, add `routing: None` to the `Default` impl, and define:

```rust
/// Curated routing text. `hint` replaces the derived description when present.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct RoutingConfig {
    #[serde(default)]
    pub hint: Option<String>,
}
```

- [ ] **Step 4: Implement `route_candidates.rs`**

`build_candidates` walks `registry.list()` (which returns `RegisteredAgent` with both the `Agent` and its resolved `AgentScope`), keeps those whose `agent.project_id` is `Some(project_id)` or `None`, drops any whose `agent_id` is in `exclude`, and **skips with a `tracing::warn!` any global agent whose id contains `::`** (Review Focus #2). The routing key is `format!("{project}::{agent}")` for project agents and the bare id for globals — the same scheme as `registry_key`, which is private to `agent_registry.rs`, so re-derive it here in one small function with a comment naming `registry_key` as the authority. Sort the result by `key` so the `Choice` criteria map is built in a stable order across calls; an unstable order makes two identical requests produce different prompts, which makes a confidence threshold impossible to interpret.

`derive_criteria_text` returns `routing.hint` when it is a non-blank `Some`. Otherwise it assembles `name` + `description`, then `Skills:` from `skills` ∪ `auto_skills`, `Tools:` from `tools_override`, `Sources:` from `mcp_sources`, omitting empty sections and joining with `"\n"`. When both `name` and `description` are blank, use the agent id **as the header and fall through**, so the sections still append — never an empty string, because an empty `Choice` criterion is worse than a terse one, and a bare id with no capability signal is worse still.

- [ ] **Step 5: Run the tests**

Run: `cargo test --target-dir target-tests services::route_candidates`
Expected: PASS, 7 tests. Also run `cargo test --target-dir target-tests domain::agent` to confirm existing manifests still deserialize.

- [ ] **Step 6: Commit**

```bash
git add src/services/route_candidates.rs src/services/mod.rs src/domain/agent.rs
git commit -m "feat(router): derive routing candidates from the registry

Keys reuse the registry's project::agent scheme so two same-named agents
cannot collide in a Choice map. A description-less agent keeps its id as
its text instead of dropping out: removing it would turn a 200 into a 400.

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 5: The decision ladder

**Files:**
- Create: `src/services/agent_router.rs`
- Modify: `src/services/mod.rs`

**Interfaces:**
- Consumes: Task 1's `DecisionResponse`/`Answer`/`AppError`; Task 4's `RouteCandidate`.
- Produces — used by Tasks 7 and 8:
  - `pub enum GeneralReason { RouterUnavailable, UnknownKey, NotSpecialised, LowConfidence, GeneralWon }`
  - `pub enum NoRoute { NoCandidates, NoGeneralAgent }`
  - `pub enum RoutingDecision { Agent { project_id: String, agent_id: String, confidence: f32, specialised: f32 }, ProjectGeneral { project_id: String, agent_id: String, confidence: f32, reason: GeneralReason } }`
  - `pub const CHOICE_KEY: &str = "agent";` and `pub const NOUL_KEY: &str = "specialized";` — the question names, used by **both** `build_questions` (which writes them) and `route` (which reads them back out of the response), so the two cannot drift
  - `pub fn build_questions(candidates: &[RouteCandidate]) -> BTreeMap<String, Question>`
  - `pub fn route(project_id: &str, candidates: &[RouteCandidate], min_confidence: f32, min_specialised: f32, response: Result<DecisionResponse, AppError>) -> Result<RoutingDecision, NoRoute>`
  - `impl GeneralReason { pub fn as_str(&self) -> &'static str }`

- [ ] **Step 1: Write the failing tests, one per rung**

```rust
// Review Focus #5: an empty set must short-circuit, not send a zero-option Choice.
#[test]
fn no_candidates_never_reaches_the_router() {
    let calls = std::cell::Cell::new(0);
    let err = route("fudi", &[], 0.6, 0.5, Ok(unreachable_response(&calls)));
    assert!(matches!(err, Err(NoRoute::NoCandidates)));
    assert_eq!(calls.get(), 0, "the router must not be called with no options");
}

#[test]
fn a_router_error_degrades_to_the_projects_general_agent() {
    let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
    let decision = route("fudi", &candidates, 0.6, 0.5, Err(AppError::Provider("down".into())));
    assert!(matches!(decision, Ok(RoutingDecision::ProjectGeneral {
        reason: GeneralReason::RouterUnavailable, ..
    })));
}

#[test]
fn a_key_outside_the_candidate_set_is_discarded() {
    // The module's security invariant: the model returns the highest-probability
    // label, not a certified member of the set it was given.
    let candidates = candidates(&[("fudi::fudi", true)]);
    let response = choice_response("fudi::ops", 0.99);
    let decision = route("fudi", &candidates, 0.6, 0.5, Ok(response));
    assert!(matches!(decision, Ok(RoutingDecision::ProjectGeneral {
        reason: GeneralReason::UnknownKey, ..
    })));
}

#[test]
fn low_specialisation_forces_the_general_agent_even_when_confidence_is_high() {
    let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
    let decision = route("fudi", &candidates, 0.6, 0.5, Ok(choice_and_noul("fudi::ops", 0.95, 0.10)));
    assert!(matches!(decision, Ok(RoutingDecision::ProjectGeneral {
        reason: GeneralReason::NotSpecialised, ..
    })));
}

#[test]
fn low_confidence_forces_the_general_agent_even_when_specialisation_is_strong() {
    let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
    let decision = route("fudi", &candidates, 0.6, 0.5, Ok(choice_and_noul("fudi::ops", 0.30, 0.90)));
    assert!(matches!(decision, Ok(RoutingDecision::ProjectGeneral {
        reason: GeneralReason::LowConfidence, ..
    })));
}

#[test]
fn the_general_agent_winning_is_reported_as_such() {
    let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
    let decision = route("fudi", &candidates, 0.6, 0.5, Ok(choice_and_noul("fudi::fudi", 0.99, 0.05)));
    assert!(matches!(decision, Ok(RoutingDecision::ProjectGeneral {
        reason: GeneralReason::GeneralWon, ..
    })));
}

#[test]
fn a_confident_specialist_wins() {
    let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
    let decision = route("fudi", &candidates, 0.6, 0.5, Ok(choice_and_noul("fudi::ops", 0.88, 0.91)));
    match decision.unwrap() {
        RoutingDecision::Agent { agent_id, confidence, specialised, .. } => {
            assert_eq!(agent_id, "ops");
            assert!((confidence - 0.88).abs() < 1e-6);
            assert!((specialised - 0.91).abs() < 1e-6);
        }
        other => panic!("expected a specialist, got {other:?}"),
    }
}

#[test]
fn a_project_with_no_general_agent_cannot_degrade() {
    let candidates = candidates(&[("fudi::ops", false)]);
    let decision = route("fudi", &candidates, 0.6, 0.5, Err(AppError::Provider("down".into())));
    assert!(matches!(decision, Err(NoRoute::NoGeneralAgent)));
}

#[test]
fn a_missing_noul_answer_is_treated_as_no_specialisation() {
    let candidates = candidates(&[("fudi::ops", false), ("fudi::fudi", true)]);
    let decision = route("fudi", &candidates, 0.6, 0.5, Ok(choice_only("fudi::ops", 0.99)));
    assert!(matches!(decision, Ok(RoutingDecision::ProjectGeneral {
        reason: GeneralReason::NotSpecialised, ..
    })), "an absent gate must not default to trusting a specialist");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --target-dir target-tests services::agent_router`
Expected: FAIL — `cannot find NoRoute in this scope`.

- [ ] **Step 3: Implement `agent_router.rs`**

`build_questions` emits both questions in one map: a `Choice` under `CHOICE_KEY` whose `criteria` maps each candidate's `key` to its `criteria` text, with `instructions` = `"Which agent should handle this request? Pick the general agent if no specialisation fits."`; and a `Noul` under `NOUL_KEY` with `instructions` = `"Does this request need a specialised agent?"`. **If `candidates` is empty, return an empty map** — this is what makes the zero-option `Choice` from Review Focus #5 unrepresentable rather than merely untested.

`route` evaluates strictly in this order:

1. `candidates.is_empty()` → `Err(NoRoute::NoCandidates)`.
2. Find the candidate with `is_project_general`; none → `Err(NoRoute::NoGeneralAgent)`. **This check precedes the router call**, because every remaining branch degrades to the general agent and there is nothing to degrade to.
3. `response` is `Err` → `ProjectGeneral { reason: RouterUnavailable }`.
4. Extract the `Choice` under `CHOICE_KEY`; absent or not a `Choice` → `RouterUnavailable` (the response did not answer what was asked).
5. Look its `key` up in `candidates`; not found → `UnknownKey`.
6. Extract the `Noul` under `NOUL_KEY`; absent → treat `specialised` as `0.0`. Then `specialised < min_specialised` → `NotSpecialised`.
7. `confidence < min_confidence` → `LowConfidence`.
8. The winner *is* the general candidate → `GeneralWon`.
9. Otherwise → `Agent`.

`GeneralReason::as_str` returns `"router_unavailable"`, `"unknown_key"`, `"not_specialised"`, `"low_confidence"`, `"general_won"`.

Step 6's ordering matters and deserves a comment: the specialisation gate is checked **before** confidence, because a low-confidence answer to "does this need a specialist" and a low-confidence answer to "which specialist" are different problems, and only the first one licenses ignoring the second.

- [ ] **Step 4: Run the tests**

Run: `cargo test --target-dir target-tests services::agent_router`
Expected: PASS, 9 tests.

- [ ] **Step 5: Commit**

```bash
git add src/services/agent_router.rs src/services/mod.rs
git commit -m "feat(router): the decision ladder

Pure, so every branch is testable without a server. Two gates, not one:
the Choice confidence decides trust, the Noul decides whether any
specialisation is warranted at all.

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 6: Observability and health

**Files:**
- Modify: `src/api/observability.rs`
- Modify: `src/api/health.rs`
- Modify: `src/api/handlers.rs` (add `router`, `router_config`, `router_healthy` to `AppState`)

**Interfaces:**
- Consumes: Task 3's `RouterConfig`; Task 5's `GeneralReason::as_str`.
- Produces:
  - `AppObservability::record_route_to_agent()`, `record_route_to_general()`, `record_router_error()`, `record_router_latency(u64)`, `record_router_health(bool)`
  - `AppObservability::router_decisions() -> u64`
  - Four new fields on `ObservabilitySnapshot`: `routed_to_agent: u64`, `routed_to_general: u64`, `router_errors: u64`, `router_latency_ms_total: u64`
  - `pub struct RouterHealth { pub enabled: bool, pub healthy: bool, pub url: String }` with `Serialize` + `ToSchema`
  - `AppState.router: RwLock<Option<Arc<dyn DecisionRouter>>>`, `AppState.router_config: Arc<RouterConfig>`, `AppState.router_healthy: AtomicBool`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_snapshot_counts_each_routing_outcome_separately() {
    let obs = AppObservability::new();
    obs.record_route_to_agent();
    obs.record_route_to_agent();
    obs.record_route_to_general();
    obs.record_router_error();
    obs.record_router_latency(30);
    let snap = obs.snapshot();
    assert_eq!(snap.routed_to_agent, 2);
    assert_eq!(snap.routed_to_general, 1);
    assert_eq!(snap.router_errors, 1);
    assert_eq!(snap.router_latency_ms_total, 30);
}

#[test]
fn a_disabled_router_is_enabled_but_neither_healthy_nor_counted() {
    let response = HealthResponse {
        status: "ok",
        provider_healthy: true,
        agent_count: 0, context_count: 0, api_running: false, grpc_running: false,
        saved_tokens: 0, total_tokens_processed: 0,
        observability: AppObservability::new().snapshot(),
        router: RouterHealth { enabled: false, healthy: false, url: "http://127.0.0.1:8080".into() },
    };
    assert_eq!(response.status, "ok", "a feature that is off must not degrade the service");
    assert!(!response.router.enabled);
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --target-dir target-tests api::observability`
Expected: FAIL — `no field named routed_to_agent on ObservabilitySnapshot`.

- [ ] **Step 3: Add the counters**

Four `AtomicU64` fields following the existing pattern, four `record_*` methods using `Ordering::Relaxed`, and the four fields on `ObservabilitySnapshot`. Add `router_decisions()` returning `routed_to_agent + routed_to_general`, which is what the dashboard needs to show a routing *rate* rather than an absolute count.

- [ ] **Step 4: Add the `AppState` fields and the health field**

In `src/api/handlers.rs`, add to `AppState`:

```rust
/// Decision router. `RwLock` rather than a plain field so tests can swap in a
/// fake without `build_app_state` — which five call sites construct — growing
/// a parameter. Mirrors the existing `known_mcp_servers` field.
pub router: RwLock<Option<Arc<dyn DecisionRouter>>>,
pub router_config: Arc<RouterConfig>,
/// Last known router reachability, set by the ladder. Deliberately not a live
/// probe: `health()` already probes the provider on every call.
pub router_healthy: AtomicBool,
```

In `src/api/health.rs`, add `pub router: RouterHealth` to `HealthResponse` and set `status` to `"ok"` only when the provider is healthy **and** (`!router_config.enabled` or `router_healthy` is true).

- [ ] **Step 5: Wire the new `AppState` fields at construction so the build stays green**

`build_app_state` in `src/runtime.rs` gains no parameters. It calls `RouterConfig::load()` internally and handles the result by value, not by `?`: on `Err`, log a warning naming the bad variable and fall back to `RouterConfig::default()` rather than failing the boot — an unusable router config must not stop a server that otherwise works. Construct `Arc::new(LlamaCppRouter::new(config.url.clone(), config.model.clone(), config.timeout_ms))` only when `config.enabled`, wrapping it in `Ok(None)` or `Ok(Some(..))`. Initialise `router_healthy` to `false`.

**All five `build_app_state` call sites must still compile with no edits.** Run `cargo check --target-dir target-tests --all-targets` to prove it.

- [ ] **Step 6: Run the tests**

Run: `cargo test --target-dir target-tests api::observability && cargo test --target-dir target-tests api::health`
Expected: PASS, 2 tests. Then `cargo check --target-dir target-tests --all-targets` — clean.

- [ ] **Step 7: Commit**

```bash
git add src/api/observability.rs src/api/health.rs src/api/handlers.rs src/runtime.rs
git commit -m "feat(router): count routing outcomes and report router health

A misconfigured threshold is invisible without counters. Health follows
the provider's own rule: configured but unreachable is degraded, because
that is a capability the operator asked for and is not getting.

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 7: Dispatch wiring

**Files:**
- Create: `src/api/chat_core.rs` (add `ResolvedSelection`, `resolve_selection`, rework `engine_target`)
- Modify: `src/api/chat_core.rs` (`execute_chat`, `execute_chat_stream`)
- Create: `tests/router_dispatch_tests.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–6.
- Produces:
  - `pub struct ResolvedSelection { pub project_id: Option<String>, pub agent_id: Option<String>, pub routed: Option<RoutedDebug> }` (all owned, `Debug`, `Clone`, `Default`)
  - `pub struct RoutedDebug { pub project_id: String, pub outcome: &'static str, pub reason: Option<&'static str>, pub winner: Option<String>, pub confidence: Option<f32>, pub specialised: Option<f32>, pub probabilities: BTreeMap<String, f32>, pub candidates: Vec<String> }` with `Serialize` + `ToSchema`
  - `async fn resolve_selection(state: &AppState, selection: AgentSelection<'_>, messages: &[ChatMessage]) -> Result<ResolvedSelection, AppError>` — delegates to `resolve_selection_with(&state, &state.agent_registry, ...)`
  - `async fn resolve_selection_with(state: &AppState, registry: &AgentRegistry, selection: AgentSelection<'_>, messages: &[ChatMessage]) -> Result<ResolvedSelection, AppError>` — the real body; the registry is a parameter so the hot-reload race (Review Focus #4) is testable with a registry the test controls
  - `fn engine_target(state: &AppState, selection: &ResolvedSelection, original_model: &str) -> Option<(Option<String>, Option<String>)>`
  - `fn build_state_text(messages: &[ChatMessage], budget_chars: usize) -> String` — private, tested directly

- [ ] **Step 1: Write the failing tests**

`tests/router_dispatch_tests.rs`, using the `build_app_state` helper pattern from `tests/skill_api_tests.rs`, with a `FakeRouter` implementing `DecisionRouter` from Task 1 and installed via `*state.router.write().unwrap() = Some(Arc::new(fake))`.

```rust
// Review Focus #1: the most important test in the plan.
#[tokio::test]
async fn with_the_router_disabled_a_project_request_is_exactly_as_it_was() {
    let state = state_with_project_agent("fudi");
    // No router installed; router_config.enabled == false.
    let resolved = resolve_selection(
        &state,
        AgentSelection { project_id: Some("fudi"), agent_id: None, conversation_id: None, debug: false },
        &[user("hola")],
    ).await.unwrap();
    assert_eq!(resolved.agent_id.as_deref(), Some("fudi"),
        "disabled router must leave X-Project alone pointing at the project's own agent");
}

#[tokio::test]
async fn an_explicit_agent_header_is_never_routed() {
    let state = state_with_project_agent("fudi");
    let fake = FakeRouter::returning(choice_for("fudi::ops"));
    *state.router.write().unwrap() = Some(Arc::new(fake.with_call_counter()));
    let resolved = resolve_selection(
        &state,
        AgentSelection { project_id: Some("fudi"), agent_id: Some("fudi"), conversation_id: None, debug: false },
        &[user("hola")],
    ).await.unwrap();
    assert_eq!(resolved.agent_id.as_deref(), Some("fudi"));
    assert!(!fake.was_called(), "an explicit header must not spend a decision");
}

#[tokio::test]
async fn a_project_request_without_an_agent_reaches_the_router() {
    let state = state_with_project_agent("fudi");
    let fake = FakeRouter::returning(choice_for("fudi::ops"));
    *state.router.write().unwrap() = Some(Arc::new(fake.clone()));
    let resolved = resolve_selection(
        &state,
        AgentSelection { project_id: Some("fudi"), agent_id: None, conversation_id: None, debug: false },
        &[user("cancela el pedido 42")],
    ).await.unwrap();
    assert_eq!(resolved.agent_id.as_deref(), Some("ops"));
    assert!(fake.was_called());
}

#[tokio::test]
async fn a_request_with_no_project_header_is_never_routed() {
    let state = state_with_project_agent("fudi");
    let fake = FakeRouter::returning(choice_for("fudi::ops"));
    *state.router.write().unwrap() = Some(Arc::new(fake.clone()));
    let resolved = resolve_selection(
        &state,
        AgentSelection { project_id: None, agent_id: None, conversation_id: None, debug: false },
        &[user("hola")],
    ).await.unwrap();
    assert!(resolved.project_id.is_none() && resolved.agent_id.is_none());
    assert!(!fake.was_called());
}

#[tokio::test]
async fn a_down_router_leaves_the_project_general_agent_in_place() {
    let state = state_with_project_agent("fudi");
    *state.router.write().unwrap() = Some(Arc::new(FakeRouter::failing()));
    let resolved = resolve_selection(
        &state,
        AgentSelection { project_id: Some("fudi"), agent_id: None, conversation_id: None, debug: false },
        &[user("hola")],
    ).await.unwrap();
    assert_eq!(resolved.agent_id.as_deref(), Some("fudi"));
}

// Review Focus #3: an oversized conversation truncates, never fails.
#[test]
fn state_text_keeps_the_most_recent_turns() {
    let messages: Vec<ChatMessage> = (0..500)
        .map(|i| ChatMessage { role: "user".into(), content: format!("turn {i} {}", "x".repeat(200)) })
        .collect();
    let text = build_state_text(&messages, 2_000);
    assert!(text.len() <= 2_000 + 200, "budget overshoot must stay within one message");
    assert!(text.contains("turn 499"), "the newest turn must survive truncation");
    assert!(!text.contains("turn 0"), "the oldest turn must be the one dropped");
}

// Review Focus #4: hot reload can remove the chosen agent between the
// decision and the use. Resolution must re-check and degrade, not error.
#[tokio::test]
async fn an_agent_removed_by_a_reload_between_decision_and_use_degrades() {
    // `state` has fudi::ops and fudi::fudi registered; the router picks ops.
    let state = state_with_project_agent("fudi");
    *state.router.write().unwrap() = Some(Arc::new(FakeRouter::returning(choice_for("fudi::ops"))));

    // Resolution against a registry that no longer holds the winner, as after
    // a hot reload. `AgentRegistry::new()` starts empty, which is enough.
    let reloaded = AgentRegistry::new();
    let resolved = resolve_selection_with(&state, &reloaded, AgentSelection {
        project_id: Some("fudi"), agent_id: None, conversation_id: None, debug: false,
    }, &[user("cancela el pedido 42")]).await.unwrap();
    assert_eq!(resolved.agent_id.as_deref(), Some("fudi"),
        "a vanished winner must fall back to the project's general agent");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --target-dir target-tests --test router_dispatch_tests`
Expected: FAIL — `cannot find resolve_selection in this scope`, and `ResolvedSelection` is not yet exported.

- [ ] **Step 3: Add the resolved-selection types and `build_state_text`**

`ResolvedSelection` replaces the borrowed `AgentSelection` for everything downstream of resolution. Keep `AgentSelection` exactly as it is — it is the header-derived input, constructed in `handlers.rs` and `grpc.rs` — and add the owned type beside it. That split is what lets `engine_target` drop its lifetimes without touching the two call sites that build the borrowed one.

`build_state_text(messages, budget_chars)` joins the messages as `"{role}: {content}"` lines **newest-last**, accumulates from the newest backwards while the total fits `budget_chars`, and drops any `system` role (a system prompt is not conversation, and sending it would let a project's system prompt steer the routing). The overshoot bound in the test above comes from this: the loop stops after the message that would exceed the budget, so at most one message's length overshoots.

- [ ] **Step 4: Implement `resolve_selection`**

```
if selection.agent_id.is_some()      → return as-is (owned copy)
if selection.project_id.is_none()    → return as-is
if router_config.enabled == false    → return as-is
if state.router is None              → return as-is
build candidates for project_id, excluding config.exclude
if candidates empty                  → return as-is   (ladder's NoCandidates would 400;
                                                        not routing must not)
router.decide(DecisionRequest { state: build_state_text(messages, budget), questions })
match route(project_id, &candidates, config.min_confidence, config.min_specialised, result):
    Ok(decision)  → project_id + agent_id from the decision, routed = Some(debug)
    Err(NoCandidates)              → return as-is
    Err(NoGeneralAgent)            → Err(AppError::Validation(<today's message>))
```

On every path that consults the router, record the counters and `router_healthy`: `record_router_latency(ms)` always; `record_route_to_agent()` or `record_route_to_general()` per outcome; `record_router_error()` plus `record_router_health(false)` when the call errored; `record_router_health(true)` when it answered. Emit one `tracing::info!` naming project, chosen key, confidence, specialised, candidate count and latency.

The state budget: a fixed constant of 8 000 characters, comfortably inside d1-3B's 32K context, with a comment saying so.

**Review Focus #4 — the hot-reload race.** After `route` returns an `Agent`, resolve that key through `state.agent_registry` *again* (`get_project_agent` for a project agent, `get_agent` for a global) before accepting it. If the registry no longer holds it — the agents were reloaded mid-request — fall back to the project's general agent and log a warning. Do not return an id that was never verified to exist.

- [ ] **Step 5: Rework `engine_target`, `execute_chat` and `execute_chat_stream`**

`engine_target` takes `&ResolvedSelection`. Its body is unchanged apart from `.as_deref()`:

```rust
fn engine_target(state: &AppState, selection: &ResolvedSelection, original_model: &str)
    -> Option<(Option<String>, Option<String>)>
{
    if selection.project_id.is_some() || selection.agent_id.is_some() {
        return Some((selection.project_id.clone(), selection.agent_id.clone()));
    }
    if state.agent_registry.get_agent(original_model).is_some() {
        return Some((None, Some(original_model.to_string())));
    }
    None
}
```

That first arm is the whole trick: an explicit header and a routed selection are indistinguishable from here down, which is exactly why nothing downstream changes.

`requested_target(&self, fallback_model)` moves from `AgentSelection` to `ResolvedSelection`.

`src/api/grpc.rs` needs no edit: it already builds the borrowed `AgentSelection` from `x-project` / `x-agent` metadata and calls `execute_chat`, so it inherits routing for free. Add nothing — that inheritance is the spec's "API surfaces affected" section, and a comment at the `grpc.rs` call site saying so is worth more than a code change.

In both `execute_chat` and `execute_chat_stream`, resolve first, then use the resolved value for `requested_target`, `engine_target`, `prepare_request` and `build_runtime_request`:

```rust
let resolved = resolve_selection(state, selection, &payload.messages).await?;
let requested_model = resolved.requested_target(&payload.model);
let original_model = payload.model.clone();
let engine_ids = engine_target(state, &resolved, &original_model);
let prepared = prepare_request(state, payload, &resolved).await?;
```

`prepare_request` takes `&ResolvedSelection` and reads `project_id`/`agent_id` as `Option<&str>` via `.as_deref()`. Its existing 400 for "project given, no agent found" is preserved verbatim — with routing, a project that has a general agent always resolves, so that arm only fires for a genuinely agentless project, which is what it always meant.

- [ ] **Step 6: Run the tests**

Run: `cargo test --target-dir target-tests --test router_dispatch_tests`
Expected: PASS, 7 tests.

Then the regression check that matters most:

```bash
cargo test --target-dir target-tests --test api_integration
```
Expected: PASS. `api_integration.rs` predates this feature; if it fails, routing changed a path it should not have.

- [ ] **Step 7: Commit**

```bash
git add src/api/chat_core.rs tests/router_dispatch_tests.rs
git commit -m "feat(router): resolve the agent before the existing dispatch

Routing produces the same (project, agent) pair the header would have,
so engine_target cannot tell them apart and nothing downstream changes.

An explicit header short-circuits before the router is consulted, and
the chosen key is re-resolved against the registry, since hot reload can
remove it between the decision and the use.

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 8: Dry-run endpoint and `debug_route`

**Files:**
- Create: `src/api/route_api.rs`
- Modify: `src/api/mod.rs`, `src/runtime.rs`, `src/domain/models.rs`
- Create: `tests/route_api_tests.rs`

**Interfaces:**
- Consumes: Tasks 3–7.
- Produces:
  - `POST /api/route` → `route_preview(State(Arc<AppState>), Json(RoutePreviewRequest)) -> Result<Json<RoutePreviewResponse>, AppError>` behind `X-Debug: true`
  - `pub struct RoutePreviewRequest { pub project_id: String, pub prompt: String }`
  - `pub struct RoutePreviewResponse { pub candidates: Vec<RouteCandidateView>, pub raw: serde_json::Value, pub decision: String, pub reason: Option<String>, pub confidence: Option<f32>, pub specialised: Option<f32>, pub probabilities: BTreeMap<String, f32>, pub router_latency_ms: u64 }` with `Serialize` + `ToSchema`
  - `pub struct RouteCandidateView { pub key: String, pub label: String, pub criteria: String, pub is_project_general: bool }`
  - `ChatResponse.debug_route: Option<RoutedDebug>`, `#[serde(skip_serializing_if = "Option::is_none")]`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn preview_requires_the_debug_header() {
    let state = test_state();
    let err = route_preview(State(state), Json(RoutePreviewRequest {
        project_id: "fudi".into(), prompt: "hola".into(),
    })).await.unwrap_err();
    assert_eq!(err.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn preview_reports_501_when_the_router_is_off() {
    let state = test_state_with(RouterConfig::default());
    let err = with_debug_header(route_preview(State(state), Json(RoutePreviewRequest {
        project_id: "fudi".into(), prompt: "hola".into(),
    }))).await.unwrap_err();
    assert_eq!(err.status_code(), StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn preview_returns_the_candidates_it_would_have_offered() {
    let state = test_state_with_fake_router(choice_for("fudi::ops"));
    let body = with_debug_header(route_preview(State(state), Json(RoutePreviewRequest {
        project_id: "fudi".into(), prompt: "cancela el pedido 42".into(),
    }))).await.unwrap().0;
    assert_eq!(body.decision, "agent");
    assert!(body.candidates.iter().any(|c| c.key == "fudi::ops"));
    assert!(body.probabilities.contains_key("fudi::ops"));
}
```

Follow `tests/rag_api_tests.rs` for how the debug header is asserted, so the guard reads 403-before-body exactly as the RAG endpoints do.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --target-dir target-tests --test route_api_tests`
Expected: FAIL — `unresolved import route_preview`.

- [ ] **Step 3: Implement `route_api.rs`**

Check `X-Debug: true` **before** parsing the body, so the guard costs nothing and a large body cannot be used to probe the router. Then `503`-free: a disabled router returns `AppError::Unavailable`, which `src/error.rs` already maps to `501 NOT_IMPLEMENTED` with the comment "A deliberately disabled feature is not a server fault" — the right code, already decided.

Build candidates, call the router, run the same `route()` from Task 5, and return the candidate list, the raw response and the derived decision. **Do not execute the chat.** A preview that ran the agent would defeat its purpose and cost a full LLM turn.

- [ ] **Step 4: Add `debug_route` to `ChatResponse`**

In `src/domain/models.rs`, add the field with the same `skip_serializing_if` treatment as `debug_prompt`. In `chat_core.rs`, set `response.debug_route = resolved.routed` on the non-streaming path only. `pb::ChatResponse` in the gRPC proto has no debug fields and this feature does not add one, so gRPC silently drops it — noted in the spec.

- [ ] **Step 5: Register the route**

Add `pub mod route_api;` to `src/api/mod.rs`, and in `runtime.rs`:
```rust
.route("/api/route", post(route_preview))
```
in the route block, after the RAG routes. Add the `utoipa::path` annotation so `/docs` lists it, as the RAG endpoints do.

- [ ] **Step 6: Run the tests**

Run: `cargo test --target-dir target-tests --test route_api_tests`
Expected: PASS, 3 tests.

- [ ] **Step 7: Commit**

```bash
git add src/api/route_api.rs src/api/mod.rs src/runtime.rs src/domain/models.rs tests/route_api_tests.rs
git commit -m "feat(router): a dry-run endpoint for tuning the threshold

Calibrating MIN_CONFIDENCE against live traffic is guesswork. This runs
the decision and returns it without executing anything.

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 9: Documentation

**Files:**
- Modify: `AGENTS.md`, `README.md`, `.env.example`

**Interfaces:**
- Consumes: the shipped behaviour of Tasks 1–8.
- Produces: docs. No code.

- [ ] **Step 1: Update `AGENTS.md`**

Under Environment, add the six variables with their defaults from Task 3. Under "HTTP API Quick Reference", add `POST /api/route` with its `X-Debug` requirement. In the Chat Endpoints section, amend the `X-Project` paragraph with **both** behaviour changes from the spec's strict-validation section:

- `X-Project` without `X-Agent` now routes among that project's agents plus global agents, instead of always loading the project's general agent.
- A project with specialised agents but no general agent can now route; if no specialist is selected, it returns `400` as before.

State that gRPC inherits the routing through the same dispatch, and that the TUI does not route.

- [ ] **Step 2: Update `.env.example`**

Add the six variables with their defaults and a one-line comment on each giving the `d1` launch command:

```
# Agent routing (opt-in). Needs a d1 decision model on llama.cpp:
#   llama-server -hf LiquidAI/d1-3B-GGUF:Q8_0
ROUTER_ENABLED=false
ROUTER_URL=http://127.0.0.1:8080
ROUTER_MODEL=d1-3b
ROUTER_TIMEOUT_MS=3000
MIN_CONFIDENCE=0.60
MIN_SPECIALIZED=0.50
```

- [ ] **Step 3: Update `README.md`**

Add a short subsection describing the router: what it does, that it is off by default, that it needs a `llama-server` running d1, and how to try it end to end.

- [ ] **Step 4: Verify the docs match the code**

```bash
cargo test --target-dir target-tests
grep -c ROUTER AGENTS.md .env.example
```
Expected: the full suite passes; both files mention `ROUTER`. Then re-read each documented default against `RouterConfig::default()` and confirm they match character for character. A documented default that differs from the code is worse than an undocumented one.

- [ ] **Step 5: Commit**

```bash
git add AGENTS.md README.md .env.example
git commit -m "docs: describe the agent router and its configuration

Co-Authored-By: opencode <opencode@local>"
```