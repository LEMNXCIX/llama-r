//! Candidate set and per-agent text for the routing decision model.
//!
//! Routing is a `Choice` over named candidates, so somebody has to decide
//! *which* agents d1 may pick between and what each one is called in the
//! prompt. That is this module: [`build_candidates`] turns the registry into
//! a list keyed exactly like the registry's own keys — `registry_key` in
//! [`crate::services::agent_registry`] is the authority, because a global
//! `ops` and a project `ops` must not collide in one `criteria` map — and
//! [`derive_criteria_text`] writes the short description d1 scores each
//! candidate against.
//!
//! Two decisions are worth knowing before editing. A blank description never
//! removes a candidate: the agent id stands in for the missing name and
//! description, the capability sections still follow it, and a warning is
//! logged — because dropping the agent would turn a request that resolves
//! the general agent today into a `400`: an agent with a weak description
//! routes badly, an agent that cannot be routed at all is worse, and
//! silently. And the list is sorted by key, because an unstable order hands
//! d1 two different prompts for the same request, which makes a confidence
//! threshold impossible to interpret.

use crate::domain::agent::Agent;
use crate::services::agent_registry::AgentRegistry;

/// One agent the router may pick between, and the text d1 scores it against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteCandidate {
    /// The key in the `Choice` criteria map, and the string a decision comes
    /// back as: `project::agent` for project agents, the bare id for globals.
    pub key: String,
    pub agent_id: String,
    pub project_id: Option<String>,
    /// Human-readable name for logs and previews. Blank names fall back to
    /// the agent id, so this is never empty either.
    pub label: String,
    /// The description d1 scores this candidate against.
    pub criteria: String,
    /// Whether this is the project's own general agent (`p/agents/p.toml`),
    /// which the ladder treats as its fallback rather than a specialist.
    pub is_project_general: bool,
}

/// The agents routing may choose between for `project_id`: every global
/// agent plus every agent of that project, minus `exclude` (matched on agent
/// id, the unit `RouterConfig::exclude` documents).
///
/// The result is sorted by `key`. `registry.list()` walks a `HashMap`, so
/// its order varies between calls; an unstable order would build the
/// `Choice` criteria map differently for two identical requests, and a
/// confidence threshold interpreted against different prompts is noise.
pub fn build_candidates(
    registry: &AgentRegistry,
    project_id: &str,
    exclude: &[String],
) -> Vec<RouteCandidate> {
    let mut candidates: Vec<RouteCandidate> = registry
        .list()
        .into_iter()
        .filter(|registered| match &registered.agent.project_id {
            Some(project) => project == project_id,
            None => true,
        })
        .filter(|registered| !exclude.contains(&registered.agent.id))
        .filter_map(|registered| {
            let agent = registered.agent;

            // A global id containing `::` would split back ambiguously: the
            // bare key `ops::night` reads as project `ops`, agent `night`,
            // which is exactly what the project prefix exists to disambiguate.
            // Project ids cannot have this problem — their key carries the
            // separator once, around the whole agent id.
            if agent.project_id.is_none() && agent.id.contains("::") {
                tracing::warn!(
                    agent_id = %agent.id,
                    "skipping global agent whose id contains the routing key separator"
                );
                return None;
            }

            let hint = agent
                .config
                .routing
                .as_ref()
                .and_then(|routing| routing.hint.as_deref());
            let has_hint = hint.map(|hint| !hint.trim().is_empty()).unwrap_or(false);
            // An agent nothing wrote a description for still has to be
            // routable — dropping it would turn a request that succeeds today
            // into a 400 — so it stays, and the weakness is made visible
            // here, once, naming the agent, rather than silently.
            if !has_hint && agent.config.description.trim().is_empty() {
                tracing::warn!(
                    agent = %agent.qualified_id(),
                    "routing candidate has no description; its criteria text is derived without one"
                );
            }

            Some(RouteCandidate {
                key: candidate_key(agent.project_id.as_deref(), &agent.id),
                agent_id: agent.id.clone(),
                project_id: agent.project_id.clone(),
                label: label_for(&agent),
                criteria: derive_criteria_text(&agent),
                // A *global* agent that happens to share the project's id is
                // not the project's general agent — that one lives at
                // `contextos/projects/<id>/agents/<id>.toml`.
                is_project_general: agent.project_id.is_some()
                    && is_project_general(project_id, &agent.id),
            })
        })
        .collect();
    candidates.sort_by(|a, b| a.key.cmp(&b.key));
    candidates
}

