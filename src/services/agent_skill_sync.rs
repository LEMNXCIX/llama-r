use crate::api::handlers::AppState;
use crate::context::store::ProjectContext;
use crate::domain::agent::AgentConfig;
use crate::domain::models::{ChatMessage, ChatRequest, Skill};
use crate::error::AppError;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct AgentSkillSyncReport {
    pub updated_agents: Vec<AgentSkillUpdate>,
}

#[derive(Debug, Clone)]
pub struct AgentSkillUpdate {
    pub agent_id: String,
    pub auto_skill_count: usize,
}

#[derive(Debug, Clone)]
struct ProjectProfile {
    language: String,
    has_backend_api: bool,
    has_frontend_ui: bool,
    is_library: bool,
}

pub async fn sync_project_agent_skills(
    state: &AppState,
    context: &ProjectContext,
) -> Result<AgentSkillSyncReport, AppError> {
    let agents_dir = crate::core::paths::get_project_agents_dir(&context.project_id);
    if !agents_dir.is_dir() {
        return Ok(AgentSkillSyncReport {
            updated_agents: Vec::new(),
        });
    }

    let project_path = Path::new(&context.path);
    let all_skills = state.skill_manager.list_skills_for_project(project_path);
    if all_skills.is_empty() {
        return Ok(AgentSkillSyncReport {
            updated_agents: Vec::new(),
        });
    }

    let profile = build_project_profile(project_path, &context.project_type);
    let compatible = compatible_skills(&profile, &all_skills);
    let mut updates = Vec::new();

    for entry in fs::read_dir(&agents_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }

        let content = fs::read_to_string(&path)?;
        let mut config: AgentConfig = toml::from_str(&content)?;
        let agent_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| AppError::Runtime(format!("Invalid agent file name: {}", path.display())))?
            .to_string();

        let candidates = candidate_skills_for_agent(&compatible, &agent_id, &config.name, &config.system_prompt);
        let selected = select_skills_for_agent(state, context, &profile, &agent_id, &config.name, &config.system_prompt, &candidates).await;

        if config.auto_skills != selected {
            config.auto_skills = selected.clone();
            let serialized = toml::to_string(&config).map_err(|err| AppError::Runtime(err.to_string()))?;
            fs::write(&path, serialized)?;
        }

        updates.push(AgentSkillUpdate {
            agent_id,
            auto_skill_count: selected.len(),
        });
    }

    if !updates.is_empty() {
        state.agent_manager.load_agents()?;
    }

    Ok(AgentSkillSyncReport {
        updated_agents: updates,
    })
}

pub fn summarize_sync_report(report: &AgentSkillSyncReport) -> serde_json::Value {
    serde_json::json!({
        "updated_agent_count": report.updated_agents.len(),
        "agents": report.updated_agents.iter().map(|update| {
            serde_json::json!({
                "agent_id": update.agent_id,
                "auto_skill_count": update.auto_skill_count
            })
        }).collect::<Vec<_>>()
    })
}

pub fn merge_skill_ids(manual_skills: &[String], auto_skills: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    manual_skills
        .iter()
        .chain(auto_skills.iter())
        .filter(|skill_id| seen.insert(skill_id.as_str()))
        .cloned()
        .collect()
}

fn build_project_profile(project_path: &Path, project_type: &str) -> ProjectProfile {
    let package_json = project_path.join("package.json");
    let cargo_toml = project_path.join("Cargo.toml");

    let language = match project_type {
        "rust" => "rust",
        "node" => "javascript",
        "python" => "python",
        "go" => "go",
        "java" => "java",
        _ => "generic",
    }
    .to_string();

    let has_backend_api = project_path.join("src/api").exists()
        || project_path.join("src/routes").exists()
        || project_path.join("api").exists()
        || project_path.join("proto").exists()
        || project_path.join("openapi.yaml").exists()
        || project_path.join("openapi.json").exists()
        || file_contains_any(&package_json, &["express", "fastify", "koa", "nestjs"]);

    let has_frontend_ui = project_path.join("src/components").exists()
        || project_path.join("src/ui").exists()
        || project_path.join("app").exists()
        || project_path.join("pages").exists()
        || project_path.join("index.html").exists()
        || file_contains_any(&package_json, &["react", "next", "vue", "svelte", "vite", "tailwind"]);

    let is_library = project_path.join("src/lib.rs").exists()
        || project_path.join("lib").exists()
        || file_contains_any(&cargo_toml, &["[lib]"]);

    ProjectProfile {
        language,
        has_backend_api,
        has_frontend_ui,
        is_library,
    }
}

