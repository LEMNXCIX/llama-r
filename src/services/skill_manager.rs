use crate::domain::models::{Skill, SkillMetadata, SkillScope};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Directories searched for skills, relative to a home dir or a project root.
///
/// Shared by the global and project scans so the two cannot drift apart. The
/// project scan previously knew only `skills/` and `.agents/skills/`, which
/// meant a repo carrying its own `.claude/skills/` was invisible unless
/// llama-r happened to run from that root.
pub const SKILL_DIR_NAMES: [&str; 5] = [
    "skills",
    ".agents/skills",
    ".claude/skills",
    ".cursor/skills",
    ".agent/skills",
];

/// Home directories belonging to other harnesses. Skills found here are
/// tagged [`SkillScope::Harness`] instead of [`SkillScope::LlamaR`].
pub const HARNESS_DIR_NAMES: [&str; 5] = [".cursor", ".claude", ".agent", ".windsurf", ".agents"];

pub struct SkillManager {
    skills: Arc<RwLock<HashMap<String, Skill>>>,
    base_paths: Vec<PathBuf>,
    /// Ids that exist in more than one scope, and what shadowed what.
    shadowed: Arc<RwLock<Vec<ShadowedSkill>>>,
}

/// A skill that exists in a narrower scope but was hidden by a broader one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowedSkill {
    pub id: String,
    /// Scope of the skill that ended up being used.
    pub winner: SkillScope,
    /// Scope of the skill that was hidden.
    pub loser: SkillScope,
}

/// Insert a skill, letting a narrower scope win.
///
/// A wider scope never replaces a narrower one. When it would, the swap is
/// recorded so the shadowing is visible instead of silent — previously two
/// skills with the same id simply overwrote each other and nobody found out.
fn insert_with_precedence(
    skill: Skill,
    skills: &mut HashMap<String, Skill>,
    shadowed: &mut Vec<ShadowedSkill>,
) {
    match skills.get(&skill.id) {
        Some(existing) if existing.scope >= skill.scope => {
            shadowed.push(ShadowedSkill {
                id: skill.id.clone(),
                winner: existing.scope,
                loser: skill.scope,
            });
        }
        Some(existing) => {
            shadowed.push(ShadowedSkill {
                id: skill.id.clone(),
                winner: skill.scope,
                loser: existing.scope,
            });
            skills.insert(skill.id.clone(), skill);
        }
        None => {
            skills.insert(skill.id.clone(), skill);
        }
    }
}

