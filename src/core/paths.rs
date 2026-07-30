use std::env;
use std::path::PathBuf;

/// Returns the base directory for Llama-R data (agents, contexts, etc.)
/// Priority: LLAMA_R_DIR env var > XDG data home > Development project root > Current directory.
pub fn get_base_dir() -> PathBuf {
    // 1. Check environment variable
    if let Ok(dir) = env::var("LLAMA_R_DIR") {
        return PathBuf::from(dir);
    }

    // 2. XDG data home (Linux / macOS)
    if let Ok(xdg) = env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("llama-r");
        }
    }
    // 2b. ~/.local/share/llama-r  (POSIX default)
    if let Ok(home) = env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".local/share/llama-r");
        }
    }

    // 3. Check executable directory — only use as data root when inside a
    //    cargo workspace (target/debug or target/release).  For installed
    //    binaries we fall through to the XDG path above.
    if let Ok(exe_path) = env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let check_dir = if exe_dir.ends_with("deps") {
                exe_dir.parent()
            } else {
                Some(exe_dir)
            };
            if let Some(d) = check_dir {
                if d.ends_with("debug") || d.ends_with("release") {
                    if let Some(project_root) = d.parent().and_then(|p| p.parent()) {
                        return project_root.to_path_buf();
                    }
                }
            }
        }
    }

    // 4. Last resort: current working directory
    PathBuf::from(".")
}

/// Returns the global directory for agent configurations
pub fn get_agents_dir() -> PathBuf {
    get_base_dir().join("agents")
}

/// Returns the base directory for all project contexts
pub fn get_contexts_dir() -> PathBuf {
    get_base_dir().join("contextos/projects")
}

/// Returns the root directory for a specific project
pub fn get_project_dir(project_id: &str) -> PathBuf {
    get_contexts_dir().join(project_id)
}

/// Returns the context (metadata) directory for a specific project
pub fn get_project_context_dir(project_id: &str) -> PathBuf {
    get_project_dir(project_id).join("context")
}

/// Returns the specialized agents directory for a specific project
pub fn get_project_agents_dir(project_id: &str) -> PathBuf {
    get_project_dir(project_id).join("agents")
}

/// Runtime data root: `{base}/data`
pub fn get_data_dir() -> PathBuf {
    get_base_dir().join("data")
}

/// Persistent RAG root: `{base}/data/lancedb`
///
/// Phase 4 stores FileRagStore collections here (JSONL per source_id).
/// Path name kept for plan compatibility; LanceDB can reuse the same root later.
pub fn get_lancedb_dir() -> PathBuf {
    get_data_dir().join("lancedb")
}

/// Returns the path to the SQLite history database file.
pub fn get_history_db_path() -> PathBuf {
    get_data_dir().join("history.db")
}

/// Helper to ensure global directories exist
pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(get_agents_dir())?;
    std::fs::create_dir_all(get_contexts_dir())?;
    std::fs::create_dir_all(get_data_dir())?;
    std::fs::create_dir_all(get_lancedb_dir())?;
    Ok(())
}
