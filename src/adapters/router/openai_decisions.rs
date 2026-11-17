//! OpenRouter decisions adapter: the Liquid AI d1 decisions endpoint hosted at
//! `POST https://openrouter.ai/api/alpha/decisions`.
//!
//! This is the only place in the crate that knows that URL or the decisions
//! wire shape. The API key travels only as `Authorization: Bearer` — never in a
//! URL, a body, or a log line — and every structural surprise from the upstream
//! becomes [`AppError::Provider`], which the routing ladder reads as
//! `RouterUnavailable` and degrades rather than treating as a decision.

use super::decision_wire;
use crate::error::AppError;
use crate::ports::router::{DecisionRequest, DecisionResponse, DecisionRouter};
use async_trait::async_trait;
use reqwest::Client;
use std::collections::BTreeSet;
use std::time::Duration;

const OPENROUTER_BASE: &str = "https://openrouter.ai";
const DECISIONS_PATH: &str = "/api/alpha/decisions";

/// d1 decisions behind OpenRouter's hosted alpha endpoint.
pub struct OpenRouterRouter {
    base_url: String,
    api_key: String,
    model: String,
    http: Client,
}

impl OpenRouterRouter {
    /// Production constructor, pinned to OpenRouter's hosted decisions origin.
    /// `timeout_ms` bounds one decision so a slow upstream cannot hold a chat
    /// request open past the caller's patience.
    pub fn new(api_key: String, model: String, timeout_ms: u64) -> Self {
        Self::with_endpoint(OPENROUTER_BASE, api_key, model, timeout_ms)
    }

    /// Override the origin while keeping the same path and wire format. Used by
    /// the wire tests against a local fake server; also useful behind a proxy
    /// that fronts the hosted endpoint.
    pub fn with_endpoint(
        base_url: impl Into<String>,
        api_key: String,
        model: String,
        timeout_ms: u64,
    ) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            model,
            http,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}{DECISIONS_PATH}", self.base_url)
    }
}

#[async_trait]
impl DecisionRouter for OpenRouterRouter {
    async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, AppError> {
        tracing::debug!(
            model = %self.model,
            questions = req.questions.len(),
            "openrouter decisions request"
        );

        let expected: BTreeSet<String> = req.questions.keys().cloned().collect();
        let body = decision_wire::encode_body(&self.model, &req);

        let response = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|err| {
                AppError::Provider(format!("openrouter decisions request failed: {err}"))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(AppError::Provider(format!(
                "openrouter decisions HTTP {status}: {text}"
            )));
        }

        let value: serde_json::Value = response.json().await.map_err(|err| {
            AppError::Provider(format!("openrouter decisions JSON parse failed: {err}"))
        })?;

        decision_wire::decode_response(&value, &expected)
    }
}
