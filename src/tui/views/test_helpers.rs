//! Fixtures the view tests share.
//!
//! Reading a rendered buffer back as text and building an `AppState` over a
//! throwaway data dir are needed by more than one view's tests. They live here
//! so each view does not carry its own copy.

use crate::adapters::mcp::StaticMcpRegistry;
use crate::api::handlers::AppState;
use crate::context::store::{ContextStore, ProjectContext};
use crate::providers::ollama::OllamaProvider;
use crate::runtime::build_app_state;
use crate::services::agent_registry::AgentRegistry;
use crate::services::skill_manager::SkillManager;
use ratatui::buffer::Buffer;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// Row `y` of `buffer`, one entry per terminal column.
///
/// Counting entries proves nothing on its own — the row is always exactly
/// `buffer.area.width` wide — so it is only useful as the string the
/// assertions search.
pub(super) fn row_text(buffer: &Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol().to_string())
        .collect()
}

/// Every row of `buffer`, joined by newlines.
pub(super) fn all_text(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| row_text(buffer, y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The terminal column each character of row `y` was written in.
///
/// Comparing character offsets instead of columns is wrong exactly where a view
/// is easiest to get wrong: `日本語` is three characters and six columns, so a row
/// carrying one puts every later character several columns left of where the
/// screen actually puts it. What makes the two differ is that a wide glyph and
/// the blank cell beside it are one character between two columns.
pub(super) fn char_columns(buffer: &Buffer, y: u16) -> Vec<usize> {
    let mut columns = Vec::new();
    let mut written = 0;
    for x in 0..buffer.area.width {
        columns.push(written);
        written += buffer[(x, y)].symbol().chars().count();
    }
    columns
}

/// The terminal column where `needle` starts on row `y`, if it is there.
///
/// `str::find` answers in bytes and a CJK glyph is three of them, so the offset
/// is converted to a character index before being read as a column. Left in
/// bytes it would move a CJK row's needle *with* it, which is the opposite of
/// what the caller is asking.
pub(super) fn column_of(buffer: &Buffer, y: u16, needle: &str) -> Option<usize> {
    let row = row_text(buffer, y);
    let characters = row[..row.find(needle)?].chars().count();
    char_columns(buffer, y).get(characters).copied()
}

/// An `AppState` reading a throwaway data dir holding `projects` (id,
/// project type) and, per project, how many agents it has.
///
/// Agent ids do not matter to the views under test, only how many there are.
/// `LLAMA_R_DIR` is process-wide, so every caller holds the env lock.
pub(super) fn state_with(
    projects: &[(&str, &str)],
    agents: &[(&str, usize)],
) -> (TempDir, Arc<AppState>) {
    let temp_dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", temp_dir.path());

    let context_store = Arc::new(ContextStore::new());
    for (project_id, project_type) in projects {
        context_store
            .save_context(ProjectContext {
                project_id: (*project_id).to_string(),
                path: temp_dir.path().join(project_id).display().to_string(),
                context_md: String::new(),
                project_type: (*project_type).to_string(),
                skills_injected: Vec::new(),
                last_analyzed: chrono::Utc::now(),
                custom_rules: String::new(),
            })
            .unwrap();
    }

    let agent_registry = Arc::new(AgentRegistry::new());
    for (project_id, count) in agents {
        let dir = crate::core::paths::get_project_agents_dir(project_id);
        std::fs::create_dir_all(&dir).unwrap();
        for index in 0..*count {
            std::fs::write(
                dir.join(format!("agent-{index}.toml")),
                "name = \"agente\"\nmodel = \"llama3\"\nsystem_prompt = \"hola\"\n",
            )
            .unwrap();
        }
    }
    agent_registry.reload_all(&[]).unwrap();

    let state = build_app_state(
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
    );
    (temp_dir, state)
}
