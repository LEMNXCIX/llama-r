# Repository Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** split the two review bottlenecks into focused modules and land six maintenance fixes that make the repository's security and quality gates automatic.

**Architecture:** Move code without changing names or signatures, so the compiler verifies each move. Tests move to `#[cfg(test)]` submodules to keep private access. The TUI key/hint logic goes to a new `keys.rs`; chat routing resolution goes to a new `route_resolution.rs`. Guard tests that read source text are updated to read the new files and must be shown to still fire.

**Tech Stack:** Rust 2021, serde/toml, clap, tokio, `cargo audit`, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-11-16-repo-cleanup-design.md`

## Global Constraints

- No behaviour changes to HTTP, gRPC, the TUI, or the storage format.
- Keep names and signatures verbatim in every move. No re-export shims left behind in the old module.
- `TuiApp::run` and its key-dispatch arms stay in `src/tui/app.rs` — source-position guards depend on their byte order.
- Test submodules use `#[cfg(test)] mod tests;` in the parent, with the body in `<module>/tests.rs`, preserving private access via `use super::*`.
- Guard declarations that restore the environment must come before lock guards, because Rust drops struct fields in declaration order.
- `auto_skills` stays system-managed; no API request may set it.
- Commit trailers: `Co-Authored-By: opencode <opencode@local>`. Conventional Commits, lower-case imperative subject, no trailing period.
- Baseline: 486 tests passing, `cargo fmt --check` clean, `cargo clippy --all-targets` exit 0 (report-only), `cargo audit` clean.

## Review Focus

1. **A guard that reads source text silently stops checking anything** after the code moves. → In Task 3, mutate `keys.rs` to add a hard-coded key hint and assert the detector fires; mutate `app.rs` to add a border and assert that detector fires.
2. **An agent TOML with `[subagents]` stops deserialising** after the dead config is removed. → In Task 4, parse a manifest containing a `[subagents]` table and assert it still loads.
3. **A PUT erases a hand-written `routing.hint` or `description`** because the request type lacks the field. → In Task 5, a real create → PUT → reload round trip asserting both survive.
4. **A shared env guard restores after releasing its lock**, reintroducing the race fixed earlier. → In Task 6, the helper owns both guard and lock in one struct with the field order encoded internally; a test asserts drop order by observing the environment while the lock is held.
5. **Moving `hints_for` to a free function breaks its one caller** or leaves two hint lists. → In Task 3, `cargo check` plus the guard that counts hint definitions.

---

### Task 1: Extract chat tests to a submodule

**Files:**
- Modify: `src/api/chat_core.rs` — replace the `#[cfg(test)] mod tests { ... }` body with `#[cfg(test)] mod tests;`
- Create: `src/api/chat_core/tests.rs` — the moved test module

**Interfaces:**
- Consumes: `execute_chat`, `execute_chat_stream`, `AgentSelection` from the parent module.
- Produces: unchanged public API; no new symbols.

- [ ] **Step 1: Move the test block**

Read the `#[cfg(test)] mod tests { ... }` block at the end of `src/api/chat_core.rs`. Create `src/api/chat_core/tests.rs` with exactly that content, changing only the first line from `use super::{execute_chat, execute_chat_stream, AgentSelection};` to `use super::{execute_chat, execute_chat_stream}; use crate::api::chat_core::AgentSelection;`. In `chat_core.rs` replace the whole block with `#[cfg(test)] mod tests;`.

No behaviour change; this is a mechanical move.

- [ ] **Step 2: Verify the move preserved every test**

Run: `cargo test --target-dir target-tests --lib api::chat_core`
Expected: the same number of tests as before, all passing. Record the count.

- [ ] **Step 3: Commit**

