use crate::config::ProviderKind;
use crate::context::store::{ContextStore, ProjectContext};
use crate::domain::agent::Agent;
use crate::ports::rag::EmbeddingProvider;
use crate::providers::capabilities::capabilities_for;
use crate::providers::registry::ProviderRegistry;
use crate::providers::resolver::{resolve_for, ResolvedModel};
use crate::services::agent_skill_sync::merge_skill_ids;
use crate::services::skill_index::{apply_ranking, SkillIndex};
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
                let truncated = if content.len() > 1500 {
                    &content[..1500]
                } else {
                    &content
                };
                parts.push(format!("## Cargo.toml\n```toml\n{}\n```", truncated));
            }
            for entry in &["src/main.rs", "src/lib.rs"] {
                let entry_path = path.join(entry);
                if entry_path.exists() {
                    if let Ok(content) = fs::read_to_string(&entry_path) {
                        let truncated = if content.len() > 1000 {
                            &content[..1000]
                        } else {
                            &content
                        };
                        parts.push(format!(
                            "## {} (partial)\n```rust\n{}\n```",
                            entry, truncated
                        ));
                        break;
                    }
                }
            }
        }
        "node" => {
            let pkg = path.join("package.json");
            if let Ok(content) = fs::read_to_string(pkg) {
                let truncated = if content.len() > 1500 {
                    &content[..1500]
                } else {
                    &content
                };
                parts.push(format!("## package.json\n```json\n{}\n```", truncated));
            }
        }
        "python" => {
            for file_name in &["pyproject.toml", "requirements.txt"] {
                let file_path = path.join(file_name);
                if file_path.exists() {
                    if let Ok(content) = fs::read_to_string(&file_path) {
                        let truncated = if content.len() > 1500 {
                            &content[..1500]
                        } else {
                            &content
                        };
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
        let canonical_path =
            canonicalize_project_path(project_path).map_err(|err| err.to_string())?;
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
                    let json_str =
                        if let (Some(start), Some(end)) = (trimmed.find('['), trimmed.rfind(']')) {
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
    /// Profiles from `providers.toml`, used to resolve an agent's provider
    /// name to a kind. Held as `Arc` so a swap can rebuild an enricher without
    /// cloning the whole registry.
    provider_registry: Arc<ProviderRegistry>,
    /// The process-wide `PROVIDER`, used when an agent does not name one.
    global_provider: ProviderKind,
    /// Embeddings for ranking skills semantically. `None` disables ranking, in
    /// which case every declared skill is used — the previous behaviour.
    embeddings: Option<Arc<dyn EmbeddingProvider>>,
}

impl ContextEnricher {
    pub fn new(
        context_store: Arc<ContextStore>,
        skill_manager: Arc<SkillManager>,
        default_model: String,
        provider_registry: Arc<ProviderRegistry>,
        global_provider: ProviderKind,
    ) -> Self {
        Self {
            context_store,
            skill_manager,
            default_model,
            provider_registry,
            global_provider,
            embeddings: None,
        }
    }

    /// Enable semantic skill ranking.
    pub fn with_embeddings(mut self, embeddings: Arc<dyn EmbeddingProvider>) -> Self {
        self.embeddings = Some(embeddings);
        self
    }

    /// Resolve the agent's provider and model to the full result, including
    /// the capabilities of the pair. An invalid provider in a manifest is a
    /// configuration error; the chat path degrades to the global provider with
    /// a warning rather than failing the request outright.
    pub fn resolve(&self, agent: &Agent) -> ResolvedModel {
        match resolve_for(
            &self.provider_registry,
            &agent.config,
            self.global_provider,
            &self.default_model,
        ) {
            Ok(resolved) => resolved,
            Err(err) => {
                tracing::warn!(error = %err, "invalid agent provider; using the global provider");
                let model = if agent.config.model.trim().is_empty() {
                    self.default_model.clone()
                } else {
                    agent.config.model.clone()
                };
                ResolvedModel {
                    capabilities: capabilities_for(self.global_provider, &model),
                    provider: self.global_provider,
                    model,
                }
            }
        }
    }

    /// Kept for callers that only need the model id. Delegates to [`Self::resolve`].
    pub fn resolve_model(&self, agent: &Agent) -> String {
        self.resolve(agent).model
    }

    pub async fn build_system_prompt(&self, agent: &Agent) -> String {
        let budget = agent.config.max_context_tokens;
        let mut sections = Vec::new();

        // 1. Base System Prompt (Highest priority)
        let mut sys_prompt = agent.config.system_prompt.clone();
        for (key, value) in &agent.config.variables {
            sys_prompt = sys_prompt.replace(&format!("{{{{{}}}}}", key), value);
        }
        sections.push((sys_prompt, 100)); // (content, priority)

        // 2. Agent Rules
        if !agent.config.rules.is_empty() {
            let rendered_rules = agent
                .config
                .rules
                .iter()
                .map(|rule| format!("- {}", rule))
                .collect::<Vec<_>>()
                .join("\n");
            sections.push((format!("\n## Agent Rules\n{}", rendered_rules), 90));
        }

        // 3. Project Context
        if let Some(project_id) = &agent.config.context_project {
            let ctx_md = self.context_store.get_context_md(project_id);
            if !ctx_md.is_empty() {
                sections.push((
                    format!("\n## Project Context: {}\n{}", project_id, ctx_md),
                    70,
                ));
            }
        }

        // 4. Agent Skills
        let candidate_skill_ids = merge_skill_ids(&agent.config.skills, &agent.config.auto_skills);
        if !candidate_skill_ids.is_empty() {
            let project_path = agent
                .config
                .context_project
                .as_ref()
                .and_then(|project_id| self.context_store.get_context(project_id))
                .map(|context| context.path);

            // Ranking narrows the candidates; without embeddings (or on an
            // embedding failure) every candidate is used, so retrieval can never
            // cost an agent its declared skills.
            let kept_ids: Vec<String> = match self
                .rank_skills(
                    agent,
                    &candidate_skill_ids,
                    agent.config.skills.as_slice(),
                    project_path.as_deref(),
                )
                .await
            {
                Ok(kept) => kept,
                Err(err) => {
                    tracing::warn!(error = %err, "skill ranking failed; using all candidates");
                    candidate_skill_ids.clone()
                }
            };

            // Partition the kept skills after ranking: a skill listed in
            // `deferred_skills` is announced by name + description only and its
            // body stays out of the system prompt (the model loads it with the
            // `llama_r__skill` tool). Everything else keeps the full injection,
            // byte-for-byte as before.
            let deferred_ids: std::collections::HashSet<&str> = agent
                .config
                .deferred_skills
                .iter()
                .map(String::as_str)
                .collect();

            // An id in `deferred_skills` that was never declared cannot be
            // announced. Warn and ignore it rather than failing the request.
            for id in &agent.config.deferred_skills {
                if !candidate_skill_ids.iter().any(|candidate| candidate == id) {
                    tracing::warn!(
                        skill = %id,
                        "deferred skill is not declared by the agent; ignoring"
                    );
                }
            }

            let mut full_bodies = Vec::new();
            let mut announcements = Vec::new();
            for skill_id in &kept_ids {
                let skill = if let Some(project_path) = &project_path {
                    self.skill_manager
                        .resolve_for_project(skill_id, Path::new(project_path))
                } else {
                    self.skill_manager.get_skill(skill_id)
                };
                let Some(skill) = skill else { continue };

                if deferred_ids.contains(skill_id.as_str()) {
                    // The tool resolves by skill id, so announce the id as the
                    // "name" the model must pass to llama_r__skill.
                    announcements.push(format!("- {} — {}", skill.id, skill.metadata.description));
                } else {
                    full_bodies.push(format!(
                        "### {}\nPath: {}\n\n{}",
                        skill.id, skill.path, skill.content
                    ));
                }
            }

            let mut rendered = full_bodies.join("\n\n");
            if !announcements.is_empty() {
                if !rendered.is_empty() {
                    rendered.push_str("\n\n");
                }
                rendered.push_str(
                    "Las siguientes skills están disponibles bajo demanda; para cargar sus instrucciones completas, llama a llama_r__skill con el nombre de la skill (por ejemplo, llama_r__skill({name: \"<nombre>\"})):\n",
                );
                rendered.push_str(&announcements.join("\n"));
            }

            if !rendered.is_empty() {
                sections.push((
                    format!(
                        "\n## Agent Skills\nUsa estas skills especificamente para este agente:\n\n{}",
                        rendered
                    ),
                    60,
                ));
            }
            // 5. Context Files (Lowest priority)
            for file_path in &agent.config.context_files {
                if let Ok(content) = std::fs::read_to_string(file_path) {
                    sections.push((format!("\n## Context: {}\n{}", file_path, content), 50));
                }
            }
        }

        // Build and prune
        let mut final_prompt = String::new();
        let mut current_tokens = 0;

        // Sort sections by priority (highest first)
        sections.sort_by_key(|section| std::cmp::Reverse(section.1));

        for (content, _priority) in sections {
            let content_tokens = content.len() / 4; // Rough heuristic
            if current_tokens + content_tokens <= budget || current_tokens == 0 {
                final_prompt.push_str(&content);
                current_tokens += content_tokens;
            } else {
                tracing::warn!(
                    agent_id = %agent.id,
                    budget = budget,
                    "Pruning prompt section due to token budget"
                );
            }
        }

        final_prompt
    }

    /// Narrow candidate skills to the ones relevant to this agent and project.
    ///
    /// Returns the kept ids. Candidates that ranking dropped are logged with their
    /// score rather than dropped silently.
    async fn rank_skills(
        &self,
        agent: &Agent,
        candidate_ids: &[String],
        manual_ids: &[String],
        project_path: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let Some(embeddings) = &self.embeddings else {
            return Ok(candidate_ids.to_vec());
        };

        let catalog = match project_path {
            Some(path) => self.skill_manager.list_skills_for_project(Path::new(path)),
            None => self.skill_manager.list_skills(),
        };
        let catalog: Vec<crate::domain::models::Skill> = catalog
            .into_iter()
            .filter(|skill| candidate_ids.iter().any(|id| id == &skill.id))
            .collect();
        if catalog.is_empty() {
            return Ok(Vec::new());
        }

        let mut index = SkillIndex::new(embeddings.clone());
        index.build(&catalog, manual_ids).await?;

        // Query text: what this agent is for, plus what the project is.
        let profile = agent
            .config
            .context_project
            .as_deref()
            .map(|id| self.context_store.get_context_md(id))
            .unwrap_or_default();
        let query_text = format!("{}\n{}", agent.config.system_prompt, profile);
        let query_text: String = query_text.chars().take(2000).collect();

        // Ask for every candidate so ranking decides, not the cut.
        let ranked = index.query(&query_text, candidate_ids.len().max(1)).await?;
        let selection = apply_ranking(candidate_ids, &ranked, candidate_ids.len());

        for dropped in &selection.dropped {
            let score = ranked
                .iter()
                .find(|m| m.id == dropped.id)
                .map(|m| m.score)
                .unwrap_or(f32::NAN);
            tracing::info!(
                skill = %dropped.id,
                score = %score,
                "skill dropped by relevance ranking"
            );
        }
        Ok(selection.selected)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::await_holding_lock)] // tests hold an env lock across .await to serialise access
    use super::ContextEnricher;
    use crate::config::ProviderKind;
    use crate::context::store::{ContextStore, ProjectContext};
    use crate::domain::agent::{Agent, AgentConfig, OptimizeConfig};
    use crate::ports::rag::EmbeddingProvider;
    use crate::providers::registry::ProviderRegistry;
    use crate::services::skill_manager::SkillManager;
    use std::sync::{Arc, Mutex, MutexGuard};
    use tracing_subscriber::prelude::*;

    fn lock_env() -> MutexGuard<'static, ()> {
        crate::core::paths::lock_env_for_tests()
    }

    /// Restore the process-wide environment touched by these tests.
    struct EnvRestore {
        dir: std::path::PathBuf,
        llama_dir: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn capture() -> Self {
            Self {
                dir: std::env::current_dir().unwrap(),
                llama_dir: std::env::var_os("LLAMA_R_DIR"),
            }
        }

        fn enter(&self, temp_dir: &std::path::Path) {
            std::env::set_current_dir(temp_dir).unwrap();
            std::env::set_var("LLAMA_R_DIR", temp_dir);
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.dir).unwrap();
            match &self.llama_dir {
                Some(value) => std::env::set_var("LLAMA_R_DIR", value),
                None => std::env::remove_var("LLAMA_R_DIR"),
            }
        }
    }

    /// Build an enricher whose skill manager sees only `temp_dir/skills`,
    /// never the developer's real home-directory skills.
    fn enricher_with_skills(
        temp_dir: &std::path::Path,
        specs: &[(&str, &str, &str)],
    ) -> ContextEnricher {
        let skills_root = temp_dir.join("skills");
        for (id, description, body) in specs {
            let dir = skills_root.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {id}\ndescription: {description}\n---\n{body}"),
            )
            .unwrap();
        }
        let skill_manager = Arc::new(SkillManager::with_paths(vec![skills_root]));
        skill_manager.scan_and_load();
        ContextEnricher::new(
            Arc::new(ContextStore::new()),
            skill_manager,
            "fallback-model".to_string(),
            Arc::new(ProviderRegistry::default()),
            ProviderKind::Ollama,
        )
    }

    /// An embedder that gives a clean relevance signal to the two test skills:
    /// the query ("alpha is the priority") matches `alpha-skill`, not `beta-skill`.
    struct KeywordEmbedder;

    #[async_trait::async_trait]
    impl EmbeddingProvider for KeywordEmbedder {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts
                .iter()
                .map(|text| {
                    let lower = text.to_ascii_lowercase();
                    if lower.contains("alpha") {
                        vec![1.0, 0.0]
                    } else if lower.contains("beta") {
                        vec![0.0, 1.0]
                    } else {
                        vec![0.5, 0.5]
                    }
                })
                .collect())
        }

        fn dimensions(&self) -> usize {
            2
        }
    }

    /// Capture WARN events emitted while building a prompt.
    #[derive(Clone, Default)]
    struct WarnBuffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for WarnBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for WarnBuffer {
        type Writer = WarnBuffer;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[tokio::test]
    async fn build_system_prompt_should_include_agent_rules_and_selected_skills_only() {
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

        let enricher = ContextEnricher::new(
            context_store,
            skill_manager,
            "fallback-model".to_string(),
            Arc::new(ProviderRegistry::default()),
            ProviderKind::Ollama,
        );
        let agent = Agent {
            id: "reviewer".to_string(),
            project_id: Some("demo".to_string()),
            config: AgentConfig {
                name: "Reviewer".to_string(),
                model: String::new(),
                system_prompt: "Base prompt".to_string(),
                context_project: Some("demo".to_string()),
                rules: vec!["Always explain the risk".to_string()],
                skills: vec!["reviewer-skill".to_string()],
                auto_skills: vec!["reviewer-skill".to_string()],
                optimize: OptimizeConfig::default(),
                ..Default::default()
            },
        };

        let prompt = enricher.build_system_prompt(&agent).await;

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

    #[tokio::test]
    async fn a_deferred_skill_announces_name_and_description_only() {
        let _guard = lock_env();
        let temp_dir = tempfile::tempdir().unwrap();
        let env = EnvRestore::capture();
        env.enter(temp_dir.path());

        let enricher = enricher_with_skills(
            temp_dir.path(),
            &[(
                "deferred-skill",
                "Provides deferred guidance",
                "DEFERRED BODY SECRET",
            )],
        );
        let agent = Agent {
            id: "reviewer".to_string(),
            project_id: None,
            config: AgentConfig {
                name: "Reviewer".to_string(),
                model: String::new(),
                system_prompt: "Base prompt".to_string(),
                skills: vec!["deferred-skill".to_string()],
                deferred_skills: vec!["deferred-skill".to_string()],
                ..Default::default()
            },
        };

        let prompt = enricher.build_system_prompt(&agent).await;

        assert!(prompt.contains("## Agent Skills"));
        assert!(
            prompt.contains("deferred-skill"),
            "the announced name must be present: {prompt}"
        );
        assert!(
            prompt.contains("Provides deferred guidance"),
            "the description must be announced: {prompt}"
        );
        assert!(
            prompt.contains("llama_r__skill"),
            "the model must be told how to load the body: {prompt}"
        );
        assert!(
            !prompt.contains("DEFERRED BODY SECRET"),
            "a deferred body must not be in the system prompt: {prompt}"
        );
        assert!(
            !prompt.contains("Path:"),
            "a deferred skill must not be rendered as a full entry: {prompt}"
        );
    }

    #[tokio::test]
    async fn a_non_deferred_skill_still_injects_its_body() {
        let _guard = lock_env();
        let temp_dir = tempfile::tempdir().unwrap();
        let env = EnvRestore::capture();
        env.enter(temp_dir.path());

        let enricher = enricher_with_skills(
            temp_dir.path(),
            &[("reviewer-skill", "Helps review code", "Full reviewer body.")],
        );
        let agent = Agent {
            id: "reviewer".to_string(),
            project_id: None,
            config: AgentConfig {
                name: "Reviewer".to_string(),
                model: String::new(),
                system_prompt: "Base prompt".to_string(),
                skills: vec!["reviewer-skill".to_string()],
                // No deferred_skills: baseline behavior must be byte-for-byte.
                ..Default::default()
            },
        };

        let prompt = enricher.build_system_prompt(&agent).await;

        let skill_md =
            "---\nname: reviewer-skill\ndescription: Helps review code\n---\nFull reviewer body.";
        let expected = format!(
            "\n## Agent Skills\nUsa estas skills especificamente para este agente:\n\n### reviewer-skill\nPath: {}\n\n{}",
            temp_dir.path().join("skills").join("reviewer-skill").display(),
            skill_md
        );
        assert!(
            prompt.contains(&expected),
            "non-deferred skills must keep the baseline rendering: {prompt}"
        );
        assert!(
            !prompt.contains("llama_r__skill"),
            "no deferred skills means no on-demand instruction: {prompt}"
        );
    }

    #[tokio::test]
    async fn an_undeclared_deferred_id_is_ignored_with_a_warning() {
        let _guard = lock_env();
        let temp_dir = tempfile::tempdir().unwrap();
        let env = EnvRestore::capture();
        env.enter(temp_dir.path());

        let enricher = enricher_with_skills(
            temp_dir.path(),
            &[("reviewer-skill", "Helps review code", "Reviewer body.")],
        );
        let agent = Agent {
            id: "reviewer".to_string(),
            project_id: None,
            config: AgentConfig {
                name: "Reviewer".to_string(),
                model: String::new(),
                system_prompt: "Base prompt".to_string(),
                skills: vec!["reviewer-skill".to_string()],
                deferred_skills: vec!["ghost-skill".to_string()],
                ..Default::default()
            },
        };

        let captured = WarnBuffer::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(captured.clone())
                .with_filter(tracing_subscriber::filter::LevelFilter::WARN),
        );
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let prompt = enricher.build_system_prompt(&agent).await;
        drop(_subscriber_guard);

        let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(
            logs.contains("ghost-skill"),
            "the warning must name the undeclared id: {logs}"
        );
        assert!(
            !prompt.contains("ghost-skill"),
            "the undeclared id must not leak into the prompt: {prompt}"
        );
        assert!(
            prompt.contains("Reviewer body."),
            "the declared non-deferred skill is still injected: {prompt}"
        );
        assert!(
            !prompt.contains("llama_r__skill"),
            "an undeclared id must not turn into a deferred announcement: {prompt}"
        );
    }

    #[tokio::test]
    async fn ranking_still_applies_to_deferred_skills() {
        let _guard = lock_env();
        let temp_dir = tempfile::tempdir().unwrap();
        let env = EnvRestore::capture();
        env.enter(temp_dir.path());

        let enricher = enricher_with_skills(
            temp_dir.path(),
            &[
                ("beta-skill", "beta work", "BETA BODY"),
                ("alpha-skill", "alpha work", "ALPHA BODY"),
            ],
        )
        .with_embeddings(Arc::new(KeywordEmbedder));
        let agent = Agent {
            id: "reviewer".to_string(),
            project_id: None,
            config: AgentConfig {
                name: "Reviewer".to_string(),
                model: String::new(),
                // Query matches alpha, so ranking must promote it over beta
                // even though beta is listed first in the manifest.
                system_prompt: "alpha is the priority".to_string(),
                skills: vec!["beta-skill".to_string(), "alpha-skill".to_string()],
                deferred_skills: vec!["beta-skill".to_string(), "alpha-skill".to_string()],
                ..Default::default()
            },
        };

        let prompt = enricher.build_system_prompt(&agent).await;

        let alpha_pos = prompt
            .find("alpha-skill —")
            .expect("alpha must be announced");
        let beta_pos = prompt.find("beta-skill —").expect("beta must be announced");
        assert!(
            alpha_pos < beta_pos,
            "deferred announcements must follow relevance ranking: {prompt}"
        );
        assert!(
            !prompt.contains("ALPHA BODY") && !prompt.contains("BETA BODY"),
            "deferred bodies must stay out of the prompt: {prompt}"
        );
    }
}
