# TUI Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the cluttered, box-heavy TUI with the approved dZen-minimal design: a shared chrome with a one-line context bar, and views that use no boxes at all.

**Architecture:** Two new modules own everything visual. `theme.rs` holds colour/style tokens with fixed meaning; `chrome.rs` computes the layout (bar row, body, footer row) and renders the furniture. Views stop calling `f.area()` and instead receive the `body: Rect` the chrome hands them, which is what makes the shared bar possible — today every view lays out the whole screen including the bar's row, which is why the bar silently stopped rendering.

**Tech Stack:** Rust 1.x, `ratatui = "0.30.0"`, `crossterm`. Tests render into `ratatui::backend::TestBackend` and assert on the buffer — no terminal needed.

**Spec:** `docs/superpowers/specs/2026-10-02-tui-redesign-design.md`

## Global Constraints

- Visual direction is **C — dZen minimal**: one-line context bar, thin `─` separators, **no boxes** except the modal dialogs. (An earlier revision of this line also allowed the chat input area. Task 4 replaces the 3-row bordered input box with a prompt marker, and nothing else in the interface is permitted a border — so the allowance went with the box.)
- The context bar is exactly **1 row**, with **no border**.
- The context bar always shows the active view and the current project/agent; this is the fix for context loss between views.
- Each view declares its own key hints; the chrome renders the footer. No view may hard-code hint text.
- Colour tokens carry fixed meaning: `ok` green, `warn` amber, `error` red, `chrome` `DarkGray`, `action` cyan, `active` yellow+bold, `content` white.
- Views must not call `f.area()`; they receive `body: Rect`.
- No view may use hard-coded `Constraint::Length(N)` for its own furniture; the chrome owns fixed heights.
- No new dependencies.
- Comments and identifiers in English, matching the repo.

## Review Focus

Five conditions the spec implies but no task's happy-path test covers. Each line gets a test in the task named beside it.

1. **Long project/agent names overflow the one-line bar.** A 60-char project name plus an agent name must not push the view names off the row or wrap. Expected: view names survive, the context is truncated from the right with an ellipsis.
2. **Terminal too small for the layout.** A terminal narrower or shorter than the chrome's fixed rows must not panic or underflow `u16` subtraction. Expected: body clamps to zero height; bar and footer still render.
3. **Multibyte characters break column alignment.** Project names with accents or CJK render 1–2 cells per char, so space-padded columns misalign. Expected: use `unicode-width` style width via ratatui's `Line::width()`, not `chars().count()`.
4. **A multi-line input buffer in a one-line input.** After `Shift+Enter`, the buffer has newlines but the field is one row. Expected: the last line is what shows; the newline is not silently lost on render.
5. **Empty lists.** A project with no agents, or no projects at all, must render the empty state rather than a divider pointing at nothing.

## Test Helpers

Every test module that asserts on rendered text needs these two local helpers. Test modules
do not share code, so copy them into each file that uses them:

```rust
fn row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol().to_string())
        .collect()
}

fn all_text(buffer: &ratatui::buffer::Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| row_text(buffer, y))
        .collect::<Vec<_>>()
        .join("\n")
}
```

---

### Task 1: Theme tokens

**Files:**
- Create: `src/tui/theme.rs`
- Modify: `src/tui/mod.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `theme::{ok, warn, error, chrome, action, active, content}() -> ratatui::style::Style`.

- [ ] **Step 1: Write the failing test**

Create `src/tui/theme.rs` with this test module. It pins that each token is distinguishable and that `active` is bold, which is the only style assertion that carries meaning:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    #[test]
    fn tokens_have_their_documented_colours() {
        assert_eq!(ok().fg, Some(Color::Green));
        assert_eq!(warn().fg, Some(Color::Yellow));
        assert_eq!(error().fg, Some(Color::Red));
        assert_eq!(chrome().fg, Some(Color::DarkGray));
        assert_eq!(action().fg, Some(Color::Cyan));
        assert_eq!(active().fg, Some(Color::Yellow));
        assert_eq!(content().fg, Some(Color::White));
    }

    #[test]
    fn only_active_is_bold() {
        assert!(active().add_modifier.contains(Modifier::BOLD));
        assert!(!content().add_modifier.contains(Modifier::BOLD));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --target-dir target-tests --lib tui::theme`
