//! RAG `source_id` validation and path-safe encoding.
//!
//! Supported forms (opaque ids also allowed if they pass validation):
//! - `project:{project_id}/{collection}` — e.g. `project:fudi/policies`
//! - `agent:{agent_id}/memory` — e.g. `agent:nutricion/memory`
//! - `agent:{project}/{agent}/memory` — e.g. `agent:fudi/ops/memory`
//! - `global/{collection}` — e.g. `global/docs`
//! - short `app/col` — e.g. `fudi/policies`

/// Maximum allowed length for a source_id string.
pub const MAX_SOURCE_ID_LEN: usize = 256;

/// Reject empty, traversal, null, and control characters.
pub fn validate_source_id(source_id: &str) -> Result<(), String> {
    if source_id.is_empty() {
        return Err("empty source_id".into());
    }
    if source_id.len() > MAX_SOURCE_ID_LEN {
        return Err(format!(
            "source_id exceeds max length of {MAX_SOURCE_ID_LEN}"
        ));
    }
    if source_id.contains("..") {
        return Err("invalid source_id: path traversal ('..') not allowed".into());
    }
    if source_id.contains('\0') {
        return Err("invalid source_id: null byte not allowed".into());
    }
    if source_id.chars().any(|c| c.is_control()) {
        return Err("invalid source_id: control characters not allowed".into());
    }
    Ok(())
}

/// Encode a source_id into a single path segment safe for directories/files.
///
/// - `:` → `_`
/// - `/` → `__`
///
/// Examples:
/// - `fudi/policies` → `fudi__policies`
/// - `agent:fudi/ops/memory` → `agent_fudi__ops__memory`
pub fn encode_source_id_for_path(source_id: &str) -> String {
    source_id.replace(':', "_").replace('/', "__")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_empty_and_traversal() {
        assert!(validate_source_id("").is_err());
        assert!(validate_source_id("../etc").is_err());
        assert!(validate_source_id("a\0b").is_err());
        assert!(validate_source_id("agent:a/memory").is_ok());
        assert!(validate_source_id("fudi/policies").is_ok());
        assert!(validate_source_id("project:fudi/context").is_ok());
        assert!(validate_source_id("global/docs").is_ok());
    }

    #[test]
    fn namespace_encode_roundtrip_safe() {
        let cases = [
            ("fudi/policies", "fudi__policies"),
            ("agent:fudi/ops/memory", "agent_fudi__ops__memory"),
            ("agent:ops/memory", "agent_ops__memory"),
            ("project:fudi/context", "project_fudi__context"),
            ("global/docs", "global__docs"),
        ];
        for (input, expected) in cases {
            let encoded = encode_source_id_for_path(input);
            assert_eq!(encoded, expected);
            assert!(!encoded.contains('/'));
            assert!(!encoded.contains(':'));
            assert!(validate_source_id(input).is_ok());
        }
    }

    #[test]
    fn validate_source_id_rejects_traversal() {
        let err = validate_source_id("../etc/passwd").unwrap_err();
        assert!(err.contains("traversal") || err.contains(".."));
    }
}
