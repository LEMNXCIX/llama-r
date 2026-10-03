# TUI redesign — design

**Date:** 2026-10-02
**Status:** implemented, and amended in place to describe what shipped.
**Scope:** visual redesign of the Llama-R terminal UI. No behaviour changes to the API, runtime, or agent semantics.

> **Cite this document by section, not by line.** It was amended twice after it
> was written — once when the implementation diverged from it, once when a fix
> wave corrected its own claims — and in the meantime every line-number citation
> into it rotted: three of them, in one sentence about the project rows. A
> section heading survives an edit; a line number does not.

## Problem

The TUI looks cluttered. Every view is its own hand-rolled vertical stack of bordered blocks with hard-coded constraints (`Length(3)`, `Length(7)`), its own title block, and its own footer that repeats the key hints the tab bar already shows. `projects.rs` is 651 lines mostly because four unrelated concerns share one file.

Two concrete defects fall out of this structure:

- **The tab bar does not render.** It is drawn into a `height: 1` rect with `Borders::ALL`, so the border consumes the only row. Users see an empty bar. No one noticed because no single view owns it.
- **Context is lost between views.** Which project and agent are active is only knowable from inside each view, so changing view means reconstructing state mentally.

## Decisions

| Decision | Choice | Why |
|---|---|---|
| Visual direction | **C — dZen minimal** | Chosen from three mockups. One-line bar, thin separators, columns without labels, typography carries hierarchy. |
| Chrome | **Shared component module** | Six views repeating chrome is why the bar broke unnoticed. One fix point instead of six. |
| Where used | Chat and Projects, equally | Determines effort split; neither view may be neglected. |
| Chat input | **1 line**, `Shift+Enter` for newline | Reclaims 2 rows of message area. Multi-line stays available via an explicit key rather than a mode. |

## Architecture

```
src/tui/
  theme.rs        colour tokens and semantic styles
  chrome.rs       layout, context bar, key-hint footer, separators
  app.rs          state, input handling, render dispatch, key hints
  views/
    chat.rs       dashboard.rs      projects.rs
    agent_form.rs analysis.rs       context.rs
    modals.rs     test_helpers.rs
```

`modals.rs` is where the two bordered dialogs live — the only boxes in the
interface, and the reason the border guard can name one file and mean it.
`test_helpers.rs` holds the fixtures the view tests share; it is not a view and
renders nothing.

`projects.rs` is split: the project list, the agent form, the analysis view, and the modal dialogs become separate files. `AgentForm`, `Analysis`, and `ContextView` are all rendered from `projects.rs` today despite being distinct `CurrentView` variants, so they become `agent_form.rs`, `analysis.rs`, and `context.rs` respectively. This is a consequence of the redesign, not a separate refactor — the shared chrome only makes sense with per-view bodies.

### `theme.rs`

Colour and style tokens with fixed meaning, so views stop choosing colours ad hoc:

| Token | Meaning |
|---|---|
| `ok` | Green — healthy, succeeded |
| `warn` | Amber — degraded, skipped |
| `error` | Red — failure |
| `chrome` | `DarkGray` — bars, hints, separators |
| `action` | Cyan — the selected item, interactive affordance |
| `active` | Yellow + bold — current view, selected row |
| `content` | White — body text |

### `chrome.rs`

Five functions, consumed by every view, over two types (`Chrome`, `Context`) and
two row constants (`BAR_HEIGHT`, `FOOTER_HEIGHT`, both `1`):

1. **`layout(area) -> Chrome { bar, body, footer }`** — the split the dispatch
   hands every view as `body`. Every subtraction saturates, so a terminal too
   small for the two fixed rows gets an empty body rather than a wrapped-around
   one. This is the piece the redesign turns on: a view that lays out
   `f.area()` includes the bar's row in its own layout and writes over it.

