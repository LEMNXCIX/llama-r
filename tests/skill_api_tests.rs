//! Tests for `GET /api/skills`, the read side of the agent form's skill picker.
//!
//! The TUI does not use this endpoint — it reads `AppState::skill_manager` in
//! process, because the TUI and the runtime share one `AppState`. This exists
//! for the callers that cannot: scripts, MCP clients, anything outside the
//! process.
//!
//! Two things matter beyond "it returns 200": that a project-scoped request
//! sees the project's skills taking precedence over the shared ones, and that
//! the response does not carry every `SKILL.md` body. A listing that ships
//! `content` grows with the size of the skill catalogue rather than the size of
//! its names.

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::skill_api::{SkillSummary, SkillsResponse};
use llama_r::context::store::ContextStore;
use llama_r::context::store::ProjectContext;
use llama_r::domain::models::{ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use llama_r::ports::mcp::McpServerRegistry;
use llama_r::providers::LLMProvider;
use llama_r::runtime::{build_app_state, build_router};
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::skill_manager::SkillManager;
use std::collections::VecDeque;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;
use tokio_stream::Stream;
use tower::ServiceExt;

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn test_lock() -> &'static Mutex<()> {
    TEST_LOCK.get_or_init(|| Mutex::new(()))
}

/// The endpoint never calls the provider, so this only has to satisfy the trait.
struct InertProvider;

#[async_trait]
impl LLMProvider for InertProvider {
    fn get_base_url(&self) -> String {
        "http://inert".to_string()
    }
    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
    async fn chat(
        &self,
        _request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        Err("not reached".into())
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    > {
        Err("not reached".into())
    }
}

fn write_skill(dir: &Path, id: &str, name: &str, description: &str) {
    fs::create_dir_all(dir.join(id)).unwrap();
    fs::write(
        dir.join(id).join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\nthe body of {id}\n"),
    )
    .unwrap();
}

fn state_with(manager: SkillManager, store: ContextStore) -> axum::Router {
    let agent_registry = Arc::new(AgentRegistry::new());
    agent_registry.reload_all(&[]).unwrap();
    let state = build_app_state(
        Arc::new(InertProvider),
        agent_registry,
        Arc::new(manager),
        Arc::new(store),
        "fallback-model".to_string(),
        Arc::new(Mutex::new(VecDeque::new())),
        Vec::new(),
        Arc::new(StaticMcpRegistry::new()) as Arc<dyn McpServerRegistry>,
        None,
        None,
        None,
        None,
        None,
    );
    build_router(state)
}

async fn get_json(router: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = router
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

#[tokio::test]
async fn lists_installed_skills_with_their_metadata() {
    let _guard: MutexGuard<'static, ()> = test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());
    write_skill(&dir.path().join("skills"), "nutrition-standards", "Nutrition", "clinical rules");

    let manager = SkillManager::with_paths(vec![dir.path().join("skills")]);
    manager.scan_and_load();
    let router = state_with(manager, ContextStore::new());

    let (status, body) = get_json(router, "/api/skills").await;

    assert_eq!(status, StatusCode::OK, "body was {body}");
    let response: SkillsResponse = serde_json::from_value(body).unwrap();
    let nutrition = response
        .skills
        .iter()
        .find(|s| s.id == "nutrition-standards")
        .expect("the skill was written to disk and scanned");

    assert_eq!(nutrition.name, "Nutrition");
    assert_eq!(nutrition.description, "clinical rules");
    assert_eq!(nutrition.scope, "LlamaR");
}

#[tokio::test]
async fn the_response_carries_no_skill_bodies_and_no_paths() {
    let _guard: MutexGuard<'static, ()> = test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());
    write_skill(
        &dir.path().join("skills"),
        "verbose",
        "Verbose",
        "has a long body",
    );

    let manager = SkillManager::with_paths(vec![dir.path().join("skills")]);
    manager.scan_and_load();
    let router = state_with(manager, ContextStore::new());

    let (status, body) = get_json(router, "/api/skills").await;
    assert_eq!(status, StatusCode::OK);
    let rendered = body.to_string();

    // The skill's body is on disk in full; a listing that shipped it would grow
    // with the catalogue instead of with its names.
    assert!(
        !rendered.contains("the body of verbose"),
        "the listing must not carry SKILL.md bodies: {body}"
    );
    // `path` is the other field `Skill` has, and it describes the server's
    // filesystem layout to whoever can reach the port.
    assert!(
        !rendered.contains(dir.path().to_str().unwrap()),
        "the listing must not carry server paths: {body}"
    );

    // Pin the exact key set, so a field added to `Skill` later cannot leak into
    // the response by accident: the response type does not deny unknown fields.
    let entry = &body["skills"][0];
    let mut keys: Vec<&str> = entry.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["description", "id", "name", "scope", "tags"],
        "the summary carries exactly what a picker needs"
    );
}

#[tokio::test]
async fn a_project_scoped_request_sees_the_projects_own_skills() {
    let _guard: MutexGuard<'static, ()> = test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let base = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", base.path());
    let project: PathBuf = base.path().join("project");
    write_skill(&base.path().join("skills"), "shared-skill", "Shared", "everywhere");
    write_skill(&project.join("skills"), "project-skill", "Project", "just here");

    let manager = SkillManager::with_paths(vec![base.path().join("skills")]);
    manager.scan_and_load();
    let store = ContextStore::new();
    store
        .save_context(ProjectContext {
            project_id: "proj".to_string(),
            path: project.to_string_lossy().to_string(),
            context_md: String::new(),
            project_type: "rust".to_string(),
            skills_injected: Vec::new(),
            last_analyzed: chrono::Utc::now(),
            custom_rules: String::new(),
        })
        .unwrap();
    let router = state_with(manager, store);

    let (status, body) = get_json(router, "/api/skills?project_id=proj").await;

    assert_eq!(status, StatusCode::OK, "body was {body}");
    let response: SkillsResponse = serde_json::from_value(body).unwrap();
    let ids: Vec<&str> = response.skills.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&"project-skill"), "the project's own skill: {ids:?}");
    assert!(ids.contains(&"shared-skill"), "and the shared one: {ids:?}");
}

#[tokio::test]
async fn an_unknown_project_is_rejected_rather_than_silently_ignored() {
    let _guard: MutexGuard<'static, ()> = test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LLAMA_R_DIR", dir.path());

    let manager = SkillManager::with_paths(vec![dir.path().join("skills")]);
    manager.scan_and_load();
    let router = state_with(manager, ContextStore::new());

    let (status, _) = get_json(router, "/api/skills?project_id=nope").await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a typo'd project id must not read as 'no project, so no scoping'"
    );
}

#[tokio::test]
async fn the_summary_is_serialisable_on_its_own() {
    // `SkillsResponse` is what the endpoint returns and what an external client
    // deserialises into. If it stops deriving `Deserialize`, a client in another
    // language still works but none of this repo's own can.
    let summary = SkillSummary {
        id: "a".into(),
        name: "A".into(),
        description: "d".into(),
        tags: vec!["t".into()],
        scope: "LlamaR".into(),
    };
    let json = serde_json::to_string(&summary).unwrap();
    let back: SkillSummary = serde_json::from_str(&json).unwrap();
    assert_eq!(back.id, "a");
    assert_eq!(back.tags, vec!["t".to_string()]);
}
