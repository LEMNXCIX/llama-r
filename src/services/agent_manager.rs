use crate::domain::agent::{Agent, AgentConfig};
use crate::error::AppError;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::RwLock;

pub struct AgentManager {
    agents: RwLock<HashMap<String, Agent>>,
}

impl AgentManager {
    pub fn new() -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
        }
    }

    pub fn load_agents(&self) -> Result<(), AppError> {
        let mut loaded_agents = HashMap::new();

        let global_agents_dir = crate::core::paths::get_agents_dir();
        self.load_from_dir(&global_agents_dir, &mut loaded_agents)?;

        let projects_dir = crate::core::paths::get_contexts_dir();
        if projects_dir.exists() {
            for entry in fs::read_dir(projects_dir)? {
                let entry = entry?;
                let project_agents_dir = entry.path().join("agents");
                if project_agents_dir.is_dir() {
                    self.load_from_dir(&project_agents_dir, &mut loaded_agents)?;
                }
            }
        }

        let count = loaded_agents.len();
        let mut agents_guard = self
            .agents
            .write()
            .map_err(|_| AppError::Runtime("AgentManager lock poisoned while loading agents".to_string()))?;
        *agents_guard = loaded_agents;

        tracing::info!(agent_count = count, "Loaded agents from disk");
        Ok(())
    }

    fn load_from_dir(
        &self,
        dir: &Path,
        loaded_agents: &mut HashMap<String, Agent>,
    ) -> Result<(), AppError> {
        if !dir.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|s| s.to_str()) == Some("toml") {
                let content = fs::read_to_string(&path)?;
                match toml::from_str::<AgentConfig>(&content) {
                    Ok(config) => {
                        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                            tracing::warn!(path = %path.display(), "Skipping agent file with invalid stem");
                            continue;
                        };
                        let id = stem.to_string();
                        let agent = Agent {
                            id: id.clone(),
                            config,
                        };
                        loaded_agents.insert(id, agent);
                    }
                    Err(err) => {
                        tracing::error!(path = %path.display(), error = %err, "Failed to parse agent config");
                    }
                }
            }
        }
        Ok(())
    }

    pub fn get_agent(&self, id: &str) -> Option<Agent> {
        self.agents
            .read()
            .ok()
            .and_then(|agents_guard| agents_guard.get(id).cloned())
    }

    pub fn list_agents(&self) -> Vec<Agent> {
        self.agents
            .read()
            .map(|agents_guard| agents_guard.values().cloned().collect())
            .unwrap_or_default()
    }
}