Expected: FAIL — `cannot find function ok in this scope`.

- [ ] **Step 3: Implement the tokens**

In `src/tui/theme.rs`, above the test module, implement seven functions each returning `Style::default().fg(<colour>)`, with `active()` also carrying `Modifier::BOLD`. Add `pub mod theme;` to `src/tui/mod.rs`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --target-dir target-tests --lib tui::theme`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add src/tui/theme.rs src/tui/mod.rs
git commit -m "feat(tui): add theme tokens with fixed semantic meaning"
```

---

### Task 2: Chrome layout, context bar, key hints

**Files:**
- Create: `src/tui/chrome.rs`
- Modify: `src/tui/mod.rs`

**Interfaces:**
- Consumes: `theme::{ok, warn, error, chrome, action, active, content}` from Task 1.
- Produces:
  - `pub struct Chrome { pub bar: Rect, pub body: Rect, pub footer: Rect }`
  - `pub struct Context { pub project: Option<String>, pub agent: Option<String> }`
  - `pub const BAR_HEIGHT: u16 = 1;` and `pub const FOOTER_HEIGHT: u16 = 1;`
  - `pub fn layout(area: Rect) -> Chrome`
  - `pub fn render_bar(f: &mut Frame, rect: Rect, views: &[&str], active: usize, ctx: &Context, api_running: bool)` — `api_running`, not `healthy`: the dot is the HTTP *listener's* liveness, and a parameter called `healthy` is how a reader comes to believe it reports the provider.
  - `pub fn render_footer(f: &mut Frame, rect: Rect, hints: &[(&str, &str)])`
  - `pub fn separator(f: &mut Frame, rect: Rect)`
  - `pub fn context_label(ctx: &Context) -> String` — `"fudi/ops"`, `"fudi"`, `"ops"`, or `"direct"`.

- [ ] **Step 1: Write the failing tests**

