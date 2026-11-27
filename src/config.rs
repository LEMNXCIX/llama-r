use crate::error::AppError;
use dotenvy::dotenv;
use std::env;
use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

/// Default OpenRouter endpoint. Shared by the chat provider and the Rig engine
/// so the two cannot drift to different origins.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

// Defaults for the admin / approval-parking / OTLP settings. Defined here so
// `Config` is the single source of truth: the gate, reaper and exporter derive
// their `Duration`s from these numbers instead of reading the environment
// themselves.
pub const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_PARK_GRACE_MS: u64 = 1000;
pub const DEFAULT_REAPER_INTERVAL_SECS: u64 = 5;
pub const DEFAULT_OTEL_ENDPOINT: &str = "http://127.0.0.1:4318/v1/traces";
pub const DEFAULT_OTEL_SERVICE_NAME: &str = "llama-r";

/// Default outbound HTTP request timeout, in milliseconds. Applied by the
/// shared client factory, never read from the environment by an adapter.
pub const DEFAULT_HTTP_TIMEOUT_MS: u64 = 30000;
/// Default CORS allowlist: loopback only. A wildcard is never a default and is
/// rejected outright rather than silently widened.
pub const DEFAULT_CORS_ORIGINS: &str = "http://localhost,http://127.0.0.1";

/// The documented default allowlist, derived from `DEFAULT_CORS_ORIGINS` so the
/// constant and the parsed list cannot drift apart.
fn default_cors_origins() -> Vec<String> {
    DEFAULT_CORS_ORIGINS
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse `CORS_ORIGINS` as a comma-separated allowlist. Blank entries are
/// dropped; an empty list (absent, empty or only separators) means the
/// documented default. `*` is refused rather than treated as "any origin", and
/// every entry must be a valid header value so an unrepresentable origin fails
/// at boot instead of at the CORS layer. Whitespace inside an entry is refused
/// for the same reason: `HeaderValue::from_str` accepts visible ASCII, so
/// `not a url` would otherwise pass here and be rejected later by
/// `net::cors_layer`, making boot abort on a value config load accepted.
fn cors_origins_from_env() -> Result<Vec<String>, AppError> {
    let Ok(raw) = env::var("CORS_ORIGINS") else {
        return Ok(default_cors_origins());
    };

    let origins: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(str::to_string)
        .collect();
    if origins.is_empty() {
        return Ok(default_cors_origins());
    }

    for origin in &origins {
        if origin == "*" {
            return Err(AppError::Config(
                "CORS_ORIGINS cannot contain '*'".to_string(),
            ));
        }
        let invalid = || {
            AppError::Config(format!(
                "CORS_ORIGINS contains an invalid origin: '{origin}'"
            ))
        };
        // An origin is a scheme, a host and an optional port with no whitespace
        // anywhere, so an interior space (or an empty entry) is refused here
        // under the same message `net::cors_layer` uses. Otherwise config load
        // would accept what that factory rejects, turning a config error into a
        // boot abort.
        if origin.is_empty() || origin.chars().any(char::is_whitespace) {
            return Err(invalid());
        }
        // An origin is a header value: validating here rejects control
        // characters before the CORS layer ever sees them.
        axum::http::HeaderValue::from_str(origin).map_err(|_| invalid())?;
    }

    Ok(origins)
}

/// The four spellings treated as `true` by the boolean settings. Anything else
/// (including an absent variable, which is handled by the caller's default) is
/// `false`.
fn env_truthy(name: &str, default: bool) -> bool {
    match env::var(name) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => default,
    }
}

