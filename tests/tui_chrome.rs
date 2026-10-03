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
//!
//! Rule 2 has a second detector, because the prose heuristic misses the shape it
//! was written for: the idiomatic way to draw a hint is a `chrome::render_footer`
//! call with the hints as data, and `&[("a", "analyze")]` matches neither
//! spelling. So `only_the_dispatch_draws_the_footer` asserts the property the
//! heuristic cannot reach — there is exactly one footer in the TUI.

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::Frame;
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

/// Every module under `src/tui/views/`, read from disk at test time.
///
/// Deliberately not a list: a hand-maintained one goes stale the moment a module
/// is added, and the module that goes unguarded is then the new one — which is
/// exactly what happened when the modals moved into `views/`. Walking the
/// directory means a file cannot be added without also being guarded, and
/// `include_str!` cannot silently fall back to nothing.
///
/// No exemption here. [`MODAL_MODULE`] is applied by the one check that needs it
/// — the border one — and left in the hint scan, which the delete confirmation
/// used to be excluded from and no longer needs to be: its answers moved to
/// `hints_for` with `KeyTarget::DeleteConfirm`.
fn view_modules() -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui/views");
    let mut found: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.expect("a readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .map(|path| {
            let name = path
                .file_stem()
                .expect("a .rs file has a stem")
                .to_string_lossy()
                .into_owned();
            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            (name, source)
        })
        .collect();
    found.sort();
    assert!(
        !found.is_empty(),
        "no view modules found under {} — the walk found nothing, so every check below \
         would pass for the wrong reason",
        dir.display()
    );
    found
}

/// The one module under `src/tui/views/` permitted to draw a border.
///
/// A dialog is the one thing on this screen that has to read as separate from the
/// page behind it. That is the whole of the exemption and it is about borders:
/// `only_the_modals_draw_borders` is the only check that skips this module, so
/// the rule stays narrow enough that a box here cannot become invisible — a
/// *second* check skipping it would quietly widen it to whatever that check
/// looks for, and the guard would stop being a statement about boxes.
const MODAL_MODULE: &str = "modals";

/// The dispatch that owns the chrome and `hints_for`.
const APP: &str = include_str!("../src/tui/app.rs");

/// The module that draws the bar.
const CHROME: &str = include_str!("../src/tui/chrome.rs");

/// `app.rs` with `hints_for`'s body removed — the one place hint text is allowed
/// to live, and the reason `app.rs` is otherwise in scope for the hint check.
///
/// Brace-balanced from the `fn` keyword, which is exact for a function whose
/// strings contain no braces (this one does not) and keeps the doc comment above
/// it, which may legitimately describe the hints.
fn app_without_hint_text() -> String {
    let Some(start) = APP.find("fn hints_for(") else {
        panic!("app.rs no longer has hints_for — rename it and this exclusion with it");
    };
    let mut depth = 0usize;
    let mut end = APP.len();
    for (offset, ch) in APP[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    end = start + offset + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    format!("{}{}", &APP[..start], &APP[end..])
}

/// The shipped code of a module: everything before its test module.
///
/// A test may *name* a hint it expects not to be drawn; that is the test doing
/// its job, not the interface hard-coding one.
fn shipped(source: &str) -> &str {
    source.split("#[cfg(test)]").next().unwrap_or(source)
}

/// Any spelling of "ask ratatui for a border" this codebase has used.
fn border_ask(source: &str) -> Option<&'static str> {
    const ASKS: [&str; 4] = ["Borders::", ".borders(", ".bordered(", ".border_type("];
    ASKS.into_iter().find(|ask| source.contains(ask))
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

    bracket_hint(line)
}

