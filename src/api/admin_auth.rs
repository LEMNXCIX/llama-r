//! Shared-token authentication for the admin endpoint group.
//!
//! The whole admin surface is guarded by one `from_fn_with_state` middleware
//! applied to one sub-router, which is then merged into the main router. That
//! single application point is what keeps the protected set exact: a new admin
//! route added to the group inherits the gate, and a public route in the other
//! group cannot acquire it by accident.
//!
//! The token is deliberately NOT on `AppState` in this task. Task 5 moves it
//! to `Config`; until then `admin_token_from_env` is the only reader, so Task 1
//! is self-contained and no config field or dependency is introduced.

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

/// Read the shared admin token from the environment.
///
/// A missing or empty value is `None`, which makes every admin route fail
/// closed. Reading it per request is cheap and keeps the gate in sync with a
/// rotated value without a restart; Task 5 replaces this helper with the
/// `Config` field while keeping the middleware signature unchanged.
pub fn admin_token_from_env() -> Option<String> {
    std::env::var("ADMIN_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
}

/// `from_fn` middleware guarding the admin router group.
///
/// `State` is unused in Task 1 but kept in the signature so Task 5 can read
/// `Config` from it without changing any wiring.
pub async fn require_admin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    // Task 1 reads the token from the environment, not from the state handle.
    let _ = &state;

    let presented = headers.get(ADMIN_TOKEN_HEADER);
    let accepted = match (admin_token_from_env(), presented) {
        (Some(expected), Some(actual)) => constant_time_eq(expected.as_bytes(), actual.as_bytes()),
        _ => false,
    };

    if !accepted {
        return AppError::Unauthorized(REFUSAL.to_string()).into_response();
    }

    next.run(request).await
}
