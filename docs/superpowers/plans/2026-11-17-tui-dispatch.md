# TUI Key Dispatch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace two byte-position source guards with behavioural tests by extracting the TUI key handler into a testable method.

**Architecture:** `run` keeps the terminal, drawing and `event::read()`. The `Event::Key` body moves verbatim into `TuiApp::dispatch(&mut self, key: KeyEvent) -> DispatchOutcome`, which decides and executes. Tests build a `TuiApp` from a stub `AppState`, open a dialog, inject a key and assert on the app state. The two source-position guards are deleted; text scans that do not depend on control flow stay.

**Tech Stack:** Rust 2021, ratatui/crossterm (`KeyEvent`, `KeyCode`, `KeyModifiers`), existing test fixtures.

**Spec:** `docs/superpowers/specs/2026-11-17-tui-dispatch-design.md`

## Global Constraints

- No TUI behaviour change: the dialog order and every key arm stay as they are.
- Move the key-handling body **verbatim**; replace only `continue` with `return DispatchOutcome::Continue` and `break` with `DispatchOutcome::Quit`.
- `TuiApp::dispatch` is a private method; tests live in `src/tui/app/tests.rs` with `use super::*`. No visibility widening.
- `TuiApp::new` must not start touching the terminal — tests depend on constructing it without one.
- The input filter `if key.kind != Press { continue; }` stays in `run`.
- Text-scanning guards (`no_view_hard_codes_a_key_hint`, `only_the_modals_draw_borders`, `only_the_dispatch_draws_the_footer`, `keys_without_hint_text`) stay unchanged except for removing the two position-based ones.
- Commit trailer `Co-Authored-By: opencode <opencode@local>`. Conventional Commits, lower-case imperative subject, no trailing period.
- Baseline: 489 tests passing, fmt/clippy/audit clean.

## Review Focus

1. **A dialog becomes inert because a view guard eats its key.** → behaviour test `an_open_dialog_consumes_keys_the_form_would_handle`, table-driven over the four dialogs, in Task 2.
2. **An opener is typed into the form instead of opening its dialog.** → `the_picker_opener_is_not_typed_into_the_form` and `the_rules_opener_is_not_typed_into_the_form` in Task 2.
3. **The new tests pass without holding the invariant.** → Task 2 includes mutation checks: move the form guard above a dialog's opener in `dispatch`, confirm the test fails, restore and confirm green.
4. **`dispatch` diverges from the old key arm.** → Task 1 verifies the moved body is byte-equivalent to the extracted block except for `continue`/`break` substitutions.
5. **A surviving text scan is broken by the edit.** → Task 3 runs `tui_chrome` in full and mutation-checks the hint detector.

---

### Task 1: Extract `TuiApp::dispatch`

**Files:**
- Modify: `src/tui/app.rs`
- Modify: `src/tui/app/tests.rs` (test helpers only if needed)

**Interfaces:**
- Produces: `pub(super) enum DispatchOutcome { Continue, Quit }` and `fn dispatch(&mut self, key: event::KeyEvent) -> DispatchOutcome` on `TuiApp`, both in `app.rs`.

- [ ] **Step 1: Record the exact block to move**

Run: `sed -n '799,1432p' src/tui/app.rs` and save it. This is the `Event::Key(key) => { ... }` arm from `run`, ending at the closing brace before `Event::Mouse`.

- [ ] **Step 2: Add the `DispatchOutcome` enum and the `dispatch` method**

In `app.rs`, next to `impl TuiApp`, define:

```rust
pub(super) enum DispatchOutcome { Continue, Quit }
```

Add `fn dispatch(&mut self, key: event::KeyEvent) -> DispatchOutcome` to `impl TuiApp`. Move the recorded block into it verbatim, with two mechanical substitutions:
- `continue;` (inside the key arm) → `return DispatchOutcome::Continue;`
- `break;` (the `KeyCode::Char('q')` arm) → `return DispatchOutcome::Quit;`

The `if key.kind != event::KeyEventKind::Press { continue; }` check stays in `run` and is **not** moved.

- [ ] **Step 3: Replace the arm in `run`**

In `run`, replace the whole `Event::Key(key) => { ... }` block with:

```rust
Event::Key(key) => {
    if key.kind != event::KeyEventKind::Press {
        continue;
    }
    if let DispatchOutcome::Quit = self.dispatch(key) {
        break;
    }
}
```

- [ ] **Step 4: Verify the move and behaviour**

Run: `cargo fmt --check && cargo test --target-dir target-tests --test tui_chrome && cargo test --target-dir target-tests`.
Expected: all green, test count unchanged at 489. `dispatch` has one caller; the compiler catches a missed substitution.

- [ ] **Step 5: Commit**

