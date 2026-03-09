use crate::error::AppError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectContext {
    pub project_id: String,
    pub path: String,
    pub context_md: String,
    pub project_type: String,
    pub skills_injected: Vec<String>,
    pub last_analyzed: DateTime<Utc>,
    pub custom_rules: String,
}

pub struct ContextStore {
    contexts: RwLock<HashMap<String, ProjectContext>>,
    base_dir: PathBuf,
}

impl ContextStore {
    pub fn new() -> Self {
        let base_dir = crate::core::paths::get_contexts_dir();
        let store = Self {
            contexts: RwLock::new(HashMap::new()),
            base_dir,
        };
        store.load_from_disk();
        store
    }

    fn load_from_disk(&self) {
        if !self.base_dir.exists() {
            return;
        }

        let Ok(entries) = fs::read_dir(&self.base_dir) else {
            tracing::warn!(path = %self.base_dir.display(), "Could not read contexts directory during startup");
            return;
        };
        let Ok(mut contexts) = self.contexts.write() else {
            tracing::error!("ContextStore lock poisoned while loading from disk");
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let ctx_file = path.join("context").join("context.json");
            if !ctx_file.exists() {
                continue;
            }
            match fs::read_to_string(&ctx_file)
                .ok()
                .and_then(|content| serde_json::from_str::<ProjectContext>(&content).ok())
            {
                Some(ctx) => {
                    contexts.insert(ctx.project_id.clone(), ctx);
                }
                None => tracing::warn!(path = %ctx_file.display(), "Skipping unreadable project context"),
            }
        }
        tracing::info!(context_count = contexts.len(), "ContextStore loaded project contexts");
    }

    pub fn get_context(&self, project_id: &str) -> Option<ProjectContext> {
        self.contexts
            .read()
            .ok()
            .and_then(|contexts| contexts.get(project_id).cloned())
    }

    pub fn list_contexts(&self) -> Vec<ProjectContext> {
        self.contexts
            .read()
            .map(|contexts| contexts.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn save_context(&self, ctx: ProjectContext) -> Result<(), AppError> {
        let project_context_dir = crate::core::paths::get_project_context_dir(&ctx.project_id);
        fs::create_dir_all(&project_context_dir)?;

        let md_path = project_context_dir.join("context.md");
        fs::write(&md_path, &ctx.context_md)?;

        let json_path = project_context_dir.join("context.json");
        let json = serde_json::to_string_pretty(&ctx)?;
        fs::write(&json_path, json)?;

        let project_id = ctx.project_id.clone();
        let mut contexts = self
            .contexts
            .write()
            .map_err(|_| AppError::Runtime("ContextStore lock poisoned while saving context".to_string()))?;
        contexts.insert(project_id, ctx);
        Ok(())
    }

    pub fn delete_context(&self, project_id: &str) -> bool {
        let Ok(mut contexts) = self.contexts.write() else {
            tracing::error!("ContextStore lock poisoned while deleting context");
            return false;
        };
        if contexts.remove(project_id).is_some() {
            let project_dir = self.base_dir.join(project_id);
            if let Err(err) = fs::remove_dir_all(&project_dir) {
                tracing::warn!(path = %project_dir.display(), error = %err, "Failed to remove project context directory");
            }
            true
        } else {
            false
        }
    }

    pub fn get_context_md(&self, project_id: &str) -> String {
        self.get_context(project_id)
            .map(|context| context.context_md)
            .unwrap_or_default()
    }
}
