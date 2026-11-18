use crate::error::AppError;
use dotenvy::dotenv;
use std::env;
use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;

/// Default OpenRouter endpoint. Shared by the chat provider and the Rig engine
/// so the two cannot drift to different origins.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Which upstream serves chat, the decision model and (where supported)
/// embeddings. One value selects every service; there is no per-service mix.
///
/// This is a non-secret discriminator (it may be logged and shown in health
/// output). The secret-bearing counterpart is `EngineTarget`, which deliberately
/// does not implement `Debug`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProviderKind {
    #[default]
    Ollama,
    OpenRouter,
    OpenCode,
    OpenAiCompatible,
    Anthropic,
    Gemini,
}

/// The one canonical spelling of each kind. Used by `Display` and the error
/// message, so accepted values and their documentation cannot drift apart.
pub const PROVIDER_KIND_SPELLINGS: &[(&str, ProviderKind)] = &[
    ("ollama", ProviderKind::Ollama),
    ("openrouter", ProviderKind::OpenRouter),
    ("opencode", ProviderKind::OpenCode),
    ("openai-compatible", ProviderKind::OpenAiCompatible),
    ("anthropic", ProviderKind::Anthropic),
    ("gemini", ProviderKind::Gemini),
];

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (name, _) = PROVIDER_KIND_SPELLINGS
            .iter()
            .find(|(_, kind)| kind == self)
            .expect("every ProviderKind has a spelling");
        f.write_str(name)
    }
}

impl FromStr for ProviderKind {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let lower = value.trim().to_ascii_lowercase();
        if let Some((_, kind)) = PROVIDER_KIND_SPELLINGS
            .iter()
            .find(|(spelling, _)| *spelling == lower)
        {
            return Ok(*kind);
        }