/// Parse an unsigned env var, or return its default when unset. A malformed
/// value is a config error naming both the variable and what was received —
/// no silent clamp. `0` is a valid value here; callers that need a positive
/// number apply their own documented fallback.
fn u64_env(name: &str, default: u64) -> Result<u64, AppError> {
    match env::var(name) {
        Ok(raw) => raw.trim().parse::<u64>().map_err(|_| {
            AppError::Config(format!(
                "{name} must be a valid non-negative integer, received '{}'",
                raw
            ))
        }),
        Err(_) => Ok(default),
    }
}

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
    /// Shared token guarding the admin endpoint group. `None` (missing or
    /// empty) fails every admin route closed. Never logged or echoed.
    pub admin_token: Option<String>,
    /// When true, a pending human approval never expires (the bridge awaits
    /// its oneshot with no deadline). Defaults to true.
    pub approval_park: bool,
    /// Bounded approval deadline in seconds, used when parking is off. `0` is
    /// the other spelling of "no deadline".
    pub approval_timeout_secs: u64,
    /// How long the HTTP handler waits before acknowledging a parked turn.
    pub park_grace_ms: u64,
    /// How often the boot reaper checks for decided orphaned turns. A
    /// non-positive value falls back to the default.
    pub reaper_interval_secs: u64,
    /// Opt-in OTLP/HTTP span exporter. Off by default, so an update that ships
    /// it changes nothing until an operator enables it.
    pub otel_enabled: bool,
    /// OTLP/HTTP traces endpoint, read only on the enabled path.
    pub otel_endpoint: String,
    /// `service.name` resource attribute sent with each span.
    pub otel_service_name: String,
    /// Outbound HTTP request timeout in milliseconds. Must be greater than `0`;
    /// `0` (a client with no deadline) is refused at boot.
    pub http_timeout_ms: u64,
    /// Allowed CORS origins. Loopback by default; `*` is refused rather than
    /// widened, and an empty list means the default.
    pub cors_origins: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 3000,
            provider: ProviderKind::default(),
            ollama_url: "http://localhost:11434".to_string(),
            default_model: String::new(),
            embedding_model: "nomic-embed-text".to_string(),
            embedding_dimensions: 768,
            rag_enabled: true,
            history_retention_days: 90,
            openrouter_api_key: None,
            opencode_api_key: None,
            opencode_url: "http://127.0.0.1:4096".to_string(),
            openai_base_url: None,
            admin_token: None,
            approval_park: true,
            approval_timeout_secs: DEFAULT_APPROVAL_TIMEOUT_SECS,
            park_grace_ms: DEFAULT_PARK_GRACE_MS,
            reaper_interval_secs: DEFAULT_REAPER_INTERVAL_SECS,
            otel_enabled: false,
            otel_endpoint: DEFAULT_OTEL_ENDPOINT.to_string(),
            otel_service_name: DEFAULT_OTEL_SERVICE_NAME.to_string(),
            http_timeout_ms: DEFAULT_HTTP_TIMEOUT_MS,
            cors_origins: default_cors_origins(),
        }
    }
}

impl Config {
    /// The gate's effective bridge deadline: `None` parks the turn when
    /// `APPROVAL_PARK` is on or `APPROVAL_TIMEOUT_SECS` is zero, otherwise the
    /// bounded timeout. One place so boot, tests and docs cannot disagree.
    pub fn approval_timeout(&self) -> Option<Duration> {
        if self.approval_park || self.approval_timeout_secs == 0 {
            None
        } else {
            Some(Duration::from_secs(self.approval_timeout_secs))
        }
    }