The first test is the regression guard for the bug being fixed; the bar must be visible in a one-row rect.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // `row_text` from Test Helpers.
    fn row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect()
    }

    #[tokio::test]
    async fn context_bar_renders_into_a_single_row() {
        let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let ctx = Context { project: Some("fudi".into()), agent: Some("ops".into()) };
        terminal
            .draw(|f| {
                render_bar(f, f.area(), &["Dashboard", "Projects", "Chat"], 2, &ctx, true);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row = row_text(&buffer, 0);
        assert!(row.contains("Chat"), "active view must be on the bar: {row:?}");
        assert!(row.contains("fudi/ops"), "context must be on the bar: {row:?}");
        assert!(
            !row.contains('│') && !row.contains('┌'),
            "the bar must not draw a border: {row:?}"
        );
    }
}
```

Fix the call in the snippet to `render_bar(f, f.area(), &[...], 2, &ctx, true)`.

Add two more tests:

```rust
    #[test]
    fn layout_reserves_bar_and_footer_rows() {
        let c = layout(Rect::new(0, 0, 80, 24));
        assert_eq!(c.bar.height, BAR_HEIGHT);
        assert_eq!(c.footer.height, FOOTER_HEIGHT);
        assert_eq!(c.body.y, 1);
        assert_eq!(c.body.height, 22);
    }

    #[test]
    fn layout_clamps_instead_of_underflowing_on_a_tiny_terminal() {
        let c = layout(Rect::new(0, 0, 4, 1));
        assert_eq!(c.body.height, 0, "must clamp, not wrap around");
    }
```

And Review Focus 1 and 2:

```rust
    #[tokio::test]
    async fn long_context_is_truncated_and_view_names_survive() {
        let terminal = Terminal::new(TestBackend::new(50, 10)).unwrap();
        let ctx = Context {
            project: Some("un-proyecto-con-nombre-realmente-larguísimo".into()),
            agent: Some("un-agente-igual-de-largo-y-que-no-cabe".into()),
        };
        terminal.draw(|f| render_bar(f, f.area(), &["Dashboard", "Projects", "Chat"], 0, &ctx, true)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row = row_text(&buffer, 0);
        assert!(row.contains("Dashboard"), "view names must survive: {row:?}");
        assert!(row.contains('…'), "the long context must be truncated: {row:?}");
        // Not `row.chars().count() == 50`: `row_text` returns one symbol per column
        // and the buffer is 50 wide, so that holds for every implementation. What
        // the one-row rule actually forbids is the bar spilling onto row 1.
        assert_eq!(
            row_text(&buffer, 1).trim(),
            "",
            "the bar must stay on one row: {:?}",
            row_text(&buffer, 1)
        );
    }

    #[tokio::test]
    async fn tiny_terminal_renders_without_panicking() {
        for (w, h) in [(1u16, 1u16), (3, 2), (10, 1), (2, 40)] {
            let terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            let c = layout(Rect::new(0, 0, w, h));
            terminal
                .draw(|f| {
                    render_bar(f, c.bar, &["Dashboard"], 0, &Context::default(), false);
                    render_footer(f, c.footer, &[("q", "quit")]);
                })
                .unwrap();
        }
    }
```

Give `Context` a `Default` derive so `Context::default()` works.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --target-dir target-tests --lib tui::chrome`
Expected: FAIL — `cannot find function render_bar in this scope`.

- [ ] **Step 3: Implement chrome.rs**

Implement, in order:

- `BAR_HEIGHT`/`FOOTER_HEIGHT` as `u16 = 1`.
- `Chrome` with three `Rect` fields, and `layout(area)` returning bar at `(area.x, area.y, area.width, BAR_HEIGHT)`, footer pinned to the last row, and body as the rows between. Use `saturating_sub` on every subtraction so a short terminal clamps to zero rather than wrapping.
- `Context` deriving `Default`, plus `context_label`.
- `render_bar` — one `Line` of spans: view names joined by `" │ "` in `chrome()`, the active one in `active()`; a spacer; the context label in `chrome()`; `●`/`○` in `ok()`/`error()`. **Truncation rule:** fit *the context label only*, shortening it from the right and ending it in `…`; hand the assembled line to a `Paragraph`, which does not wrap and clips at `rect.width`. Nothing is dropped selectively — past the cut, the gap, then the dot, then the last view names go, and the product's name is the last thing the cut reaches rather than a thing it never reaches. (An earlier revision of this rule said "view names are never dropped", which the clip falsifies: the six shipped names need 66 columns and the group needs three more, so the dot needs 69 before the label has a column of its own. Pin that width — `the_dot_needs_sixty_nine_columns_before_the_label_gets_one` in `src/tui/chrome.rs` does.) Compute widths with `Line::width()`, never `chars().count()` (Review Focus 3).
- `render_footer` — a `Line` of `key: hint` pairs, keys in `action()`, hints in `chrome()`.
- `separator` — a `Line` of `─` filling `rect.width`, in `chrome()`.

Add `pub mod chrome;` to `src/tui/mod.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --target-dir target-tests --lib tui::chrome`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add src/tui/chrome.rs src/tui/mod.rs
git commit -m "feat(tui): add shared chrome with a one-row context bar"
```

---

### Task 3: Give views a body rect instead of the whole screen

**Files:**
- Modify: `src/tui/app.rs` (the `terminal.draw` closure, ~line 524)
- Modify: `src/tui/views/chat.rs`, `dashboard.rs`, `projects.rs` (signature + layout root)

**Interfaces:**
- Consumes: `chrome::{Chrome, layout, render_bar, render_footer}` and `Context` from Task 2.
- Produces: every `render_*` function now takes `body: Rect` as its second parameter and lays out inside it.

- [ ] **Step 1: Write the failing test**

The bug this task exists to fix: every view calls `.split(f.area())`, so a view's own layout covers the bar's row. Add this to `src/tui/chrome.rs` tests — it draws the bar and *then* a view that honours `body`, and asserts both survive:

```rust
    #[test]
    fn a_view_drawn_into_body_leaves_the_bar_intact() {
        let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| {
            let c = layout(f.area());
            let ctx = Context { project: Some("fudi".into()), agent: Some("ops".into()) };
            render_bar(f, c.bar, &["Dashboard", "Projects", "Chat"], 0, &ctx, true);
            // Stand-in for a view: fill the body region only.
            f.render_widget(ratatui::widgets::Paragraph::new("body content"), c.body);
        }).unwrap();
        let buffer = terminal.backend().buffer().clone();
        assert!(row_text(&buffer, 0).contains("Dashboard"), "bar row must survive");
        assert!(
            (0..buffer.area.width).any(|x| buffer[(x, 1)].symbol() != " "),
            "the body must start on the row after the bar"
        );
    }

    #[test]
    fn body_does_not_overlap_the_bar_or_the_footer() {
        let c = layout(Rect::new(0, 0, 80, 24));
        assert!(c.body.y >= c.bar.y + c.bar.height, "body must start below the bar");
        assert!(c.body.y + c.body.height <= c.footer.y, "body must end above the footer");
    }
```

- [ ] **Step 2: Run the tests to verify they pass, and confirm the real bug by inspection**

Run: `cargo test --target-dir target-tests --lib tui::chrome`
Expected: PASS — `layout` and `render_bar` already satisfy these, since the bar renders first and nothing overwrites it in this test.

That PASS is expected here; the test pins the invariant that the *wiring* must preserve. The bug it guards against is visible by inspection until Step 3: `grep -n "f.area()" src/tui/views/*.rs` currently matches every view, and each of them splits the whole screen including row 0.

- [ ] **Step 3: Change the render signatures and the dispatch**

In `src/tui/app.rs`, replace the `render_tab_bar(f, &self.current_view);` call and every per-view call inside the `terminal.draw` closure with:

```rust
let chrome = chrome::layout(f.area());
let ctx = Context {
    project: self.context_store.list_all_projects().get(self.project_index).map(|p| p.project_id.clone()),
    agent: self.chat_selected_agent.clone(),
};
chrome::render_bar(f, chrome.bar, &["Dashboard", "Projects", "Chat"], self.current_view.index(), &ctx, self.state.api_running.load(Ordering::SeqCst));
match self.current_view {
    CurrentView::Dashboard => render_dashboard(f, chrome.body, &self.state, self.log_scroll),
    // ...each arm gains `chrome.body` as its second argument
}
chrome::render_footer(f, chrome.footer, &self.hints_for(self.current_view));
```

Add `impl CurrentView { fn index(&self) -> usize }` mapping the six variants to `0..5` in declaration order. Add `fn hints_for(&self, view: CurrentView) -> Vec<(&'static str, &'static str)>` returning each view's key hints; this is the single place hint text lives (Task 9 deletes the per-view copies).

In each view, add `body: Rect` as the second parameter and replace `.split(f.area())` with `.split(body)` and `let area = f.area()` with `let area = body`. Views keep their current internal layout here — only the rect they draw into changes.

`render_chat` currently returns `(usize, Rect)` and `app.rs` stores both into
`self.chat_scroll_max` and `self.chat_messages_rect` for later click handling. Keep the return
type unchanged in this task — the message rect is still derived inside the view. Note that
`app.rs` also uses `self.context_store.list_all_projects().get(self.project_index).map(|p| p.project_id.clone())`
for the bar's `Context`; `Ordering` is already imported at `src/tui/app.rs:21`.

- [ ] **Step 4: Verify the bar is now visible and nothing overlaps**

Run: `cargo build --target-dir target-tests 2>&1 | grep -E "^error"` then
`cargo test --target-dir target-tests 2>&1 | grep -E "^test result"`
Expected: compiles; all pre-existing tests still PASS. The visual change lands in Tasks 4–8; this task only removes the overlap that made the bar invisible.

- [ ] **Step 5: Commit**

```bash
git add src/tui/app.rs src/tui/views/
git commit -m "refactor(tui): give views a body rect so the shared bar survives"
```

---

### Task 4: Chat view, one-line input

**Files:**
- Modify: `src/tui/views/chat.rs`
- Modify: `src/tui/app.rs` (key handling for `Shift+Enter`)

**Interfaces:**
- Consumes: `chrome::{render_footer, separator}`, `theme`, `body: Rect` from Task 3.
- Produces: `chat::input_display(buffer: &str, width: u16) -> String` — the last line of a possibly multi-line buffer, truncated to `width` (Review Focus 4). Chat renders no bordered block at all, the input area included.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // `all_text` from Test Helpers.

    #[test]
    fn input_display_shows_the_last_line_of_a_multiline_buffer() {
        assert_eq!(input_display("one\ntwo\nthree", 40), "three");
        assert_eq!(input_display("single", 40), "single");
        assert_eq!(input_display("", 40), "");
    }

    #[test]
    fn input_display_truncates_to_the_available_width() {
        let shown = input_display("abcdefghij", 5);
        // Display width, not `chars().count()`: the two differ on any name with an
        // accent or a wide glyph, and only the first one is what a column is.
        assert!(ratatui::text::Line::from(shown.as_str()).width() <= 5, "{shown:?}");
    }

    #[test]
    fn chat_renders_without_any_bordered_block() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| {
            render_chat(
                f,
                Rect::new(0, 1, 80, 22),
                &[("user".to_string(), "hola".to_string())],
                "",
                false,
                None,
                &Some("soporte".to_string()),
                &Some("fudi".to_string()),
                &[],
                0,
                0,
            );
        }).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text = all_text(&buffer);
        // No box characters anywhere: the input area has none either, so there is
        // nothing to except.
        for ch in ['┌', '┐', '└', '┘', '│'] {
            assert!(!text.contains(ch), "chat must not draw box char {ch:?}");
        }
        assert!(text.contains('hola'), "the message must render");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --target-dir target-tests --lib tui::views::chat`
Expected: FAIL — `cannot find function input_display in this scope`.

- [ ] **Step 3: Implement the one-line chat**

In `chat.rs`: implement `input_display`, then rebuild the layout inside `body` as a vertical split of `[Min(0), Length(1), Length(1)]` — messages, a separator, the input row. The input row is `"> "` in `action()` followed by `input_display`. No `Block::default().borders(Borders::ALL)` anywhere in this file.

Keep the existing `thinking_line(elapsed)` behaviour and `agent_header_label` unchanged — the agent/project context moved to the shared bar in Task 3, so delete the per-view header block.

In `app.rs`, in the chat input key handler, accept a shifted Enter as a newline: check `matches!(key.modifiers, KeyModifiers::SHIFT)` alongside `KeyCode::Enter` and push `'\n'` instead of sending.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --target-dir target-tests --lib tui::views::chat` then `cargo test --target-dir target-tests 2>&1 | grep -E "^test result"`
Expected: PASS; the full suite still green.

- [ ] **Step 5: Commit**

```bash
git add src/tui/views/chat.rs src/tui/app.rs
git commit -m "feat(tui): rebuild the chat with no boxes and a one-line input"
```

---

### Task 5: Projects list

**Files:**
- Modify: `src/tui/views/projects.rs` (`render_projects` only)

**Interfaces:**
- Consumes: `theme`, `separator`, `body: Rect`.
- Produces: `render_projects(f, body: Rect, state, project_index, agent_index, active_in_project_list)` — no boxes; per-project rows show name, agent count, and an `● analizado` marker; the action hints row is rendered by the chrome footer, not inside this function.

- [ ] **Step 1: Write the failing test**

Add a `#[cfg(test)] mod tests` block with the two helpers from **Test Helpers**, then:

```rust
#[test]
fn projects_render_without_borders_and_show_counts() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    // Build state via the existing test helpers in tests/api_integration.rs
    // (AgentRegistry + ContextStore), save two contexts, then:
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| {
        render_projects(f, Rect::new(0, 1, 80, 22), &state, 0, 0, true);
    }).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let text = all_text(&buffer);
    for ch in ['┌', '┐', '└', '┘', '│'] {
        assert!(!text.contains(ch), "projects must not draw box char {ch:?}");
    }
    assert!(text.contains("agente"), "the agent count must be shown: {text}");
}
```

Add the empty state (Review Focus 5):

```rust
#[test]
fn projects_renders_an_empty_state_instead_of_a_bare_divider() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let empty = /* ContextStore with no projects */;
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| render_projects(f, Rect::new(0, 1, 80, 22), &empty_state, 0, 0, true)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let text = all_text(&buffer);
    assert!(text.contains("No projects"), "an empty list must say so: {text}");
    assert!(!text.contains('─'), "no separator should point at nothing: {text}");
}
```

And the alignment guard (Review Focus 3). This is the test that fails if the padding
uses `chars().count()` instead of display width — `clinica` has 8 cells, `日本語` has 6
cells but 3 chars. `str::find` answers in *bytes*, and a CJK glyph is three of them, so
the offset is converted to a character index and read back as a terminal column; a byte
offset would travel with the glyph and report the column that glyph is in, which is
the opposite of the question being asked. The scan stops at the separator, or the
agent list's own ` agentes de fudi` label answers the needle instead:

```rust
#[test]
fn the_agent_count_column_aligns_with_multibyte_names() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    // Three contexts named: "fudi", "clinica", "日本語プロジェクト"
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| render_projects(f, Rect::new(0, 1, 80, 22), &state, 0, 0, true)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    // `column_of(buffer, y, needle)` — the terminal column `needle` starts on
    // row `y`, in display columns rather than byte offsets.
    let projects_end = (0..buffer.area.height)
        .find(|y| row_text(&buffer, *y).contains('─'))
        .unwrap();
    let columns: Vec<usize> = (0..projects_end)
        .filter_map(|y| column_of(&buffer, y, "agente"))
        .collect();
    assert_eq!(columns.len(), 3, "each project row must show a count: {columns:?}");
    assert!(
        columns.windows(2).all(|w| w[0] == w[1]),
        "the count column must align across multibyte names: {columns:?}"
    );
}
```

`column_of` belongs in the shared test helpers (`src/tui/views/test_helpers.rs`), not
copied per module: it is the only place the byte-offset-to-column conversion lives, and
two copies of it would be two places to get wrong.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --target-dir target-tests --lib tui::views::projects`
Expected: FAIL — box characters are present, the empty state says nothing, and if the
current implementation pads with `chars().count()` the alignment test fails too.

