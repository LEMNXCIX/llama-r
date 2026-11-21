//! Channel configuration loaded from the optional `channels.toml`.
//!
//! A channel routes an inbound webhook to an agent. `auth_env` is the NAME of
//! the environment variable holding the shared secret — never the value — so
//! this struct has no secret field and is safe to derive `Debug` on and to log.

use serde::{Deserialize, Serialize};

/// Delivery mode for a channel. Defaults to [`ChannelMode::Async`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChannelMode {
    /// Answer the webhook synchronously with the agent's reply.
    Sync,
    /// Acknowledge immediately and process the turn in the background.
    #[default]
    Async,
}

/// One inbound channel. The secret is referenced by `auth_env` only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub id: String,
    pub agent_id: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub mode: ChannelMode,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default = "default_signature_header")]
    pub signature_header: String,
    #[serde(default)]
    pub auth_env: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_signature_header() -> String {
    "X-Signature".to_string()
}

fn default_enabled() -> bool {
    true
}
