//! Task 2: the permission engine is a pure function compiled from the agent
//! TOML into `AgentScope`. No HITL wiring belongs here (that is Task 4).

use llama_r::domain::agent::{Agent, AgentConfig};
use llama_r::domain::policy::{
    ApprovalDecision, ApprovalPolicy, ApprovalRule, PermissionEngine, PermissionPolicy,
};
use llama_r::services::scope_builder::ScopeBuilder;

use ApprovalDecision::{Allow, Approve};

fn policy(approvals: Vec<ApprovalRule>) -> PermissionPolicy {
    PermissionPolicy {
        connections_allow: Vec::new(),
        approvals,
        auto_threshold: None,
    }
}

fn rule(tool_pattern: &str, policy: ApprovalPolicy) -> ApprovalRule {
    ApprovalRule {
        tool_pattern: tool_pattern.to_string(),
        policy,
    }
}

#[test]
fn first_matching_pattern_wins() {
    let p = policy(vec![
        rule("danger__*", ApprovalPolicy::Always),
        rule("*", ApprovalPolicy::Never),
    ]);
    // The specific rule is listed first, so the catch-all never applies.
    assert_eq!(PermissionEngine::authorize(&p, "danger__drop_all"), Approve);
    assert_eq!(PermissionEngine::authorize(&p, "safe__read"), Allow);
}

#[test]
fn first_match_wins_even_when_a_later_rule_is_more_specific() {
    // Order, not specificity, decides. A broad first rule shadows a narrow one.
    let p = policy(vec![
        rule("*", ApprovalPolicy::Always),
        rule("safe__*", ApprovalPolicy::Never),
    ]);
    assert_eq!(PermissionEngine::authorize(&p, "safe__read"), Approve);
}

#[test]
fn default_is_never_when_no_rule_matches() {
    let p = policy(vec![rule("danger__*", ApprovalPolicy::Always)]);
    assert_eq!(PermissionEngine::authorize(&p, "safe__read"), Allow);

    // An entirely empty policy always allows.
    let empty = PermissionPolicy::default();
    assert_eq!(PermissionEngine::authorize(&empty, "anything"), Allow);
}

#[test]
fn auto_behaves_as_always_today() {
    let auto = policy(vec![rule("*", ApprovalPolicy::Auto)]);
    let always = policy(vec![rule("*", ApprovalPolicy::Always)]);
    for tool in ["t", "server__tool", "llama_r__skill"] {
        assert_eq!(
            PermissionEngine::authorize(&auto, tool),
            PermissionEngine::authorize(&always, tool)
        );
        assert_eq!(PermissionEngine::authorize(&auto, tool), Approve);
    }
}

#[test]
fn once_requires_approval_in_the_pure_engine() {
    // `Once` differs from `Always` only at the gate (approval on first use).
    // Before a call, both require approval, so the pure decision is `Approve`.
    let p = policy(vec![rule("*", ApprovalPolicy::Once)]);
    assert_eq!(PermissionEngine::authorize(&p, "t"), Approve);
}

#[test]
fn glob_star_is_a_simple_wildcard() {
    let p = policy(vec![rule("svc__delete*", ApprovalPolicy::Always)]);
    assert_eq!(PermissionEngine::authorize(&p, "svc__delete"), Approve);
    assert_eq!(PermissionEngine::authorize(&p, "svc__delete_all"), Approve);
    assert_eq!(PermissionEngine::authorize(&p, "svc__undeleted"), Allow);

    // Multiple stars, including a leading one, behave as wildcards.
    let p2 = policy(vec![rule("*__*", ApprovalPolicy::Always)]);
    assert_eq!(PermissionEngine::authorize(&p2, "server__tool"), Approve);
    assert_eq!(PermissionEngine::authorize(&p2, "plain_tool"), Allow);
}

#[test]
fn scope_builder_compiles_policy_from_toml() {
    let raw = r#"
name = "Ops"
system_prompt = "ops"

[policy]
connections_allow = ["fudi"]

[[policy.approvals]]
tool_pattern = "fudi__drop*"
policy = "always"

[[policy.approvals]]
tool_pattern = "llama_r__skill"
policy = "never"
"#;
    let config: AgentConfig = toml::from_str(raw).unwrap();
    let agent = Agent {
        id: "ops".into(),
        project_id: Some("fudi".into()),
        config,
    };
    let scope = ScopeBuilder::build(&agent).unwrap();

    assert_eq!(scope.permission.connections_allow, vec!["fudi"]);
    assert_eq!(scope.permission.approvals.len(), 2);
    assert_eq!(scope.permission.approvals[0].tool_pattern, "fudi__drop*");
    assert_eq!(scope.permission.approvals[0].policy, ApprovalPolicy::Always);
    assert_eq!(
        PermissionEngine::authorize(&scope.permission, "fudi__drop_all"),
        Approve
    );
    assert_eq!(
        PermissionEngine::authorize(&scope.permission, "llama_r__skill"),
        Allow
    );
}

#[test]
fn v1_manifest_without_policy_loads_and_allows() {
    let raw = r#"
name = "Legacy"
system_prompt = "v1 agent"
mcp_sources = ["fudi"]
tools_override = ["*"]
"#;
    let config: AgentConfig = toml::from_str(raw).unwrap();
    assert!(config.policy.approvals.is_empty());
    assert!(config.policy.connections_allow.is_empty());

    let agent = Agent {
        id: "legacy".into(),
        project_id: None,
        config,
    };
    let scope = ScopeBuilder::build(&agent).unwrap();

    // No tool requires approval, and pre-existing scope behavior is unchanged.
    assert_eq!(
        PermissionEngine::authorize(&scope.permission, "fudi__anything"),
        Allow
    );
    assert!(scope.allows_tool("fudi", "anything"));
}