impl SkillManager {
    pub fn new() -> Self {
        let mut base_paths = Vec::new();

        if let Some(home) = dirs::home_dir() {
            for dir in [
                ".cursor",
                ".claude",
                ".agent",
                ".windsurf",
                ".agents",
                ".llama-r",
            ] {
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
            shadowed: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn scan_and_load(&self) {
        let mut new_skills: HashMap<String, Skill> = HashMap::new();
        let mut shadowed: Vec<ShadowedSkill> = Vec::new();

        for path in &self.base_paths {
            let scope = self.scope_for(path);
            self.load_dir_into(path, scope, &mut new_skills, &mut shadowed);
        }

        if let Ok(mut skills_lock) = self.skills.write() {
            *skills_lock = new_skills;
            tracing::info!(
                skill_count = skills_lock.len(),
                shadowed = shadowed.len(),
                "SkillManager loaded skills"
            );
            for entry in &shadowed {
                tracing::warn!(
                    id = %entry.id,
                    winner = ?entry.winner,
                    loser = ?entry.loser,
                    "skill shadowed by a higher-precedence scope"
                );
            }
            if let Ok(mut shadow_lock) = self.shadowed.write() {
                *shadow_lock = shadowed;
            }
        } else {
            tracing::error!("SkillManager lock poisoned while loading skills");
        }
    }

    /// Skills shadowed by a narrower scope, for display in the TUI.
    pub fn shadowed(&self) -> Vec<ShadowedSkill> {
        self.shadowed.read().map(|s| s.clone()).unwrap_or_default()
    }

    /// Classify a search path. A path under a known harness home directory
    /// belongs to that harness; anything else is shared Llama-R.
    fn scope_for(&self, path: &Path) -> SkillScope {
        let is_harness = HARNESS_DIR_NAMES.iter().any(|dir| {
            dirs::home_dir()
                .map(|home| path.starts_with(home.join(dir)))
                .unwrap_or(false)
        });
        if is_harness {
            SkillScope::Harness
        } else {
            SkillScope::LlamaR
        }
    }

    fn load_dir_into(
        &self,
        path: &Path,
        scope: SkillScope,
        skills: &mut HashMap<String, Skill>,
        shadowed: &mut Vec<ShadowedSkill>,
    ) {
        if !path.exists() || !path.is_dir() {
            return;
        }

        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let skill_path = entry.path();
                if skill_path.is_dir() {
                    if let Some(mut skill) = self.load_skill(&skill_path) {
                        skill.scope = scope;
                        insert_with_precedence(skill, skills, shadowed);
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
            // Overwritten by the caller, which knows the scope it was found in.
            scope: SkillScope::LlamaR,
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
                name = line
                    .replace("name:", "")
                    .trim()
                    .trim_matches('"')
                    .to_string();
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
                        .map(|value| {
                            value
                                .trim()
                                .trim_matches('"')
                                .trim_matches('\'')
                                .to_string()
                        })
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

    /// Persist newly observed shadowing so the TUI can surface it.
    fn record_shadowing(&self, entries: Vec<ShadowedSkill>) {
        if entries.is_empty() {
            return;
        }
        if let Ok(mut lock) = self.shadowed.write() {
            lock.extend(entries);
        }
    }

    /// Skills visible to a project: everything global, plus whatever the
    /// project itself carries. Project entries win.
    pub fn list_skills_for_project(&self, project_path: &Path) -> Vec<Skill> {
        let mut combined: HashMap<String, Skill> = self
            .skills
            .read()
            .map(|skills_lock| skills_lock.clone())
            .unwrap_or_default();
        let mut shadowed: Vec<ShadowedSkill> = Vec::new();

        for name in SKILL_DIR_NAMES {
            let local_path = project_path.join(name);
            self.load_dir_into(
                &local_path,
                SkillScope::Project,
                &mut combined,
                &mut shadowed,
            );
        }

        self.record_shadowing(shadowed);
        combined.into_values().collect()
    }

    /// Same precedence as [`Self::list_skills_for_project`], but for one id.
    pub fn resolve_for_project(&self, id: &str, project_path: &Path) -> Option<Skill> {
        for name in SKILL_DIR_NAMES {
            let skill_path = project_path.join(name).join(id);
            if skill_path.is_dir() {
                if let Some(mut skill) = self.load_skill(&skill_path) {
                    skill.scope = SkillScope::Project;
                    // A project copy hiding a global one is worth reporting too.
                    if let Some(global) = self.get_skill(id) {
                        if global.scope < SkillScope::Project {
                            self.record_shadowing(vec![ShadowedSkill {
                                id: id.to_string(),
                                winner: SkillScope::Project,
                                loser: global.scope,
                            }]);
                        }
                    }
                    return Some(skill);
                }
            }
        }

        self.get_skill(id)
    }

    pub fn get_skill(&self, id: &str) -> Option<Skill> {
        self.skills
            .read()
            .ok()
            .and_then(|skills_lock| skills_lock.get(id).cloned())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, dir: &str, id: &str, description: &str) {
        let skill_dir = root.join(dir).join(id);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "---\nname: {id}\ndescription: {description}\ntags: [test]\n---\n\nBody for {id}.\n"
            ),
        )
        .unwrap();
    }

    /// A manager whose global scan points at `<root>/skills`, so tests do not read
    /// the developer's real ~/.claude/skills.
    fn manager_with_root(root: &Path) -> SkillManager {
        let mut manager = SkillManager::new();
        manager.base_paths = vec![root.join("skills")];
        manager.scan_and_load();
        manager
    }

    #[test]
    fn project_skills_are_scoped_and_discovered() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo");
        fs::create_dir_all(&project).unwrap();
        // A repo carrying its own harness dir must be visible.
        write_skill(
            &project,
            ".claude/skills",
            "repo-conventions",
            "Project rules",
        );
        write_skill(&project, "skills", "local-skill", "Local only");

        let manager = manager_with_root(tmp.path());
        let ids: Vec<String> = manager
            .list_skills_for_project(&project)
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert!(ids.contains(&"repo-conventions".to_string()), "{ids:?}");
        assert!(ids.contains(&"local-skill".to_string()), "{ids:?}");

        let skill = manager
            .resolve_for_project("repo-conventions", &project)
            .unwrap();
        assert_eq!(skill.scope, SkillScope::Project);
    }

    #[test]
    fn shadowing_is_recorded_not_silent() {
        // Same id in the shared scan and in the project: the project wins and the
        // loser is reported, so a user can find out their skill was replaced.
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        fs::create_dir_all(&shared).unwrap();
        write_skill(&shared, "skills", "dup", "Shared version");

        let project = tmp.path().join("repo");
        fs::create_dir_all(&project).unwrap();
        write_skill(&project, "skills", "dup", "Project version");

        let manager = manager_with_root(&shared);
        let resolved = manager.resolve_for_project("dup", &project).unwrap();
        assert_eq!(resolved.scope, SkillScope::Project);
        assert!(
            manager
                .shadowed()
                .iter()
                .any(|entry| entry.id == "dup" && entry.loser == SkillScope::LlamaR),
            "the shadowed skill must be reported, got {:?}",
            manager.shadowed()
        );
    }

    #[test]
    fn project_scope_wins_over_shared() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        fs::create_dir_all(&shared).unwrap();
        write_skill(&shared, "skills", "shared-skill", "Shared version");

        let project = tmp.path().join("repo");
        fs::create_dir_all(&project).unwrap();
        write_skill(&project, "skills", "shared-skill", "Project version");

        let manager = manager_with_root(&shared);
        let resolved = manager
            .resolve_for_project("shared-skill", &project)
            .unwrap();
        assert_eq!(
            resolved.scope,
            SkillScope::Project,
            "the project's own copy must win"
        );
        assert!(resolved.content.contains("Project version"));
    }

    #[test]
    fn shared_scope_wins_over_harness() {
        let mut skills: HashMap<String, Skill> = HashMap::new();
        let mut shadowed = Vec::new();
        let skill = |scope: SkillScope, marker: &str| Skill {
            id: "pdf".into(),
            path: format!("/tmp/{marker}"),
            metadata: SkillMetadata {
                name: "pdf".into(),
                description: marker.into(),
                tags: None,
            },
            content: marker.into(),
            scope,
        };

        // Harness first, then LlamaR: the narrower scope replaces it.
        insert_with_precedence(
            skill(SkillScope::Harness, "harness"),
            &mut skills,
            &mut shadowed,
        );
        insert_with_precedence(
            skill(SkillScope::LlamaR, "llamar"),
            &mut skills,
            &mut shadowed,
        );
        assert_eq!(skills["pdf"].content, "llamar");

        // Harness again: must not clobber LlamaR, and the swap is recorded.
        insert_with_precedence(
            skill(SkillScope::Harness, "harness2"),
            &mut skills,
            &mut shadowed,
        );
        assert_eq!(skills["pdf"].content, "llamar");
        assert_eq!(shadowed.len(), 2);
        assert_eq!(shadowed[1].winner, SkillScope::LlamaR);
        assert_eq!(shadowed[1].loser, SkillScope::Harness);
    }

    #[test]
    fn harness_scoped_paths_are_tagged_harness() {
        // A path under a harness home dir is Harness; anything else is shared.
        let manager = SkillManager::new();
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                manager.scope_for(&home.join(".claude/skills")),
                SkillScope::Harness
            );
        }
        assert_eq!(
            manager.scope_for(Path::new("/tmp/whatever")),
            SkillScope::LlamaR
        );
    }
}