- [ ] **Step 3: Implement the project list**

Replace the block-based body with `List` lines: the selected project in `active()`, others in `content()`; `"{n} agentes"` right-aligned by padding computed with `Line::width()`, not `chars().count()` (Review Focus 3); `●` in `ok()` for analysed projects. Draw a `separator` under the list. Emit an empty-state line when there are no projects.

Do not move `render_agent_form`, `render_analysis`, `render_context`, `render_confirm_delete`, or `render_skill_proposals` in this task.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --target-dir target-tests --lib tui::views::projects`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/tui/views/projects.rs
git commit -m "feat(tui): rebuild the projects list without boxes"
```

---

### Task 6: Agent form, analysis, and context views

**Files:**
- Create: `src/tui/views/agent_form.rs`, `src/tui/views/analysis.rs`, `src/tui/views/context.rs`
- Modify: `src/tui/views/projects.rs` (remove the three moved functions), `src/tui/views/mod.rs`, `src/tui/app.rs` (call sites)

**Interfaces:**
- Consumes: `theme`, `body: Rect`.
- Produces: `agent_form::render_agent_form(f, body: Rect, ...)` with the same field parameters it has today, `analysis::render_analysis(f, body: Rect, state: &AnalysisState)`, `context::render_context(f, body: Rect, state, project_index, scroll)`. The three are re-exports from `projects.rs` so the split is mechanical: move the function verbatim, add `body: Rect`, delete it from `projects.rs`, then point `app.rs` at the new modules.

