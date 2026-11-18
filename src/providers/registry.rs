//! Optional `providers.toml` profile registry.
//!
//! A profile describes *how to reach* an upstream: its kind, base URL, the
//! **name** of the environment variable holding its key, and a timeout. It
//! never holds a key value — that is read at client-construction time from
//! `api_key_env`, and stays out of logs, health responses and error messages.

use crate::config::{ProviderKind, OPENROUTER_BASE_URL};
use crate::core::paths::get_base_dir;
use crate::error::AppError;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::Path;

/// Timeout used when a profile does not set `timeout_ms`.
pub const DEFAULT_PROVIDER_TIMEOUT_MS: u64 = 30_000;

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
const DEFAULT_OPENCODE_URL: &str = "http://127.0.0.1:4096";
const REGISTRY_FILE: &str = "providers.toml";

/// One named upstream profile. `api_key_env` is a variable *name*
/// (e.g. `ANTHROPIC_API_KEY`), never the secret itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProfile {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub timeout_ms: u64,
}

/// The profiles available to a running process. Missing file → built-ins;
/// a file with profiles that reuse built-in names overlays only the fields it
/// actually sets.
#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    profiles: HashMap<String, ProviderProfile>,
}

impl ProviderRegistry {
    /// Read `{base_dir}/providers.toml`. A missing file is fine and yields the
    /// built-in profiles; an unreadable or malformed one is a configuration
    /// error that names the path, mirroring `RouterConfig::overlay_toml`.
    pub fn load() -> Result<Self, AppError> {
        let path = get_base_dir().join(REGISTRY_FILE);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::builtins()),
            Err(err) => {
                return Err(AppError::Config(format!(
                    "{} could not be read: {}",
                    path.display(),
                    err
                )));
            }
        };
        Self::from_toml(&raw, &path)
    }

    /// Look up a profile by the name used in `providers.toml` (or by the
    /// built-in name). Unknown names simply return `None`; the caller decides
    /// whether that is a named configuration error.
    pub fn get(&self, name: &str) -> Option<&ProviderProfile> {
        self.profiles.get(name)
    }

    fn from_toml(raw: &str, path: &Path) -> Result<Self, AppError> {
        let file: RegistryFile = toml::from_str(raw).map_err(|err| {
            AppError::Config(format!("{} is not valid TOML: {}", path.display(), err))
        })?;

        // Start from the built-ins so a small file that only tunes one profile
        // does not silently remove every other usable upstream.
        let mut profiles = Self::builtins().profiles;
        for (name, def) in file.providers {
            let builtin = profiles.get(&name).cloned();
            let profile = build_profile(&name, def, builtin.as_ref(), path)?;
            profiles.insert(name, profile);
        }
        Ok(Self { profiles })
    }

    fn builtins() -> Self {
        let definitions: [(&str, ProviderKind, Option<&str>); 6] = [
            ("ollama", ProviderKind::Ollama, None),
            (
                "openrouter",
                ProviderKind::OpenRouter,
                Some("OPENROUTER_API_KEY"),
            ),
            ("opencode", ProviderKind::OpenCode, Some("OPENCODE_API_KEY")),
            (
                "openai-compatible",
                ProviderKind::OpenAiCompatible,
                Some("OPENAI_API_KEY"),
            ),
            (
                "anthropic",
                ProviderKind::Anthropic,
                Some("ANTHROPIC_API_KEY"),
            ),
            ("gemini", ProviderKind::Gemini, Some("GEMINI_API_KEY")),
        ];

        let mut profiles = HashMap::new();
        for (name, kind, key_env) in definitions {
            profiles.insert(
                name.to_string(),
                ProviderProfile {
                    name: name.to_string(),
                    kind,
                    base_url: default_base_url(kind),
                    api_key_env: key_env.map(str::to_string),
                    timeout_ms: DEFAULT_PROVIDER_TIMEOUT_MS,
                },
            );
        }
        Self { profiles }
    }
}

/// The on-disk shape. Every key is optional so a file can set just the fields
/// it cares about; `type` is parsed through `ProviderKind::from_str`, the same
/// source of truth as `PROVIDER`, so the two accept identical spellings.
#[derive(Debug, Default, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    providers: BTreeMap<String, ProfileDef>,
}

#[derive(Debug, Default, Deserialize)]
struct ProfileDef {
    #[serde(rename = "type")]
    kind: Option<String>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    timeout_ms: Option<u64>,
}

fn build_profile(
    name: &str,
    def: ProfileDef,
    builtin: Option<&ProviderProfile>,
    path: &Path,
) -> Result<ProviderProfile, AppError> {
    let kind = match def.kind.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => raw.parse::<ProviderKind>().map_err(|err| {
            AppError::Config(format!(
                "{}: provider '{}' has an invalid type: {}",
                path.display(),
                name,
                err
            ))
        })?,
        None => builtin.map(|p| p.kind).unwrap_or_default(),
    };

    let base_url = def
        .base_url
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| builtin.and_then(|p| p.base_url.clone()))
        .or_else(|| default_base_url(kind));

    let api_key_env = def
        .api_key_env
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| builtin.and_then(|p| p.api_key_env.clone()));

    let timeout_ms = def.timeout_ms.unwrap_or_else(|| {
        builtin
            .map(|p| p.timeout_ms)
            .unwrap_or(DEFAULT_PROVIDER_TIMEOUT_MS)
    });
    if timeout_ms == 0 {
        return Err(AppError::Config(format!(
            "{}: provider '{}' has timeout_ms = 0, which must be greater than 0",
            path.display(),
            name
        )));
    }

    Ok(ProviderProfile {
        name: name.to_string(),
        kind,
        base_url,
        api_key_env,
        timeout_ms,
    })
}

