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
            Box::new(|f: &mut Frame, body: Rect| render_projects(f, body, &state, 0, 0, true)),
        ),
        (
            "projects · agent list",
            Box::new(|f: &mut Frame, body: Rect| render_projects(f, body, &state, 0, 0, false)),
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