/// `[key] what it does`, the second spelling this codebase has used.
///
/// Deliberately narrow: a hint is a bracketed token of at most twelve characters
/// followed by words. That keeps indexing (`chunks[2]`, `values[index]`), array
/// types (`[&str; 7]`) and empty brackets (`vec![]`) out of it, so the check
/// fires on a hint and not on Rust's punctuation.
fn bracket_hint(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    chars.iter().enumerate().any(|(start, ch)| {
        if *ch != '[' {
            return false;
        }
        let close = match chars[start + 1..]
            .iter()
            .position(|c| *c == ']')
            .filter(|offset| *offset <= 12)
        {
            Some(offset) => start + 1 + offset,
            None => return false,
        };
        let key: String = chars[start + 1..close].iter().collect();
        !key.is_empty()
            && key.chars().all(|c| !c.is_whitespace())
            && key.chars().any(|c| c.is_alphanumeric())
            // A word follows: `[Esc] Cancel` is a hint, `chunks[2],` is not.
            && chars
                .get(close + 1)
                .is_some_and(|after| after.is_whitespace())
            && chars[close + 2..].iter().any(|c| !c.is_whitespace())
    })
}

/// The key hint the footer shows for a screen may only be written in
/// `hints_for`. A view that repeats one has two places to update and one of
/// them will be stale.
fn env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// One screen, as a closure over whatever fixture it needs.
type Screen<'a> = (&'a str, Box<dyn Fn(&mut Frame, Rect) + 'a>);

/// Every screen that can be reached — the six views plus the two states whose
/// keys belong to no view of their own — has to render, and render box-free,
/// inside the chrome's body. This is the whole style in one test: a view that
/// grew a block back, or one whose keys nothing reaches, both fail here.
#[test]
fn every_view_renders_inside_the_body_without_a_box() {
    use llama_r::tui::chrome::{self, Context};
    use llama_r::tui::views::{
        agent_form::render_agent_form, analysis::render_analysis, chat::render_chat,
        context::render_context, dashboard::render_dashboard, projects::render_projects,
    };
    // `LLAMA_R_DIR` is process-wide, so this screen walk holds a lock of its own.
    // `core::paths::lock_env_for_tests` is `#[cfg(test)]` and so is not visible from
    // an integration test — this is the same trick `api_integration.rs` uses.
    let _env = env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());
    let state = fixture_state(dir.path());

    // Each arm is one screen. `body` is the rect the chrome leaves over, which
    // is what a view is allowed to draw on — a view that reached for `f.area()`
    // instead would draw over the bar, and the bar is drawn first here, so the
    // bar surviving at the end is checked per screen below.
    let screens: Vec<Screen<'_>> = vec![
        (
            "dashboard",
            Box::new(|f: &mut Frame, body: Rect| render_dashboard(f, body, &state, 0)),
        ),
        (
            "projects",
            Box::new(|f: &mut Frame, body: Rect| {
                render_projects(f, body, &state, 0, 0, true);
            }),
        ),
        (
            "projects · agent list",
            Box::new(|f: &mut Frame, body: Rect| {
                render_projects(f, body, &state, 0, 0, false);
            }),
        ),
        (
            "agent form",
            Box::new(|f: &mut Frame, body: Rect| {
                render_agent_form(
                    f,
                    body,
                    "ops",
                    "Nutrición",
                    "llama3",
                    "fudi",
                    "habla español",
                    "sé breve",
                    "demo",
                    "eres un asistente",
                    0,
                )
            }),
        ),
        (
            "analysis · idle",
            Box::new(|f: &mut Frame, body: Rect| {
                render_analysis(f, body, &llama_r::tui::app::AnalysisState::Idle)
            }),
        ),
        (
            "analysis · loading",
            Box::new(|f: &mut Frame, body: Rect| {
                render_analysis(
                    f,
                    body,
                    &llama_r::tui::app::AnalysisState::Loading {
                        started_at: std::time::Instant::now(),
                    },
                )
            }),
        ),
        (
            "analysis · loaded",
            Box::new(|f: &mut Frame, body: Rect| {
                render_analysis(
                    f,
                    body,
                    &llama_r::tui::app::AnalysisState::Loaded(
                        "# Reglas\n- hablar en español\n- no inventar".to_string(),
                    ),
                )
            }),
        ),
        (
            "analysis · error",
            Box::new(|f: &mut Frame, body: Rect| {
                render_analysis(
                    f,
                    body,
                    &llama_r::tui::app::AnalysisState::Error("se rompió el análisis".to_string()),
                )
            }),
        ),
        (
            "context",
            Box::new(|f: &mut Frame, body: Rect| render_context(f, body, &state, 0, 0)),
        ),
        (
            "chat",
            Box::new(|f: &mut Frame, body: Rect| {
                render_chat(
                    f,
                    body,
                    &[("user".to_string(), "hola".to_string())],
                    "una pregunta",
                    false,
                    None,
                    &None,
                    &None,
                    &[],
                    0,
                    0,
                );
            }),
        ),
    ];

    for (label, render) in screens {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                // The real dispatch order: bar, then the view into the body.
                let c = chrome::layout(f.area());
                chrome::render_bar(
                    f,
                    c.bar,
                    &["Dashboard", "Projects", "Chat"],
                    0,
                    &Context::default(),
                    true,
                );
                render(f, c.body);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        // Corners across the whole screen: no view may frame anything, and the
        // bar is already pinned to be border-free by its own test.
        for ch in ['┌', '┐', '└', '┘'] {
            assert!(
                !all_text(&buffer).contains(ch),
                "the {label} screen drew box char {ch:?}"
            );
        }
        // The vertical checked over the body alone: `│` is the bar's own
        // punctuation between view names, which the spec's mockup shows, so a
        // whole-screen check would blame the view for the bar.
        let body = chrome::layout(Rect::new(0, 0, 80, 24)).body;
        let body_text: String = (body.y..body.y + body.height)
            .map(|y| {
                (body.x..body.x + body.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !body_text.contains('│'),
            "the {label} screen drew a vertical in its body: {body_text}"
        );
        // A screen that drew nothing passes every check above, so the bar is
        // asserted too: it is the one thing this test can know every screen
        // must contain, and its survival is what the shared layout exists for.
        let bar: String = (0..80)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            bar.contains("Llama-R"),
            "the bar must survive the {label} screen: {bar:?}"
        );
    }
}

