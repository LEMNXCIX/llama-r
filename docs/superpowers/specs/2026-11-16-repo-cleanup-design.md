# Repository cleanup and module reorganisation — design

**Date:** 2026-11-16
**Status:** proposed
**Scope:** six maintenance items and one structural split. No behaviour changes to the API, the router, the TUI or the storage format.

## Goal

Two files have become review bottlenecks: `src/api/chat_core.rs` (1911 lines, ~870 of them tests) and `src/tui/app.rs` (3272 lines, ~1240 of them tests). A reviewer has to hold both the routing decision machinery and the chat execution in their head at once, and the TUI's state, key dispatch and render loop share one file. The aim is to give each concern a file whose name says what it holds, so a diff touches one subject rather than three.

Alongside the split, six small fixes that were identified during the audit and CI work: make `cargo audit` a gate, make clippy a gate, remove dead config, stop the agent API erasing a routing hint, correct a wrong default in `.env.example`, and deduplicate the four env guards in test code.

## Non-goals

- Moving `TuiApp::run` or its key dispatch arms. `tests/tui_chrome.rs` compares **byte positions** of the dialog guards against the typing arm inside `run`; moving those arms would break the check that caught the picker shipping inert.
- A full `chat/` + `tui/` domain restructure. Out of the agreed scope.
- Changing the proto, the storage format, or any public HTTP contract.

## File layout

### `src/api/route_resolution.rs` (new, ~560 lines)

Moves out of `chat_core.rs`, keeping names and signatures verbatim:

- `AgentSelection<'a>`, `ResolvedSelection`
- `ROUTE_STATE_BUDGET_CHARS`, `STALE_WINNER`
- `build_state_text`, `skip_decision_without_a_fallback`
- `resolve_selection`, `resolve_selection_with`
- `WinnerRecheck`, `recheck_winner`
- `PeekedAnswer`, `peeked_answer`

`RoutedDebug` already lives in `src/domain/models.rs`; it stays there.

Consumers to update: `src/api/handlers.rs` and `src/api/grpc.rs` import `AgentSelection` from `chat_core`; both switch to `route_resolution`. No `pub use` shim is left in `chat_core` — a stale re-export would hide which module owns the type.

### `src/api/chat_core.rs` (shrinks to ~470 lines)

Keeps: `AppChatStream`, `split_engine_messages`, `engine_target`, `agent_not_found`, `build_runtime_request`, `engine_events_to_chat_stream`, `require_messages`, `prepare_request`, `run_with_fallback`, `run_stream_with_fallback`, `execute_chat`, `execute_chat_stream`.

Imports the resolution types it needs from `route_resolution`.

### `src/tui/keys.rs` (new, ~400 lines)

Moves out of `app.rs`, with signatures unchanged:

- constants `SKILLS_FIELD`, `RULES_FIELD`, `OPTIMIZE_FIELD`, `VIEW_NAMES`
- `CurrentView`, `AnalysisState`, `KeyTarget`, `ScreenState`
- `key_target`, `agent_list_is_drawn`, `selected_agent`, `move_agent_selection`, `may_move_list_focus`, `focus_after_agent_delete`
- `TuiApp::hints_for`, `ChatAction`, `chat_input_action`, `rule_editor_opening`

`hints_for` becomes a free function in `keys.rs` rather than an associated function, because its whole reason for being on `TuiApp` was the old "one place for hints" convention; the guard that enforces that convention moves with it.

### `src/tui/app.rs` (shrinks to ~1600 lines)

Keeps `TuiApp`, its `impl` including `run()`, `bar_context` and `available_skills`. Re-exports nothing: the views import `CurrentView` and `AnalysisState` from `keys.rs` directly.

### Tests as submodules

`src/api/chat_core/tests.rs` and `src/tui/app/tests.rs` take the existing `#[cfg(test)] mod tests` blocks, with `#[cfg(test)] mod tests;` left in the parent. They stay unit tests with private access — moving them to `tests/*.rs` integration crates would force making internals `pub`, which is a larger API change than the cleanup warrants.

The chat-core tests exercise both execution and resolution; they use `use super::*;` plus one explicit import from `super::super::route_resolution::*` where needed.

## Guards that must be updated

`tests/tui_chrome.rs` reads `src/tui/app.rs` as a string (`include_str!`) and searches for literal needles. These are the changes:

- Add `const KEYS: &str = include_str!("../src/tui/keys.rs");`.
- `app_without_hint_text()` reads `KEYS` and strips `hints_for` from there; `APP` is no longer excluded. This makes `no_view_hard_codes_a_key_hint` scan `app.rs` with no exception at all — a strengthening, not a weakening.
- `a_dialog_opener_outranks_the_typing_arm` keeps reading `APP`: `run()` does not move.
- `only_the_modals_draw_borders` adds `src/tui/keys.rs` to the scanned source list. It has no borders today; the point is to catch one added tomorrow.
- The "one `render_footer` call, in `app.rs`" check stays as-is.
- `a_dialog_is_intercepted_before_the_view_it_sits_over` stays in `app.rs`.

Every guard must still be demonstrated to fire: the test that proves the source detector spots a hint must be adjusted to point at the new source string, not silently weakened to keep passing.

## The six small fixes

### 1. `cargo audit` as a gate

`cargo audit` now reports 0 vulnerabilities and 0 warnings over 476 crates. Add a CI step installing `cargo-audit` and running it as a hard gate. Pin the advisory database via the action's normal behaviour; no vendoring.

### 2. clippy as a gate

The 43 lints seen earlier are now lower, and the 16 `await_holding_lock` warnings were established to be in `#[cfg(test)]` blocks, not production. Fix the remaining lints (mostly `into_iter`, redundant `clone`, same-type casts, `Default` impls, `if let`), then change the workflow step from report-only to `cargo clippy --all-targets -- -D warnings`.

No behaviour change in any fix. Each one is verified by the existing tests.

### 3. Remove dead `subagents` config

Delete `SubagentsConfig` and `AgentConfig.subagents` from `src/domain/agent.rs`, plus the `Default` entry. Grep confirms no reads in `src/`. Existing agent TOMLs that happen to contain `[subagents]` will then fail to deserialize under `toml::Deserialize` unless the field is tolerated. **Guard:** keep a private, ignored field with `#[serde(default, skip_serializing)]` named `subagents` of a unit-like type that accepts any TOML table (`serde_json::Value` via `toml::Value` is not available, so use `Option<toml::Table>` with `#[serde(default)]`). This preserves backwards compatibility while the runtime still ignores it. Document the compatibility stub in `AGENTS.md`.

### 4. Preserve `routing.hint` through `PUT /api/agents/:id`

`build_agent_toml` rewrites the whole manifest from `CreateAgentRequest`, which has no `routing` field, so a curated hint is silently erased. Add `routing: Option<RoutingConfig>` to `CreateAgentRequest` (with `#[serde(default)]`) and emit `[agent.routing] hint = "..."` from `build_agent_toml` when present.

The same request type also lacks `description` and `auto_skills`, and `build_agent_toml` hardcodes `auto_skills = []` — so a PUT erases those too. Add `description: String` with `#[serde(default)]` and emit it when non-empty. Leave `auto_skills` alone: it is system-managed by `sync_project_agent_skills`, and a client should not be able to set it.

`CreateAgentRequest` derives `ToSchema` and is registered in `docs.rs`; the added fields appear there automatically.

Add a test: create an agent with a description and a hint, PUT an update carrying both, reload from disk, assert both survive.

### 5. `.env.example` port

`PORT=8080` contradicts the code default (`3000`) and collides with `llama-server` on 8080 — a documented trap hit during the live router test. Change it to `3000`. Update the adjacent comment to say the router's URL is separate.

### 6. Shared env guard for tests

`RouterEnvGuard` is duplicated in `src/services/router_config.rs` (unit tests), `tests/api_integration.rs`, `tests/router_dispatch_tests.rs` and `tests/route_api_tests.rs`, plus a near-identical `DispatchEnv`/`PreviewEnv`. Create `tests/common/mod.rs` exposing `RouterEnvGuard` and `TestLock`, and have the integration tests import it. The unit-test copy inside `router_config.rs` stays private: a unit-test module cannot import an integration-test helper.

Field order stays load-bearing: a guard that restores the environment must be declared before the lock guard, because Rust drops struct fields in declaration order. Encode that in the helper itself so no caller can get it wrong.

## Verification

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --target-dir target-tests`, `cargo audit` all clean.
- Every source-scanning guard demonstrated to still fire under mutation — specifically the hint detector after moving `hints_for` to `keys.rs`, and the border detector after adding `keys.rs` to its input list.
- The routing tests and the gRPC wire test still pass unchanged, proving the moves preserved the interfaces.
- A real `PUT /api/agents/:id` round trip pins `routing.hint` survival.

## Risks

- Moving `hints_for` to a free function changes the call site in `app.rs`. It is private and has one caller; the compiler catches a miss.
- The compatibility stub for `subagents` uses `toml::Table`, which accepts any table shape but not a scalar. No shipped manifest uses a scalar, so this is safe; a scalar would be a malformed manifest anyway.
- The structural split touches many imports. Keeping names and signatures verbatim means each move is mechanical andcompiler-checked.