- [ ] **Step 1: Write the failing test**

```rust
// In a `#[cfg(test)] mod tests` block in analysis.rs, with `all_text` from Test Helpers.
#[test]
fn analysis_proposals_render_without_borders() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let state = AnalysisState::Proposals {
        project_path: "/tmp/p".into(),
        proposals: vec![SkillProposal {
            id: "demo".into(), name: "Demo".into(),
            description: "d".into(), tags: vec![], content: "body".into(),
        }],
        selected: 0,
        results: vec![],
    };
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| analysis::render_analysis(f, Rect::new(0, 1, 80, 22), &state)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let text = all_text(&buffer);
    for ch in ['┌', '┐', '└', '┘'] {
        assert!(!text.contains(ch), "analysis must not draw box char {ch:?}");
    }
    assert!(text.contains("demo"), "the proposal must be shown");
}
```

Separator rules are `─`, not `│`: `│` is the context bar's own joiner between view names and belongs to no view.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --target-dir target-tests --lib tui::views::analysis`
Expected: FAIL — module `analysis` does not exist.

- [ ] **Step 3: Split and restyle**

Create the three modules by moving the functions out of `projects.rs` verbatim, adding `body: Rect` as the second parameter, and changing `.split(f.area())` to `.split(body)`. Then strip their `Borders::ALL` blocks: the agent form's fields become plain label/value lines with the active field in `action()`; the analysis view keeps the existing `Loading` spinner and the proposals approval layout but loses its outer block; the context view keeps its scroll behaviour without a block.