/// Environment-derived defaults for a profile that does not name its own
/// base URL. Anthropic and Gemini use their clients' native endpoints, so
/// there is no URL to supply here.
fn default_base_url(kind: ProviderKind) -> Option<String> {
    match kind {
        ProviderKind::Ollama => {
            Some(env_non_empty("OLLAMA_URL").unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string()))
        }
        ProviderKind::OpenRouter => Some(
            env_non_empty("OPENAI_BASE_URL").unwrap_or_else(|| OPENROUTER_BASE_URL.to_string()),
        ),
        ProviderKind::OpenAiCompatible => env_non_empty("OPENAI_BASE_URL"),
        ProviderKind::OpenCode => {
            Some(env_non_empty("OPENCODE_URL").unwrap_or_else(|| DEFAULT_OPENCODE_URL.to_string()))
        }
        ProviderKind::Anthropic | ProviderKind::Gemini => None,
    }
}

fn env_non_empty(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const ENV_VARS: &[&str] = &[
        "LLAMA_R_DIR",
        "OLLAMA_URL",
        "OPENAI_BASE_URL",
        "OPENCODE_URL",
    ];

    /// Fresh base directory and cleared URL variables, restored on drop.
    struct BaseDirGuard {
        saved: Vec<(&'static str, Option<String>)>,
        dir: tempfile::TempDir,
    }

    impl BaseDirGuard {
        fn fresh() -> Self {
            let saved = ENV_VARS
                .iter()
                .map(|name| (*name, env::var(name).ok()))
                .collect();
            let dir = tempfile::tempdir().expect("create temp base dir");
            for name in ENV_VARS {
                env::remove_var(name);
            }
            env::set_var("LLAMA_R_DIR", dir.path());
            Self { saved, dir }
        }

        fn path(&self) -> PathBuf {
            self.dir.path().to_path_buf()
        }

        fn write(&self, contents: &str) {
            fs::write(self.path().join(REGISTRY_FILE), contents).expect("write registry");
        }
    }

    impl Drop for BaseDirGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                match value {
                    Some(value) => env::set_var(name, value),
                    None => env::remove_var(name),
                }
            }
        }
    }

    #[test]
    fn builtin_profiles_cover_every_kind_and_hold_no_key_values() {
        let _lock = crate::core::paths::lock_env_for_tests();
        let _env = BaseDirGuard::fresh();

        let registry = ProviderRegistry::load().expect("builtins with no file");
        for (name, kind) in crate::config::PROVIDER_KIND_SPELLINGS {
            let profile = registry.get(name).expect("built-in profile");
            assert_eq!(profile.kind, *kind);
            // With OPENAI_BASE_URL cleared, only the providers with a fixed
            // origin have a built-in URL; the others use their client defaults.
            let has_url = matches!(
                kind,
                ProviderKind::Ollama | ProviderKind::OpenRouter | ProviderKind::OpenCode
            );
            assert_eq!(profile.base_url.is_some(), has_url, "built-in {name} URL");
        }
        let dump = format!("{:?}", registry);
        assert!(!dump.contains("sk-"), "no key value in a registry: {dump}");
    }

    #[test]
    fn a_toml_profile_overlays_only_the_fields_it_sets() {
        let _lock = crate::core::paths::lock_env_for_tests();
        let _env = BaseDirGuard::fresh();
        _env.write(
            r#"
[providers.openrouter]
base_url = "http://example.test:9000/v1"
"#,
        );

        let registry = ProviderRegistry::load().expect("load overlay");
        let profile = registry.get("openrouter").expect("openrouter profile");
        assert_eq!(
            profile.kind,
            ProviderKind::OpenRouter,
            "type kept from builtin"
        );
        assert_eq!(
            profile.base_url.as_deref(),
            Some("http://example.test:9000/v1")
        );
        assert_eq!(profile.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
        assert_eq!(profile.timeout_ms, DEFAULT_PROVIDER_TIMEOUT_MS);
    }

    #[test]
    fn an_empty_base_url_uses_the_env_default() {
        let _lock = crate::core::paths::lock_env_for_tests();
        let _env = BaseDirGuard::fresh();
        env::set_var("OLLAMA_URL", "http://example.test:11434");
        _env.write(
            r#"
[providers.ollama]
base_url = "   "
"#,
        );

        let registry = ProviderRegistry::load().expect("load overlay");
        assert_eq!(
            registry.get("ollama").unwrap().base_url.as_deref(),
            Some("http://example.test:11434")
        );
    }
}