/// An `AppState` over `dir`, with one project and two agents in it.
///
/// `build_app_state` is what the runtime calls; the pieces are the ones the TUI
/// actually reads — a context store, an agent registry and a log buffer.
fn fixture_state(dir: &std::path::Path) -> std::sync::Arc<llama_r::api::handlers::AppState> {
    use llama_r::adapters::mcp::StaticMcpRegistry;
    use llama_r::context::store::{ContextStore, ProjectContext};
    use llama_r::providers::ollama::OllamaProvider;
    use llama_r::runtime::build_app_state;
    use llama_r::services::agent_registry::AgentRegistry;
    use llama_r::services::skill_manager::SkillManager;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    let context_store = Arc::new(ContextStore::new());
    context_store
        .save_context(ProjectContext {
            project_id: "fudi".to_string(),
            path: dir.join("fudi").display().to_string(),
            context_md: "# Reglas\n- hablar en español".to_string(),
            project_type: "rust".to_string(),
            skills_injected: Vec::new(),
            last_analyzed: chrono::Utc::now(),
            custom_rules: String::new(),
        })
        .unwrap();

    let agent_registry = Arc::new(AgentRegistry::new());
    let agents_dir = dir
        .join("contextos")
        .join("projects")
        .join("fudi")
        .join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    for name in ["nutricion", "pediatra"] {
        std::fs::write(
            agents_dir.join(format!("{name}.toml")),
            "name = \"Agente\"\nmodel = \"llama3\"\nsystem_prompt = \"hola\"\n",
        )
        .unwrap();
    }
    agent_registry.reload_all(&[]).unwrap();

    build_app_state(
        Arc::new(OllamaProvider::new("http://localhost:11434".to_string())),
        agent_registry,
        Arc::new(SkillManager::new()),
        context_store,
        "llama3".to_string(),
        Arc::new(Mutex::new(VecDeque::new())),
        Vec::new(),
        Arc::new(StaticMcpRegistry::new()),
        None,
        None,
        None,
        None,
        None,
    )
}