/// The text d1 scores one agent against.
///
/// A non-blank [`crate::domain::agent::RoutingConfig::hint`] replaces
/// everything else when present — one curated sentence beats a derived
/// paragraph for the agents where the floor is not enough. Otherwise the
/// manifest describes itself: `name` and `description` first, then whichever
/// of `Skills:`, `Tools:` and `Sources:` sections have content, joined by
/// newlines.
///
/// When both `name` and `description` are blank the agent id takes their
/// place as the **header**, and the sections still follow it — the id is the
/// floor, never the whole text: an agent with no name, no description, no
/// skills and no tools gets the bare id (an empty `Choice` criterion is
/// worse than a terse one), while one with no words but real skills still
/// shows them.
pub fn derive_criteria_text(agent: &Agent) -> String {
    let config = &agent.config;

    if let Some(hint) = config
        .routing
        .as_ref()
        .and_then(|routing| routing.hint.as_deref())
    {
        if !hint.trim().is_empty() {
            return hint.trim().to_string();
        }
    }

    let name = config.name.trim();
    let description = config.description.trim();
    // With both blank the id takes the header's place instead of ending the
    // function: the sections below still follow, because an agent that
    // cannot name itself may still hold exactly the capability d1 needs to
    // pick it — the id is the floor, never the whole text.
    let header = match (name.is_empty(), description.is_empty()) {
        (true, true) => agent.id.clone(),
        (false, true) => name.to_string(),
        (true, false) => description.to_string(),
        (false, false) => format!("{name} — {description}"),
    };

    let mut sections = vec![header];

    let skills = union(&config.skills, &config.auto_skills);
    if !skills.is_empty() {
        sections.push(format!("Skills: {}", skills.join(", ")));
    }
    let tools = present(&config.tools_override);
    if !tools.is_empty() {
        sections.push(format!("Tools: {}", tools.join(", ")));
    }
    let sources = present(&config.mcp_sources);
    if !sources.is_empty() {
        sections.push(format!("Sources: {}", sources.join(", ")));
    }

    sections.join("\n")
}

/// Whether `agent_id` is the project's general agent: the manifest whose id
/// matches the project's (`contextos/projects/p/agents/p.toml`).
pub fn is_project_general(project_id: &str, agent_id: &str) -> bool {
    agent_id == project_id
}

/// The routing key, in `registry_key`'s scheme — that private function in
/// [`crate::services::agent_registry`] is the authority, and this is a
/// deliberate re-derivation of it rather than a second scheme: the `Choice`
/// criteria map is keyed by these, so a global `ops` beside a project `ops`
/// would collide under a bare id. It is re-derived instead of shared because
/// `registry_key` is private to its module and widening its visibility is
/// not this module's call.
fn candidate_key(project_id: Option<&str>, agent_id: &str) -> String {
    match project_id {
        Some(project) => format!("{project}::{agent_id}"),
        None => agent_id.to_string(),
    }
}

/// The display name: the manifest's `name`, or the id when that is blank.
fn label_for(agent: &Agent) -> String {
    let name = agent.config.name.trim();
    if name.is_empty() {
        agent.id.clone()
    } else {
        name.to_string()
    }
}

/// Trimmed, non-empty entries of `left` followed by `right`'s additions,
/// without duplicates — `skills` and `auto_skills` routinely name the same
/// skill, and `Skills: x, x` reads like two.
fn union(left: &[String], right: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for entry in left.iter().chain(right) {
        let entry = entry.trim();
        if !entry.is_empty() && !out.iter().any(|seen| seen == entry) {
            out.push(entry.to_string());
        }
    }
    out
}

