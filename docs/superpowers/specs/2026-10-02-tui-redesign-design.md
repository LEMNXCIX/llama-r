# TUI redesign — design

**Date:** 2026-10-02
**Status:** draft for review
**Scope:** visual redesign of the Llama-R terminal UI. No behaviour changes to the API, runtime, or agent semantics.

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
  chrome.rs       context bar, key-hint footer, separators
  app.rs          state, input handling, render dispatch
  views/
    chat.rs       dashboard.rs      projects.rs
    agent_form.rs analysis.rs       context.rs
```

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

Three public pieces, consumed by every view:

1. **`context_bar(current, context, api_running)`** — one row, no border:
   ```
   Llama-R │ Dashboard │ Projects │ Chat             fudi/ops  ● q
   ```
   The product's name leads, then the view names, then the context (project + agent) right-aligned and always visible. Replaces the broken tab bar and every per-view title block.

   The name is the one thing the rest of the row cannot say: the view names and the context say *where you are*, not *what this is*. When the dashboard's banner went (below), this became the only place the product is named — which is why it belongs here rather than in a view.

   The dot is the HTTP **listener's** liveness (`api_running`), not the provider's. Nothing in the TUI reports whether the provider answers; the dashboard says so where a reader would otherwise assume the bar does.

2. **`key_hints(&[(&str, &str)])`** — the footer. Each view declares its own shortcuts; the chrome renders them. Ends the duplicated hint text.

3. **`separator(width)`** — a thin `─` rule.

## Visual language

Hard rules, so the style does not erode:

- **No boxes.** Borders only where grouping earns its keep: the chat input area and modal dialogs.
- **No decorative titles.** The context bar carries identity; view bodies start straight into content.
- **Typography is the hierarchy.** Bold for labels and keys, `chrome` for furniture, `content` for text.
- **One-row bar, actually one row.** No border.
- **No magic constraints.** Bodies take `Min(0)`; the chrome owns the fixed heights.

## Per-view layout

### Chat

```
Llama-R │ Dashboard │ Projects │ Chat             fudi/ops  ● q

  You  ¿plazo de reembolso?

  AI   24 horas, según la política de cancelación.

  ⠋ Thinking

─────────────────────────────────────────────────────────
> _
 Enter enviar  Shift+Enter nueva línea  ↑↓ scroll  ←→ agente
```

`Enter` sends; `Shift+Enter` inserts a newline. The prompt marker replaces the 3-row bordered input box.

### Projects

```
  fudi              3 agentes   ● analizado
  clinica           1 agente
─────────────────────────────────────────────────────────
 agentes de fudi
  nutricion         llama3
  pediatra          llama3
─────────────────────────────────────────────────────────
 Tab vista  ←→ agentes  ↑↓ seleccionar  a analizar  n nuevo  d borrar  q salir
```

Two lists of plain rows separated by a thin rule — no box, no title on either.

The upper list is the projects, one row each: name, agent count, whether it has been analysed. The counts stay inline rather than only inside a sub-view.

**The selected project's agents are listed underneath, and stay visible.** Four keys act on them — `←/→` moves focus between the two lists, `↑/↓` moves within whichever has it, `e` edits the selected agent and `d` deletes it — so a project row alone would leave all four driving state the user cannot see. Retargeting them at the project list is not available either: a project row carries a *count*, not an agent, so `e` would have to pick one invisibly. The agent list is what makes every one of those keys act on a row that is on screen.

The mockup this replaces advertised `Enter ver agentes`, a key that was never bound to anything. There is no sub-view: the agents are on this screen.

Both lists are windows on their own selection rather than the head of the list, and each keeps at least one row whenever the other has something to show. So on a body too short for both, the selected row of each is still drawn and every row stays reachable. Actions permanently visible instead of hidden.

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

- **No stray borders.** Rendering a view produces no box-drawing characters outside the input and dialogs. This is the regression guard for "no boxes".
- **The bar renders.** Its row contains the active view name and the current context. Guards the bug being fixed.
- **Context survives navigation.** The context bar shows the same project/agent before and after a view switch.
- **Key hints match the view.** The footer row contains exactly the shortcuts that view declares.
- **Chat layout.** Message area grows when the terminal grows; `Shift+Enter` adds a line instead of sending.

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