use std::env;
use std::path::PathBuf;

/// Returns the base directory for Llama-R data (agents, contexts, etc.)
/// Priority: LLAMA_R_DIR env var > Executable location > Current directory.
pub fn get_base_dir() -> PathBuf {
    // 1. Check environment variable
    if let Ok(dir) = env::var("LLAMA_R_DIR") {
        return PathBuf::from(dir);
    }

    // 2. Check executable directory
    if let Ok(exe_path) = env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            // Avoid creating data inside 'target/debug' or similar during development
            if exe_dir.ends_with("debug") || exe_dir.ends_with("release") {
                if let Some(project_root) = exe_dir.parent().and_then(|p| p.parent()) {
                    return project_root.to_path_buf();
                }
            }
            return exe_dir.to_path_buf();
        }
    }

    // 3. Fallback to current directory
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

/// Helper to ensure global directories exist
pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(get_agents_dir())?;
    std::fs::create_dir_all(get_contexts_dir())?;
    Ok(())
}
