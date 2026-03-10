use crate::context::store::{ContextStore, ProjectContext};
use crate::domain::agent::Agent;
use crate::services::agent_skill_sync::merge_skill_ids;
use crate::services::skill_manager::SkillManager;
use crate::services::validation::canonicalize_project_path;
use std::fs;
use std::path::Path;
use std::sync::Arc;

fn detect_project_type(path: &Path) -> String {
    if path.join("Cargo.toml").exists() {
        "rust".to_string()
    } else if path.join("package.json").exists() {
        "node".to_string()
    } else if path.join("pyproject.toml").exists() || path.join("requirements.txt").exists() {
        "python".to_string()
    } else if path.join("go.mod").exists() {
        "go".to_string()
    } else if path.join("pom.xml").exists() {
        "java".to_string()
    } else {
        "generic".to_string()
    }
}

fn collect_project_files(path: &Path, project_type: &str) -> String {
    let mut parts = Vec::new();

    for readme in &["README.md", "README.txt", "readme.md"] {
        let readme_path = path.join(readme);
        if readme_path.exists() {
            if let Ok(content) = fs::read_to_string(&readme_path) {
                let truncated = if content.len() > 2000 {
                    &content[..2000]
                } else {
                    &content
                };
                parts.push(format!("## README\n{}", truncated));
                break;
            }
        }
    }

    match project_type {
        "rust" => {
            let cargo = path.join("Cargo.toml");
            if let Ok(content) = fs::read_to_string(cargo) {
                let truncated = if content.len() > 1500 { &content[..1500] } else { &content };
                parts.push(format!("## Cargo.toml\n```toml\n{}\n```", truncated));
            }
            for entry in &["src/main.rs", "src/lib.rs"] {
                let entry_path = path.join(entry);
                if entry_path.exists() {
                    if let Ok(content) = fs::read_to_string(&entry_path) {
                        let truncated = if content.len() > 1000 { &content[..1000] } else { &content };
                        parts.push(format!("## {} (partial)\n```rust\n{}\n```", entry, truncated));
                        break;
                    }
                }
            }
        }
        "node" => {
            let pkg = path.join("package.json");
            if let Ok(content) = fs::read_to_string(pkg) {
                let truncated = if content.len() > 1500 { &content[..1500] } else { &content };
                parts.push(format!("## package.json\n```json\n{}\n```", truncated));
            }
        }
        "python" => {
            for file_name in &["pyproject.toml", "requirements.txt"] {
                let file_path = path.join(file_name);
                if file_path.exists() {
                    if let Ok(content) = fs::read_to_string(&file_path) {
                        let truncated = if content.len() > 1500 { &content[..1500] } else { &content };
                        parts.push(format!("## {}\n```\n{}\n```", file_name, truncated));
                        break;
                    }
                }
            }
        }
        _ => {}
    }

    parts.join("\n\n")
}

fn build_analysis_prompt(
    project_name: &str,
    project_type: &str,
    file_content: &str,
    skills_info: &str,
) -> String {
    format!(
        r#"Analyze this software project and generate a concise developer context document.

# Project: {project_name}
# Type: {project_type}

## Project Files
{file_content}

{skills_info}

Generate a structured developer context with EXACTLY these sections in markdown:
1. **Project Overview** (2-3 sentences)
2. **Architecture** (key modules, patterns used)  
3. **Tech Stack** (main dependencies/frameworks)
4. **Development Rules** (coding style, patterns to follow)
5. **Key Conventions** (naming, error handling, etc.)

Be CONCISE. Max 500 words total. Focus on what a developer agent needs to know to contribute effectively."#
    )
}

pub struct ProjectAnalyzer {
    skill_manager: Arc<SkillManager>,
}

impl ProjectAnalyzer {
    pub fn new(skill_manager: Arc<SkillManager>) -> Self {
        Self { skill_manager }
    }

