//! Task 1: the two new `AgentConfig` fields must be additive and round-trip
//! through TOML. These tests use only the public manifest type, so they also
//! prove that v1 manifests and the default config keep working for callers
//! outside the crate.

use llama_r::domain::agent::AgentConfig;

#[test]
fn v1_manifest_deserialises_without_new_fields() {
    let raw = r#"
name = "Legacy Agent"
model = "llama3.2"
system_prompt = "legacy prompt"
skills = ["legacy-skill"]
rules = ["Be brief."]
"#;
    let cfg: AgentConfig = toml::from_str(raw).unwrap();

    assert_eq!(cfg.name, "Legacy Agent");
    assert_eq!(cfg.skills, vec!["legacy-skill"]);
    assert!(
        cfg.tools_local.is_empty(),
        "deny-all default must survive an old manifest"
    );
    assert!(
        cfg.deferred_skills.is_empty(),
        "no skill is deferred in a v1 manifest"
    );
}

#[test]
fn new_fields_round_trip_through_toml() {
    let raw = r#"
name = "V2 Agent"
system_prompt = "v2 prompt"
skills = ["reviewer-skill", "legacy-skill"]
tools_local = ["llama_r__skill", "llama_r__rag_query"]
deferred_skills = ["reviewer-skill"]
"#;
    let cfg: AgentConfig = toml::from_str(raw).unwrap();
    assert_eq!(
        cfg.tools_local,
        vec!["llama_r__skill", "llama_r__rag_query"]
    );
    assert_eq!(cfg.deferred_skills, vec!["reviewer-skill"]);

    let encoded = toml::to_string(&cfg).unwrap();
    assert!(encoded.contains("tools_local = [\"llama_r__skill\", \"llama_r__rag_query\"]"));
    assert!(encoded.contains("deferred_skills = [\"reviewer-skill\"]"));

    let reparsed: AgentConfig = toml::from_str(&encoded).unwrap();
    assert_eq!(reparsed.tools_local, cfg.tools_local);
    assert_eq!(reparsed.deferred_skills, cfg.deferred_skills);
}

#[test]
fn default_config_serialises_both_fields_as_empty_arrays() {
    let encoded = toml::to_string(&AgentConfig::default()).unwrap();
    assert!(
        encoded.contains("tools_local = []"),
        "default deny-all must be visible in generated manifests: {encoded}"
    );
    assert!(
        encoded.contains("deferred_skills = []"),
        "default no-deferral must be visible in generated manifests: {encoded}"
    );
}