2. **`render_bar(f, rect, views, active, ctx, api_running)`** — one row, no border:
   ```
   Llama-R │ Dashboard │ Projects │ Agent │ Analysis │ Context │ Chat   fudi/ops  ●
   ```
   Drawn to scale at 80 columns, which is where the mockup above sits: the fixed
   part is 66 columns and the group needs three more for its gap and dot, so the
   dot needs 69 before the label has a column of its own. The product's name leads,
   then the view names, then the context (project + agent) right-aligned. It
   replaces the broken tab bar and every per-view title block.

   The name is the one thing the rest of the row cannot say: the view names and the context say *where you are*, not *what this is*. When the dashboard's banner went (below), this became the only place the product is named — which is why it belongs here rather than in a view.

   The dot is the HTTP **listener's** liveness (`api_running`), not the provider's. No view states whether the provider answers; the dashboard's own doc says so where a reader would otherwise assume the bar does.

   Only the context *label* is fitted, and only it is truncated — from the right, ending in `…`. The rest of the row is clipped, not chosen: the line is assembled left to right and cut at the row's width, so past the cut the gap goes first, then the dot, then the last view names. The product's name is the last thing that cut reaches, not a thing the cut never reaches.

3. **`render_footer(f, rect, hints)`** — the footer. Each screen declares its own shortcuts and the chrome renders them; the text lives in one function, `TuiApp::hints_for`, which is keyed on a `KeyTarget` rather than on the view. Ends the duplicated hint text.

4. **`separator(f, rect)`** — a thin `─` rule across `rect.width`.

5. **`context_label(ctx) -> String`** — how the scope reads: `fudi/ops`, `fudi`, `ops`, or `direct`.

## Visual language

Hard rules, so the style does not erode:

- **No boxes.** Borders only where grouping earns its keep: the modal dialogs, and
  nowhere else. (An earlier revision of this line also permitted the chat input
  area. The prompt marker that replaced the 3-row input box below leaves nothing
  to frame, so the permission was dropped rather than kept as an allowance.)
- **No decorative titles.** The context bar carries identity; view bodies start straight into content.
- **Typography is the hierarchy.** Bold for labels and keys, `chrome` for furniture, `content` for text.
- **One-row bar, actually one row.** No border.
- **No magic constraints.** Bodies take `Min(0)`; the chrome owns the fixed heights.

## Per-view layout

### Chat

```
Llama-R │ Dashboard │ Projects │ Agent │ Analysis │ Context │ Chat   fudi/ops  ●

  You  ¿plazo de reembolso?

  AI   24 horas, según la política de cancelación.

  ⠋ Thinking

─────────────────────────────────────────────────────────
> _
```

`Enter` sends; `Shift+Enter` inserts a newline. The prompt marker replaces the 3-row bordered input box. The hint row is not drawn here: the key hints are the chrome footer's, on the last row of the screen, in one row shared by every view. Six fit at 80 columns; a seventh would be clipped from the right, which takes `Esc: back` first, so `PgUp`/`PgDn` still scroll the chat without being advertised.

### Projects

```
  fudi              3 agentes   ● analizado   ✗
  clinica           1 agente
─────────────────────────────────────────────────────────
 agentes de fudi
  nutricion         llama3
  pediatra          llama3
```

The hint row is likewise the shared footer's, and here it changes with the focus:
`←→` names the list the next `←→` moves *onto*, so the row reads `agentes` while
the project list has focus and `projects` while the agent list has it.

Two lists of plain rows separated by a thin rule — no box, no title on either. One rule, between the two lists; nothing closes the agent list, exactly as nothing closes the chat input above its rule.

The upper list is the projects, one row each: name, agent count, whether it has been analysed. The counts stay inline rather than only inside a sub-view.

**The selected project's agents are listed underneath, and stay visible.** Four keys act on them — `←/→` moves focus between the two lists, `↑/↓` moves within whichever has it, `e` edits the selected agent and `d` deletes it — so a project row alone would leave all four driving state the user cannot see. Retargeting them at the project list is not available either: a project row carries a *count*, not an agent, so `e` would have to pick one invisibly. The agent list is what makes every one of those keys act on a row that is on screen.

