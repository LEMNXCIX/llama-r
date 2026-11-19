//! TOML-shaped eval case and suite types, plus the `expect` desugaring rule.

use serde::{Deserialize, Deserializer, Serialize};

use super::check::EvalCheck;

/// Which runner path a case uses. `plain`/`tool`/`stream` are the v1 provider
/// cases; `agent` runs a full agent turn so `called_tool` has events to inspect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvalCaseKind {
    Plain,
    Tool,
    Stream,
    Agent,
}

/// How a suite treats failures. Strict is the CI default; skipped checks never
/// count as failures in either mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvalMode {
    #[default]
    Strict,
    Lenient,
}

/// One eval case.
///
/// `expect` is the v1 sugar: it is folded into `checks` as a [`EvalCheck::Contains`]
/// at load time. Call [`EvalCase::finalize`] when building a case by hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvalCase {
    pub id: String,
    pub kind: EvalCaseKind,
    pub prompt: String,
    pub expect: Option<String>,
    pub checks: Vec<EvalCheck>,
    pub agent: Option<String>,
}

/// The wire shape of a case, before `expect` is folded into `checks`.
#[derive(Deserialize)]
struct RawEvalCase {
    id: String,
    kind: EvalCaseKind,
    prompt: String,
    #[serde(default)]
    expect: Option<String>,
    #[serde(default)]
    checks: Vec<EvalCheck>,
    #[serde(default)]
    agent: Option<String>,
}

impl<'de> Deserialize<'de> for EvalCase {
    /// Deserialize and immediately desugar, so every load path (`toml::from_str`
    /// of a case or a whole suite) hands the runner a ready-to-check case.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawEvalCase::deserialize(deserializer)?;
        let mut case = Self {
            id: raw.id,
            kind: raw.kind,
            prompt: raw.prompt,
            expect: raw.expect,
            checks: raw.checks,
            agent: raw.agent,
        };
        case.finalize();
        Ok(case)
    }
}

impl EvalCase {
    /// Fold `expect` into `checks`.
    ///
    /// The generated `Contains` is **appended** after the explicit checks, so
    /// declared checks run first in their written order and the legacy
    /// `expect` acts as a final backstop. Idempotent: calling twice (or loading
    /// a round-tripped case that already holds the folded check) never adds a
    /// duplicate.
    pub fn finalize(&mut self) {
        let Some(text) = self.expect.clone() else {
            return;
        };
        let folded = self.checks.iter().any(
            |check| matches!(check, EvalCheck::Contains { text: existing } if existing == &text),
        );
        if !folded {
            self.checks.push(EvalCheck::Contains { text });
        }
    }
}

/// A provider entry from the `[[providers]]` array of a v1 eval file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalProvider {
    pub name: String,
    pub kind: String,
}

/// A whole eval file: cases plus the optional provider list and mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EvalSuite {
    #[serde(default)]
    pub cases: Vec<EvalCase>,
    #[serde(default)]
    pub providers: Vec<EvalProvider>,
    #[serde(default)]
    pub mode: EvalMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fragment using all four check tags, plus legacy `expect` sugar.
    const ALL_CHECKS: &str = r#"
        [[cases]]
        id = "rich"
        kind = "plain"
        prompt = "answer"
        expect = "legacy"
        checks = [
          { type = "contains", text = "alpha" },
          { type = "not_contains", text = "beta" },
          { type = "called_tool", name = "server__tool" },
          { type = "judge", question = "Is this polite?" },
        ]
    "#;

    #[test]
    fn deserializes_all_four_check_types_and_expect_sugar() {
        let suite: EvalSuite = toml::from_str(ALL_CHECKS).expect("valid eval TOML");
        let case = &suite.cases[0];

        assert_eq!(case.checks.len(), 5, "four checks + one from expect");
        assert_eq!(
            case.checks,
            vec![
                EvalCheck::Contains {
                    text: "alpha".into()
                },
                EvalCheck::NotContains {
                    text: "beta".into()
                },
                EvalCheck::CalledTool {
                    name: "server__tool".into()
                },
                EvalCheck::Judge {
                    question: "Is this polite?".into()
                },
                // expect desugars to Contains, appended last.
                EvalCheck::Contains {
                    text: "legacy".into()
                },
            ]
        );
    }

    #[test]
    fn expect_desugars_to_a_contains_check() {
        let suite: EvalSuite = toml::from_str(
            r#"
            [[cases]]
            id = "expect-only"
            kind = "plain"
            prompt = "say hi"
            expect = "hello"
            "#,
        )
        .expect("valid eval TOML");

        assert_eq!(
            suite.cases[0].checks,
            vec![EvalCheck::Contains {
                text: "hello".into()
            }]
        );
        // The raw field is preserved for introspection.
        assert_eq!(suite.cases[0].expect.as_deref(), Some("hello"));
    }

    #[test]
    fn v1_expect_only_toml_loads_unchanged() {
        // The shape the six-provider smoke file has always used.
        let src = r#"
            [[cases]]
            id = "plain-greeting"
            kind = "plain"
            prompt = "greet"
            expect = "hello"

            [[cases]]
            id = "streaming"
            kind = "stream"
            prompt = "stream"
            expect = "one"

            [[providers]]
            name = "ollama"
            kind = "ollama"
        "#;
        let suite: EvalSuite = toml::from_str(src).expect("v1 TOML must still load");

        assert_eq!(suite.cases.len(), 2);
        assert_eq!(suite.providers.len(), 1);
        assert_eq!(suite.mode, EvalMode::Strict);
        for (case, expected) in suite.cases.iter().zip(["hello", "one"]) {
            assert_eq!(
                case.checks,
                vec![EvalCheck::Contains {
                    text: expected.into()
                }]
            );
        }
    }

    #[test]
    fn kind_agent_is_accepted() {
        let case: EvalCase = toml::from_str(
            r#"
            id = "tool-turn"
            kind = "agent"
            prompt = "use the tool"
            agent = "helper"
            "#,
        )
        .expect("valid agent case");

        assert_eq!(case.kind, EvalCaseKind::Agent);
        assert_eq!(case.agent.as_deref(), Some("helper"));
    }

    #[test]
    fn unknown_check_type_is_a_named_error() {
        let src = r#"
            [[cases]]
            id = "bad"
            kind = "plain"
            prompt = "p"
            checks = [{ type = "bogus", text = "x" }]
        "#;
        let err = toml::from_str::<EvalSuite>(src).expect_err("unknown type must be rejected");
        let message = err.to_string();
        assert!(
            message.contains("bogus"),
            "error must name the bad type: {message}"
        );
        assert!(
            message.contains("contains") && message.contains("judge"),
            "error must name the accepted types: {message}"
        );
    }

    #[test]
    fn finalize_is_idempotent() {
        let mut case = EvalCase {
            id: "c".into(),
            kind: EvalCaseKind::Plain,
            prompt: "p".into(),
            expect: Some("hi".into()),
            checks: Vec::new(),
            agent: None,
        };
        case.finalize();
        case.finalize();
        assert_eq!(case.checks, vec![EvalCheck::Contains { text: "hi".into() }]);
    }
}