    pub async fn analyze(
        &self,
        project_id: &str,
        project_path: &str,
        llm_responder: impl Fn(
            String,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, String>> + Send>,
        >,
    ) -> Result<ProjectContext, String> {
        let canonical_path = canonicalize_project_path(project_path)
            .map_err(|err| err.to_string())?;
        let project_type = detect_project_type(&canonical_path);
        let file_content = collect_project_files(&canonical_path, &project_type);

        let analysis_prompt = build_analysis_prompt(project_id, &project_type, &file_content, "");
        tracing::info!(project_id = %project_id, path = %canonical_path.display(), "Phase 1/2: generating project context");

        let context_md = llm_responder(analysis_prompt)
            .await
            .map_err(|err| format!("LLM context generation failed: {}", err))?;

        let all_skills = self.skill_manager.list_skills();
        let skills_injected = if all_skills.is_empty() {
            vec![]
        } else {
            tracing::info!(project_id = %project_id, available_skills = all_skills.len(), "Phase 2/2: selecting relevant skills");
            let skills_manifest = all_skills
                .iter()
                .map(|skill| format!("- \"{}\": {}", skill.id, skill.metadata.description))
                .collect::<Vec<_>>()
                .join("\n");

            let skill_selection_prompt = format!(
                r#"You are a developer tool. Based on this project context:

## Project: {project_id} ({project_type})
{context_snippet}

## Available Skills
{skills_manifest}

TASK: Select ONLY the skills that are directly relevant to this specific project.
Respond with a JSON array of skill IDs. Example: ["rust-best-practices", "api-design-principles"]
Return ONLY the JSON array, nothing else. If no skills are relevant, return []."#,
                context_snippet = if context_md.len() > 800 {
                    &context_md[..800]
                } else {
                    &context_md
                },
            );

            match llm_responder(skill_selection_prompt).await {
                Ok(response) => {
                    let trimmed = response.trim();
                    let json_str = if let (Some(start), Some(end)) = (trimmed.find('['), trimmed.rfind(']')) {
                        &trimmed[start..=end]
                    } else {
                        "[]"
                    };

                    match serde_json::from_str::<Vec<String>>(json_str) {
                        Ok(selected_ids) => {
                            let all_skill_ids: std::collections::HashSet<_> =
                                all_skills.iter().map(|skill| skill.id.as_str()).collect();
                            selected_ids
                                .into_iter()
                                .filter(|id| all_skill_ids.contains(id.as_str()))
                                .collect()
                        }
                        Err(err) => {
                            tracing::warn!(project_id = %project_id, error = %err, "Could not parse skill selection response");
                            vec![]
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(project_id = %project_id, error = %err, "Skill selection failed; continuing without injected skills");
                    vec![]
                }
            }
        };

        Ok(ProjectContext {
            project_id: project_id.to_string(),
            path: canonical_path.to_string_lossy().into_owned(),
            context_md,
            project_type,
            skills_injected,
            last_analyzed: chrono::Utc::now(),
            custom_rules: String::new(),
        })
    }
}

pub struct ContextEnricher {
    context_store: Arc<ContextStore>,
    skill_manager: Arc<SkillManager>,
    default_model: String,
}

impl ContextEnricher {
    pub fn new(
        context_store: Arc<ContextStore>,
        skill_manager: Arc<SkillManager>,
        default_model: String,
    ) -> Self {
        Self {
            context_store,
            skill_manager,
            default_model,
        }
    }

    pub fn resolve_model(&self, agent: &Agent) -> String {
        if agent.config.model.is_empty() {
            self.default_model.clone()
        } else {
            agent.config.model.clone()
        }
    }

    pub fn build_system_prompt(&self, agent: &Agent) -> String {
        let mut parts = Vec::new();

        let mut sys_prompt = agent.config.system_prompt.clone();
        for (key, value) in &agent.config.variables {
            sys_prompt = sys_prompt.replace(&format!("{{{{{}}}}}", key), value);
        }
        parts.push(sys_prompt);

        if !agent.config.rules.is_empty() {
            let rendered_rules = agent
                .config
                .rules
                .iter()
                .map(|rule| format!("- {}", rule))
                .collect::<Vec<_>>()
                .join("\n");
            parts.push(format!("\n## Agent Rules\n{}", rendered_rules));
        }

        if let Some(project_id) = &agent.config.context_project {
            let ctx_md = self.context_store.get_context_md(project_id);
            if !ctx_md.is_empty() {
                parts.push(format!("\n## Project Context: {}\n{}", project_id, ctx_md));
            }
        }

        for file_path in &agent.config.context_files {
            if let Ok(content) = std::fs::read_to_string(file_path) {
                parts.push(format!("\n## Context: {}\n{}", file_path, content));
            }
        }

        let selected_skill_ids = merge_skill_ids(&agent.config.skills, &agent.config.auto_skills);
        if !selected_skill_ids.is_empty() {
            let project_path = agent
                .config
                .context_project
                .as_ref()
                .and_then(|project_id| self.context_store.get_context(project_id))
                .map(|context| context.path);

            let selected_skills = selected_skill_ids
                .iter()
                .filter_map(|skill_id| {
                    if let Some(project_path) = &project_path {
                        self.skill_manager.get_skill_for_project(skill_id, Path::new(project_path))
                    } else {
                        self.skill_manager.get_skill(skill_id)
                    }
                    .map(|skill| format!("### {}\nPath: {}\n\n{}", skill.id, skill.path, skill.content))
                })
                .collect::<Vec<_>>();

            if !selected_skills.is_empty() {
                parts.push(format!(
                    "\n## Agent Skills\nUsa estas skills especificamente para este agente:\n\n{}",
                    selected_skills.join("\n\n")
                ));
            }
        }

        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::ContextEnricher;
    use crate::context::store::{ContextStore, ProjectContext};
    use crate::domain::agent::{Agent, AgentConfig, OptimizeConfig};
    use crate::services::skill_manager::SkillManager;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap()
    }

    #[test]
    fn build_system_prompt_should_include_agent_rules_and_selected_skills_only() {
        let _guard = lock_env();
        let temp_dir = tempfile::tempdir().unwrap();
        let previous_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp_dir.path()).unwrap();
        std::env::set_var("LLAMA_R_DIR", temp_dir.path());

        let skill_dir = temp_dir.path().join("skills").join("reviewer-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: reviewer-skill\ndescription: Helps review code\n---\nUse this skill for targeted code review.",
        )
        .unwrap();

        let unused_skill_dir = temp_dir.path().join("skills").join("unused-skill");
        std::fs::create_dir_all(&unused_skill_dir).unwrap();
        std::fs::write(
            unused_skill_dir.join("SKILL.md"),
            "---\nname: unused-skill\ndescription: Should not be injected\n---\nThis must stay out of the prompt.",
        )
        .unwrap();

        let skill_manager = Arc::new(SkillManager::new());
        skill_manager.scan_and_load();

        let context_store = Arc::new(ContextStore::new());
        context_store
            .save_context(ProjectContext {
                project_id: "demo".to_string(),
                path: temp_dir.path().display().to_string(),
                context_md: "project context body".to_string(),
                project_type: "rust".to_string(),
                skills_injected: vec![],
                last_analyzed: chrono::Utc::now(),
                custom_rules: String::new(),
            })
            .unwrap();

        let enricher = ContextEnricher::new(context_store, skill_manager, "fallback-model".to_string());
        let agent = Agent {
            id: "reviewer".to_string(),
            project_id: Some("demo".to_string()),
            config: AgentConfig {
                name: "Reviewer".to_string(),
                model: String::new(),
                system_prompt: "Base prompt".to_string(),
                context_project: Some("demo".to_string()),
                context_files: vec![],
                rules: vec!["Always explain the risk".to_string()],
                skills: vec!["reviewer-skill".to_string()],
                auto_skills: vec!["reviewer-skill".to_string()],
                variables: HashMap::new(),
                optimize: OptimizeConfig::default(),
            },
        };

        let prompt = enricher.build_system_prompt(&agent);

        assert!(prompt.contains("## Agent Rules"));
        assert!(prompt.contains("Always explain the risk"));
        assert!(prompt.contains("## Agent Skills"));
        assert!(prompt.contains("reviewer-skill"));
        assert!(prompt.contains("Use this skill for targeted code review."));
        assert!(prompt.contains("project context body"));
        assert!(!prompt.contains("unused-skill"));
        assert!(!prompt.contains("This must stay out of the prompt."));

        std::env::set_current_dir(previous_dir).unwrap();
    }
}
