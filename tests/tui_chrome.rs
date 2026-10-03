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

/// The view modules, read from disk at test time.
///
/// Deliberately not a list: a hand-maintained one goes stale the moment a module
/// is added, and the module that goes unguarded is then the new one — which is
/// exactly what happened when the modals moved into `views/`. Walking the
/// directory means a file cannot be added without also being guarded, and
/// `include_str!` cannot silently fall back to nothing.
///
/// `modals.rs` is exempt and named by [`MODAL_MODULE`], because borders are
/// permitted there and nowhere else.
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
        .filter(|(name, _)| name != MODAL_MODULE)
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

/// The one module under `src/tui/views/` permitted to draw a border. A dialog is
/// the one thing on this screen that has to read as separate from the page behind
/// it, and `design.md` permits a box nowhere else.
const MODAL_MODULE: &str = "modals";

/// The dispatch that owns the chrome and `hints_for`.
const APP: &str = include_str!("../src/tui/app.rs");

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

/// The product's name is in the bar and in no view. It went when the dashboard's
/// banner went, and the spec's claim that the bar "does that job" is false: the
/// bar draws view names, a context label and a health dot, which say where you
/// are, not what this is.
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