/// Every `.rs` file under `src/tui`, with the path it was read from, trimmed to
/// its shipped code.
///
/// Recursive, and not the `views/` list plus `app.rs`, because the thing being
/// guarded here is *one call site in the whole TUI*: a second footer anywhere
/// under `src/tui` is the violation, wherever it lives.
fn tui_modules() -> Vec<(String, String)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui");
    let mut found = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        {
            let path = entry.expect("a readable directory entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !path.extension().is_some_and(|ext| ext == "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            let name = path
                .strip_prefix(&root)
                .expect("a file under the root")
                .display()
                .to_string();
            found.push((name, source));
        }
    }
    found.sort();
    assert!(
        !found.is_empty(),
        "no modules found under {} — the walk found nothing, so this check would \
         pass for the wrong reason",
        root.display()
    );
    found
}

/// Whether `line` *calls* `render_footer` rather than declaring it.
fn footer_call(line: &str) -> bool {
    if !line.contains("render_footer(") {
        return false;
    }
    let trimmed = line.trim_start();
    !(trimmed.starts_with("fn ")
        || trimmed.starts_with("pub fn ")
        || trimmed.starts_with("pub(crate) fn "))
}

/// One footer, drawn once, by the dispatch.
///
/// `names_a_key` cannot see a second one, and the bypass is the idiomatic
/// spelling: `chrome::render_footer(f, rect, &[("a", "analyze")])` matches
/// neither `press '` nor a `[key] word`, because the hints arrive as data. So a
/// view can draw its own footer beside the shared one, advertise a key its own
/// handler never answers, and pass every other check in this file — which is the
/// violation that produced Task 3's Important finding.
///
/// The count is asserted rather than the set, so the message names the second
/// site instead of only failing on a number.
#[test]
fn only_the_dispatch_draws_the_footer() {
    let mut calls: Vec<String> = Vec::new();
    for (name, source) in tui_modules() {
        for (number, line) in shipped(&source).lines().enumerate() {
            if footer_call(line) {
                calls.push(format!("src/tui/{name}:{}", number + 1));
            }
        }
    }
    assert_eq!(
        calls.len(),
        1,
        "the key hints are drawn once, by `TuiApp::run`, and every other call \
         site is a second footer naming keys nothing answers: {calls:?}"
    );
    assert!(
        calls[0].starts_with("src/tui/app.rs:"),
        "the shared footer belongs to the dispatch, which is the only place \
         `hints_for` is reachable from: {calls:?}"
    );
}

/// The footer check fires on the call it was written for and not on the
/// declaration. Without this, `only_the_dispatch_draws_the_footer` would pass
/// for a reason nobody chose.
#[test]
fn the_footer_check_spots_a_call_and_spares_the_declaration() {
    assert!(
        footer_call("                chrome::render_footer(f, c.footer, &hints);"),
        "the dispatch's own call must be counted"
    );
    assert!(
        footer_call("        f.render_widget(chrome::render_footer(f, r, &[(\"a\", \"x\")]), r);"),
        "so must the one-liner a view would write"
    );
    assert!(
        !footer_call("pub fn render_footer(f: &mut Frame, rect: Rect, hints: &[(&str, &str)]) {"),
        "the declaration is not a second footer"
    );
    assert!(
        !footer_call("    /// The row `render_footer` draws is one line, truncated."),
        "nor is naming it in a doc comment"
    );
    assert!(!footer_call(
        "    chrome::render_bar(f, c.bar, &names, 0, &context, true);"
    ));
}

/// Key hints live in `hints_for` and nowhere else — including in `app.rs`, which
/// owns that function and could therefore grow a second copy of the text without
/// any view being involved.
///
/// The file list is walked, not written down, so a module added tomorrow is
/// guarded tomorrow; and `app.rs` is in scope with `hints_for`'s own body
/// excluded, which is what makes the rule hold in the one file where a hint is
/// legitimate.
#[test]
fn no_view_hard_codes_a_key_hint() {
    let mut checked: Vec<String> = Vec::new();
    for (name, source) in view_modules() {
        for (number, line) in shipped(&source).lines().enumerate() {
            assert!(
                !names_a_key(line),
                "src/tui/views/{name}.rs:{} names a key outside `hints_for`: {line:?}",
                number + 1
            );
        }
        checked.push(format!("src/tui/views/{name}.rs"));
    }
    for (number, line) in shipped(&app_without_hint_text()).lines().enumerate() {
        assert!(
            !names_a_key(line),
            "src/tui/app.rs:{} names a key outside `hints_for`: {line:?}",
            number + 1
        );
    }
    checked.push("src/tui/app.rs".to_string());
    println!("hint guard covers: {}", checked.join(", "));
}

