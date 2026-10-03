//! `GET /api/skills` — what is installed, and what a project's own copy wins over.
//!
//! This is the read side of the agent form's skill picker. The TUI does not use
//! it: the TUI and the runtime share one `AppState`, so the picker reads
//! `AppState::skill_manager` in process. This exists for the callers that
//! cannot — scripts, MCP clients, anything outside the process — who otherwise
//! have to go and read a directory to learn a skill's id.
//!
//! Not gated behind `X-Debug`, unlike the RAG admin routes: this reads nothing
//! and mutates nothing, and a picker with a list of names behind a debug header
//! is a picker nobody reaches. What it does not do is hand out the catalogue —
//! no `SKILL.md` bodies, no server paths — so the answer to "does listing this
//! leak anything" is bounded by [`SkillSummary`]'s field list rather than by
//! what happens to be on disk.

use crate::api::handlers::AppState;
use crate::domain::models::{Skill, SkillScope};
use crate::error::AppError;
use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use std::path::Path as FsPath;
use std::sync::Arc;
use utoipa::ToSchema;

/// One installed skill, as a listing needs to describe it.
///
/// Deliberately not `Skill`: that carries `content` (the whole `SKILL.md` body,
/// which is the bulk of every skill) and `path` (the server's filesystem layout).
/// Both are excluded on purpose, and `the_response_carries_no_skill_bodies_and_no_paths`
/// pins the field list so neither can reappear by accident.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, ToSchema)]
pub struct SkillSummary {
    /// The id: the directory's name, which is also what an agent's `skills`
    /// list has to contain. Never the `name:` inside `SKILL.md`.
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// `Harness`, `LlamaR` or `Project`, as the enum's own name. A string rather
    /// than the enum so an external client is not broken by adding a scope.
    pub scope: String,
}

impl SkillSummary {
    fn of(skill: &Skill) -> Self {
        Self {
            id: skill.id.clone(),
            name: skill.metadata.name.clone(),
            description: skill.metadata.description.clone(),
            tags: skill.metadata.tags.clone().unwrap_or_default(),
            scope: scope_name(skill.scope).to_string(),
        }
    }
}

fn scope_name(scope: SkillScope) -> &'static str {
    match scope {
        SkillScope::Harness => "Harness",
        SkillScope::LlamaR => "LlamaR",
        SkillScope::Project => "Project",
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct SkillsResponse {
    pub skills: Vec<SkillSummary>,
    /// Skills that exist in more than one scope, and what won. Shadowing is
    /// recorded rather than applied silently, so a listing that shows
    /// `shared-skill` without saying another copy of it lost is half the story.
    #[serde(default)]
    pub shadowed: Vec<ShadowedEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ShadowedEntry {
    pub id: String,
    pub winner: String,
    pub loser: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct SkillsQuery {
    /// Scope the listing to a project, so its own skills take precedence over the
    /// shared ones. Omitted for a project-agnostic listing.
    pub project_id: Option<String>,
}

/// List installed skills, optionally scoped to one project.
pub async fn list_skills_api(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SkillsQuery>,
) -> Result<Json<SkillsResponse>, AppError> {
    let skills = match query
        .project_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(project_id) => {
            let context = state
                .context_store
                .get_context(project_id)
                .ok_or_else(|| AppError::Validation(format!("Unknown project '{project_id}'")))?;
            let path = FsPath::new(&context.path);
            state.skill_manager.list_skills_for_project(path)
        }
        None => state.skill_manager.list_skills(),
    };

    let shadowed = state
        .skill_manager
        .shadowed()
        .into_iter()
        .map(|entry| ShadowedEntry {
            id: entry.id,
            winner: scope_name(entry.winner).to_string(),
            loser: scope_name(entry.loser).to_string(),
        })
        .collect();

    Ok(Json(SkillsResponse {
        skills: skills.iter().map(SkillSummary::of).collect(),
        shadowed,
    }))
}
