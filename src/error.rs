use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: ErrorInfo,
}

#[derive(Debug, Serialize)]
pub struct ErrorInfo {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Invalid input: {0}")]
    Validation(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Conflict: {0}")]
    Conflict(String),
    #[error("Provider error: {0}")]
    Provider(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    SerdeJson(#[from] serde_json::Error),
    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("HTTP client error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("Watcher error: {0}")]
    Notify(#[from] notify::Error),
    #[error("Runtime error: {0}")]
    Runtime(String),
}

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Config(_) => "config_error",
            Self::Validation(_) => "validation_error",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Provider(_) => "provider_error",
            Self::Io(_) => "io_error",
            Self::SerdeJson(_) => "serialization_error",
            Self::Toml(_) => "toml_error",
            Self::Reqwest(_) => "http_error",
            Self::Notify(_) => "watcher_error",
            Self::Runtime(_) => "runtime_error",
        }
    }

    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::Validation(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Provider(_) => StatusCode::BAD_GATEWAY,
            Self::Io(_)
            | Self::SerdeJson(_)
            | Self::Toml(_)
            | Self::Reqwest(_)
            | Self::Notify(_)
            | Self::Runtime(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn into_body(self) -> ErrorBody {
        ErrorBody {
            error: ErrorInfo {
                code: self.code(),
                message: self.to_string(),
            },
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = self.into_body();
        (status, Json(body)).into_response()
    }
}

impl From<Box<dyn std::error::Error + Send + Sync>> for AppError {
    fn from(value: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self::Runtime(value.to_string())
    }
}
