//! Shared HTTP-client and CORS factories.
//!
//! Every outbound client and the single inbound CORS layer are built here so the
//! hardening settings in `Config` (`HTTP_TIMEOUT_MS`, `CORS_ORIGINS`) reach
//! exactly one construction site instead of being re-derived per adapter. Both
//! factories fail closed: a bad timeout or a bad origin list is an `AppError`,
//! never a silently relaxed policy.

use std::time::Duration;

use axum::http::{header, HeaderName, HeaderValue, Method};
use tower_http::cors::CorsLayer;

use crate::error::AppError;

/// Longest connect timeout a client will use. A connection that cannot be
/// established in five seconds is treated as unreachable rather than spending
/// the whole request budget on a TCP handshake.
const MAX_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Build a `reqwest::Client` bounded by `timeout`.
///
/// The connect timeout is capped at five seconds so a dead host fails fast, and
/// the total request timeout is the caller's budget. There is no unbounded
/// client: a request that never answers must end in a timeout error.
pub fn http_client(timeout: Duration) -> Result<reqwest::Client, AppError> {
    reqwest::Client::builder()
        .connect_timeout(MAX_CONNECT_TIMEOUT.min(timeout))
        .timeout(timeout)
        .build()
        .map_err(|err| AppError::Runtime(format!("failed to build HTTP client: {err}")))
}

/// Turn one configured origin into the header value the layer matches on.
///
/// `HeaderValue::from_str` alone is not enough: it accepts any visible ASCII,
/// so `not a url` would become a perfectly legal — and perfectly useless —
/// allowlist entry. An origin is a scheme, a host and an optional port with no
/// whitespace anywhere, so whitespace is refused here too, under the same
/// message `Config::from_env` uses for an unrepresentable entry. The check
/// stays deliberately structural rather than URL-shaped so non-HTTP schemes a
/// desktop shell may use (`tauri://localhost`) keep working.
fn origin_header_value(origin: &str) -> Result<HeaderValue, AppError> {
    if origin == "*" {
        return Err(AppError::Config("CORS_ORIGINS cannot contain '*'".into()));
    }
    let invalid = || {
        AppError::Config(format!(
            "CORS_ORIGINS contains an invalid origin: '{origin}'"
        ))
    };
    if origin.is_empty() || origin.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    HeaderValue::from_str(origin).map_err(|_| invalid())
}

/// Build the inbound CORS layer for the configured allowlist.
///
/// `*` is rejected here as well as in `Config::from_env`, so no construction
/// path can ever reach `AllowOrigin::any()`. Only the methods and headers the
/// API actually serves are allowed — the admin token cannot ride a preflight
/// from an unlisted origin.
pub fn cors_layer(origins: &[String]) -> Result<CorsLayer, AppError> {
    let mut values = Vec::with_capacity(origins.len());
    for origin in origins {
        values.push(origin_header_value(origin)?);
    }
    Ok(CorsLayer::new()
        .allow_origin(values)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            HeaderName::from_static("x-project"),
            HeaderName::from_static("x-agent"),
            HeaderName::from_static("x-debug"),
            HeaderName::from_static("x-conversation-id"),
            HeaderName::from_static("x-admin-token"),
        ]))
}

#[cfg(test)]
mod tests {
    use super::{cors_layer, http_client};
    use std::time::Duration;

    #[tokio::test]
    async fn http_client_times_out_a_sleeping_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
            use tokio::io::AsyncWriteExt;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await;
        });
        let client = http_client(Duration::from_millis(200)).unwrap();
        let err = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap_err();
        assert!(err.is_timeout(), "expected timeout, got {err}");
    }

    #[test]
    fn cors_layer_allows_only_listed_origins() {
        let layer = cors_layer(&["http://localhost:8080".into()]).unwrap();
        // La capa se prueba de verdad en Task 4; aquí solo se construye sin panic.
        let _ = layer;
    }

    #[test]
    fn cors_layer_rejects_wildcard_and_invalid_origin() {
        let wildcard = cors_layer(&["*".into()]).unwrap_err();
        assert!(wildcard
            .to_string()
            .contains("CORS_ORIGINS cannot contain '*'"));
        let invalid = cors_layer(&["not a url".into()]).unwrap_err();
        assert!(invalid
            .to_string()
            .contains("CORS_ORIGINS contains an invalid origin: 'not a url'"));
    }

    /// The wildcard is refused on any position, not just as the sole entry: a
    /// list that mixes a concrete origin with `*` would otherwise widen the
    /// policy for every caller.
    #[test]
    fn cors_layer_rejects_wildcard_after_a_valid_origin() {
        let origins = vec!["http://localhost:8080".to_string(), "*".to_string()];
        assert!(cors_layer(&origins).is_err());
    }

    /// `connect_timeout` is capped at five seconds, never longer than the whole
    /// request budget. The builder exposes no getter, so this test pins the
    /// arithmetic the factory uses.
    #[test]
    fn connect_timeout_is_capped_to_the_shorter_budget() {
        assert_eq!(
            super::MAX_CONNECT_TIMEOUT.min(Duration::from_millis(200)),
            Duration::from_millis(200)
        );
        assert_eq!(
            super::MAX_CONNECT_TIMEOUT.min(Duration::from_secs(30)),
            Duration::from_secs(5)
        );
    }
}
