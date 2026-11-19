//! Recorded-response fixture used by provider-class eval cases.
//!
//! Moved here from `tests/provider_eval_smoke.rs` so the runner and that test
//! share one helper. A one-shot HTTP/1.1 listener serves a single recorded
//! body, so no live key or network is involved. The body is a JSON value plus a
//! trailing newline, which is both a valid single JSON document (non-streaming
//! `chat`) and a valid one-chunk NDJSON stream.

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::config::ProviderKind;
use crate::providers::anthropic::AnthropicProvider;
use crate::providers::gemini::GeminiProvider;
use crate::providers::ollama::OllamaProvider;
use crate::providers::openai_compatible::OpenAiCompatibleProvider;
use crate::providers::opencode::OpenCodeProvider;
use crate::providers::LLMProvider;

use super::{EvalSuite, EVAL_TOML};

/// The model label used only to build adapters and shape recorded responses.
pub const EVAL_MODEL: &str = "eval-model";

/// Parse the shipped v1 smoke TOML into a ready-to-run suite.
///
/// `expect` is folded into `checks` by `EvalCase`'s deserializer, so callers get
/// the same ready-to-check cases the runner would build by hand.
pub fn load_eval() -> EvalSuite {
    toml::from_str(EVAL_TOML).expect("smoke.eval.toml must be valid")
}

/// A one-shot HTTP/1.1 listener serving a single recorded response body.
pub struct FixtureServer {
    addr: SocketAddr,
    handle: JoinHandle<()>,
}

impl FixtureServer {
    pub async fn start(body: serde_json::Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut payload = serde_json::to_vec(&body).unwrap();
        payload.push(b'\n');

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let payload = payload.clone();
                tokio::spawn(async move {
                    if read_request(&mut socket).await.is_none() {
                        return;
                    }
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&payload).await;
                    let _ = socket.flush().await;
                });
            }
        });

        Self { addr, handle }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Drain one full POST (headers + body announced by Content-Length) so the
/// client never sees a connection close mid-write.
async fn read_request(socket: &mut TcpStream) -> Option<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buf[..end]);
            let len = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if buf.len() >= end + 4 + len {
                return Some(());
            }
        }
    }
}

/// Build the real adapter for a provider kind, pointed at the local fixture
/// server with a dummy key. Keys are never URLs, log lines or error strings.
pub fn build_provider(kind: ProviderKind, base_url: &str) -> Box<dyn LLMProvider> {
    match kind {
        ProviderKind::Ollama => Box::new(OllamaProvider::new(base_url.to_string())),
        ProviderKind::OpenRouter => Box::new(OpenAiCompatibleProvider::new(
            base_url.to_string(),
            "sk-eval-dummy".to_string(),
            EVAL_MODEL.to_string(),
        )),
        ProviderKind::OpenCode => Box::new(OpenCodeProvider::new(
            base_url.to_string(),
            Some("oc-eval-dummy".to_string()),
            EVAL_MODEL.to_string(),
        )),
        ProviderKind::OpenAiCompatible => Box::new(OpenAiCompatibleProvider::with_provider_name(
            base_url.to_string(),
            "sk-eval-dummy".to_string(),
            EVAL_MODEL.to_string(),
            "openai-compatible",
        )),
        ProviderKind::Anthropic => Box::new(AnthropicProvider::new(
            Some(base_url.to_string()),
            "ant-eval-dummy".to_string(),
            EVAL_MODEL.to_string(),
        )),
        ProviderKind::Gemini => Box::new(GeminiProvider::new(
            Some(base_url.to_string()),
            "gem-eval-dummy".to_string(),
            EVAL_MODEL.to_string(),
        )),
    }
}

/// The recorded response for one provider kind, carrying the case's expected
/// token so each case actually asserts something about the decoded answer.
pub fn recorded_body(kind: ProviderKind, answer: &str) -> serde_json::Value {
    match kind {
        ProviderKind::Ollama => serde_json::json!({
            "model": EVAL_MODEL, "created_at": "2026-01-01T00:00:00Z",
            "message": { "role": "assistant", "content": answer },
            "done": true, "prompt_eval_count": 1, "eval_count": 1
        }),
        ProviderKind::OpenRouter | ProviderKind::OpenAiCompatible => serde_json::json!({
            "id": "chatcmpl-eval", "object": "chat.completion", "created": 1,
            "model": EVAL_MODEL,
            "choices": [{ "index": 0,
                "message": { "role": "assistant", "content": answer },
                "finish_reason": "stop" }]
        }),
        ProviderKind::OpenCode => serde_json::json!({ "data": { "text": answer } }),
        ProviderKind::Anthropic => serde_json::json!({
            "id": "msg-eval", "type": "message", "role": "assistant",
            "content": [{ "type": "text", "text": answer }],
            "model": EVAL_MODEL, "stop_reason": "end_turn"
        }),
        ProviderKind::Gemini => serde_json::json!({
            "candidates": [{ "content": { "role": "model", "parts": [{ "text": answer }] },
                "finishReason": "STOP" }],
            "modelVersion": EVAL_MODEL
        }),
    }
}
