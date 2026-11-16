//! Router configuration: the switch, the endpoint, and the two thresholds.
//!
//! Routing changes *who* answers a message, so the feature must be inert until
//! someone deliberately turns it on: `enabled` defaults to `false` and an
//! update that only ships this code changes nothing. Values arrive in two
//! layers — the environment for deployment, `{base}/router.toml` for
//! per-project tuning — where the file overrides only the keys it actually
//! sets, so a one-line `router.toml` cannot reset the endpoint back to a
//! default behind the operator's back.
//!
//! Thresholds are validated, never clamped: a `MIN_CONFIDENCE` of `1.4`
//! rejected at load reads as the mistake it is, while the same value silently
//! clamped would look configured and route every message to the fallback
//! agent. Same reason `ROUTER_TIMEOUT_MS` refuses 0 — a zero timeout fails
//! every decision, and that failure belongs at startup, not on the first chat.
//!
//! The env names are deliberately US-spelled where the struct is not:
//! `MIN_SPECIALIZED` (z) vs the field `min_specialised` (s), matching the
//! repo's existing mixed convention (`summarize_every_n_turns` next to
//! `synchronise`). The mismatch is not a typo.

use crate::error::AppError;
use serde::Deserialize;
use std::env;
use std::path::Path;

const DEFAULT_URL: &str = "http://127.0.0.1:8080";
const DEFAULT_MODEL: &str = "d1-3b";
const DEFAULT_TIMEOUT_MS: u64 = 3000;
const DEFAULT_MIN_CONFIDENCE: f32 = 0.60;
const DEFAULT_MIN_SPECIALISED: f32 = 0.50;

/// How the router is switched on, pointed at a decision model, and tuned.
///
/// Produced by [`RouterConfig::load`] and consumed by the router builder:
/// `url`, `model` and `timeout_ms` are exactly what
/// `LlamaCppRouter::new(base_url, model, timeout_ms)` takes.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    /// Master switch. Off by default: routing changes who answers, so an
    /// update that only ships this struct changes nothing.
    pub enabled: bool,
    /// Decision-model origin, e.g. llama-server's `http://127.0.0.1:8080`.
    pub url: String,
    /// Model name sent with each decision.
    pub model: String,
    /// Milliseconds one decision may take before the caller gives up.
    pub timeout_ms: u64,
    /// Below this confidence the message stays with the general agent.
    pub min_confidence: f32,
    /// Threshold a specialised candidate must reach to win over the general
    /// agent. Spelled with an `s` while its env var uses a `z` — see the
    /// module docs.
    pub min_specialised: f32,
    /// Agent ids routing must never select. File-only: there is no env var
    /// for a list, and a deployment that needs one can use `router.toml`.
    pub exclude: Vec<String>,
}

impl Default for RouterConfig {
    /// The documented table: inert until `enabled` says otherwise.
    fn default() -> Self {
        Self {
            enabled: false,
            url: DEFAULT_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            min_confidence: DEFAULT_MIN_CONFIDENCE,
            min_specialised: DEFAULT_MIN_SPECIALISED,
            exclude: Vec::new(),
        }
    }
}