        let accepted = PROVIDER_KIND_SPELLINGS
            .iter()
            .map(|(spelling, _)| *spelling)
            .collect::<Vec<_>>()
            .join(", ");
        // `value` is a provider name supplied by the caller, never a stored
        // credential: no key material is read or rendered here.
        Err(AppError::Config(format!(
            "PROVIDER must be one of {accepted}; received '{}'",
            value
        )))
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    /// The single provider used for chat, routing and embeddings.
    pub provider: ProviderKind,
    pub ollama_url: String,
    pub default_model: String,
    /// Embedding model for RAG (Ollama). Default: `nomic-embed-text`.
    pub embedding_model: String,
    /// Expected embedding vector size (must match model output).
    pub embedding_dimensions: usize,
    /// When false, RAG store is not built (chat works without retrieval).
    pub rag_enabled: bool,
    /// History retention period in days (default: 90).
    pub history_retention_days: u32,
    /// OpenRouter API key, used as a Bearer token. Never placed in a URL.
    pub openrouter_api_key: Option<String>,
    /// OpenCode server password, sent as HTTP basic auth. Never in a URL.
    pub opencode_api_key: Option<String>,
    /// Base URL for OpenCode's generate/decision endpoints.
    pub opencode_url: String,
    /// Optional override for OpenAI-compatible endpoint base URLs.
    pub openai_base_url: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, AppError> {
        dotenv().ok();

        let raw_port = env::var("PORT").unwrap_or_else(|_| "3000".to_string());
        let port = raw_port.parse().map_err(|_| {
            AppError::Config(format!(
                "PORT must be a valid u16 number, received '{}'",
                raw_port
            ))
        })?;

        // An absent or empty PROVIDER means the documented default. Any other
        // value is validated by `FromStr`, whose error names the value and the
        // six accepted spellings.
        let raw_provider = env::var("PROVIDER").unwrap_or_default();
        let provider = if raw_provider.trim().is_empty() {
            ProviderKind::default()
        } else {
            raw_provider.parse::<ProviderKind>()?
        };

        let ollama_url = env::var("OLLAMA_URL")
            .unwrap_or_else(|_| "http://localhost:11434".to_string())
            .trim()
            .to_string();
        if ollama_url.is_empty() {
            return Err(AppError::Config(
                "OLLAMA_URL cannot be empty when provided".to_string(),
            ));
        }

        let opencode_url = env::var("OPENCODE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:4096".to_string())
            .trim()
            .to_string();
        if opencode_url.is_empty() {
            return Err(AppError::Config(
                "OPENCODE_URL cannot be empty when provided".to_string(),
            ));
        }

        // Credentials are stored verbatim (no trimming): a key is opaque and
        // may legally contain `=` or spaces. Empty/absent means "not set".
        let openrouter_api_key = env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|value| !value.is_empty());
        let opencode_api_key = env::var("OPENCODE_API_KEY")
            .ok()
            .filter(|value| !value.is_empty());
        let openai_base_url = env::var("OPENAI_BASE_URL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        let default_model = env::var("DEFAULT_MODEL")
            .unwrap_or_else(|_| String::new())
            .trim()
            .to_string();

        let embedding_model = env::var("EMBEDDING_MODEL")
            .unwrap_or_else(|_| "nomic-embed-text".to_string())
            .trim()
            .to_string();
        if embedding_model.is_empty() {
            return Err(AppError::Config(
                "EMBEDDING_MODEL cannot be empty when provided".to_string(),
            ));
        }

        let raw_dims = env::var("EMBEDDING_DIMENSIONS").unwrap_or_else(|_| "768".to_string());
        let embedding_dimensions = raw_dims.parse().map_err(|_| {
            AppError::Config(format!(
                "EMBEDDING_DIMENSIONS must be a positive usize, received '{}'",
                raw_dims
            ))
        })?;
        if embedding_dimensions == 0 {
            return Err(AppError::Config(
                "EMBEDDING_DIMENSIONS must be greater than 0".to_string(),
            ));
        }

        let rag_enabled = env::var("RAG_ENABLED")
            .map(|value| {
                let lower = value.trim().to_ascii_lowercase();
                !(lower == "0" || lower == "false" || lower == "no" || lower == "off")
            })
            .unwrap_or(true);

        let history_retention_days = env::var("HISTORY_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90);

        Ok(Self {
            port,
            provider,
            ollama_url,
            default_model,
            embedding_model,
            embedding_dimensions,
            rag_enabled,
            history_retention_days,
            openrouter_api_key,
            opencode_api_key,
            opencode_url,
            openai_base_url,
        })
    }

    pub fn is_configured(&self) -> bool {
        !self.default_model.is_empty()
    }

    pub fn save_to_env(&self) -> Result<(), AppError> {
        let env_path = Path::new(".env");
        let mut content = if env_path.exists() {
            fs::read_to_string(env_path)?
        } else {
            String::new()
        };

        let updates = [
            ("PORT", self.port.to_string()),
            ("OLLAMA_URL", self.ollama_url.clone()),
            ("DEFAULT_MODEL", self.default_model.clone()),
            ("PROVIDER", self.provider.to_string()),
            (
                "OPENROUTER_API_KEY",
                self.openrouter_api_key.clone().unwrap_or_default(),
            ),
            (
                "OPENCODE_API_KEY",
                self.opencode_api_key.clone().unwrap_or_default(),
            ),
            ("OPENCODE_URL", self.opencode_url.clone()),
            (
                "OPENAI_BASE_URL",
                self.openai_base_url.clone().unwrap_or_default(),
            ),
        ];

        for (key, value) in &updates {
            // The process env always receives the raw value; only the file
            // representation is quoted, so a same-process reload sees the exact
            // value the caller supplied.
            let line_value = env_value(value);
            let mut found = false;
            let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
            for line in &mut lines {
                if line.starts_with(&format!("{}=", key)) {
                    *line = format!("{}={}", key, line_value);
                    found = true;
                    break;
                }
            }
            if found {
                content = lines.join("\n");
            } else {
                if !content.is_empty() && !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push_str(&format!("{}={}", key, line_value));
            }
            env::set_var(key, value);
        }

        fs::write(env_path, &content)?;

        // Secrets live in this file, so keep it private. Best effort: a
        // restrictive chmod failing (e.g. on a filesystem without permissions)
        // must never turn a successful save into an error.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(err) = fs::set_permissions(env_path, fs::Permissions::from_mode(0o600)) {
                tracing::warn!(
                    path = %env_path.display(),
                    error = %err,
                    "Could not restrict .env permissions to 0600"
                );
            }
        }

        Ok(())
    }
}

