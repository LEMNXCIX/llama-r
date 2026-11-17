//! OpenCode decisions adapter: the experimental decisions endpoint at
//! `POST {base}/api/experimental/generate/decisions`.
//!
//! Where OpenRouter uses a Bearer key, OpenCode's optional server password is
//! sent as HTTP **basic auth** (`opencode:<password>`), matching the chat
//! adapter in `providers/opencode.rs`. The decision wire shape is shared with
//! the OpenRouter adapter (`decision_wire`), so the two differ only in origin
//! and auth.

use super::decision_wire;
use crate::error::AppError;
use crate::ports::router::{DecisionRequest, DecisionResponse, DecisionRouter};
use async_trait::async_trait;
use reqwest::{Client, RequestBuilder};
use std::collections::BTreeSet;
use std::time::Duration;

const DECISIONS_PATH: &str = "/api/experimental/generate/decisions";

/// d1 decisions behind an OpenCode server's experimental endpoint.
pub struct OpenCodeRouter {
    base_url: String,
    /// The server password, sent as basic auth. `None` means no password is set,
    /// in which case no Authorization header is sent at all.
    api_key: Option<String>,
    model: String,
    http: Client,
}

impl OpenCodeRouter {
    pub fn new(base_url: String, api_key: Option<String>, model: String, timeout_ms: u64) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            http,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}{DECISIONS_PATH}", self.base_url)
    }

    /// Attach basic auth only when a password was configured. A Bearer header
    /// is wrong for OpenCode and is never sent.
    fn with_auth(&self, builder: RequestBuilder) -> RequestBuilder {
        match &self.api_key {
            Some(password) => builder.basic_auth("opencode", Some(password)),
            None => builder,
        }
    }
}

#[async_trait]
impl DecisionRouter for OpenCodeRouter {
    async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, AppError> {
        tracing::debug!(
            model = %self.model,
            questions = req.questions.len(),
            "opencode decisions request"
        );

        let expected: BTreeSet<String> = req.questions.keys().cloned().collect();
        let body = decision_wire::encode_body(&self.model, &req);

        let response = self
            .with_auth(self.http.post(self.endpoint()).json(&body))
            .send()
            .await
            .map_err(|err| {
                AppError::Provider(format!("opencode decisions request failed: {err}"))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(AppError::Provider(format!(
                "opencode decisions HTTP {status}: {text}"
            )));
        }

        let value: serde_json::Value = response.json().await.map_err(|err| {
            AppError::Provider(format!("opencode decisions JSON parse failed: {err}"))
        })?;

        decision_wire::decode_response(&value, &expected)
    }
}