fn file_contains_any(path: &Path, needles: &[&str]) -> bool {
    fs::read_to_string(path)
        .map(|content| {
            let lowered = content.to_lowercase();
            needles.iter().any(|needle| lowered.contains(needle))
        })
        .unwrap_or(false)
}

fn compatible_skills(profile: &ProjectProfile, skills: &[Skill]) -> Vec<Skill> {
    skills
        .iter()
        .filter(|skill| is_skill_compatible(profile, skill))
        .cloned()
        .collect()
}

fn is_skill_compatible(profile: &ProjectProfile, skill: &Skill) -> bool {
    let text = format!(
        "{} {} {}",
        skill.id.to_lowercase(),
        skill.metadata.name.to_lowercase(),
        skill.metadata.description.to_lowercase()
    );

    let language_match = match profile.language.as_str() {
        "rust" => !contains_any(&text, &["python", "django", "flask", "pandas", "swiftui", "flutter"]),
        "python" => !contains_any(&text, &["rust", "tokio", "cargo", "swiftui", "flutter"]),
        "javascript" => !contains_any(&text, &["rust async", "tokio", "cargo", "swiftui", "flutter"]),
        "go" => !contains_any(&text, &["tokio", "cargo", "swiftui", "flutter", "django"]),
        "java" => !contains_any(&text, &["tokio", "cargo", "swiftui", "flutter", "django"]),
        _ => true,
    };

    if !language_match {
        return false;
    }

    if contains_any(&text, &["api", "rest", "graphql"]) && !profile.has_backend_api {
        return false;
    }

    if contains_any(&text, &["frontend", "ui", "ux", "design", "web interface"]) && !profile.has_frontend_ui {
        return false;
    }

    if contains_any(&text, &["library", "crate"]) && !profile.is_library {
        return false;
    }

    true
}

fn candidate_skills_for_agent(skills: &[Skill], agent_id: &str, agent_name: &str, system_prompt: &str) -> Vec<Skill> {
    let purpose = format!(
        "{} {} {}",
        agent_id.to_lowercase(),
        agent_name.to_lowercase(),
        system_prompt.to_lowercase()
    );

    let mut scored = skills
        .iter()
        .map(|skill| {
            let text = format!(
                "{} {} {}",
                skill.id.to_lowercase(),
                skill.metadata.name.to_lowercase(),
                skill.metadata.description.to_lowercase()
            );
            let mut score = 0i32;

            if contains_any(&purpose, &["api", "backend", "route", "endpoint", "server"]) && contains_any(&text, &["api", "rest", "graphql"]) {
                score += 4;
            }
            if contains_any(&purpose, &["frontend", "ui", "ux", "design", "component"]) && contains_any(&text, &["frontend", "ui", "ux", "design"]) {
                score += 4;
            }
            if contains_any(&purpose, &["rust", "tokio", "async"]) && contains_any(&text, &["rust", "tokio", "async"]) {
                score += 4;
            }
            if contains_any(&purpose, &["review", "audit", "check"]) && contains_any(&text, &["guidelines", "best practices", "review"]) {
                score += 3;
            }
            if contains_any(&purpose, &["error", "failure", "resilience"]) && contains_any(&text, &["error", "resilien"]) {
                score += 3;
            }
            if contains_any(&text, &["best practices", "architecture", "testing"]) {
                score += 1;
            }

            (score, skill.clone())
        })
        .collect::<Vec<_>>();

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
    scored
        .into_iter()
        .filter(|(score, _)| *score > 0)
        .map(|(_, skill)| skill)
        .take(8)
        .collect()
}