/// The key-hint check has to fire on the two strings it was written for, and on
/// nothing the views legitimately contain. Without this, a detector that matched
/// no hint at all would leave `no_view_hard_codes_a_key_hint` passing for the
/// wrong reason — a guard that cannot fail is not a guard.
#[test]
fn the_key_hint_check_spots_a_hint_and_spares_the_rest() {
    // The two spellings that shipped in views before this file existed.
    assert!(
        names_a_key("\"Press 'a' on a project to start analysis\","),
        "the prose form must be caught"
    );
    assert!(
        names_a_key(" \" [y] Yes   [n] No   [Esc] Cancel\","),
        "the bracketed form must be caught"
    );
    assert!(
        names_a_key(" \"[Enter] approve  [a] approve all  [d] discard\","),
        "the old proposals footer must be caught"
    );

    // What the views actually contain: Rust's punctuation and doc prose. Each of
    // these appears in a view file today.
    for innocent in [
        "chunks[2],",
        "    let values = [id, name, model, project_id];",
        "const FIELDS: [&str; 7] = [",
        "    f.render_widget(List::new(items), chunks[0]);",
        "/// The subscriber writes `[12:00:02] ERROR message`, so the level comes first —",
        "/// [`theme::action`], so where the user is typing is a colour rather than a",
        "    _available_agents: &[String],",
    ] {
        assert!(
            !names_a_key(innocent),
            "{innocent:?} is not a key hint and must not be reported as one"
        );
    }

    // A doc comment is not shipped code: a view may name the hint it used to
    // have in a comment explaining why it does not.
    assert!(
        !names_a_key("    // no longer says Press 'a' to analyze"),
        "a comment is not the interface"
    );
}

/// The border check fires on each spelling of asking ratatui for one, and on the
/// whole-source case: a view that draws its own corners does not have to write
/// `Borders::` to violate the rule the buffer checks enforce.
#[test]
fn the_border_check_spots_every_spelling() {
    for ask in [
        "    .block(Block::default().borders(Borders::ALL))",
        "    let block = Block::default().bordered();",
        "    .border_type(BorderType::Plain)",
    ] {
        assert_eq!(
            border_ask(ask),
            Some(
                ["Borders::", ".borders(", ".bordered(", ".border_type("]
                    .into_iter()
                    .find(|needle| ask.contains(needle))
                    .unwrap()
            ),
            "{ask:?} asks for a border and must be reported"
        );
    }
    assert_eq!(
        border_ask("    f.render_widget(Paragraph::new(line), rect);"),
        None
    );
}