```bash
git add src/tui/app.rs
git commit -m "refactor(tui): extract the key handler into dispatch

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 2: Replace the two source-position guards with behaviour tests

**Files:**
- Modify: `tests/tui_chrome.rs` — delete `a_dialog_opener_outranks_the_typing_arm` and `every_dialog_is_intercepted_before_the_view_it_sits_over`
- Modify: `src/tui/app/tests.rs` — add the four behavioural tests

**Interfaces:**
- Consumes: `TuiApp::dispatch`, `DispatchOutcome` from Task 1; `TuiApp::new(Arc<AppState>)`.
- Fixtures: build `AppState` with the stub pattern from `tests/router_dispatch_tests.rs::state_with_project_agent`, or a minimal in-module stub. `SkillPicker::for_agent(Vec<InstalledSkill>, &[String], &[String])` is the picker constructor.

- [ ] **Step 1: Write the failing picker-opener test**

In `src/tui/app/tests.rs`:

```rust
#[test]
fn the_picker_opener_is_not_typed_into_the_form() {
    // AppState stub, TuiApp::new(state), form_field_index = SKILLS_FIELD,
    // form_skills = "" and a picker openable for the project.
    // dispatch(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
    // assert: form_skills is still "" and skill_picker.is_some()
}
```

Run: `cargo test --target-dir target-tests --lib the_picker_opener_is_not_typed_into_the_form`.
Expected: FAIL until the guard order is exercised — if it passes immediately, the fixture did not actually hit the form arm; make the setup reach `AgentForm` and `SKILLS_FIELD` explicitly.

- [ ] **Step 2: Write the rules-opener test**

```rust
#[test]
fn the_rules_opener_is_not_typed_into_the_form() {
    // form_field_index = RULES_FIELD, form_rules empty
    // dispatch(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
    // assert: form_rules still empty and rule_editor.is_some()
}
```

Run it; expected FAIL for the same reason until the fixture is right.

- [ ] **Step 3: Write the table-driven dialog-interception test**

```rust
#[test]
fn an_open_dialog_consumes_keys_the_form_would_handle() {
    // For each of: skill_picker, rule_editor, opt_tools, confirm_delete
    //   build a TuiApp with that dialog open and current_view = AgentForm
    //   dispatch a key the form would act on: KeyCode::Char('s'), Esc, Tab
    //   assert the form state did not change (form_field_index, form_skills,
    //   form_rules, current_view all as before)
}
```

One case per dialog, mirroring the four source checks it replaces. Each case must open the dialog with a real constructor, not a dummy flag, or the test cannot prove reachability.

- [ ] **Step 4: Prove the tests can fail (Review Focus #2, #3)**

Temporarily move the `if self.current_view == CurrentView::AgentForm {` guard above one dialog's guard in `dispatch` and run the table-driven test. Expected: FAIL naming that dialog. Restore. Then swap `KeyCode::Char(c)` above the `s` opener and run the opener tests. Expected: FAIL. Restore. Confirm the tree is byte-identical to before the probes with `git status --porcelain`.

- [ ] **Step 5: Delete the two source-position guards**

Remove `a_dialog_opener_outranks_the_typing_arm` and `every_dialog_is_intercepted_before_the_view_it_sits_over` from `tests/tui_chrome.rs`, together with their doc comments. Keep `shifted`/`shipped()` helpers — the surviving scans use them.

- [ ] **Step 6: Verify and commit**

Run: `cargo test --target-dir target-tests --test tui_chrome && cargo test --target-dir target-tests`.
Expected: all green; test count = 489 − 2 + 3 = 490.

```bash
git add src/tui/app.rs src/tui/app/tests.rs tests/tui_chrome.rs
git commit -m "test(tui): intercept keys by behaviour, not source position

Co-Authored-By: opencode <opencode@local>"
```

---

### Task 3: Update documentation for the removed guard

**Files:** Modify `AGENTS.md`

**Interfaces:** none.

- [ ] **Step 1: Rewrite the stale paragraph**

The TUI section currently says `a_dialog_is_intercepted_before_the_view_it_sits_over` in `tests/tui_chrome.rs` pins the ordering and is "a source-position check, because there is nowhere to inject a keystroke". Replace it with: `TuiApp::dispatch` is the one place keys are handled; `tests/tui/app/tests.rs` injects keys into a `TuiApp` built from a stub `AppState` and asserts that each open dialog consumes the key before the view underneath. No other sentence about dialog priority should remain false.

- [ ] **Step 2: Verify and commit**

Run: `grep -n 'source-position check' AGENTS.md` → no match.

```bash
git add AGENTS.md
git commit -m "docs: key interception is tested by behaviour now

Co-Authored-By: opencode <opencode@local>"
```

---

## Final verification

- `cargo fmt --check` clean
- `cargo clippy --all-targets -- -D warnings` exit 0
- `cargo test --target-dir target-tests` → 490 passing, 0 failed
- `cargo audit` clean
- `tests/tui_chrome.rs` still contains exactly three source scans (borders, hint text, footer count) and no position-based guard
- The two mutation probes in Task 2 step 4 were demonstrated and the tree restored byte-identically
