//! Shared-token authentication for the admin endpoint group.
//!
//! The whole admin surface is guarded by one `from_fn_with_state` middleware
//! applied to one sub-router, which is then merged into the main router. That
//! single application point is what keeps the protected set exact: a new admin
//! route added to the group inherits the gate, and a public route in the other
//! group cannot acquire it by accident.
//!
//! The token lives on `Config`, which `AppState` owns. Reading it from the
//! state handle (instead of the environment per request) keeps one source of
//! truth and means a rotated token takes effect exactly when the config is
//! reloaded — the same moment the rest of the stack is rebuilt.

use crate::adapters::channel::hmac::constant_time_eq;
use crate::api::handlers::AppState;
use crate::error::AppError;
use axum::{
    extract::{Request, State},
    http::HeaderMap,
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

/// The header a caller presents the shared token in. `HeaderMap` lookups are
/// case-insensitive, so the same constant serves the docs and the middleware.
pub const ADMIN_TOKEN_HEADER: &str = "X-Admin-Token";

/// The only refusal text. Fixed and generic on purpose: it says neither
/// whether the token was missing, empty or wrong, and never echoes a value —
/// not in the body, a log line or a response header.
const REFUSAL: &str = "admin authentication required";

/// `from_fn` middleware guarding the admin router group.
pub async fn require_admin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let presented = headers.get(ADMIN_TOKEN_HEADER);
    let accepted = match (state.config.admin_token.as_deref(), presented) {
        (Some(expected), Some(actual)) => constant_time_eq(expected.as_bytes(), actual.as_bytes()),
        _ => false,
    };

    if !accepted {
        return AppError::Unauthorized(REFUSAL.to_string()).into_response();
    }

    next.run(request).await
}