Update `app.rs` call sites to the new module paths, and `views/mod.rs` to declare the three modules.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --target-dir target-tests --lib tui::views` then the full suite.
Expected: PASS; full suite green.

- [ ] **Step 5: Commit**

```bash
git add src/tui/views/
git commit -m "refactor(tui): split agent form, analysis and context out of projects.rs"
```

---

### Task 7: Dashboard

**Files:**
- Modify: `src/tui/views/dashboard.rs`

**Interfaces:**
- Consumes: `theme`, `separator`, `body: Rect`.
- Produces: `render_dashboard(f, body: Rect, state, log_scroll)` — no title banner, no block; status as one dense row, then the log list under a separator.

- [ ] **Step 1: Write the failing test**

```rust
// In a `#[cfg(test)] mod tests` block in dashboard.rs, with `all_text` from Test Helpers.
#[test]
fn dashboard_drops_the_title_banner_and_the_boxes() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| render_dashboard(f, Rect::new(0, 1, 80, 22), &state, 0)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let text = all_text(&buffer);
    for ch in ['┌', '┐', '└', '┘', '│'] {
        assert!(!text.contains(ch), "dashboard must not draw box char {ch:?}");
    }
    assert!(!text.contains("High-Performance"), "the title banner must be gone: {text}");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --target-dir target-tests --lib tui::views::dashboard`
Expected: FAIL — box characters and the banner are present.

- [ ] **Step 3: Implement the dense dashboard**

Remove the 3-row title block entirely; the shared bar carries identity. Replace the two bordered panels with: one status line (`● api`, `● grpc`, agent and project counts, request totals), a `separator`, then the log list as plain `Line`s using the existing colour mapping (`ok` for INFO, `warn` for WARN, `error` for ERROR, `chrome` otherwise). Keep the scroll clamp exactly as it is today.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --target-dir target-tests --lib tui::views::dashboard` then the full suite.
Expected: PASS; full suite green.

