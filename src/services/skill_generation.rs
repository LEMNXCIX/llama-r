//! Skill generation: proposals, and writing the approved ones.
//!
//! Generation never writes on its own. `analyze` produces proposals, the user
//! approves them in the TUI, and only then is a `SKILL.md` written.

use crate::domain::models::SkillMetadata;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Frontmatter key marking a skill as written by Llama-R rather than by hand.
///
/// Regeneration only ever overwrites a file carrying this key, so a
/// hand-written skill that happens to share an id is never clobbered.
pub const GENERATED_BY_KEY: &str = "generated_by";
pub const GENERATED_BY_VALUE: &str = "llama-r";

/// A skill `analyze` proposes, pending approval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillProposal {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The body injected into the prompt when the skill is used.
    pub content: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WriteOutcome {
    /// Written (or deliberately replaced).
    Written(PathBuf),
    /// Refused: an id that is not a safe directory name.
    RejectedInvalidId(String),
    /// Refused: a hand-written skill already occupies this id.
    RejectedHandwritten(String),
}

/// Whether an existing `SKILL.md` was produced by Llama-R.
pub fn is_generated(content: &str) -> bool {
    let Some(frontmatter) = frontmatter(content) else {
        return false;
    };
    frontmatter.lines().any(|line| {
        let mut parts = line.splitn(2, ':');
        matches!(
            (parts.next().map(str::trim), parts.next().map(str::trim)),
            (Some(key), Some(value))
                if key == GENERATED_BY_KEY && value == GENERATED_BY_VALUE
        )
    })
}

/// The YAML frontmatter of a `SKILL.md`, if present.
fn frontmatter(content: &str) -> Option<&str> {
    if !content.starts_with("---") {
        return None;
    }
    let parts: Vec<&str> = content.split("---").collect();
    if parts.len() < 3 {
        return None;
    }
    Some(parts[1])
}

/// Reject ids that could escape the skills directory or are not usable names.
///
/// The id becomes a directory name, so anything with a separator or a parent
/// reference has to be refused rather than sanitised into something the user
/// did not approve.
pub fn validate_proposal_id(id: &str) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err("skill id must not be empty".into());
    }
    if id.len() > 128 {
        return Err("skill id is too long (max 128 characters)".into());
    }
    if id == "." || id == ".." || id.contains("..") {
        return Err("skill id must not contain '..'".into());
    }
    if id.contains('/') || id.contains('\\') {
        return Err("skill id must not contain a path separator".into());
    }
    if id.chars().any(|c| c.is_control()) {
        return Err("skill id must not contain control characters".into());
    }
    Ok(())
}

/// Render a proposal to the `SKILL.md` file format.
pub fn render_skill_md(proposal: &SkillProposal) -> String {
    let tags = proposal
        .tags
        .iter()
        .map(|tag| format!("\"{}\"", tag.replace('"', "")))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "---\nname: {}\ndescription: {}\ntags: [{}]\n{}: {}\n---\n\n{}\n",
        proposal.name,
        proposal.description,
        tags,
        GENERATED_BY_KEY,
        GENERATED_BY_VALUE,
        proposal.content.trim_end(),
    )
}

/// Metadata as parsed back by `SkillManager`.
pub fn metadata_of(proposal: &SkillProposal) -> SkillMetadata {
    SkillMetadata {
        name: proposal.name.clone(),
        description: proposal.description.clone(),
        tags: if proposal.tags.is_empty() {
            None
        } else {
            Some(proposal.tags.clone())
        },
    }
}

/// Write an approved proposal into `<project>/skills/<id>/SKILL.md`.
///
/// Refuses rather than overwriting when a hand-written skill already owns the
/// id: regenerating must never destroy something the user wrote by hand.
pub fn write_proposal(
    project_root: &Path,
    proposal: &SkillProposal,
) -> Result<WriteOutcome, String> {
    if let Err(err) = validate_proposal_id(&proposal.id) {
        return Ok(WriteOutcome::RejectedInvalidId(err));
    }

    let skill_dir = project_root.join("skills").join(&proposal.id);
    let file = skill_dir.join("SKILL.md");

    if let Ok(existing) = std::fs::read_to_string(&file) {
        if !is_generated(&existing) {
            return Ok(WriteOutcome::RejectedHandwritten(proposal.id.clone()));
        }
    }

    std::fs::create_dir_all(&skill_dir).map_err(|err| format!("skill mkdir: {err}"))?;
    std::fs::write(&file, render_skill_md(proposal))
        .map_err(|err| format!("skill write '{}': {err}", file.display()))?;
    Ok(WriteOutcome::Written(file))
}

/// Parse the LLM's reply into proposals, dropping anything malformed.
///
/// The model returns JSON and is not trusted: an entry with an unsafe id, an
/// empty body or no description is dropped rather than written.
pub fn parse_proposals(response: &str) -> Vec<SkillProposal> {
    let json = extract_json_array(response).unwrap_or_else(|| response.to_string());
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.trim().to_string();
            if validate_proposal_id(&id).is_err() {
                return None;
            }
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();
            let description = item.get("description")?.as_str()?.trim().to_string();
            if description.is_empty() {
                return None;
            }
            let content = item.get("content")?.as_str()?.trim().to_string();
            if content.is_empty() {
                return None;
            }
            let tags = item
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|t| t.as_str().map(|s| s.trim().to_string()))
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            Some(SkillProposal {
                id,
                name,
                description,
                tags,
                content,
            })
        })
        .collect()
}