/// Render a value for a `.env` line. dotenvy treats whitespace, `#`, quotes,
/// backslashes and `$` specially in an unquoted value — a value such as
/// `ab=c d` fails to parse — so those values are written as a double-quoted
/// string with the characters dotenvy interprets escaped. Values with none of
/// those characters keep their existing unquoted spelling.
fn env_value(value: &str) -> String {
    let needs_quotes = value
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '#' | '\'' | '"' | '\\' | '$'));
    if !needs_quotes {
        return value.to_string();
    }

    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        match ch {
            '\\' | '\'' | '"' | '$' => {
                quoted.push('\\');
                quoted.push(ch);
            }
            '\n' => quoted.push_str("\\n"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG_ENV_VARS: &[&str] = &[
        "PORT",
        "OLLAMA_URL",
        "DEFAULT_MODEL",
        "EMBEDDING_MODEL",
        "EMBEDDING_DIMENSIONS",
        "RAG_ENABLED",
        "HISTORY_RETENTION_DAYS",
        "PROVIDER",
        "OPENROUTER_API_KEY",
        "OPENCODE_API_KEY",
        "OPENCODE_URL",
        "OPENAI_BASE_URL",
    ];

    /// Clears every config env var on entry and restores them on drop. Held
    /// together with `lock_env_for_tests`, this gives each test the documented
    /// defaults and stops a secret from leaking into the next test.
    struct ConfigEnvGuard {
        previous: Vec<(&'static str, Option<String>)>,
    }

    impl ConfigEnvGuard {
        fn cleared() -> Self {
            let previous = CONFIG_ENV_VARS
                .iter()
                .map(|name| (*name, env::var(name).ok()))
                .collect();
            for name in CONFIG_ENV_VARS {
                env::remove_var(name);
            }
            Self { previous }
        }
    }

    impl Drop for ConfigEnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.previous {
                match value {
                    Some(value) => env::set_var(name, value),
                    None => env::remove_var(name),
                }
            }
        }
    }

    /// Points the process at a fresh temp directory so `save_to_env` writes to
    /// a throwaway `.env` and never the repository's real one. Restores CWD on
    /// drop, even on panic.
    struct CwdGuard {
        previous: std::path::PathBuf,
        _temp: tempfile::TempDir,
    }

    impl CwdGuard {
        fn fresh() -> Self {
            let previous = env::current_dir().expect("read current dir");
            let temp = tempfile::tempdir().expect("create temp dir");
            env::set_current_dir(temp.path()).expect("enter temp dir");
            Self {
                previous,
                _temp: temp,
            }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            env::set_current_dir(&self.previous).expect("restore current dir");
        }
    }

    #[test]
    fn provider_defaults_to_ollama() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        let config = Config::from_env().expect("config from a cleared env");
        assert_eq!(config.provider, ProviderKind::Ollama);
    }

    #[test]
    fn provider_parses_all_six_kinds() {
        let cases = [
            ("ollama", ProviderKind::Ollama),
            ("openrouter", ProviderKind::OpenRouter),
            ("opencode", ProviderKind::OpenCode),
            ("openai-compatible", ProviderKind::OpenAiCompatible),
            ("anthropic", ProviderKind::Anthropic),
            ("gemini", ProviderKind::Gemini),
        ];
        for (spelling, kind) in cases {
            assert_eq!(spelling.parse::<ProviderKind>().unwrap(), kind);
            assert_eq!(kind.to_string(), spelling);
        }
        // Display is the canonical spelling and parsing ignores case.
        assert_eq!(
            "OpenRouter".parse::<ProviderKind>().unwrap(),
            ProviderKind::OpenRouter
        );
    }

    #[test]
    fn an_unknown_provider_is_a_config_error_naming_the_value() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        env::set_var("PROVIDER", "totally-not-a-provider");
        let err = Config::from_env().unwrap_err();
        let message = err.to_string();
        assert!(matches!(err, AppError::Config(_)));
        assert!(
            message.contains("totally-not-a-provider"),
            "error must name the bad value: {message}"
        );
        for spelling in [
            "ollama",
            "openrouter",
            "opencode",
            "openai-compatible",
            "anthropic",
            "gemini",
        ] {
            assert!(
                message.contains(spelling),
                "error must list {spelling} as accepted: {message}"
            );
        }
    }

    #[test]
    fn save_to_env_round_trips_a_key_with_equals_and_spaces() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        let config = Config {
            port: 3000,
            ollama_url: "http://localhost:11434".to_string(),
            default_model: "some-model".to_string(),
            embedding_model: "nomic-embed-text".to_string(),
            embedding_dimensions: 768,
            rag_enabled: true,
            history_retention_days: 90,
            provider: ProviderKind::OpenRouter,
            openrouter_api_key: Some("ab=c d".to_string()),
            opencode_api_key: None,
            opencode_url: "http://127.0.0.1:4096".to_string(),
            openai_base_url: None,
        };

        config.save_to_env().expect("save config");

        // save_to_env sets the raw value on the process env, so a same-process
        // reload reads it back unchanged.
        assert_eq!(env::var("OPENROUTER_API_KEY").unwrap(), "ab=c d");
        assert_eq!(
            Config::from_env()
                .expect("reload config")
                .openrouter_api_key
                .as_deref(),
            Some("ab=c d")
        );

        // On restart dotenvy parses the file. It rejects an unquoted value with
        // a space, so the saved line must be quoted to survive that parse.
        let parsed: Vec<(String, String)> = dotenvy::from_path_iter(".env")
            .expect("open .env")
            .map(|item| item.expect("parse .env line"))
            .collect();
        assert!(
            parsed
                .iter()
                .any(|(key, value)| key == "OPENROUTER_API_KEY" && value == "ab=c d"),
            "the saved value must round-trip through dotenvy: {parsed:?}"
        );
    }
}