impl RouterConfig {
    /// Read the environment on top of [`RouterConfig::default`].
    ///
    /// Absent variables keep their default; present ones are validated in the
    /// `PORT` error style — the message names the variable and the value
    /// received, so a bad deployment value is diagnosable from the log alone.
    /// `.env` is deliberately not loaded here: that is `Config::from_env`'s
    /// job at boot, and doing it twice would make this function depend on the
    /// caller's working directory.
    pub fn from_env() -> Result<Self, AppError> {
        // `RAG_ENABLED` idiom: the four negatives (trimmed, case-insensitive)
        // are false, any other present value is true. Absent means `false`,
        // not `true` — the router must not come alive on an update alone.
        let enabled = env::var("ROUTER_ENABLED")
            .map(|value| {
                let lower = value.trim().to_ascii_lowercase();
                !(lower == "0" || lower == "false" || lower == "no" || lower == "off")
            })
            .unwrap_or(false);

        let url = non_empty(
            "ROUTER_URL",
            &env::var("ROUTER_URL").unwrap_or_else(|_| DEFAULT_URL.to_string()),
        )?;
        let model = non_empty(
            "ROUTER_MODEL",
            &env::var("ROUTER_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
        )?;

        let raw_timeout =
            env::var("ROUTER_TIMEOUT_MS").unwrap_or_else(|_| DEFAULT_TIMEOUT_MS.to_string());
        let timeout_ms: u64 = raw_timeout.parse().map_err(|_| {
            AppError::Config(format!(
                "ROUTER_TIMEOUT_MS must be a valid u64 number, received '{}'",
                raw_timeout
            ))
        })?;
        if timeout_ms == 0 {
            return Err(AppError::Config(format!(
                "ROUTER_TIMEOUT_MS must be greater than 0, received '{}'",
                raw_timeout
            )));
        }

        let min_confidence = threshold_from_env("MIN_CONFIDENCE", DEFAULT_MIN_CONFIDENCE)?;
        let min_specialised = threshold_from_env("MIN_SPECIALIZED", DEFAULT_MIN_SPECIALISED)?;

        Ok(Self {
            enabled,
            url,
            model,
            timeout_ms,
            min_confidence,
            min_specialised,
            exclude: Vec::new(),
        })
    }

    /// Apply the keys `path` sets, leaving every other field as it is.
    ///
    /// The file is an optional overlay, not a second source of truth: a
    /// missing file is `Ok(())` because most deployments never write one and
    /// `load()` runs on every boot. A file that exists but cannot be read or
    /// parsed is [`AppError::Config`] naming the path — a broken tuning file
    /// must fail loudly at startup rather than degrade into "the router
    /// ignored my settings".
    pub fn overlay_toml(&mut self, path: &Path) -> Result<(), AppError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(AppError::Config(format!(
                    "{} could not be read: {}",
                    path.display(),
                    err
                )))
            }
        };
        let over: RouterOverrides = toml::from_str(&raw).map_err(|err| {
            AppError::Config(format!("{} is not valid TOML: {}", path.display(), err))
        })?;

        if let Some(enabled) = over.enabled {
            self.enabled = enabled;
        }
        if let Some(url) = over.url {
            self.url = file_non_empty(path, "url", &url)?;
        }
        if let Some(model) = over.model {
            self.model = file_non_empty(path, "model", &model)?;
        }
        if let Some(timeout_ms) = over.timeout_ms {
            if timeout_ms == 0 {
                return Err(AppError::Config(format!(
                    "{}: `timeout_ms` must be greater than 0",
                    path.display()
                )));
            }
            self.timeout_ms = timeout_ms;
        }
        if let Some(min_confidence) = over.min_confidence {
            self.min_confidence = file_threshold(path, "min_confidence", min_confidence)?;
        }
        if let Some(min_specialised) = over.min_specialised {
            self.min_specialised = file_threshold(path, "min_specialised", min_specialised)?;
        }
        if let Some(exclude) = over.exclude {
            self.exclude = exclude;
        }
        Ok(())
    }

    /// Environment first, then `router.toml` from the base directory on top.
    pub fn load() -> Result<Self, AppError> {
        let mut config = Self::from_env()?;
        config.overlay_toml(&crate::core::paths::get_router_config_path())?;
        Ok(config)
    }
}

/// Every key `router.toml` accepts, all optional so an absent key leaves the
/// current value alone. Unknown keys are ignored, as everywhere else in this
/// repo (`AgentConfig` does the same): a typo silently keeps the default —
/// which is why the defaults themselves are the safe ones.
#[derive(Debug, Default, Deserialize)]
struct RouterOverrides {
    enabled: Option<bool>,
    url: Option<String>,
    model: Option<String>,
    timeout_ms: Option<u64>,
    min_confidence: Option<f32>,
    min_specialised: Option<f32>,
    exclude: Option<Vec<String>>,
}

/// `PORT`-style rejection: name the variable, name the value received.
fn non_empty(name: &str, value: &str) -> Result<String, AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::Config(format!(
            "{} cannot be empty when provided",
            name
        )));
    }
    Ok(trimmed.to_string())
}