    pub fn from_env() -> Result<Self, AppError> {
        dotenv().ok();

        // Secrets live in `.env`, so keep it private on boot too. Best effort:
        // a restrictive chmod failing (e.g. on a filesystem without permissions)
        // must never turn a successful load into an error, and the path — never
        // any secret content — is the only thing named in the warning.
        #[cfg(unix)]
        {
            let base_env = crate::core::paths::get_base_dir().join(".env");
            restrict_env_permissions(&base_env);
            let cwd_env = Path::new(".env");
            if cwd_env != base_env.as_path() && cwd_env.exists() {
                restrict_env_permissions(cwd_env);
            }
        }

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

        // An explicitly empty OLLAMA_URL means "no Ollama": it skips the
        // Ollama embeddings rung so an OpenAI-compatible fallback can be
        // selected. An unset variable keeps the documented localhost default.
        let ollama_url = match env::var("OLLAMA_URL") {
            Ok(value) => value.trim().to_string(),
            Err(_) => "http://localhost:11434".to_string(),
        };

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

        // Admin token: a secret stored verbatim, exactly like the other keys.
        // Empty or absent means "not configured" — every admin route fails
        // closed. It is written to `.env` (0600) by `save_to_env`, never logged.
        let admin_token = env::var("ADMIN_TOKEN")
            .ok()
            .filter(|value| !value.is_empty());

        let approval_park = env_truthy("APPROVAL_PARK", true);
        let approval_timeout_secs =
            u64_env("APPROVAL_TIMEOUT_SECS", DEFAULT_APPROVAL_TIMEOUT_SECS)?;
        let park_grace_ms = u64_env("PARK_GRACE_MS", DEFAULT_PARK_GRACE_MS)?;
        // The reaper is the one numeric exception to "malformed is an error":
        // any non-positive value (including a negative one, which `u64` cannot
        // parse at all) keeps the default, matching the old helper. A value
        // that is not an integer is still a config error naming the variable.
        let reaper_interval_secs = match env::var("REAPER_INTERVAL_SECS") {
            Ok(raw) => {
                let seconds = raw.trim().parse::<i64>().map_err(|_| {
                    AppError::Config(format!(
                        "REAPER_INTERVAL_SECS must be a valid integer, received '{}'",
                        raw
                    ))
                })?;
                if seconds <= 0 {
                    DEFAULT_REAPER_INTERVAL_SECS
                } else {
                    u64::try_from(seconds).unwrap_or(DEFAULT_REAPER_INTERVAL_SECS)
                }
            }
            Err(_) => DEFAULT_REAPER_INTERVAL_SECS,
        };

        let otel_enabled = env_truthy("OTEL_ENABLED", false);
        let otel_endpoint = env::var("OTEL_ENDPOINT")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_OTEL_ENDPOINT.to_string());
        let otel_service_name = env::var("OTEL_SERVICE_NAME")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_OTEL_SERVICE_NAME.to_string());

        // A request with no deadline can hang a connection forever, so `0` is
        // a configuration error rather than a silent "unbounded" spelling.
        let http_timeout_ms = u64_env("HTTP_TIMEOUT_MS", DEFAULT_HTTP_TIMEOUT_MS)?;
        if http_timeout_ms == 0 {
            return Err(AppError::Config(
                "HTTP_TIMEOUT_MS must be greater than 0".to_string(),
            ));
        }
        let cors_origins = cors_origins_from_env()?;

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
            admin_token,
            approval_park,
            approval_timeout_secs,
            park_grace_ms,
            reaper_interval_secs,
            otel_enabled,
            otel_endpoint,
            otel_service_name,
            http_timeout_ms,
            cors_origins,
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
            ("ADMIN_TOKEN", self.admin_token.clone().unwrap_or_default()),
            ("APPROVAL_PARK", self.approval_park.to_string()),
            (
                "APPROVAL_TIMEOUT_SECS",
                self.approval_timeout_secs.to_string(),
            ),
            ("PARK_GRACE_MS", self.park_grace_ms.to_string()),
            (
                "REAPER_INTERVAL_SECS",
                self.reaper_interval_secs.to_string(),
            ),
            ("OTEL_ENABLED", self.otel_enabled.to_string()),
            ("OTEL_ENDPOINT", self.otel_endpoint.clone()),
            ("OTEL_SERVICE_NAME", self.otel_service_name.clone()),
            ("HTTP_TIMEOUT_MS", self.http_timeout_ms.to_string()),
            ("CORS_ORIGINS", self.cors_origins.join(",")),
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
        restrict_env_permissions(env_path);

        Ok(())
    }
}