```bash
git add src/api/chat_core.rs src/api/chat_core/tests.rs
git commit -m "refactor(chat): move chat tests into a submodule

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 2: Extract routing resolution from `chat_core.rs`

**Files:**
- Create: `src/api/route_resolution.rs`
- Modify: `src/api/chat_core.rs`, `src/api/handlers.rs`, `src/api/grpc.rs`, `src/api/chat_core/tests.rs`

**Interfaces:**
- Produces, all `pub` or `pub(crate)` as they are today: `AgentSelection<'a>`, `ResolvedSelection`, `ROUTE_STATE_BUDGET_CHARS`, `STALE_WINNER`, `build_state_text(messages: &[ChatMessage], budget_chars: usize) -> String`, `skip_decision_without_a_fallback(candidates: &[RouteCandidate]) -> Option<AppError>`, `resolve_selection`, `resolve_selection_with`, `WinnerRecheck`, `recheck_winner`, `PeekedAnswer`, `peeked_answer`.
- Consumes: `AppState`, `RouteCandidate`, `DecisionResponse`, `Answer`, `GeneralReason`.

- [ ] **Step 1: Record the baseline test list**

Run: `cargo test --target-dir target-tests --lib api::chat_core -- --list` and save the output. Every test name must still exist after the move.

- [ ] **Step 2: Create `route_resolution.rs` by moving code, not rewriting it**

Move these items verbatim from `chat_core.rs` into the new module: `AgentSelection`, `ResolvedSelection`, the two constants, `build_state_text`, `skip_decision_without_a_fallback`, `resolve_selection`, `resolve_selection_with`, `WinnerRecheck`, `recheck_winner`, `PeekedAnswer`, `peeked_answer`. Add only the imports their bodies need — do not edit their bodies.

- [ ] **Step 3: Update consumers**

In `chat_core.rs`, import what execution needs from `crate::api::route_resolution`. In `src/api/handlers.rs` and `src/api/grpc.rs`, change the `AgentSelection` import path from `chat_core` to `route_resolution`. In `src/api/chat_core/tests.rs`, import `AgentSelection` from `crate::api::route_resolution`.

No `pub use` shim in `chat_core.rs`: a stale re-export would hide which module owns the type.

- [ ] **Step 4: Register the module and verify**

Add `pub mod route_resolution;` to `src/api/mod.rs` (alphabetical). Run: `cargo test --target-dir target-tests --lib api::chat_core -- --list` and diff against the saved list — names and count must match. Then `cargo test --target-dir target-tests` — 486 passing, 0 failed.

- [ ] **Step 5: Commit**

```bash
git add src/api/route_resolution.rs src/api/chat_core.rs src/api/chat_core/tests.rs src/api/handlers.rs src/api/grpc.rs src/api/mod.rs
git commit -m "refactor(api): move routing resolution out of chat_core

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 3: Extract TUI keys and hints, move app tests

**Files:**
- Create: `src/tui/keys.rs`, `src/tui/app/tests.rs`
- Modify: `src/tui/app.rs`, `src/tui/mod.rs`, `src/tui/views/analysis.rs`, `tests/tui_chrome.rs`

**Interfaces:**
- Produces from `keys.rs`: `CurrentView`, `AnalysisState`, `KeyTarget`, `ScreenState<'a>`, `key_target(state: &ScreenState<'_>) -> KeyTarget`, `hints_for(target: KeyTarget) -> Vec<(&'static str, &'static str)>`, `ChatAction`, `chat_input_action`, `rule_editor_opening`, and the constants `SKILLS_FIELD`, `RULES_FIELD`, `OPTIMIZE_FIELD`, `VIEW_NAMES`.
- `hints_for` becomes a **free function**, not an associated function. Update its two call sites in `app.rs` from `Self::hints_for(...)` to `hints_for(...)`.
- Consumes: `ListBounds` from `views::projects`, `RuleEditor` from `views::rule_editor`.

- [ ] **Step 1: Move the app test block to a submodule**

Replace the `#[cfg(test)] mod tests { ... }` block at the end of `app.rs` with `#[cfg(test)] mod tests;`, putting its content in `src/tui/app/tests.rs`. Keep `use super::*;` so private items stay reachable. Verify with `cargo test --target-dir target-tests --test tui_chrome` before going further.

- [ ] **Step 2: Create `keys.rs` by moving code verbatim**

Move the constants, `CurrentView`, `AnalysisState`, `KeyTarget`, `ScreenState`, `key_target`, the five list helpers, `ChatAction`, `chat_input_action` and `rule_editor_opening` from `app.rs` to `keys.rs`. Move `hints_for` as the last item, converting it from `fn hints_for(target: KeyTarget)` inside `impl TuiApp` to `pub fn hints_for(target: KeyTarget)` at module level. Do not edit any function body.