/// Trimmed, non-empty entries — a blank line in a list is a typo, not a
/// capability to print.
fn present(list: &[String]) -> Vec<String> {
    list.iter()
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{Agent, AgentConfig, RoutingConfig};

    /// The global `ops` — the half of the collision the key scheme exists
    /// for.
    const GLOBAL_OPS: &str = "name = \"Ops Global\"\n\
                              system_prompt = \"base\"\n\
                              description = \"Global operations agent.\"\n";

    /// `contextos/projects/fudi/agents/fudi.toml`: id equals project id, so
    /// it is the project's general agent.
    const GENERAL: &str = "name = \"Fudi\"\n\
                           system_prompt = \"base\"\n\
                           description = \"Asistente general de Fudi.\"\n";

    /// No `name` and no `description` at all — the manifest shape nothing
    /// writes a description for, whose criteria text must fall back to the
    /// agent id rather than drop out of the set.
    const NAMELESS_OPS: &str = "name = \"\"\nsystem_prompt = \"base\"\n";

    /// Loads `manifests` (path relative to the temp base dir → TOML) into a
    /// registry over a throwaway `LLAMA_R_DIR`.
    ///
    /// Every fixture goes through `AgentRegistry::reload_all` rather than a
    /// hand-built map, so ids come from filenames and the TOML takes the same
    /// deserialization path production uses.
    ///
    /// The caller must hold `lock_env_for_tests()` for the whole test:
    /// `LLAMA_R_DIR` is process-wide.
    fn registry_with(manifests: &[(&str, &str)]) -> (tempfile::TempDir, AgentRegistry) {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LLAMA_R_DIR", dir.path());
        for (relative, toml) in manifests {
            let path = dir.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, toml).unwrap();
        }
        let registry = AgentRegistry::new();
        registry.reload_all(&[]).unwrap();
        (dir, registry)
    }

    /// A global `ops` beside project `fudi`'s own `ops`, plus the project's
    /// general agent and a nameless, description-less one.
    fn fudi_registry() -> (tempfile::TempDir, AgentRegistry) {
        registry_with(&[
            ("agents/ops.toml", GLOBAL_OPS),
            ("contextos/projects/fudi/agents/fudi.toml", GENERAL),
            ("contextos/projects/fudi/agents/ops.toml", NAMELESS_OPS),
        ])
    }

    /// The standard fixture plus a global agent whose id contains the
    /// separator (Review Focus #2).
    fn registry_with_global(id: &str) -> (tempfile::TempDir, AgentRegistry) {
        let night =
            "name = \"Night\"\nsystem_prompt = \"base\"\ndescription = \"Guardia nocturna.\"\n";
        let separator_id = format!("agents/{id}.toml");
        registry_with(&[
            ("agents/ops.toml", GLOBAL_OPS),
            (&separator_id, night),
            ("contextos/projects/fudi/agents/fudi.toml", GENERAL),
            ("contextos/projects/fudi/agents/ops.toml", NAMELESS_OPS),
        ])
    }

    /// A directly-built agent for the pure `derive_criteria_text` tests: no
    /// registry, no `LLAMA_R_DIR`, no environment lock.
    fn agent_with(
        routing: RoutingConfig,
        description: &str,
        skills: &[&str],
        tools: &[&str],
    ) -> Agent {
        Agent {
            id: "demo".into(),
            project_id: None,
            config: AgentConfig {
                routing: Some(routing),
                description: description.to_string(),
                skills: skills.iter().map(|s| s.to_string()).collect(),
                tools_override: tools.iter().map(|t| t.to_string()).collect(),
                system_prompt: "base".into(),
                ..AgentConfig::default()
            },
        }
    }

    #[test]
    fn keys_are_unique_across_project_and_global_agents() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let (_dir, registry) = fudi_registry();
        // Register a global "ops" and a project "fudi" agent named "ops".
        let candidates = build_candidates(&registry, "fudi", &[]);
        let keys: Vec<&str> = candidates.iter().map(|c| c.key.as_str()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            keys.len(),
            "duplicate routing key in {keys:?}"
        );
        assert!(keys.contains(&"fudi::ops"));
        assert!(keys.contains(&"ops"));
    }

    #[test]
    fn the_projects_own_agent_id_is_flagged_as_general() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let (_dir, registry) = fudi_registry();
        let candidates = build_candidates(&registry, "fudi", &[]);
        let general: Vec<&RouteCandidate> =
            candidates.iter().filter(|c| c.is_project_general).collect();
        assert_eq!(general.len(), 1);
        assert_eq!(general[0].key, "fudi::fudi");
    }

    #[test]
    fn an_agent_with_no_description_falls_back_to_its_id_and_stays_a_candidate() {
        // The spec rejects excluding it: a project whose agents all lack a
        // description would otherwise 400 on requests that succeed today.
        //
        // Selected on the routing key, never on `agent_id`: the fixture also
        // registers a *described* global `ops`, so a `find` on the id could
        // match that one and stay green even after every description-less
        // agent is dropped from the set.
        let _guard = crate::core::paths::lock_env_for_tests();
        let (_dir, registry) = fudi_registry();
        let candidates = build_candidates(&registry, "fudi", &[]);
        let ops = candidates
            .iter()
            .find(|c| c.key == "fudi::ops")
            .expect("the description-less project agent must stay a candidate");
        assert!(!ops.is_project_general);
        // Exact, not just non-empty: the criteria must be this agent's id —
        // the fixture gives it no name, no skills and no tools to add.
        assert_eq!(ops.criteria, "ops");
    }

    #[test]
    fn a_nameless_agent_keeps_its_capability_sections_under_the_id_header() {
        // The shape `analyze` generates: no name, no description, but the
        // skills and tools ARE the routing signal. The id takes the missing
        // header's place; it must not swallow what follows it.
        let agent = agent_with(
            RoutingConfig::default(),
            "",
            &["facturacion"],
            &["fudi/list_orders"],
        );
        assert_eq!(
            derive_criteria_text(&agent),
            "demo\nSkills: facturacion\nTools: fudi/list_orders"
        );
    }

    #[test]
    fn an_agent_with_nothing_at_all_still_gets_its_id_as_criteria() {
        // The never-empty guarantee: no name, no description, no skills, no
        // tools, no sources — the id alone has to carry the criterion, and a
        // blank `Choice` option must stay unrepresentable.
        let agent = agent_with(RoutingConfig::default(), "", &[], &[]);
        let text = derive_criteria_text(&agent);
        assert!(!text.trim().is_empty());
        assert_eq!(text, "demo");
    }

    #[test]
    fn a_routing_hint_replaces_the_derived_text() {
        let agent = agent_with(
            RoutingConfig {
                hint: Some("Solo facturacion.".into()),
            },
            "descripcion original",
            &[],
            &[],
        );
        assert_eq!(derive_criteria_text(&agent), "Solo facturacion.");
    }

    #[test]
    fn derived_text_mentions_skills_and_tools_when_present() {
        let agent = agent_with(
            RoutingConfig::default(),
            "Opera pedidos",
            &["facturacion"],
            /* tools */ &["fudi/list_orders"],
        );
        let text = derive_criteria_text(&agent);
        assert!(text.contains("Opera pedidos"));
        assert!(text.contains("facturacion"));
        assert!(text.contains("fudi/list_orders"));
    }

    #[test]
    fn exclude_removes_agents_from_the_candidate_set() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let (_dir, registry) = fudi_registry();
        let candidates = build_candidates(&registry, "fudi", &["ops".to_string()]);
        assert!(!candidates.iter().any(|c| c.agent_id == "ops"));
    }

    #[test]
    fn a_global_agent_id_containing_the_separator_is_skipped() {
        // Review Focus #2. `ops::night` would split back ambiguously.
        let _guard = crate::core::paths::lock_env_for_tests();
        let (_dir, registry) = registry_with_global("ops::night");
        // The manifest must actually load. Otherwise a future filename
        // validation in `load_dir` would kill the skip branch behind a test
        // that still passes by asserting nothing was ever there.
        assert!(
            registry.get_agent("ops::night").is_some(),
            "fixture must load the separator-named agent for the skip to mean anything"
        );
        let candidates = build_candidates(&registry, "fudi", &[]);
        assert!(!candidates.iter().any(|c| c.agent_id == "ops::night"));
    }
}
