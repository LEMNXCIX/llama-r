//! The style guard for the redesigned TUI.
//!
//! Two rules that no per-view buffer test can catch, because neither is visible
//! in one view's output:
//!
//! 1. **No boxes.** Borders belong to the two modal surfaces and nowhere else.
//!    A view that wraps itself in `Borders::ALL` renders corners, which its own
//!    test would have to be written to expect; this checks the *sources* as
//!    well as the chrome's own output, so the rule holds for every view.
//! 2. **Hints live in one place.** `TuiApp::hints_for` is the only place a key
//!    and what it does may be written down. A view that hard-codes `"Press 'a'
//!    to analyze"` draws no box, so it passes every border test in the crate
//!    while the shared footer says something else — which is exactly how the
//!    skill-proposals modal lost the only mention of its approve keys.
//!
//! The source checks are heuristics over the code the interface is made of, not
//! a parser: they look for the two spellings this codebase has used for a
//! hard-coded hint (`Press 'a'` and `[Enter] approve`) and for any request to
//! ratatui for a border. A new spelling slips through, which is why the doc
//! comment on each check says what it covers.

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

/// Every row of `buffer`, joined by newlines.
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

/// The view modules. Every one of them draws inside `body` and takes its
/// styling from `theme`; none of them may frame anything.
const VIEWS: [(&str, &str); 6] = [
    ("agent_form", include_str!("../src/tui/views/agent_form.rs")),
    ("analysis", include_str!("../src/tui/views/analysis.rs")),
    ("chat", include_str!("../src/tui/views/chat.rs")),
    ("context", include_str!("../src/tui/views/context.rs")),
    ("dashboard", include_str!("../src/tui/views/dashboard.rs")),
    ("projects", include_str!("../src/tui/views/projects.rs")),
];

/// The two modal surfaces. Borders are permitted here and nowhere else, and the
/// design keeps them: a dialog is the one thing on this screen that has to read
/// as separate from the page behind it.
const MODALS: &str = include_str!("../src/tui/views/modals.rs");

/// The dispatch that owns the chrome.
const APP: &str = include_str!("../src/tui/app.rs");

/// The shipped code of a module: everything before its test module.
///
/// A test may *name* a hint it expects not to be drawn; that is the test doing
/// its job, not the interface hard-coding one.
fn shipped(source: &str) -> &str {
    source.split("#[cfg(test)]").next().unwrap_or(source)
}

/// Any spelling of "ask ratatui for a border" this codebase has used.
fn border_ask(source: &str) -> Option<&'static str> {
    for ask in ["Borders::", ".borders(", ".bordered(", ".border_type("] {
        if source.contains(ask) {
            return Some(ask);
        }
    }
    None
}

/// Whether `line` names a key and what it does.
///
/// Two shapes, both of which this codebase has shipped in a view:
/// `Press 'a' to analyze` and `[Enter] approve  [a] approve all`.
fn names_a_key(line: &str) -> bool {
    let trimmed = line.trim_start();
    // A doc comment may write a log line or a markdown link that looks like
    // either shape; only shipped code is under test.
    if trimmed.starts_with("//") {
        return false;
    }

    let lower = line.to_lowercase();
    if lower.contains("press '") || lower.contains("press \"") {
        return true;
    }

    // `[key]` counts only when a hint follows it, so indexing (`chunks[2]`,
    // `values[index]`) and array types (`[&str; 7]`) are not mistaken for one.
    let chars: Vec<char> = line.chars().collect();
    chars.iter().enumerate().any(|(start, ch)| {
        if *ch != '[' {
            return false;
        }
        let inner: String = chars
            .iter()
            .skip(start + 1)
            .take_while(|c| **c != ']')
            .take(8)
            .collect();
        !inner.is_empty()
            && inner.chars().all(|c| !c.is_whitespace())
            && inner.chars().any(|c| c.is_alphanumeric())
            && chars
                .get(start + 2 + inner.chars().count())
                .is_some_and(|after| after.is_whitespace())
            && chars
                .get(start + 4 + inner.chars().count())
                .is_some_and(|hint| *hint != ' ')
    })
}

/// The key hint the footer shows for a screen may only be written in
/// `hints_for`. A view that repeats one has two places to update and one of
/// them will be stale.
#[test]
fn no_view_hard_codes_a_key_hint() {
    for (name, source) in VIEWS {
        for (number, line) in shipped(source).lines().enumerate() {
            assert!(
                !names_a_key(line),
                "src/tui/views/{name}.rs:{} names a key outside `hints_for`: {line:?}",
                number + 1
            );
        }
    }
}

/// Borders are for the two modals. Everything else frames content with type and
/// rules, so a `Block` with borders anywhere else is a step back to the clutter
/// this interface was redesigned to remove.
#[test]
fn only_the_modals_draw_borders() {
    for (name, source) in VIEWS {
        assert_eq!(
            border_ask(shipped(source)),
            None,
            "src/tui/views/{name}.rs asks for a border; the chrome's separators \
             and `theme` carry structure instead"
        );
    }
    assert_eq!(
        border_ask(shipped(APP)),
        None,
        "src/tui/app.rs asks for a border; the bar is `chrome::render_bar`"
    );
    assert!(
        MODALS.contains("Borders::ALL"),
        "the modals are where borders belong — if this fails, the exemption has \
         been left with nothing to exempt"
    );
}

/// The regression guard for the bug the redesign started from: the tab bar was
/// drawn into a one-row rect with `Borders::ALL`, so the border ate the only row
/// it had and the bar silently vanished. `│` is the bar's own punctuation, the
/// one the spec's mockup shows, so the check is for the characters a border
/// draws.
#[test]
fn the_context_bar_never_draws_a_border() {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| {
            llama_r::tui::chrome::render_bar(
                f,
                f.area(),
                &["Dashboard", "Projects", "Chat"],
                0,
                &llama_r::tui::chrome::Context {
                    project: Some("fudi".into()),
                    agent: Some("ops".into()),
                },
                true,
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    for ch in ['┌', '┐', '└', '┘'] {
        assert!(
            !all_text(&buffer).contains(ch),
            "the context bar drew box char {ch:?}"
        );
    }
}