- [ ] **Step 3: Update imports and call sites**

Register `pub mod keys;` in `src/tui/mod.rs`. In `app.rs`, import from `crate::tui::keys`. In `src/tui/views/analysis.rs`, change `use crate::tui::app::AnalysisState` to `crate::tui::keys::AnalysisState`. Change the two `Self::hints_for` calls to `hints_for`.

- [ ] **Step 4: Update the source-scanning guards in `tests/tui_chrome.rs`**

Add `const KEYS: &str = include_str!("../src/tui/keys.rs");`. Change `app_without_hint_text()` to strip `hints_for` from `KEYS` and return `APP` unchanged. Add `KEYS` to the source list in `only_the_modals_draw_borders`. Leave `a_dialog_opener_outranks_the_typing_arm` reading `APP` — `run()` did not move.

Update every doc comment in that test file that says hints live in `app.rs`: they live in `keys.rs` now. A stale comment is a false claim of the kind this repo has already had to retract twice.

- [ ] **Step 5: Prove the guards still fire (Review Focus #1, #5)**

Temporarily add the literal `"[Enter] approve"` inside a function body in `keys.rs` outside `hints_for`, and assert `no_view_hard_codes_a_key_hint` FAILS. Revert. Temporarily add `Borders::ALL` to `keys.rs` and assert `only_the_modals_draw_borders` FAILS. Revert. Confirm the working tree is byte-identical to before these probes.

Use `[Enter] approve`, not `["Enter", "approve"]`: `bracket_hint` caps bracket content at 12 characters, and the bracketed array literal is 18, so it does not trip the detector. This was discovered during Task 3 — the first probe shape returned green against a detector that was in fact working. The 12-character cap is deliberate in the detector (it keeps `chunks[2]` and `[&str; 7]` out), so change the probe, not the cap.

- [ ] **Step 6: Verify and commit**

Run: `cargo test --target-dir target-tests --test tui_chrome && cargo test --target-dir target-tests && cargo fmt --check`. Expected: all green, count unchanged at 486.

```bash
git add src/tui/keys.rs src/tui/app src/tui/app.rs src/tui/mod.rs src/tui/views/analysis.rs tests/tui_chrome.rs
git commit -m "refactor(tui): split key and hint logic into keys.rs

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 4: Remove dead `subagents` config with a compatibility stub

**Files:** Modify `src/domain/agent.rs`, `AGENTS.md`

**Interfaces:**
- `AgentConfig` loses `subagents: SubagentsConfig` and `SubagentsConfig` is deleted.
- `AgentConfig` gains a private `#[serde(default, skip_serializing)] subagents: Option<toml::Table>` field — a compatibility stub so manifests containing `[subagents]` keep deserialising.

- [ ] **Step 1: Write the failing compatibility test**

In a `#[cfg(test)]` test in `src/domain/agent.rs`:

```rust
#[test]
fn a_manifest_with_subagents_still_deserialises() {
    let toml = "name = \"Demo\"\nsystem_prompt = \"hi\"\n\
                [subagents]\nallowed = [\"one\"]\nallow_global = true\nmax_depth = 2\n";
    let config: AgentConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.name, "Demo");
}
```

- [ ] **Step 2: Run it to confirm the current code passes**

Run: `cargo test --target-dir target-tests --lib domain::agent`
Expected: PASS — this test guards the *removal*, so it must be green before and after.

- [ ] **Step 3: Remove the type, add the stub**

Delete `SubagentsConfig` and its `Default` impl. Remove `pub subagents: SubagentsConfig` and its `Default` entry. Add `#[serde(default, skip_serializing)] subagents: Option<toml::Table>`. Add a doc comment saying the field is a compatibility stub for manifests that still carry the table; the runtime never reads it.

- [ ] **Step 4: Verify and document**

Run: `cargo test --target-dir target-tests --lib domain::agent && cargo test --target-dir target-tests`. Expected: all green. Add one line to `AGENTS.md` noting that `[subagents]` is accepted but ignored.

- [ ] **Step 5: Commit**

```bash
git add src/domain/agent.rs AGENTS.md
git commit -m "refactor(agent): drop dead subagents config

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 5: Preserve `routing.hint` and `description` through PUT

**Files:** Modify `src/api/agent_api.rs`; add a test to `tests/api_integration.rs`

**Interfaces:**
- `CreateAgentRequest` gains `#[serde(default)] pub description: String` and `#[serde(default)] pub routing: Option<RoutingConfig>`.
- `build_agent_toml(req: &CreateAgentRequest, project_id: Option<&str>) -> String` emits both when present.

- [ ] **Step 1: Write the failing round-trip test**

In `tests/api_integration.rs`, using the existing `TestApp` fixture:

```rust
#[tokio::test]
async fn a_put_preserves_a_curated_routing_hint_and_description() {
    // create an agent with description "Curada" and [routing] hint "Solo facturacion"
    // PUT the same id with those two fields set
    // read the manifest from disk and assert both strings are present
}
```

The exact HTTP helper calls come from the tests already in that file; follow their pattern rather than inventing a client.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --target-dir target-tests --test api_integration a_put_preserves`
Expected: FAIL — the fields are missing from the request type so the PUT erases them.

- [ ] **Step 3: Implement**

Add the two fields to `CreateAgentRequest`. In `build_agent_toml`, emit `description = "..."` when non-empty (escape quotes like the existing `rules` code does) and:

```toml
[routing]
hint = "..."
```

when `req.routing` has a non-blank `hint`. Escape quotes the same way. Do not add `auto_skills` to the request.

- [ ] **Step 4: Run the test and the suite**

Run: `cargo test --target-dir target-tests --test api_integration a_put_preserves` → PASS. Then the full suite → 488 passing (486 + 2 new tests from Tasks 4 and 5).

- [ ] **Step 5: Commit**

```bash
git add src/api/agent_api.rs tests/api_integration.rs
git commit -m "fix(agent): stop PUT erasing description and routing hints

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 6: Share the env guard across integration tests

**Files:**
- Create: `tests/common/mod.rs`
- Modify: `tests/api_integration.rs`, `tests/router_dispatch_tests.rs`, `tests/router_wire_tests.rs`, `tests/route_api_tests.rs`

**Interfaces:**
- Produces: `pub struct RouterEnvGuard` with `fn cleared() -> Self`; `pub struct TestEnv { _env: RouterEnvGuard, _lock: MutexGuard<'static, ()> }` with `fn new(lock: &'static Mutex<()>) -> Self`.

- [ ] **Step 1: Create `tests/common/mod.rs`**

Move one copy of `RouterEnvGuard` there, with the full `ROUTER_ENV_VARS` list. Add `TestEnv` with the field order `_env` then `_lock`, and a doc comment explaining that this order is load-bearing because Rust drops fields in declaration order.

- [ ] **Step 2: Switch the four test files to the shared helper**

Each file adds `mod common;` and `use common::{RouterEnvGuard, TestEnv};`, deletes its local definitions, and replaces its local `DispatchEnv` / `PreviewEnv` / inline guard-plus-lock with `TestEnv::new(&TEST_LOCK)`. The per-file `TEST_LOCK` stays: integration test binaries are separate processes.

Keep the `RouterEnvGuard` copy inside `src/services/router_config.rs`'s unit-test module — a unit-test module cannot import an integration-test helper.

- [ ] **Step 3: Prove the guard still holds the lock while restoring (Review Focus #4)**

Add the test to `tests/api_integration.rs`, **not** to `tests/common/mod.rs`: every integration file declares `mod common;`, so a `#[test]` placed in the helper module would run four times — once per binary — and inflate the suite count.

```rust
#[test]
fn the_env_guard_restores_before_releasing_the_lock() {
    // build a TestEnv over a static mutex with ROUTER_ENABLED set to a sentinel
    // drop it
    // lock the same mutex from this thread and assert ROUTER_ENABLED is back to
    // its pre-guard value
}
```

If the field order in `TestEnv` were reversed, the mutex would be released before the restore and a concurrent test could observe the cleared value. Holding the lock in the same thread after `drop` does not prove that directly; the structural guarantee is that `TestEnv` owns both guards in the right order, so no caller can reorder them. The test pins the observable half: the environment is restored by the time `TestEnv` is dropped.

- [ ] **Step 4: Verify and commit**

Run: `cargo test --target-dir target-tests --test api_integration --test router_dispatch_tests --test router_wire_tests --test route_api_tests` → all green. Then `ROUTER_ENABLED=true ROUTER_URL=http://127.0.0.1:1 cargo test --target-dir target-tests` → still green, proving the ambient env cannot leak.

```bash
git add tests/common tests/api_integration.rs tests/router_dispatch_tests.rs tests/router_wire_tests.rs tests/route_api_tests.rs
git commit -m "refactor(test): share the router env guard across test binaries

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 7: Correct `.env.example`

**Files:** Modify `.env.example`

- [ ] **Step 1: Change the port**

`PORT=8080` → `PORT=3000`, matching `Config::from_env()` and avoiding the `llama-server` port. Adjust the adjacent comment so it says the router URL points at `llama-server` and is separate from `PORT`.

- [ ] **Step 2: Verify and commit**

Run: `grep -n '^PORT=' .env.example` → `3000`. No test changes needed.

```bash
git add .env.example
git commit -m "docs: align the example port with the code default

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 8: Make `cargo audit` a CI gate

**Files:** Modify `.github/workflows/ci.yml`

- [ ] **Step 1: Add the step**

After the existing "Install Rust" step, install `cargo-audit` with `cargo install cargo-audit --locked`. Before the Tests step, add:

```yaml
- name: Security advisories
  run: cargo audit
```

as a hard gate. Update the job name to include audit.

- [ ] **Step 2: Verify the command locally**

Run: `cargo audit` → exit 0, "0 vulnerabilities found". Record the exact output.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: fail the build on dependency advisories

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 9: Clear clippy lints and make clippy a gate

**Files:** All lint sites reported by `cargo clippy --all-targets`; `.github/workflows/ci.yml`

- [ ] **Step 1: Capture the full lint list**

Run: `cargo clippy --all-targets --message-format short 2>&1 | tee /tmp/opencode/clippy-before.txt`. Count by category. Expected categories from the spec: `await_holding_lock` ×16 (all in `#[cfg(test)]`), `into_iter` ×5, redundant `clone` ×6, same-type casts ×2, `too_many_arguments` ×3, `new_without_default` ×4, and the handful of one-offs.

- [ ] **Step 2: Fix the mechanical lints**

Apply `cargo clippy --fix --allow-dirty --all-targets` for the automatically fixable ones (the `into_iter`, clone, cast, `if let`, `is_ok`, `sort_by_key`, deref and `Error::other` cases). Review the diff before keeping it: no behaviour change is acceptable.

- [ ] **Step 3: Fix the remaining lints by hand**

`new_without_default`: add `Default` impls. `too_many_arguments`: add `#[allow(clippy::too_many_arguments)]` at each site with a comment naming the stable interface it protects — `build_app_state` is called from five places and changing its signature is out of scope. `await_holding_lock`: all 16 are in tests; add `#[allow(clippy::await_holding_lock)]` to those test functions with a comment that the guard is held deliberately to serialise environment access across the test runtime.

- [ ] **Step 4: Flip the CI step to a gate**

Change `- name: Clippy (report only)` / `run: cargo clippy --all-targets` to a hard gate with `-D warnings`. Remove the report-only comment and replace it with one stating that all lints are now resolved.

- [ ] **Step 5: Full verification**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --target-dir target-tests && cargo audit`. Expected: all four exit 0, 489 tests passing.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "style: clear clippy lints and gate CI on warnings

Co-Authored-By: opencode <opencode@local>"
```

---

## Final verification (after all tasks)

- `cargo fmt --check` clean
- `cargo clippy --all-targets -- -D warnings` exit 0
- `cargo test --target-dir target-tests` → 489 passing, 0 failed (486 baseline + 1 manifest-compat test in Task 4 + 1 PUT round-trip in Task 5 + 1 guard-order test in Task 6)
- `cargo audit` → 0 vulnerabilities, 0 warnings
- `wc -l src/api/chat_core.rs src/tui/app.rs` → both under 1700, with the split modules and test submodules accounting for the rest
- The two source-scanning guards demonstrated to fire under mutation (Task 3 step 5)
