//! Task 1: the open `ProviderKind` surface and the `providers.toml` profile
//! registry. The registry holds environment variable *names*; these tests
//! deliberately set secret values in the environment and assert they never
//! appear in a profile or an error message.

use llama_r::config::ProviderKind;
use llama_r::providers::registry::{
    ProviderProfile, ProviderRegistry, DEFAULT_PROVIDER_TIMEOUT_MS,
};
use std::sync::{Mutex, MutexGuard, OnceLock};

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn test_lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Every variable read by `get_base_dir()` or `ProviderRegistry::load()`.
const REGISTRY_ENV_VARS: &[&str] = &[
    "LLAMA_R_DIR",
    "OLLAMA_URL",
    "OPENAI_BASE_URL",
    "OPENCODE_URL",
    "OPENROUTER_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "OPENAI_API_KEY",
    "OPENCODE_API_KEY",
];

/// Points the process at a fresh base directory so no developer `providers.toml`
/// can influence a test, and restores every touched variable on drop.
struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
    dir: tempfile::TempDir,
}

impl EnvGuard {
    fn fresh() -> Self {
        let saved = REGISTRY_ENV_VARS
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect();
        let dir = tempfile::tempdir().expect("create temp base dir");
        std::env::set_var("LLAMA_R_DIR", dir.path());
        Self { saved, dir }
    }

    fn write_registry(&self, contents: &str) {
        std::fs::write(self.dir.path().join("providers.toml"), contents)
            .expect("write providers.toml");
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

const SPELLINGS: &[(&str, ProviderKind)] = &[
    ("ollama", ProviderKind::Ollama),
    ("openrouter", ProviderKind::OpenRouter),
    ("opencode", ProviderKind::OpenCode),
    ("openai-compatible", ProviderKind::OpenAiCompatible),
    ("anthropic", ProviderKind::Anthropic),
    ("gemini", ProviderKind::Gemini),
];

#[test]
fn provider_kind_parses_all_six_spellings() {
    for (spelling, expected) in SPELLINGS {
        assert_eq!(
            spelling.parse::<ProviderKind>().unwrap(),
            *expected,
            "spelling {spelling} must parse"
        );
        assert_eq!(
            expected.to_string(),
            *spelling,
            "Display is the canonical spelling"
        );
    }
    // Parsing ignores case and the default stays Ollama.
    assert_eq!(
        "OpenAI-Compatible".parse::<ProviderKind>().unwrap(),
        ProviderKind::OpenAiCompatible
    );
    assert_eq!(ProviderKind::default(), ProviderKind::Ollama);
}

#[test]
fn unknown_provider_error_names_value_and_all_six_spellings() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    std::env::set_var("ANTHROPIC_API_KEY", "sk-secret-never-in-an-error");

    let err = "totally-not-a-provider"
        .parse::<ProviderKind>()
        .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("totally-not-a-provider"),
        "the error must name the received value: {message}"
    );
    for (spelling, _) in SPELLINGS {
        assert!(
            message.contains(spelling),
            "the error must list {spelling} as accepted: {message}"
        );
    }
    // A key value living in the environment is never echoed by this error.
    assert!(!message.contains("sk-secret-never-in-an-error"));
}

#[test]
fn registry_loads_profiles_from_toml_in_base_dir() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    _env.write_registry(
        r#"
[providers.local-openai]
type = "openai-compatible"
base_url = "http://127.0.0.1:8000/v1"
api_key_env = "LOCAL_OPENAI_KEY"
timeout_ms = 12345
"#,
    );

    let registry = ProviderRegistry::load().expect("load registry");
    let profile = registry.get("local-openai").expect("declared profile");
    assert_eq!(profile.name, "local-openai");
    assert_eq!(profile.kind, ProviderKind::OpenAiCompatible);
    assert_eq!(
        profile.base_url.as_deref(),
        Some("http://127.0.0.1:8000/v1")
    );
    assert_eq!(profile.api_key_env.as_deref(), Some("LOCAL_OPENAI_KEY"));
    assert_eq!(profile.timeout_ms, 12345);
}

#[test]
fn a_profile_stores_the_env_var_name_not_the_value() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-secret-never-in-a-profile");
    _env.write_registry(
        r#"
[providers.anthropic]
type = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"
"#,
    );

    let registry = ProviderRegistry::load().expect("load registry");
    let profile = registry.get("anthropic").expect("anthropic profile");
    assert_eq!(profile.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    let dump = format!("{:?}", (profile, &registry));
    assert!(
        !dump.contains("sk-ant-secret-never-in-a-profile"),
        "a profile must never carry the key value: {dump}"
    );
}

#[test]
fn a_malformed_registry_names_the_path() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    let path = _env.dir.path().join("providers.toml");
    std::fs::write(&path, "this is not = valid toml [[").expect("write broken toml");

    let err = ProviderRegistry::load().unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains(path.to_str().expect("utf8 path")),
        "the error must name the offending path: {message}"
    );
}

#[test]
fn an_unknown_profile_type_names_the_path_value_and_spellings() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    let path = _env.dir.path().join("providers.toml");
    _env.write_registry(
        r#"
[providers.weird]
type = "telepathy"
"#,
    );

    let err = ProviderRegistry::load().unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains(path.to_str().expect("utf8 path")),
        "{message}"
    );
    assert!(message.contains("telepathy"), "{message}");
    assert!(message.contains("anthropic"), "{message}");
}

#[test]
fn a_missing_registry_returns_the_builtin_profiles() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();

    let registry = ProviderRegistry::load().expect("load with no file");
    for (name, kind) in SPELLINGS {
        let profile = registry
            .get(name)
            .unwrap_or_else(|| panic!("built-in profile {name} is missing"));
        assert_eq!(profile.kind, *kind, "built-in {name} kind");
        assert_eq!(profile.name, *name, "built-in {name} name");
    }
    assert_eq!(
        registry.get("anthropic").unwrap().api_key_env.as_deref(),
        Some("ANTHROPIC_API_KEY")
    );
    assert_eq!(
        registry.get("ollama").unwrap().timeout_ms,
        DEFAULT_PROVIDER_TIMEOUT_MS
    );
}

#[test]
fn builtin_base_urls_follow_the_documented_env_defaults() {
    let _lock = test_lock();
    let _env = EnvGuard::fresh();
    std::env::set_var("OLLAMA_URL", "http://example.test:12345");

    let registry = ProviderRegistry::load().expect("load with no file");
    assert_eq!(
        registry.get("ollama").unwrap().base_url.as_deref(),
        Some("http://example.test:12345")
    );
    // A profile that names no base URL of its own still gets a usable default.
    assert_eq!(
        registry.get("anthropic").unwrap().base_url,
        None,
        "Anthropic's native endpoint is the client's own default"
    );
}

#[test]
fn a_profile_literal_has_no_place_to_put_a_secret_value() {
    // Compile-time shape check: these are all the fields a profile has, and
    // each one is either a name, a kind, a URL or a number — never a key value.
    fn fields(profile: &ProviderProfile) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            profile.name,
            profile.kind,
            profile.base_url.as_deref().unwrap_or(""),
            profile.api_key_env.as_deref().unwrap_or(""),
            profile.timeout_ms
        )
    }
    let profile = ProviderProfile {
        name: "anthropic".to_string(),
        kind: ProviderKind::Anthropic,
        base_url: None,
        api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
        timeout_ms: 30_000,
    };
    assert_eq!(
        fields(&profile),
        "anthropic|anthropic||ANTHROPIC_API_KEY|30000"
    );
}