- [ ] **Step 5: Commit**

```bash
git add src/tui/views/dashboard.rs
git commit -m "feat(tui): make the dashboard a dense status view"
```

---

### Task 8: Remove the old chrome and lock the style in

**Files:**
- Modify: `src/tui/app.rs` (delete `render_tab_bar`)
- Modify: all `src/tui/views/*.rs` (delete leftover per-view footers and titles)
- Create: `tests/tui_chrome.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–7.
- Produces: no `Borders::ALL` remains outside `render_confirm_delete` and `render_skill_proposals`; a regression test that fails if boxes reappear.

- [ ] **Step 1: Write the failing test**

Create `tests/tui_chrome.rs` as the style guard the spec calls for:

```rust
//! Guards the "no boxes" rule. Fails if box-drawing characters reappear
//! anywhere outside the two modal surfaces.
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

fn all_text(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_no_corners(text: &str, context: &str) {
    for ch in ['┌', '┐', '└', '┘'] {
        assert!(!text.contains(ch), "{context} drew box char {ch:?}");
    }
}

#[test]
fn context_bar_never_draws_a_border() {
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| {
        llama_r::tui::chrome::render_bar(
            f, f.area(), &["Dashboard", "Projects", "Chat"], 0,
            &llama_r::tui::chrome::Context { project: Some("fudi".into()), agent: Some("ops".into()) },
            true,
        );
    }).unwrap();
    let buffer = terminal.backend().buffer().clone();
    assert_no_corners(&all_text(&buffer), "context bar");
}
```

`render_bar`, `Context`, `chrome` and the views must be `pub` and reachable from the crate root for this integration test to compile.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --target-dir target-tests --test tui_chrome`
Expected: FAIL — `chrome` is private, so the integration test cannot see it.

