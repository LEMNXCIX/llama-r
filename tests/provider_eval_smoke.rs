//! Task 11: multi-provider eval smoke harness.
//!
//! Reads `evals/providers/smoke.eval.toml` and runs every case against every
//! configured provider, printing a pass/fail table. Fixture mode (the only mode
//! CI runs) binds a local `TcpListener` per case and serves a recorded response,
//! so no live key or network is involved. A live smoke against real keys is
//! optional, labelled in the TOML, and deliberately not performed here.

use llama_r::config::ProviderKind;
use llama_r::domain::models::{ChatMessage, ChatRequest};
use llama_r::providers::anthropic::AnthropicProvider;
use llama_r::providers::gemini::GeminiProvider;
use llama_r::providers::ollama::OllamaProvider;
use llama_r::providers::openai_compatible::OpenAiCompatibleProvider;
use llama_r::providers::opencode::OpenCodeProvider;
use llama_r::providers::LLMProvider;
use serde::Deserialize;
use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

const EVAL_TOML: &str = include_str!("../evals/providers/smoke.eval.toml");
/// The model label used only to build adapters and shape recorded responses.
const EVAL_MODEL: &str = "eval-model";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CaseKind {
    Plain,
    Tool,
    Stream,
}

impl fmt::Display for CaseKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            CaseKind::Plain => "plain",
            CaseKind::Tool => "tool",
            CaseKind::Stream => "stream",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct EvalCase {
    id: String,
    kind: CaseKind,
    prompt: String,
    expect: String,
}

#[derive(Debug, Clone, Deserialize)]
struct EvalProvider {
    name: String,
    kind: String,
}

#[derive(Debug, Deserialize)]
struct EvalFile {
    cases: Vec<EvalCase>,
    providers: Vec<EvalProvider>,
}

fn load_eval() -> EvalFile {
    toml::from_str(EVAL_TOML).expect("smoke.eval.toml must be valid")
}

#[derive(Debug)]
struct ResultRow {
    provider: String,
    case: String,
    kind: CaseKind,
    passed: bool,
    detail: String,
}

/// A one-shot HTTP/1.1 listener serving a single recorded response body. The
/// body is a JSON value plus a trailing newline, which is both a valid single
/// JSON document (non-streaming `chat`) and a valid one-chunk NDJSON stream.
struct FixtureServer {
    addr: SocketAddr,
    handle: JoinHandle<()>,
}

impl FixtureServer {
    async fn start(body: serde_json::Value) -> Self {
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

    fn url(&self) -> String {
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
fn build_provider(kind: ProviderKind, base_url: &str) -> Box<dyn LLMProvider> {
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
fn recorded_body(kind: ProviderKind, answer: &str) -> serde_json::Value {
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

async fn run_case(case: &EvalCase, provider: &str, kind: ProviderKind) -> ResultRow {
    let answer = format!("fixture answer: {}", case.expect);
    let server = FixtureServer::start(recorded_body(kind, &answer)).await;
    let client = build_provider(kind, &server.url());
    let request = ChatRequest {
        model: String::new(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: case.prompt.clone(),
        }],
        stream: case.kind == CaseKind::Stream,
    };

    let (passed, detail) = match case.kind {
        CaseKind::Plain | CaseKind::Tool => match client.chat(request).await {
            Ok(response) if response.message.content.contains(&case.expect) => {
                (true, "matched expected answer".to_string())
            }
            Ok(response) => (
                false,
                format!("missing '{}' in: {}", case.expect, response.message.content),
            ),
            Err(err) => (false, err.to_string()),
        },
        CaseKind::Stream => match client.chat_stream(request).await {
            Ok(mut stream) => {
                let mut text = String::new();
                let mut error = None;
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(event) => text.push_str(&event.message.content),
                        Err(err) => {
                            error = Some(err.to_string());
                            break;
                        }
                    }
                }
                match error {
                    Some(err) => (false, err),
                    None if text.contains(&case.expect) => {
                        (true, "matched streamed answer".to_string())
                    }
                    None => (
                        false,
                        format!("missing '{}' in stream: {text}", case.expect),
                    ),
                }
            }
            // Providers without streaming report Unavailable without connecting.
            // That is their recorded, expected behavior for a stream case.
            Err(err)
                if err
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("streaming is not supported") =>
            {
                (true, "streaming unavailable (expected)".to_string())
            }
            Err(err) => (false, err.to_string()),
        },
    };

    ResultRow {
        provider: provider.to_string(),
        case: case.id.clone(),
        kind: case.kind,
        passed,
        detail,
    }
}

async fn run_eval(eval: &EvalFile) -> Vec<ResultRow> {
    let mut rows = Vec::new();
    for p in &eval.providers {
        let kind = ProviderKind::from_str(&p.kind).expect("valid provider kind in eval TOML");
        for case in &eval.cases {
            rows.push(run_case(case, &p.name, kind).await);
        }
    }
    rows
}

fn render_table(rows: &[ResultRow]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<20} {:<18} {:<8} {:<6} {}\n",
        "provider", "case", "kind", "result", "detail"
    ));
    for row in rows {
        let result = if row.passed { "PASS" } else { "FAIL" };
        out.push_str(&format!(
            "{:<20} {:<18} {:<8} {:<6} {}\n",
            row.provider, row.case, row.kind, result, row.detail
        ));
    }
    out
}

#[tokio::test]
async fn runner_loads_cases_and_reports_all_case_provider_pairs() {
    let eval = load_eval();
    assert_eq!(eval.cases.len(), 3, "three smoke cases in the TOML");
    assert_eq!(eval.providers.len(), 6, "six provider profiles in the TOML");

    let rows = run_eval(&eval).await;
    assert_eq!(
        rows.len(),
        eval.cases.len() * eval.providers.len(),
        "one row per case x provider"
    );

    let table = render_table(&rows);
    println!("{table}");
    for case in &eval.cases {
        assert!(table.contains(&case.id), "table must name case {}", case.id);
    }
    for p in &eval.providers {
        assert!(
            table.contains(&p.name),
            "table must name provider {}",
            p.name
        );
    }
    assert_eq!(
        table.lines().count(),
        rows.len() + 1,
        "header + one line per row"
    );
    assert!(
        rows.iter().all(|row| row.passed),
        "every fixture-backed case should pass:\n{table}"
    );
    // Dummy keys only: nothing that resembles a credential may reach the table.
    assert!(!table.contains("sk-eval-dummy"));
    assert!(!table.contains("ant-eval-dummy"));
}
