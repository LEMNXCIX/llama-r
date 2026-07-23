use crate::domain::agent::{Agent, AgentConfig};
use crate::domain::scope::AgentScope;
use crate::error::AppError;
use crate::services::scope_builder::ScopeBuilder;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

#[derive(Debug, Clone)]
pub struct RegisteredAgent {
    pub agent: Agent,
    pub scope: AgentScope,
    pub path: PathBuf,
    pub version_hash: u64,
}

pub struct AgentRegistry {
    agents: RwLock<HashMap<String, RegisteredAgent>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
        }
    }

    pub fn reload_all(&self, known_mcp_servers: &[String]) -> Result<usize, AppError> {
        let mut loaded = HashMap::new();

        self.load_dir(
            &crate::core::paths::get_agents_dir(),
            None,
            known_mcp_servers,
            &mut loaded,
        )?;

        let projects = crate::core::paths::get_contexts_dir();
        if projects.exists() {
            for entry in fs::read_dir(&projects)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let project_id = entry.file_name().to_string_lossy().to_string();
                let agents_dir = entry.path().join("agents");
                self.load_dir(
                    &agents_dir,
                    Some(&project_id),
                    known_mcp_servers,
                    &mut loaded,
                )?;
            }
        }

        let count = loaded.len();
        let mut guard = self
            .agents
            .write()
            .map_err(|_| AppError::Runtime("AgentRegistry lock poisoned".into()))?;
        *guard = loaded;
        tracing::info!(agent_count = count, "AgentRegistry reloaded");
        Ok(count)
    }

    fn load_dir(
        &self,
        dir: &Path,
        project_id: Option<&str>,
        known_mcp_servers: &[String],
        out: &mut HashMap<String, RegisteredAgent>,
    ) -> Result<(), AppError> {
        if !dir.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
                continue;
            }
            match self.load_one(&path, project_id, known_mcp_servers) {
                Ok(registered) => {
                    let key = registry_key(project_id, &registered.agent.id);
                    out.insert(key, registered);
                }
                Err(err) => {
                    tracing::error!(path = %path.display(), error = %err, "skipping agent");
                }
            }
        }
        Ok(())
    }

    fn load_one(
        &self,
        path: &Path,
        project_id: Option<&str>,
        known_mcp_servers: &[String],
    ) -> Result<RegisteredAgent, AppError> {
        let content = fs::read_to_string(path)?;
        let config: AgentConfig = toml::from_str(&content)?;
        let id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| AppError::Validation("invalid agent filename".into()))?
            .to_string();

        let agent = Agent {
            id,
            project_id: project_id.map(str::to_string),
            config,
        };

        // Validate mcp_sources against known server IDs
        for source in &agent.config.mcp_sources {
            if !known_mcp_servers.iter().any(|s| s == source) {
                tracing::warn!(
                    agent_id = %agent.qualified_id(),
                    mcp_source = %source,
                    "mcp_source references unknown MCP server; scope still built"
                );
            }
        }

        let scope = ScopeBuilder::build(&agent)?;
        let version_hash = fnv1a64(content.as_bytes());

        Ok(RegisteredAgent {
            agent,
            scope,
            path: path.to_path_buf(),
            version_hash,
        })
    }

    // -- Backward-compatible resolver matching AgentManager interface --

    pub fn resolve_agent(&self, project_id: Option<&str>, agent_id: Option<&str>) -> Option<Agent> {
        self.resolve(project_id, agent_id).map(|r| r.agent)
    }

    pub fn get_agent(&self, id: &str) -> Option<Agent> {
        self.agents
            .read()
            .ok()
            .and_then(|guard| guard.get(&registry_key(None, id)).cloned())
            .map(|r| r.agent)
    }

    pub fn get_project_agent(&self, project_id: &str, id: &str) -> Option<Agent> {
        self.agents
            .read()
            .ok()
            .and_then(|guard| guard.get(&registry_key(Some(project_id), id)).cloned())
            .map(|r| r.agent)
    }

    pub fn resolve(
        &self,
        project_id: Option<&str>,
        agent_id: Option<&str>,
    ) -> Option<RegisteredAgent> {
        let key = match (project_id, agent_id) {
            (Some(project), Some(agent)) => registry_key(Some(project), agent),
            (Some(project), None) => registry_key(Some(project), project),
            (None, Some(agent)) => registry_key(None, agent),
            (None, None) => return None,
        };
        self.agents
            .read()
            .ok()
            .and_then(|guard| guard.get(&key).cloned())
    }

    pub fn resolve_scope(
        &self,
        project_id: Option<&str>,
        agent_id: Option<&str>,
    ) -> Option<AgentScope> {
        self.resolve(project_id, agent_id).map(|r| r.scope)
    }

    pub fn list(&self) -> Vec<RegisteredAgent> {
        self.agents
            .read()
            .map(|guard| guard.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn list_agents(&self) -> Vec<Agent> {
        self.list().into_iter().map(|r| r.agent).collect()
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn registry_key(project_id: Option<&str>, agent_id: &str) -> String {
    match project_id {
        Some(project) => format!("{project}::{agent_id}"),
        None => agent_id.to_string(),
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