/// Borders are for the two modals. Everything else frames content with type and
/// rules, so a `Block` with borders anywhere else is a step back to the clutter
/// this interface was redesigned to remove. The file list is walked, so a view
/// module added tomorrow is in scope tomorrow.
#[test]
fn only_the_modals_draw_borders() {
    for (name, source) in view_modules() {
        if name == MODAL_MODULE {
            continue;
        }
        assert_eq!(
            border_ask(shipped(&source)),
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
    let modals = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/tui/views")
            .join(format!("{MODAL_MODULE}.rs")),
    )
    .expect("the exempt module is named in MODAL_MODULE and must exist");
    assert!(
        modals.contains("Borders::ALL"),
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

/// Design spec and implementation plan, for the citation sweep below.
const SPEC: &str = include_str!("../docs/superpowers/specs/2026-10-02-tui-redesign-design.md");
const PLAN: &str = include_str!("../docs/superpowers/plans/2026-10-02-tui-redesign.md");

/// The spec by the path the code names it by, so "the spec does not exist" is a
/// message this guard can produce rather than one a reader has to discover.
const SPEC_PATH: &str = "docs/superpowers/specs/2026-10-02-tui-redesign-design.md";

/// The heading each piece of the spec is cited from. What these are for is the
/// sentence, not the function: a citation says *where the rule is written*, and
/// a heading still names that after the document grows a paragraph.
const SPEC_SECTIONS: [&str; 7] = [
    "### `theme.rs`",
    "### `chrome.rs`",
    "## Visual language",
    "Per-view layout",
    "### Chat",
    "### Projects",
    "## Testing",
];

/// The doc comment on `render_bar` used to say neither the product's name nor a
/// view name is ever dropped — which `Paragraph`'s clip falsifies, and which sat
/// for two revisions because a neighbouring `AGENTS.md` bullet had been corrected
/// and this one had not. A module's own doc comment has to survive its neighbours
/// being fixed, and nothing here checked that.
#[test]
fn the_bar_says_it_clips_its_right_edge_rather_than_choosing_what_to_drop() {
    let doc = between(CHROME, "/// Draws the context bar", "pub fn render_bar(");
    assert!(
        doc.contains("clipped") && doc.contains("`Paragraph`"),
        "the bar's own doc must say the row is clipped at its width and never say \
         the names cannot be dropped: {doc}"
    );
    assert!(
        !doc.contains("never dropped") && !doc.contains("never truncated"),
        "the bar drops whatever falls past the cut, including the dot: {doc}"
    );
}

/// The text between `from` and `to` in `source`, or the whole source with a note
/// when either marker is gone — a moved item should say so, not return nothing.
fn between(source: &str, from: &str, to: &str) -> String {
    let (Some(start), Some(end)) = (source.find(from), source.find(to)) else {
        panic!("`{from}` .. `{to}` is not where this test looks any more");
    };
    source[start..end].to_string()
}

/// One call to `chrome::render_bar`, in `app.rs` — the only place the bar is
/// drawn, and the reason a view cannot draw a second one over the dispatch's.
#[test]
fn only_the_dispatch_draws_the_bar() {
    let mut calls: Vec<String> = Vec::new();
    for (name, source) in tui_modules() {
        for (number, line) in shipped(&source).lines().enumerate() {
            if footer_call(line) || bar_call(line) {
                calls.push(format!("src/tui/{name}:{}", number + 1));
            }
        }
    }
    assert_eq!(
        calls.len(),
        2,
        "one call to `render_footer` and one to `render_bar`, both in the dispatch: {calls:?}"
    );
    for call in &calls {
        assert!(
            call.starts_with("src/tui/app.rs:"),
            "the bar and the footer are the chrome's two rows and are drawn once, \
             by the dispatch: {calls:?}"
        );
    }
}

/// Whether `line` *calls* `render_bar` rather than declaring it.
fn bar_call(line: &str) -> bool {
    if !line.contains("render_bar(") {
        return false;
    }
    let trimmed = line.trim_start();
    !(trimmed.starts_with("fn ")
        || trimmed.starts_with("pub fn ")
        || trimmed.starts_with("pub(crate) fn "))
}

/// **Every citation of the spec names a section that is still a heading.** Two
/// halves, because either alone passes for the wrong reason: a line number that
/// is still in range says nothing about whether it still points at the rule it
/// was cited for, and a section name that has drifted away says nothing about
/// the sentences around it.
///
/// Not "the citation is in range" — that is what the three rotted citations in
/// `projects.rs` looked like. `design.md:100-101` was correct when it was written
/// and pointed at `### Projects` and a blank line after the spec grew by fifteen
/// rows. What the code meant was *the project rows' mockup*, so that is what it
/// has to say, and a heading survives an edit.
#[test]
fn every_citation_of_the_spec_names_a_section_that_still_exists() {
    let citations = cited_sections();
    assert!(
        !citations.is_empty(),
        "no citation of the design spec found under src/tui — either the sources \
         stopped citing it or the scan has stopped finding them, and a sweep that \
         finds nothing has checked nothing"
    );
    for (where_, section) in &citations {
        assert!(
            SPEC_SECTIONS.contains(&section.as_str()),
            "{where_} cites the spec's {section:?}, which is not one of the headings \
             it has: {SPEC_SECTIONS:?}"
        );
        assert!(
            SPEC.lines().any(|line| {
                let heading = line.trim();
                heading == format!("### {section}")
                    || heading == format!("## {section}")
                    || heading.ends_with(&format!(" {section}"))
            }),
            "{SPEC_PATH} has no heading {section:?}, so the citation is a name with \
             nothing behind it"
        );
    }
}

/// A line number is the form that rotted, three times, over one document this
/// branch edits in the same commit that changes the code it describes. So none is
/// used: the rule is asserted instead of pointed at (`render_bar`'s doc, the
/// project-row comment), and where the *sentences* matter the citation names the
/// section they are in.
#[test]
fn no_source_cites_a_line_number_in_the_spec() {
    let mut checked = 0usize;
    for (name, source) in tui_modules() {
        for (number, line) in shipped(&source).lines().enumerate() {
            if line.contains("design.md") {
                checked += 1;
            }
            assert!(
                !line_number_citation(line),
                "src/tui/{name}:{} cites the spec by line number: {line:?} — cite the \
                 section, or the number goes stale the next time the document grows",
                number + 1
            );
        }
    }
    assert!(
        checked > 0,
        "no source under src/tui mentions the spec at all, so this found nothing to \
         check — the scan is broken, not the code"
    );
}

/// Whether `line` cites the spec by line number: the path, or its own short name,
/// followed by a colon and a digit.
fn line_number_citation(line: &str) -> bool {
    [SPEC_PATH, "design.md"].iter().any(|needle| {
        let Some(index) = line.find(needle) else {
            return false;
        };
        let mut rest = line[index + needle.len()..].trim_start();
        if let Some(stripped) = rest.strip_prefix(':') {
            rest = stripped.trim_start().trim_start_matches('`').trim_start();
        }
        rest.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// The `(file:line, section)` pairs every source under `src/tui` cites.
///
/// A citation is one sentence and may wrap, and the `§` marker landed on the line
/// after the path in the citation this was written for — so each line is read
/// together with the one after it.
fn cited_sections() -> Vec<(String, String)> {
    let mut found = Vec::new();
    for (name, source) in tui_modules() {
        let lines: Vec<&str> = shipped(&source).lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let window = lines
                .get(index + 1)
                .map_or_else(|| (*line).to_string(), |next| format!("{line} {next}"));
            if !window.contains("design.md") {
                continue;
            }
            let Some((_, after)) = window.split_once('§') else {
                continue;
            };
            let section: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == ' ')
                .collect();
            found.push((
                format!("src/tui/{name}:{}", index + 1),
                section.trim().to_string(),
            ));
        }
    }
    found
}

/// The plan's two anchors into the spec: it points at the file, and it does not
/// re-decide what the spec decides. A copy of a decision in the plan is how the
/// two drift — the plan said the chat input area may be boxed, the spec's
/// per-view layout says the prompt marker replaced that box, and the code has
/// neither.
#[test]
fn the_plan_points_at_the_spec_without_re_deciding_it() {
    assert!(
        PLAN.contains(SPEC_PATH),
        "the plan must name the spec it implements"
    );
    assert!(
        PLAN.contains("no boxes** except the modal dialogs"),
        "the plan still permits a chat input box that the spec replaced with a \
         prompt marker — the premise that put a false claim in two views"
    );
}

/// The product's name is in the bar and in no view. It went when the dashboard's
/// banner went, and the spec's claim that the bar "does that job" was half
/// false: the bar draws view names, a context label and a liveness dot, which
/// say where you are, not what this is — the product's name is the part it added.
#[test]
fn the_context_bar_names_the_product() {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| {
            llama_r::tui::chrome::render_bar(
                f,
                f.area(),
                &["Dashboard", "Projects", "Chat"],
                0,
                &llama_r::tui::chrome::Context::default(),
                true,
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let row: String = (0..buffer.area.width)
        .map(|x| buffer[(x, 0)].symbol().to_string())
        .collect();
    assert!(
        row.starts_with("Llama-R "),
        "the bar must say what interface this is: {row:?}"
    );
}
