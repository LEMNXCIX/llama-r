//! Human-in-the-loop approval HTTP endpoints.
//!
//! Two surfaces for the one [`ApprovalGate`](crate::services::approval::
//! ApprovalGate) on `AppState`:
//! - `GET /api/approvals` lists undecided pending approvals so a non-streaming
//!   client (which has no SSE event carrying the id) can discover what to decide;
//! - `POST /api/approvals/:id` delivers the decision that unblocks the bridge.
//!
//! The streaming path gets the same id in an `ApprovalRequired` SSE event, but
//! both paths resolve through this gate. `AppState` owns the single instance and
//! a settings Apply rebuilds the engine from the same `Arc`, so a pending
//! decision survives a hot swap.
//!
//! No `X-Debug` header is required: these endpoints are the mechanism by which
//! a human answers a live approval, not an admin/debug view. Authentication and
//! authorization are deliberately deferred to Fase 7; today any caller that can
//! reach the API can list and decide pending approvals.
//!
//! # Secret handling
//! The gate stores the raw tool arguments so the list endpoint can show *what*
//! is being approved. Any field whose name looks like a credential is replaced
//! with [`REDACTED_MARKER`] before serialization — recursively, including inside
//! arrays — so this endpoint can never echo a key, token or password back to a
//! caller. The SSE event never carries raw arguments at all (only their hash).

use crate::api::handlers::AppState;
use crate::error::AppError;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

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

/// Decode the 64-character lowercase hex hash echoed back by an SSE client.
fn parse_hash_hex(hex: &str) -> Result<[u8; 32], AppError> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AppError::Validation(
            "args_hash must be 64 hexadecimal characters".into(),
        ));
    }
    let mut out = [0u8; 32];
    for (index, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(chunk).expect("hex bytes are ASCII");
        out[index] = u8::from_str_radix(pair, 16).expect("chunk already validated as hex");
    }
    Ok(out)
}

/// One pending decision as exposed by `GET /api/approvals`. Arguments are
/// redacted; the id is a string so the schema derives without a UUID feature.
#[derive(Debug, Serialize, ToSchema)]
pub struct PendingApprovalDto {
    pub id: String,
    pub tool_name: String,
    /// Tool arguments with secret-bearing fields replaced by [`REDACTED_MARKER`].
    pub args: Value,
    /// RFC 3339 creation timestamp.
    pub created_at: String,
}

/// The decision a human submits for one pending approval. `args_hash` is the
/// lowercase hex echoed from the SSE event; when omitted the gate's stored hash
/// is used, which is how a non-streaming client (with no SSE event) approves.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DecideRequest {
    pub approve: bool,
    #[serde(default)]
    pub args_hash: Option<String>,
}

/// Acknowledgement of a recorded decision.
#[derive(Debug, Serialize, ToSchema)]
pub struct DecideResponse {
    pub id: String,
    pub approved: bool,
}

/// `GET /api/approvals` — list undecided pending approvals, with secrets redacted.
#[utoipa::path(
    get,
    path = "/api/approvals",
    tag = "Approval",
    responses(
        (status = 200, description = "Undecided approvals, with secret-bearing argument fields redacted", body = Vec<PendingApprovalDto>),
    )
)]
pub async fn list_approvals(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<PendingApprovalDto>>, AppError> {
    state.observability.record_http_request();
    let pending = state.approval_gate.list_pending();
    let body = pending
        .into_iter()
        .map(|entry| PendingApprovalDto {
            id: entry.id.to_string(),
            tool_name: entry.tool_name,
            args: redact_secrets(&entry.args),
            created_at: entry.created_at.to_rfc3339(),
        })
        .collect();
    Ok(Json(body))
}

/// `POST /api/approvals/:id` — approve or deny one pending tool call.
///
/// - `200` decision recorded and the waiting bridge unblocked;
/// - `400` malformed hash, or a hash that no longer matches the pending args;
/// - `404` no such pending approval (unknown id);
/// - `409` the approval was already decided.
#[utoipa::path(
    post,
    path = "/api/approvals/{id}",
    tag = "Approval",
    request_body = DecideRequest,
    params(("id" = String, Path, description = "The pending approval id")),
    responses(
        (status = 200, description = "Decision recorded", body = DecideResponse),
        (status = 400, description = "Malformed or mismatched args_hash"),
        (status = 404, description = "No such pending approval"),
        (status = 409, description = "Approval was already decided"),
    )
)]
pub async fn decide_approval(
    State(state): State<Arc<AppState>>,
    Path(raw_id): Path<String>,
    Json(body): Json<DecideRequest>,
) -> Result<Json<DecideResponse>, AppError> {
    state.observability.record_http_request();
    let id = Uuid::parse_str(&raw_id)
        .map_err(|_| AppError::Validation(format!("invalid approval id '{raw_id}'")))?;
    let submitted = match body.args_hash {
        Some(hex) => parse_hash_hex(&hex)?,
        // Omitted: stay bound to the exact args on file rather than turning the
        // decision into an unchecked approval. An unknown/already-decided id
        // still surfaces the right error from `decide` below.
        None => state.approval_gate.stored_hash(id).unwrap_or([0u8; 32]),
    };
    state.approval_gate.decide(id, body.approve, submitted)?;
    Ok(Json(DecideResponse {
        id: id.to_string(),
        approved: body.approve,
    }))
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
    fn parse_hash_hex_validates_length_and_digits() {
        let hex = "ab".repeat(32);
        assert!(parse_hash_hex(&hex).is_ok());
        assert!(parse_hash_hex(&"AB".repeat(32)).is_ok());
        assert!(parse_hash_hex("abc").is_err(), "too short");
        assert!(
            parse_hash_hex(&format!("{}zz", "a".repeat(62))).is_err(),
            "non-hex"
        );
    }
}