/// Pull a JSON array out of a reply that may wrap it in prose or a code fence.
fn extract_json_array(response: &str) -> Option<String> {
    let start = response.find('[')?;
    let end = response.rfind(']')?;
    if end < start {
        return None;
    }
    Some(response[start..=end].to_string())
}

/// Prompt asking the model to propose skills for a project.
pub fn generation_prompt(project_id: &str, project_type: &str, context_excerpt: &str) -> String {
    format!(
        "You are proposing reusable skills for one software project. \
         Return ONLY a JSON array. Each element: \
         {{\"id\": kebab-case-name, \"name\": short title, \"description\": one line saying when to use it, \
         \"tags\": [\"...\"], \"content\": markdown instructions}}. \
         Propose at most 4 skills, only for knowledge specific to this project that is not obvious \
         from reading the code. Do not propose anything generic about the language or tooling. \
         Never propose an id that already exists.\n\n\
         Project id: {project_id}\nProject type: {project_type}\n\n\
         Project context:\n{context_excerpt}"
    )
}

/// Ask the model to propose skills, and parse whatever it returns.
///
/// A failure here yields no proposals rather than failing the analysis: skills
/// are an extra, and analysis must still succeed without them.
pub async fn generate_proposals(
    provider: Arc<dyn crate::providers::LLMProvider + Send + Sync>,
    model: &str,
    project_id: &str,
    project_type: &str,
    context_excerpt: &str,
) -> Vec<SkillProposal> {
    use crate::domain::models::{ChatMessage, ChatRequest};

    let prompt = generation_prompt(project_id, project_type, context_excerpt);
    let response = provider
        .chat(ChatRequest {
            model: model.to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: prompt,
            }],
            stream: false,
        })
        .await;

    match response {
        Ok(response) => parse_proposals(&response.message.content),
        Err(err) => {
            tracing::warn!(error = %err, "skill generation call failed");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod approval_flow_tests {
    use super::*;
    use crate::adapters::rag::store::HashEmbeddingProvider;
    use crate::services::skill_manager::SkillManager;

    /// End to end: generate-shaped JSON, approve, and the skill becomes
    /// discoverable as a Project-scoped skill for the next analyze.
    #[test]
    fn approved_proposal_becomes_a_discoverable_project_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();

        let proposals = parse_proposals(
            r##"[{"id":"deploy-flow","name":"Deploy flow","description":"How releases ship","tags":["ops"],"content":"# Deploy\n\nRun make deploy."}]"##,
        );
        assert_eq!(proposals.len(), 1);

        let outcome = write_proposal(project, &proposals[0]).unwrap();
        assert!(matches!(outcome, WriteOutcome::Written(_)));

        // A fresh manager must find it as a Project skill.
        let manager = SkillManager::with_paths(vec![tmp.path().join("empty-shared")]);
        manager.scan_and_load();
        let found = manager.resolve_for_project("deploy-flow", project).unwrap();
        assert_eq!(found.scope, crate::domain::models::SkillScope::Project);
        assert_eq!(found.metadata.description, "How releases ship");
        assert!(found.content.contains("make deploy"));
    }

    #[test]
    fn a_handwritten_skill_survives_a_full_regeneration_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skills/api-conventions");
        std::fs::create_dir_all(&dir).unwrap();
        let hand = "---\nname: Mine\ndescription: mine\n---\n\ndo not touch\n";
        std::fs::write(dir.join("SKILL.md"), hand).unwrap();

        for _ in 0..3 {
            let parsed = parse_proposals(
                r#"[{"id":"api-conventions","name":"Generated","description":"d","content":"generated body"}]"#,
            );
            let outcome = write_proposal(tmp.path(), &parsed[0]).unwrap();
            assert!(matches!(outcome, WriteOutcome::RejectedHandwritten(_)));
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            hand,
            "repeated regeneration must never touch a hand-written skill"
        );
    }

    #[test]
    fn proposal_without_valid_id_never_reaches_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let parsed = parse_proposals(
            r#"[{"id":"../../etc/passwd","name":"X","description":"d","content":"evil"}]"#,
        );
        assert!(parsed.is_empty(), "unsafe id must be dropped at parse time");
        assert!(!tmp.path().join("skills").exists());
    }

    #[allow(dead_code)]
    fn _unused(_: Arc<HashEmbeddingProvider>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(id: &str) -> SkillProposal {
        SkillProposal {
            id: id.to_string(),
            name: format!("{id} title"),
            description: "what it is for".into(),
            tags: vec!["rust".into()],
            content: "# Instructions\n\nDo the thing.".into(),
        }
    }

    #[test]
    fn writes_an_approved_proposal_where_the_project_can_see_it() {
        let tmp = tempfile::tempdir().unwrap();
        let outcome = write_proposal(tmp.path(), &proposal("api-conventions")).unwrap();
        assert!(matches!(outcome, WriteOutcome::Written(_)), "{outcome:?}");

        let file = tmp.path().join("skills/api-conventions/SKILL.md");
        assert!(file.exists());
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains("description: what it is for"));
        assert!(content.contains("Do the thing."));
    }

    #[test]
    fn never_overwrites_a_handwritten_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skills/api-conventions");
        std::fs::create_dir_all(&dir).unwrap();
        let original = "---\nname: Mine\ndescription: written by hand\n---\n\nprecious\n";
        std::fs::write(dir.join("SKILL.md"), original).unwrap();

        let outcome = write_proposal(tmp.path(), &proposal("api-conventions")).unwrap();
        assert!(
            matches!(outcome, WriteOutcome::RejectedHandwritten(_)),
            "a hand-written skill must be protected, got {outcome:?}"
        );
        let content = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert_eq!(content, original, "the user's file must be untouched");
    }

    #[test]
    fn regenerating_a_generated_skill_replaces_it() {
        let tmp = tempfile::tempdir().unwrap();
        write_proposal(tmp.path(), &proposal("auto")).unwrap();

        let mut second = proposal("auto");
        second.content = "improved instructions".into();
        let outcome = write_proposal(tmp.path(), &second).unwrap();
        assert!(matches!(outcome, WriteOutcome::Written(_)), "{outcome:?}");

        let content = std::fs::read_to_string(tmp.path().join("skills/auto/SKILL.md")).unwrap();
        assert!(content.contains("improved instructions"));
        assert!(!content.contains("Do the thing."));
    }

    #[test]
    fn rejects_ids_that_could_escape_the_skills_directory() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in ["../escape", "a/b", "..", "", "  ", "with\nnewline"] {
            let outcome = write_proposal(tmp.path(), &proposal(bad)).unwrap();
            assert!(
                matches!(outcome, WriteOutcome::RejectedInvalidId(_)),
                "id {bad:?} must be refused, got {outcome:?}"
            );
        }
        // Nothing may have been created outside the skills dir.
        assert!(!tmp.path().parent().unwrap().join("escape").exists());
    }

    #[test]
    fn generated_marker_round_trips() {
        let rendered = render_skill_md(&proposal("x"));
        assert!(is_generated(&rendered), "rendered skill must be marked");
        assert!(
            !is_generated("---\nname: Mine\ndescription: hand\n---\n\nbody"),
            "a hand-written skill must not be treated as generated"
        );
        assert!(!is_generated("no frontmatter at all"));
    }

    #[test]
    fn parses_wellformed_proposals() {
        let reply = r##"Sure! Here you go:
```json
[{"id":"api-conventions","name":"API conventions","description":"How we version REST","tags":["api"],"content":"# Rules\n\nVersion in the path."}]
```
"##;
        let parsed = parse_proposals(reply);
        assert_eq!(parsed.len(), 1, "the array must be found inside the fence");
        assert_eq!(parsed[0].id, "api-conventions");
        assert_eq!(parsed[0].tags, vec!["api".to_string()]);
        assert!(parsed[0].content.contains("Version in the path."));
    }

    #[test]
    fn drops_malformed_entries_instead_of_trusting_them() {
        let reply = r#"[
            {"id":"good","name":"G","description":"d","content":"body"},
            {"id":"../escape","name":"X","description":"d","content":"body"},
            {"id":"no-content","name":"N","description":"d"},
            {"id":"empty-desc","name":"E","description":"   ","content":"body"},
            {"description":"missing id","content":"body"},
            "not an object"
        ]"#;
        let parsed = parse_proposals(reply);
        assert_eq!(parsed.len(), 1, "only the valid entry survives: {parsed:?}");
        assert_eq!(parsed[0].id, "good");
    }

    #[test]
    fn malformed_reply_yields_nothing_rather_than_panicking() {
        assert!(parse_proposals("no json here at all").is_empty());
        assert!(parse_proposals("").is_empty());
        assert!(parse_proposals("[broken").is_empty());
        assert!(parse_proposals("{}").is_empty());
    }

    #[test]
    fn rendered_skill_is_parseable_by_the_skill_manager() {
        // Guards the coupling with SkillManager::parse_skill_metadata, which is
        // what actually reads this file back.
        let tmp = tempfile::tempdir().unwrap();
        write_proposal(tmp.path(), &proposal("api-conventions")).unwrap();
        let content =
            std::fs::read_to_string(tmp.path().join("skills/api-conventions/SKILL.md")).unwrap();
        let manager = crate::services::skill_manager::SkillManager::new();
        manager.scan_and_load();
        let parsed = manager
            .parse_skill_metadata(&content)
            .expect("the skill manager must be able to parse a generated skill");
        assert_eq!(parsed.description, "what it is for");
        assert_eq!(parsed.tags, Some(vec!["rust".to_string()]));
    }
}
