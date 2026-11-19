//! Shared secret redaction and canonical argument hashing.
//!
//! Both helpers used to live behind other modules (`api::approval_api` and
//! `services::approval`). They are collected here so the approval HTTP surface,
//! the approval gate and the execution store all use **one** implementation:
//! a second copy of either helper could drift and leak a secret or mint a
//! different hash for the same arguments.
//!
//! Redaction is recursive over objects and arrays; hashing is SHA-256 over
//! `serde_json::to_vec` of a `Value`, whose object keys are sorted (the
//! `preserve_order` feature is not enabled), so reordered input hashes equally.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

/// Replaces the value of every secret-looking field on the list endpoint.
pub const REDACTED_MARKER: &str = "[REDACTED]";

/// Substrings that mark a field name as credential-bearing. Matched
/// case-insensitively against the whole key, so `api_key`, `AccessToken` and
/// `Authorization` are all caught, not just the bare spellings.
const SECRET_KEY_FRAGMENTS: &[&str] = &["key", "token", "password", "secret", "authorization"];

/// True when a field name looks like it holds a credential.
fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_KEY_FRAGMENTS
        .iter()
        .any(|fragment| lower.contains(fragment))
}

/// Recursively replace secret-bearing fields with [`REDACTED_MARKER`].
///
/// An object key whose name matches a credential fragment is replaced wholesale
/// (its value is never inspected or copied), and every nested object and array
/// is walked so a secret buried several levels down cannot slip through.
pub fn redact_secrets(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let redacted: Map<String, Value> = map
                .iter()
                .map(|(key, child)| {
                    let child = if is_secret_key(key) {
                        Value::String(REDACTED_MARKER.to_string())
                    } else {
                        redact_secrets(child)
                    };
                    (key.clone(), child)
                })
                .collect();
            Value::Object(redacted)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact_secrets).collect()),
        other => other.clone(),
    }
}

/// Redact a structured trace payload while preserving its numeric token
/// counters. `redact_secrets` matches the `token` substring, which would mask
/// the numeric `input_tokens` / `output_tokens` fields every turn trace carries.
/// Those values are counts, never credentials, so they are restored from the
/// original value when (and only when) it is a number. A real secret — a string
/// under a key/token/password field — stays redacted. Both the trace emitter
/// and the SQLite store call this helper so the two cannot disagree about what
/// reaches disk.
pub fn redact_trace_payload(payload: &Value) -> Value {
    let mut redacted = redact_secrets(payload);
    for key in ["input_tokens", "output_tokens"] {
        if let Some(value) = payload.get(key) {
            if value.is_number() {
                redacted[key] = value.clone();
            }
        }
    }
    redacted
}

/// SHA-256 over the canonical JSON of the arguments.
///
/// `serde_json::Value` uses a `BTreeMap` for objects unless `preserve_order`
/// is enabled (it is not), so key order is sorted and a reordered payload
/// hashes identically. A nondeterministic hash would reject legitimate
/// approvals when the same arguments come back with different key order, and
/// would mint a different `operation_id` for an identical retried tool call.
pub fn args_hash(args: &Value) -> [u8; 32] {
    let canonical = serde_json::to_vec(args).expect("a serde_json::Value always serializes");
    let digest = Sha256::digest(&canonical);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// The lowercase hex form sent to (and compared from) HTTP clients and used as
/// the last component of a tool-call `operation_id`.
pub fn args_hash_hex(args: &Value) -> String {
    let mut hex = String::with_capacity(64);
    for byte in args_hash(args) {
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redaction_covers_nested_objects_and_arrays_case_insensitively() {
        let args = json!({
            "table": "users",
            "api_key": "sk-secret",
            "AccessToken": "abc",
            "PASSWORD": "hunter2",
            "my_SECRET_value": "s",
            "Authorization": "Bearer x",
            "nested": {
                "token": "t",
                "safe": { "note": "keep" },
                "list": [{"password": "p"}, {"ok": 1}, "plain"]
            }
        });
        let out = redact_secrets(&args);
        assert_eq!(out["table"], "users");
        assert_eq!(out["api_key"], REDACTED_MARKER);
        assert_eq!(out["AccessToken"], REDACTED_MARKER);
        assert_eq!(out["PASSWORD"], REDACTED_MARKER);
        assert_eq!(out["my_SECRET_value"], REDACTED_MARKER);
        assert_eq!(out["Authorization"], REDACTED_MARKER);
        assert_eq!(out["nested"]["token"], REDACTED_MARKER);
        assert_eq!(out["nested"]["safe"]["note"], "keep");
        assert_eq!(out["nested"]["list"][0]["password"], REDACTED_MARKER);
        assert_eq!(out["nested"]["list"][1]["ok"], 1);
        assert_eq!(out["nested"]["list"][2], "plain");
    }

    #[test]
    fn redaction_never_emits_the_original_secret_anywhere() {
        let args = json!({"key": "TOP-SECRET-VALUE", "outer": {"token": "ANOTHER-SECRET"}});
        let serialized = serde_json::to_string(&redact_secrets(&args)).unwrap();
        assert!(!serialized.contains("TOP-SECRET-VALUE"));
        assert!(!serialized.contains("ANOTHER-SECRET"));
    }

    #[test]
    fn trace_redaction_keeps_numeric_counters_but_hides_string_secrets() {
        let payload = json!({
            "input_tokens": 2_000_000,
            "output_tokens": 1000,
            "api_key": "sk-secret",
            "nested": {"token": "t"},
            "provider": "ollama"
        });
        let out = redact_trace_payload(&payload);
        assert_eq!(out["input_tokens"], 2_000_000);
        assert_eq!(out["output_tokens"], 1000);
        assert_eq!(out["api_key"], REDACTED_MARKER);
        assert_eq!(out["nested"]["token"], REDACTED_MARKER);
        assert_eq!(out["provider"], "ollama");
    }

    #[test]
    fn trace_redaction_never_restores_a_string_secret_under_counter_keys() {
        // The numeric-counter exception must be gated on `is_number()`: a
        // string under the same key is a credential and stays redacted.
        let payload = json!({
            "input_tokens": "sk-leak-input",
            "output_tokens": "sk-leak-output"
        });
        let out = redact_trace_payload(&payload);
        assert_eq!(out["input_tokens"], REDACTED_MARKER);
        assert_eq!(out["output_tokens"], REDACTED_MARKER);
    }

    #[test]
    fn args_hash_is_deterministic_for_reordered_keys() {
        let a = json!({"b": 2, "a": {"y": 1, "x": 0}});
        let b = json!({"a": {"x": 0, "y": 1}, "b": 2});
        assert_eq!(args_hash(&a), args_hash(&b));
        assert_eq!(args_hash_hex(&a), args_hash_hex(&b));
        assert_eq!(args_hash_hex(&a).len(), 64);
        assert_ne!(args_hash(&a), args_hash(&json!({"a": 1})));
    }
}