- [ ] **Step 3: Expose the modules and delete the old chrome**

Make `pub mod chrome;` and `pub mod theme;` re-exported so `llama_r::tui::chrome::…` resolves. Delete `render_tab_bar` and its call site from `src/tui/app.rs`. Delete every per-view footer `Paragraph` and title block still present after Tasks 4–7 — key hints now come only from `hints_for`.

Then run `grep -rn "Borders::ALL" src/tui/` and confirm the only remaining hits are inside `render_confirm_delete` and `render_skill_proposals`. Each remaining hit is permitted; any other is a missed cleanup.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --target-dir target-tests --test tui_chrome` then
`cargo test --target-dir target-tests 2>&1 | grep -E "^test result"` and
`cargo fmt --check && cargo clippy --all-targets 2>&1 | grep -c warning`
Expected: PASS; full suite green; zero warnings.

- [ ] **Step 5: Commit**

```bash
git add src/tui/ tests/tui_chrome.rs
git commit -m "refactor(tui): drop the old chrome and guard the no-boxes style"
```

---

### Task 9: Document the new TUI

**Files:**
- Modify: `AGENTS.md`

**Interfaces:**
- Consumes: the finished module layout.

- [ ] **Step 1: Update the contributor docs**

Replace the TUI description in `AGENTS.md` with: the `theme.rs` / `chrome.rs` split and their responsibilities; the rule that views receive `body: Rect` and must not call `f.area()`; the rule that key hints live only in `TuiApp::hints_for`; and the "no boxes outside the two modals" rule with `tests/tui_chrome.rs` named as its guard. Remove the claim that the tab bar lists the views, since it is now the context bar.

- [ ] **Step 2: Verify the documented commands work**

Run: `cargo fmt --check && cargo test --target-dir target-tests 2>&1 | grep -E "^test result"`
Expected: clean; full suite green.

- [ ] **Step 3: Commit**

```bash
git add AGENTS.md
git commit -m "docs(tui): document the shared chrome and the no-boxes rule"
```
