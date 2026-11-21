//! Loader for the optional `channels.toml` config.
//!
//! A missing file is fine (empty vec). An unreadable or malformed file is a
//! warning naming the FULL path and an empty vec: one bad file never blocks
//! boot, and no other state is touched.

use crate::domain::channel::ChannelConfig;
use serde::Deserialize;
use std::fs;
use std::path::Path;

const CHANNELS_FILE: &str = "channels.toml";

#[derive(Debug, Deserialize, Default)]
struct ChannelsFile {
    #[serde(default)]
    channels: Vec<ChannelConfig>,
}

/// Load `{base_dir}/channels.toml` as a list of channel configs.
///
/// Returns an empty vec on a missing, unreadable or malformed file; the last
/// two cases emit a warning naming the full path so an operator can find the
/// bad file without a crash.
pub fn load_channels(base_dir: &Path) -> Vec<ChannelConfig> {
    let path = base_dir.join(CHANNELS_FILE);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "could not read channel config; ignoring it"
            );
            return Vec::new();
        }
    };

    match toml::from_str::<ChannelsFile>(&raw) {
        Ok(file) => file.channels,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "malformed channel config; ignoring it"
            );
            Vec::new()
        }
    }
}
