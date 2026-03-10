use crate::domain::models::{Skill, SkillMetadata};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub struct SkillManager {
    skills: Arc<RwLock<HashMap<String, Skill>>>,
    base_paths: Vec<PathBuf>,
}

impl SkillManager {
    pub fn new() -> Self {
        let mut base_paths = Vec::new();

        if let Some(home) = dirs::home_dir() {
            for dir in [".cursor", ".claude", ".agent", ".windsurf", ".agents", ".llama-r"] {
                base_paths.push(home.join(dir).join("skills"));
            }
        }

        base_paths.push(PathBuf::from("./skills"));
        base_paths.push(PathBuf::from("./.cursor/skills"));
        base_paths.push(PathBuf::from("./.claude/skills"));
        base_paths.push(PathBuf::from("./.agent/skills"));

        Self {
            skills: Arc::new(RwLock::new(HashMap::new())),
            base_paths,
        }
    }

    pub fn scan_and_load(&self) {
        let mut new_skills = HashMap::new();

        for path in &self.base_paths {
            self.load_dir_into(path, &mut new_skills);
        }

        match self.skills.write() {
            Ok(mut skills_lock) => {
                *skills_lock = new_skills;
                tracing::info!(skill_count = skills_lock.len(), "SkillManager loaded skills");
            }
            Err(_) => tracing::error!("SkillManager lock poisoned while loading skills"),
        }
    }

    fn load_dir_into(&self, path: &Path, skills: &mut HashMap<String, Skill>) {
        if !path.exists() || !path.is_dir() {
            return;
        }

        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let skill_path = entry.path();
                if skill_path.is_dir() {
                    if let Some(skill) = self.load_skill(&skill_path) {
                        skills.insert(skill.id.clone(), skill);
                    }
                }
            }
        }
    }

    fn load_skill(&self, path: &Path) -> Option<Skill> {
        let skill_md_path = path.join("SKILL.md");
        if !skill_md_path.exists() {
            return None;
        }

        let content = fs::read_to_string(&skill_md_path).ok()?;
        let metadata = self.parse_skill_metadata(&content)?;
        let id = path.file_name()?.to_string_lossy().into_owned();
        Some(Skill {
            id,
            path: path.to_string_lossy().into_owned(),
            metadata,
            content,
        })
    }

    fn parse_skill_metadata(&self, content: &str) -> Option<SkillMetadata> {
        if !content.starts_with("---") {
            return None;
        }

        let parts: Vec<&str> = content.split("---").collect();
        if parts.len() < 3 {
            return None;
        }

        let yaml_content = parts[1];
        let mut name = String::new();
        let mut description = String::new();
        let mut tags = Vec::new();

        for line in yaml_content.lines().map(str::trim) {
            if line.starts_with("name:") {
                name = line.replace("name:", "").trim().trim_matches('"').to_string();
            } else if line.starts_with("description:") {
                description = line
                    .replace("description:", "")
                    .trim()
                    .trim_matches('"')
                    .to_string();
            } else if line.starts_with("tags:") {
                let trimmed = line.replace("tags:", "").trim().to_string();
                if trimmed.starts_with('[') && trimmed.ends_with(']') {
                    tags = trimmed[1..trimmed.len() - 1]
                        .split(',')
                        .map(|value| value.trim().trim_matches('"').trim_matches('\'').to_string())
                        .collect();
                }
            }
        }

        if name.is_empty() {
            return None;
        }

        Some(SkillMetadata {
            name,
            description,
            tags: if tags.is_empty() { None } else { Some(tags) },
        })
    }

    pub fn list_skills(&self) -> Vec<Skill> {
        self.skills
            .read()
            .map(|skills_lock| skills_lock.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn list_skills_for_project(&self, project_path: &Path) -> Vec<Skill> {
        let mut combined = self
            .skills
            .read()
            .map(|skills_lock| skills_lock.clone())
            .unwrap_or_default();

        for local_path in [project_path.join("skills"), project_path.join(".agents/skills")] {
            self.load_dir_into(&local_path, &mut combined);
        }

        combined.into_values().collect()
    }

    pub fn get_skill(&self, id: &str) -> Option<Skill> {
        self.skills
            .read()
            .ok()
            .and_then(|skills_lock| skills_lock.get(id).cloned())
    }

    pub fn get_skill_for_project(&self, id: &str, project_path: &Path) -> Option<Skill> {
        for local_path in [project_path.join("skills"), project_path.join(".agents/skills")] {
            let skill_path = local_path.join(id);
            if skill_path.is_dir() {
                if let Some(skill) = self.load_skill(&skill_path) {
                    return Some(skill);
                }
            }
        }

        self.get_skill(id)
    }
}

mod dirs {
    use std::path::PathBuf;

    pub fn home_dir() -> Option<PathBuf> {
        #[cfg(windows)]
        {
            std::env::var_os("USERPROFILE").map(PathBuf::from)
        }
        #[cfg(not(windows))]
        {
            std::env::var_os("HOME").map(PathBuf::from)
        }
    }
}