/// Restrict the permissions of the file at `path` to `0600` on Unix. Best
/// effort: a filesystem without permission support (or any other I/O error)
/// must never turn a successful save/load into an error. The warning names only
/// the path, never the file's contents.
#[cfg(unix)]
pub fn restrict_env_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if !path.exists() {
        return;
    }
    if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        tracing::warn!(
            path = %path.display(),
            error = %err,
            "Could not restrict .env permissions to 0600"
        );
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
        "ADMIN_TOKEN",
        "APPROVAL_PARK",
        "APPROVAL_TIMEOUT_SECS",
        "PARK_GRACE_MS",
        "REAPER_INTERVAL_SECS",
        "OTEL_ENABLED",
        "OTEL_ENDPOINT",
        "OTEL_SERVICE_NAME",
        "HTTP_TIMEOUT_MS",
        "CORS_ORIGINS",
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
    fn ollama_url_distinguishes_explicit_empty_from_unset() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        // Unset keeps the documented localhost default.
        let config = Config::from_env().expect("config from a cleared env");
        assert_eq!(config.ollama_url, "http://localhost:11434");

        // Explicitly empty is valid and means "no Ollama", so the
        // OpenAI-compatible embeddings fallback can be selected.
        env::set_var("OLLAMA_URL", "");
        let config = Config::from_env().expect("explicit empty OLLAMA_URL is valid");
        assert_eq!(config.ollama_url, "");

        // Whitespace-only is an explicit empty too, matching the parser's trim.
        env::set_var("OLLAMA_URL", "   ");
        let config = Config::from_env().expect("whitespace-only OLLAMA_URL is valid");
        assert_eq!(config.ollama_url, "");

        // A normal URL is still accepted and trimmed as before.
        env::set_var("OLLAMA_URL", "  http://example:11434  ");
        let config = Config::from_env().expect("normal OLLAMA_URL is valid");
        assert_eq!(config.ollama_url, "http://example:11434");
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
            ..Config::default()
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

    // ---- Task 5: admin / parking / OTLP settings ----------------------------

    /// Every new variable is in the guard, so `ConfigEnvGuard::cleared` really
    /// does give each test the documented defaults instead of a leaked value.
    #[test]
    fn config_env_guard_covers_every_new_variable() {
        for name in [
            "ADMIN_TOKEN",
            "APPROVAL_PARK",
            "APPROVAL_TIMEOUT_SECS",
            "PARK_GRACE_MS",
            "REAPER_INTERVAL_SECS",
            "OTEL_ENABLED",
            "OTEL_ENDPOINT",
            "OTEL_SERVICE_NAME",
            "HTTP_TIMEOUT_MS",
            "CORS_ORIGINS",
        ] {
            assert!(
                CONFIG_ENV_VARS.contains(&name),
                "{name} missing from CONFIG_ENV_VARS"
            );
        }
    }

    #[test]
    fn new_env_fields_have_the_documented_defaults() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        let config = Config::from_env().expect("config from a cleared env");
        assert_eq!(config.admin_token, None);
        assert!(config.approval_park, "APPROVAL_PARK defaults to true");
        assert_eq!(config.approval_timeout_secs, 120);
        assert_eq!(config.park_grace_ms, 1000);
        assert_eq!(config.reaper_interval_secs, 5);
        assert!(!config.otel_enabled);
        assert_eq!(config.otel_endpoint, DEFAULT_OTEL_ENDPOINT);
        assert_eq!(config.otel_service_name, DEFAULT_OTEL_SERVICE_NAME);
        // Park is on by default, so the effective bridge deadline is unbounded.
        assert_eq!(config.approval_timeout(), None);
    }

    #[test]
    fn new_env_fields_parse_valid_values() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        env::set_var("ADMIN_TOKEN", "a-token");
        env::set_var("APPROVAL_PARK", "false");
        env::set_var("APPROVAL_TIMEOUT_SECS", "45");
        env::set_var("PARK_GRACE_MS", "250");
        env::set_var("REAPER_INTERVAL_SECS", "9");
        env::set_var("OTEL_ENABLED", "yes");
        env::set_var("OTEL_ENDPOINT", "http://collector:4318/v1/traces");
        env::set_var("OTEL_SERVICE_NAME", "my-service");

        let config = Config::from_env().expect("parse the new variables");
        assert_eq!(config.admin_token.as_deref(), Some("a-token"));
        assert!(!config.approval_park);
        assert_eq!(config.approval_timeout_secs, 45);
        assert_eq!(config.park_grace_ms, 250);
        assert_eq!(config.reaper_interval_secs, 9);
        assert!(config.otel_enabled);
        assert_eq!(config.otel_endpoint, "http://collector:4318/v1/traces");
        assert_eq!(config.otel_service_name, "my-service");
        assert_eq!(config.approval_timeout(), Some(Duration::from_secs(45)));
    }

    #[test]
    fn truthy_switches_accept_the_four_spellings_and_reject_others() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        for truthy in ["1", "true", "YES", "on"] {
            env::set_var("OTEL_ENABLED", truthy);
            assert!(
                Config::from_env().unwrap().otel_enabled,
                "{truthy} must be truthy"
            );
        }
        for falsy in ["0", "false", "no", "off", "maybe"] {
            env::set_var("OTEL_ENABLED", falsy);
            assert!(
                !Config::from_env().unwrap().otel_enabled,
                "{falsy} must be falsy"
            );
        }
        // APPROVAL_PARK uses the same truthy parser; absence, not a falsy
        // spelling, is what selects the `true` default.
        env::set_var("APPROVAL_PARK", "off");
        assert!(!Config::from_env().unwrap().approval_park);
    }

    #[test]
    fn malformed_numeric_variables_are_config_errors_naming_the_variable() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        for (name, bad) in [
            ("APPROVAL_TIMEOUT_SECS", "not-a-number"),
            ("PARK_GRACE_MS", "12.5"),
            ("REAPER_INTERVAL_SECS", "bogus"),
        ] {
            env::set_var(name, bad);
            let err = Config::from_env().unwrap_err();
            assert!(matches!(err, AppError::Config(_)), "{name}: {err}");
            let message = err.to_string();
            assert!(
                message.contains(name) && message.contains(bad),
                "error must name the variable and value: {message}"
            );
            env::remove_var(name);
        }
    }

    #[test]
    fn reaper_interval_non_positive_falls_back_to_the_default() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        // Numeric but non-positive keeps the default — matching the old helper,
        // so a bad value can never spin the reaper task or stop boot. A negative
        // value used to be a u64 parse error that aborted boot.
        for bad in ["0", "-1", "-999"] {
            env::set_var("REAPER_INTERVAL_SECS", bad);
            assert_eq!(
                Config::from_env().unwrap().reaper_interval_secs,
                5,
                "REAPER_INTERVAL_SECS={bad} must fall back to 5"
            );
        }
    }

    #[test]
    fn a_zero_approval_timeout_means_no_deadline() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        env::set_var("APPROVAL_PARK", "false");
        env::set_var("APPROVAL_TIMEOUT_SECS", "0");
        let config = Config::from_env().unwrap();
        assert_eq!(config.approval_timeout_secs, 0);
        assert_eq!(
            config.approval_timeout(),
            None,
            "zero is the other spelling of park"
        );
    }

    #[test]
    fn empty_admin_token_and_otel_labels_fall_back() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        env::set_var("ADMIN_TOKEN", "");
        env::set_var("OTEL_ENDPOINT", "   ");
        env::set_var("OTEL_SERVICE_NAME", "");
        let config = Config::from_env().unwrap();
        assert_eq!(config.admin_token, None, "an empty token is fail-closed");
        assert_eq!(config.otel_endpoint, DEFAULT_OTEL_ENDPOINT);
        assert_eq!(config.otel_service_name, DEFAULT_OTEL_SERVICE_NAME);
    }

    #[test]
    fn save_to_env_round_trips_every_new_field() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        let config = Config {
            admin_token: Some("secret-token".to_string()),
            approval_park: false,
            approval_timeout_secs: 45,
            park_grace_ms: 250,
            reaper_interval_secs: 9,
            otel_enabled: true,
            otel_endpoint: "http://collector:4318/v1/traces".to_string(),
            otel_service_name: "my-service".to_string(),
            ..Config::default()
        };

        config.save_to_env().expect("save config");
        let reloaded = Config::from_env().expect("reload config");
        assert_eq!(reloaded.admin_token.as_deref(), Some("secret-token"));
        assert!(!reloaded.approval_park);
        assert_eq!(reloaded.approval_timeout_secs, 45);
        assert_eq!(reloaded.park_grace_ms, 250);
        assert_eq!(reloaded.reaper_interval_secs, 9);
        assert!(reloaded.otel_enabled);
        assert_eq!(reloaded.otel_endpoint, "http://collector:4318/v1/traces");
        assert_eq!(reloaded.otel_service_name, "my-service");

        // The secret is on disk (mode 0600), never echoed in a log line here.
        let parsed: Vec<(String, String)> = dotenvy::from_path_iter(".env")
            .expect("open .env")
            .map(|item| item.expect("parse .env line"))
            .collect();
        assert!(parsed
            .iter()
            .any(|(key, value)| key == "ADMIN_TOKEN" && value == "secret-token"));
    }

    // ---- Fase 10-A: network hardening settings ------------------------------

    #[test]
    fn http_timeout_zero_is_a_config_error() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        env::set_var("HTTP_TIMEOUT_MS", "0");
        let err = Config::from_env().expect_err("zero timeout must be rejected");
        assert!(matches!(err, AppError::Config(_)), "{err}");
        assert!(err.to_string().contains("HTTP_TIMEOUT_MS"), "{err}");
    }

    #[test]
    fn http_timeout_defaults_to_30000_and_parses_a_positive_value() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        let config = Config::from_env().expect("config from a cleared env");
        assert_eq!(config.http_timeout_ms, DEFAULT_HTTP_TIMEOUT_MS);
        assert_eq!(config.http_timeout_ms, 30_000);

        env::set_var("HTTP_TIMEOUT_MS", "5000");
        assert_eq!(Config::from_env().unwrap().http_timeout_ms, 5000);
    }

    #[test]
    fn cors_origins_wildcard_is_a_config_error() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        env::set_var("CORS_ORIGINS", "*");
        let err = Config::from_env().expect_err("wildcard origin must be rejected");
        assert!(matches!(err, AppError::Config(_)), "{err}");
        assert!(err.to_string().contains("CORS_ORIGINS"), "{err}");
    }

    #[test]
    fn cors_origins_csv_is_parsed_and_trimmed() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        env::set_var(
            "CORS_ORIGINS",
            " http://localhost:8080 , https://a.example ",
        );
        let config = Config::from_env().expect("valid CSV");
        assert_eq!(
            config.cors_origins,
            vec!["http://localhost:8080", "https://a.example"]
        );
    }

    #[test]
    fn cors_origins_interior_whitespace_is_a_config_error() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        // `HeaderValue::from_str` accepts visible ASCII, so this entry has to be
        // refused structurally here: `net::cors_layer` refuses it too, and a
        // config load that accepted it would make boot abort.
        env::set_var("CORS_ORIGINS", "http://host with space");
        let err = Config::from_env().expect_err("interior whitespace must be rejected");
        assert!(matches!(err, AppError::Config(_)), "{err}");
        let message = err.to_string();
        assert!(message.contains("CORS_ORIGINS"), "{message}");
        assert!(
            message.contains("CORS_ORIGINS contains an invalid origin: 'http://host with space'"),
            "{message}"
        );
    }

    #[test]
    fn cors_origins_default_to_loopback() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        let config = Config::from_env().expect("config from a cleared env");
        assert_eq!(
            config.cors_origins,
            vec![
                "http://localhost".to_string(),
                "http://127.0.0.1".to_string()
            ]
        );

        // An empty or whitespace-only list is the documented default, not an
        // empty allowlist.
        for empty in ["", " , , "] {
            env::set_var("CORS_ORIGINS", empty);
            assert_eq!(
                Config::from_env().unwrap().cors_origins,
                vec![
                    "http://localhost".to_string(),
                    "http://127.0.0.1".to_string()
                ]
            );
        }
    }

    #[test]
    fn save_to_env_round_trips_http_timeout_and_cors_origins() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = ConfigEnvGuard::cleared();
        let _cwd = CwdGuard::fresh();

        let config = Config {
            http_timeout_ms: 5000,
            cors_origins: vec![
                "https://a.example".to_string(),
                "http://b.example".to_string(),
            ],
            ..Config::default()
        };

        config.save_to_env().expect("save config");
        let reloaded = Config::from_env().expect("reload config");
        assert_eq!(reloaded.http_timeout_ms, 5000);
        assert_eq!(
            reloaded.cors_origins,
            vec!["https://a.example", "http://b.example"]
        );
    }
}
