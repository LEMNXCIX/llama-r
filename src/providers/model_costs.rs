//! Optional `models.toml` relative-cost table.
//!
//! The file maps a model id to an **ordinal rank** (`relative_cost: u32`),
//! never a price: a smaller rank means a simpler/cheaper model. The rank only
//! breaks ties inside a complexity tier during model selection; it is never
//! written into `ModelCapabilities` or reported as `cost_usd`. Credentials
//! have no place here — the file holds model names and integers only.
//!
//! A missing file is fine (empty table). An unreadable or malformed file is a
//! warning naming the path and an empty table: one bad file never blocks boot.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

const COSTS_FILE: &str = "models.toml";

/// Load `{base_dir}/models.toml` as `model → relative_cost`.
///
/// Returns an empty map on a missing, unreadable or malformed file; the last
/// two cases emit a warning naming the full path so an operator can find the
/// bad file without a crash.
pub fn load_relative_costs(base_dir: &Path) -> HashMap<String, u32> {
    let path = base_dir.join(COSTS_FILE);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "could not read model costs; ignoring the table"
            );
            return HashMap::new();
        }
    };

    match toml::from_str::<HashMap<String, u32>>(&raw) {
        Ok(costs) => costs,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "malformed model costs; ignoring the table"
            );
            HashMap::new()
        }
    }
}