fn file_non_empty(path: &Path, key: &str, value: &str) -> Result<String, AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::Config(format!(
            "{}: `{}` cannot be empty",
            path.display(),
            key
        )));
    }
    Ok(trimmed.to_string())
}

/// Absent keeps the default; present must parse and sit inside the unit
/// interval. `MIN_SPECIALIZED`'s `z` against `min_specialised`'s `s` is the
/// deliberate mismatch documented at the top of this module.
fn threshold_from_env(name: &str, default: f32) -> Result<f32, AppError> {
    let Ok(raw) = env::var(name) else {
        return Ok(default);
    };
    let value: f32 = raw.parse().map_err(|_| {
        AppError::Config(format!(
            "{} must be a valid f32 number, received '{}'",
            name, raw
        ))
    })?;
    threshold_violation(value).map_or(Ok(value), |detail| {
        Err(AppError::Config(format!("{} {}", name, detail)))
    })
}

fn file_threshold(path: &Path, key: &str, value: f32) -> Result<f32, AppError> {
    threshold_violation(value).map_or(Ok(value), |detail| {
        Err(AppError::Config(format!(
            "{}: `{}` {}",
            path.display(),
            key,
            detail
        )))
    })
}

/// The shared complaint for a value outside `0.0 < v <= 1.0`. NaN lands here
/// too: every comparison on it is false, so it never passes the `if`.
fn threshold_violation(value: f32) -> Option<String> {
    if value > 0.0 && value <= 1.0 {
        None
    } else {
        Some(format!(
            "must be greater than 0.0 and at most 1.0, received '{}'",
            value
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Every variable [`RouterConfig::from_env`] reads, plus `LLAMA_R_DIR`
    /// (which [`RouterConfig::load`] reaches through `get_base_dir`).
    const ROUTER_ENV_VARS: &[&str] = &[
        "ROUTER_ENABLED",
        "ROUTER_URL",
        "ROUTER_MODEL",
        "ROUTER_TIMEOUT_MS",
        "MIN_CONFIDENCE",
        "MIN_SPECIALIZED",
        "LLAMA_R_DIR",
    ];

    /// Clears the router's environment for the duration of a test and puts
    /// back whatever was there when it drops.
    ///
    /// The lock alone only stops two tests from interleaving: a test that ends
    /// on `MIN_CONFIDENCE=0` would still be read by the next test as a
    /// *poisoned default*, not as interference. Clearing on entry and restoring
    /// on exit makes each test start from the documented defaults whatever
    /// order the harness picks.
    struct RouterEnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl RouterEnvGuard {
        fn cleared() -> Self {
            let saved = ROUTER_ENV_VARS
                .iter()
                .map(|name| (*name, std::env::var(name).ok()))
                .collect();
            for name in ROUTER_ENV_VARS {
                std::env::remove_var(name);
            }
            Self { saved }
        }
    }

    impl Drop for RouterEnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    #[test]
    fn router_is_disabled_when_the_env_is_silent() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = RouterEnvGuard::cleared();
        // The guard cleared ROUTER_ENABLED; with the variable absent the
        // default must be inert.
        let config = RouterConfig::from_env().unwrap();
        assert!(!config.enabled, "the default must be inert");
    }

    #[test]
    fn defaults_match_the_documented_table() {
        let config = RouterConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.url, "http://127.0.0.1:8080");
        assert_eq!(config.model, "d1-3b");
        assert_eq!(config.timeout_ms, 3000);
        assert_eq!(config.min_confidence, 0.60_f32);
        assert_eq!(config.min_specialised, 0.50_f32);
        assert!(config.exclude.is_empty());
    }

    #[test]
    fn enabled_reads_the_false_idiom() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = RouterEnvGuard::cleared();
        for value in ["0", "false", "no", "off", "OFF", " Off "] {
            std::env::set_var("ROUTER_ENABLED", value);
            assert!(
                !RouterConfig::from_env().unwrap().enabled,
                "value {value:?}"
            );
        }
        std::env::set_var("ROUTER_ENABLED", "true");
        assert!(RouterConfig::from_env().unwrap().enabled);
        // Present-but-empty is still "a value was given" — the `RAG_ENABLED`
        // idiom only recognises the four negatives as false.
        std::env::set_var("ROUTER_ENABLED", "");
        assert!(RouterConfig::from_env().unwrap().enabled);
    }

    #[test]
    fn a_threshold_outside_the_unit_interval_is_a_config_error() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = RouterEnvGuard::cleared();
        std::env::set_var("MIN_CONFIDENCE", "1.4");
        assert!(matches!(RouterConfig::from_env(), Err(AppError::Config(_))));
        std::env::set_var("MIN_CONFIDENCE", "not-a-number");
        assert!(matches!(RouterConfig::from_env(), Err(AppError::Config(_))));
        std::env::set_var("MIN_CONFIDENCE", "0");
        assert!(
            matches!(RouterConfig::from_env(), Err(AppError::Config(_))),
            "0 means trust nothing"
        );
        std::env::set_var("MIN_CONFIDENCE", "NaN");
        assert!(
            matches!(RouterConfig::from_env(), Err(AppError::Config(_))),
            "NaN fails every comparison, so it must not pass one"
        );
        // The sibling threshold is validated by the same code.
        std::env::remove_var("MIN_CONFIDENCE");
        std::env::set_var("MIN_SPECIALIZED", "2.0");
        assert!(matches!(RouterConfig::from_env(), Err(AppError::Config(_))));
    }

    #[test]
    fn router_toml_overrides_the_env_but_only_the_keys_it_sets() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("router.toml");
        std::fs::write(&path, "min_confidence = 0.8\nexclude = [\"legacy\"]\n").unwrap();

        let mut config = RouterConfig {
            min_confidence: 0.6,
            ..RouterConfig::default()
        };
        config.overlay_toml(&path).unwrap();
        assert_eq!(config.min_confidence, 0.8_f32);
        assert_eq!(config.exclude, vec!["legacy".to_string()]);
        // Untouched by the file, so still the default:
        assert_eq!(config.min_specialised, 0.50_f32);
        assert_eq!(config.url, "http://127.0.0.1:8080");
        assert_eq!(config.timeout_ms, 3000);
    }

    #[test]
    fn a_missing_router_toml_is_not_an_error() {
        let mut config = RouterConfig::default();
        assert!(config
            .overlay_toml(Path::new("/nonexistent/router.toml"))
            .is_ok());
    }

    #[test]
    fn a_malformed_router_toml_is_a_config_error_that_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("router.toml");
        std::fs::write(&path, "min_confidence = [").unwrap();

        let mut config = RouterConfig::default();
        let err = config.overlay_toml(&path).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "the error must name the file it came from: {err}"
        );
    }

    #[test]
    fn empty_url_or_model_and_a_zero_timeout_are_config_errors() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = RouterEnvGuard::cleared();

        std::env::set_var("ROUTER_URL", "   ");
        assert!(matches!(RouterConfig::from_env(), Err(AppError::Config(_))));
        std::env::remove_var("ROUTER_URL");

        std::env::set_var("ROUTER_MODEL", "");
        assert!(matches!(RouterConfig::from_env(), Err(AppError::Config(_))));
        std::env::remove_var("ROUTER_MODEL");

        std::env::set_var("ROUTER_TIMEOUT_MS", "0");
        assert!(
            matches!(RouterConfig::from_env(), Err(AppError::Config(_))),
            "a zero timeout would fail every decision"
        );
        std::env::set_var("ROUTER_TIMEOUT_MS", "not-a-number");
        assert!(matches!(RouterConfig::from_env(), Err(AppError::Config(_))));
    }

    #[test]
    fn load_reads_router_toml_from_the_base_dir() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let _env = RouterEnvGuard::cleared();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LLAMA_R_DIR", dir.path());
        std::fs::write(
            dir.path().join("router.toml"),
            "min_specialised = 0.75\nexclude = [\"legacy\"]\n",
        )
        .unwrap();

        let config = RouterConfig::load().unwrap();
        assert_eq!(config.min_specialised, 0.75_f32);
        assert_eq!(config.exclude, vec!["legacy".to_string()]);
        // Not in the file and not in the environment, so still the default:
        assert_eq!(config.url, "http://127.0.0.1:8080");
    }
}