The mockup this replaces advertised `Enter ver agentes`, a key that was never bound to anything. There is no sub-view: the agents are on this screen.

Both lists are windows on their own selection rather than the head of the list, and from a four-row body up each keeps at least one row — the agent list never at the project list's expense. So on a body too short to show either list whole, the selected row of *each* is still drawn and every row stays reachable. Below four rows the agent list has none: the rule and the label take two, and one row is left for a project. Its four keys then do nothing and the footer says so, rather than naming keys that would act on a row nobody can see. Actions permanently visible instead of hidden.

### Dashboard

The title block is removed. Status becomes one dense row; the log list follows with a single separator. The repeated "Llama-R / High-Performance AI Gateway" banner disappears — the context bar leads with the product's name, which is the one thing the rest of that row cannot say.

### Agent form, Analysis, Context

Same rules, no bespoke treatment. The analysis view keeps the existing `Loading` spinner and `Proposals` approval flow; only the chrome around them changes.

## Defects fixed in passing

- Tab bar not rendering (1-row rect with `Borders::ALL`).
- Key hints duplicated in every view footer.
- Dashboard title banner consuming 3 rows on every launch.
- `projects.rs` mixing four concerns in 651 lines.

## Testing

The TUI is testable without a terminal by rendering into `ratatui::backend::TestBackend` and asserting on the buffer — the technique already used to verify the thinking spinner.

Assertions that encode the design, not just "it compiles":

- **No stray borders.** Every screen is rendered into the chrome's body and the whole buffer must be free of `┌┐└┘` and, over the body alone, of `│`; the bar row must survive. The sources are checked as well, so the rule holds for a view added tomorrow: no module under `src/tui/views/` may ask ratatui for a border but `modals`, and `modals` must still use it, so the one exemption cannot go vacuous. `tests/tui_chrome.rs`.
- **The bar renders.** Its row leads with the product's name, carries the active view name and the current context, and survives a view drawn after it. `src/tui/chrome.rs`.
- **The footer names what the screen answers to**, resolved per screen *state* rather than per view: the agent list inside the projects view, the skill-proposal modal, the delete confirmation and the agent form's caret each get their own row, and a row leaves when its state does. No view may hard-code hint text, and the TUI contains exactly one footer. `src/tui/app.rs` and `tests/tui_chrome.rs`.
- **Nothing drives a row that was not drawn.** The two lists in the projects view report the rows they got, and the four keys that act on the agent list consult that report — so a footer cannot advertise a list with nothing in it. `src/tui/views/projects.rs` and `src/tui/app.rs`.

Specified here and **not** implemented, recorded so the gap reads as a decision
rather than an oversight:

- *Context survives navigation* — the bar shows the same project/agent before and after a view switch. No test exercises a view switch, because `Tab` is handled in an event loop that no test drives.
- *Message area grows when the terminal grows* — the chat's message region is sized by its own `Min(0)`, so it does, but nothing asserts it. The `Shift+Enter` half of that line is covered.

## Scope

One feature, not a set of independent pieces: every view conversion is the same change applied once. The implementation sequence is therefore fixed:

1. `theme.rs` and `chrome.rs`, with their tests.
2. Convert each view to the shared chrome, one at a time, each with its own buffer assertions. Splitting `projects.rs` happens as part of converting the views it currently hosts.
3. Remove the old per-view chrome and the broken tab bar once no view references them.

Each step leaves the TUI working, so the redesign can be stopped after any view.

## Out of scope

- Keyboard-navigation rework beyond what the redesign touches (`Tab` / `←→` / `Esc` stay as they are).
- Colour themes or configurable palettes.
- Any change to the HTTP API, runtime wiring, or agent/RAG behaviour.
- Mouse support.