async fn select_skills_for_agent(
    state: &AppState,
    context: &ProjectContext,
    profile: &ProjectProfile,
    agent_id: &str,
    agent_name: &str,
    system_prompt: &str,
    candidates: &[Skill],
) -> Vec<String> {
    if candidates.is_empty() {
        return Vec::new();
    }

    let fallback = candidates.iter().map(|skill| skill.id.clone()).take(5).collect::<Vec<_>>();
    let manifest = candidates
        .iter()
        .map(|skill| format!("- \"{}\": {}", skill.id, skill.metadata.description))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "You are selecting project-specific skills for one agent. Return ONLY a JSON array with up to 5 skill IDs.\n\nProject id: {}\nProject type: {}\nPrimary language: {}\nHas backend/api: {}\nHas frontend/ui: {}\nIs library: {}\nAgent id: {}\nAgent name: {}\nAgent purpose: {}\n\nProject context:\n{}\n\nCandidate skills:\n{}",
        context.project_id,
        context.project_type,
        profile.language,
        profile.has_backend_api,
        profile.has_frontend_ui,
        profile.is_library,
        agent_id,
        agent_name,
        system_prompt,
        if context.context_md.len() > 800 { &context.context_md[..800] } else { &context.context_md },
        manifest,
    );

    let response = state
        .provider
        .chat(ChatRequest {
            model: state.default_model.clone(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: prompt,
            }],
            stream: false,
        })
        .await;

    response
        .ok()
        .and_then(|response| extract_json_array(&response.message.content))
        .map(|ids| {
            let allowed = candidates.iter().map(|skill| skill.id.as_str()).collect::<HashSet<_>>();
            ids.into_iter()
                .filter(|id| allowed.contains(id.as_str()))
                .take(5)
                .collect::<Vec<_>>()
        })
        .filter(|ids| !ids.is_empty())
        .unwrap_or(fallback)
}

fn extract_json_array(response: &str) -> Option<Vec<String>> {
    let trimmed = response.trim();
    let json_str = if let (Some(start), Some(end)) = (trimmed.find('['), trimmed.rfind(']')) {
        &trimmed[start..=end]
    } else {
        trimmed
    };

    serde_json::from_str::<Vec<String>>(json_str).ok()
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::{build_project_profile, compatible_skills, merge_skill_ids};
    use crate::domain::models::{Skill, SkillMetadata};
    use tempfile::TempDir;

    fn skill(id: &str, description: &str) -> Skill {
        Skill {
            id: id.to_string(),
            path: id.to_string(),
            metadata: SkillMetadata {
                name: id.to_string(),
                description: description.to_string(),
                tags: None,
            },
            content: String::new(),
        }
    }

    #[test]
    fn merge_skill_ids_should_preserve_manual_and_deduplicate() {
        let merged = merge_skill_ids(
            &["manual-a".to_string(), "shared".to_string()],
            &["shared".to_string(), "auto-b".to_string()],
        );
        assert_eq!(merged, vec!["manual-a", "shared", "auto-b"]);
    }

    #[test]
    fn compatible_skills_should_exclude_mismatched_language() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join("Cargo.toml"), "[package]\nname='demo'\nversion='0.1.0'\n").unwrap();
        let profile = build_project_profile(temp_dir.path(), "rust");
        let filtered = compatible_skills(&profile, &[
            skill("rust-best-practices", "Rust best practices"),
            skill("python-data", "Python data workflows"),
        ]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "rust-best-practices");
    }

    #[test]
    fn compatible_skills_should_exclude_api_skills_for_frontend_only_projects() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::create_dir_all(temp_dir.path().join("src/components")).unwrap();
        std::fs::write(temp_dir.path().join("package.json"), r#"{"dependencies":{"react":"18.0.0"}}"#).unwrap();
        let profile = build_project_profile(temp_dir.path(), "node");
        let filtered = compatible_skills(&profile, &[
            skill("frontend-design", "Frontend UI work"),
            skill("api-design-principles", "API design guidance"),
        ]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "frontend-design");
    }

    #[test]
    fn compatible_skills_should_allow_api_skills_for_backend_projects() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::create_dir_all(temp_dir.path().join("src/api")).unwrap();
        std::fs::write(temp_dir.path().join("Cargo.toml"), "[package]\nname='demo'\nversion='0.1.0'\n").unwrap();
        let profile = build_project_profile(temp_dir.path(), "rust");
        let filtered = compatible_skills(&profile, &[
            skill("rust-best-practices", "Rust best practices"),
            skill("api-design-principles", "API design guidance"),
        ]);
        assert_eq!(filtered.len(), 2);
    }
}
