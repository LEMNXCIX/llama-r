# TUI key dispatch — design

**Date:** 2026-11-17
**Status:** proposed
**Scope:** replace two byte-position source guards with behavioural tests. No TUI behaviour change.

## Problem

`TuiApp::run` reads stdin directly (`event::read()`) and handles every key in a ~1000-line `match`. Two invariants are guarded only by comparing **byte offsets in the source text**, because there is no way to inject a keystroke into `run`:

1. `a_dialog_opener_outranks_the_typing_arm` — `Ctrl+R` for the rule editor and `s` for the skill picker must appear before the `KeyCode::Char(c)` arm that types into every field. If one slips below it, `hints_for` keeps advertising a key that does nothing. This already happened once: the skill picker shipped completely inert.
2. `every_dialog_is_intercepted_before_the_view_it_sits_over` — the four dialog guards (opt tools, rule editor, delete confirm, skill picker) must come before the agent-form guard. The first guard to match consumes the key and `continue`s, so a dialog underneath the form is unreachable. This is the check that would have caught the picker bug.

Both guards are source-position checks over `include_str!("../src/tui/app.rs")`. They are fragile in a specific way: they verify the *text* is in the right order, not that the running code is. A refactor that preserves the text but changes control flow passes them.

## Goal

Give the key handler a signature and a testable seam, so these two invariants can be asserted with **behaviour**: inject a key into a `TuiApp`, observe what changed. Then delete the two source-position guards.

## Non-goals

- Reordering the actual dialog guards. The current order is correct and stays.
- Making every `KeyCode` arm individually testable. Only the interception priority — the fragile part — needs behaviour tests.
- Changing `render_footer`, bar, border or hint-text guards. Those scan source for *text*, not control flow, and are not fragile in this way. `tests/tui_chrome.rs` is explicit about the distinction.
- Changing the TUI's public API or the storage/HTTP/gRPC surfaces.

## Design

### The seam

Extract the whole `Event::Key(key) => { ... }` body from `run` into:

```rust
/// Handle one key. Returns whether the event loop should keep running.
fn dispatch(&mut self, key: event::KeyEvent) -> DispatchOutcome
```

`DispatchOutcome` is a small enum: `Continue`, `Quit`. The arms themselves are moved verbatim, with `continue` in the key handler replaced by `return DispatchOutcome::Continue` and the quit paths by `DispatchOutcome::Quit`. `run` becomes:

```rust
Event::Key(key) => {
    if key.kind != event::KeyEventKind::Press { continue; }
    if let DispatchOutcome::Quit = self.dispatch(key) { break; }
}
```

The `if key.kind != Press` check stays in `run`: it is input filtering, not dispatch.

`dispatch` takes `&mut self` and **executes** the action, as chosen. The existing pure helpers (`picker_action`, `rule_action`, `chat_input_action`, `opt_tools_action`) stay as-is — `dispatch` calls them; they remain individually unit-tested.

### Why this is testable without a terminal

`TuiApp::new(state: Arc<AppState>)` only stores the state and initialises plain fields. It does not open a terminal, enable raw mode, or touch stdout. Those live in `run`. A test can therefore:

- build an `AppState` from the existing stubs (`tests/router_dispatch_tests.rs` has `state_with_project_agent` and a `FakeRouter`; `tests/tui_chrome.rs` has render-only fixtures),
- `TuiApp::new(state)`,
- set the dialog field directly (`app.skill_picker = Some(picker)`, `app.rule_editor = Some(editor)`, …),
- call `app.dispatch(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))`,
- assert on `app.skill_picker`, `app.form_skills`, `app.current_view`, etc.

`dispatch` is a private method. Put the tests in `src/tui/app/tests.rs` (already a `#[cfg(test)]` submodule with `use super::*`) so no visibility widening is needed.

### The four behavioural tests

| Guard removed | Replacement test | Assertion |
|---|---|---|
| `a_dialog_opener_outranks_the_typing_arm` | `the_picker_opener_is_not_typed_into_the_form` | `form_field_index = SKILLS_FIELD`, available skills installed, dispatch `s` → `form_skills` unchanged, `skill_picker.is_some()` |
| same | `the_rules_opener_is_not_typed_into_the_form` | `form_field_index = RULES_FIELD`, dispatch `Ctrl+R` → `form_rules` unchanged, `rule_editor.is_some()` |
| `every_dialog_is_intercepted_before_the_view_it_sits_over` | `an_open_dialog_consumes_keys_the_form_would_handle` (table-driven over the four dialogs) | with dialog open and `current_view = AgentForm`, dispatch a key the form would act on (`Tab`, `Esc`, `Enter`) → dialog still owns the key: form state unchanged, dialog state updated or unchanged as appropriate |

The table-driven test replaces four source-position checks with one behaviour test with four cases — the same "grows a line per dialog" property, but it now proves the dialog is reachable rather than that the text is in order.

### What stays

- `keys_without_hint_text()`, `no_view_hard_codes_a_key_hint`, `only_the_modals_draw_borders`, `only_the_dispatch_draws_the_footer` — the text scans stay. `a_dialog_is_intercepted_before_the_view_it_sits_over` is the second source-position guard and **is removed**.
- `shifted` / `shipped()` helpers used by the surviving text scans stay.
- The `#[cfg(test)]` inner attribute in `app/tests.rs` that marks the file as all-test code for `shipped()` stays.

### Documentation

`AGENTS.md` describes `a_dialog_is_intercepted_before_the_view_it_sits_over` as the source-position check that caught the picker bug. That paragraph must be rewritten: the check now injects a key, not compares bytes. The sentence "it is a source-position check, because there is nowhere to inject a keystroke" becomes false and must go.

## Risks

- `dispatch` will be large (~700 lines). It is a private method with one caller; the size is a consequence of moving code, not a new design.
- Building an `AppState` in a TUI test drags in more fixtures than the render tests need. Reuse `state_with_project_agent` rather than creating a new stub.
- `KeyEvent` equality and modifiers: use `KeyEvent::new(code, modifiers)` so `kind` is `Press` by default, matching what `run` filters for.

## Verification

- `cargo test --target-dir target-tests --test tui_chrome` — all surviving guards pass, and the two removed ones are gone.
- Mutation checks: break the guard order in `dispatch` (move the form arm above a dialog's) and confirm the new behaviour test fails; restore and confirm green. Same for the opener-vs-typing order. This is the proof the new tests actually hold the invariant.
- `cargo test --target-dir target-tests` — 489+ tests unchanged in count except the two removed guards and the new tests.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo audit` — all clean.
