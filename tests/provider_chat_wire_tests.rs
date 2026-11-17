//! Wire-level validation of the two new chat adapters. A real `TcpListener`
//! records the actual HTTP request (path, headers and JSON body) and answers
//! with a canned response, so these tests pin what crosses the socket rather
//! than a struct the adapter built in memory.
//!
//! What they prove:
//! - OpenRouter uses `/v1/chat/completions` with a Bearer key, and maps the
//!   OpenAI response into `ChatResponse`.
//! - OpenCode uses `/api/experimental/generate/text` with no OpenAI path, no
//!   auth header when unconfigured, and basic auth when a password is set.
//! - `chat_stream` returns `AppError::Unavailable` and opens **no** connection.
//! - `list_models`/`health_check` hit each provider's documented paths, with
//!   the key only ever in the Authorization header.

use llama_r::domain::models::{ChatMessage, ChatRequest};
use llama_r::error::AppError;
use llama_r::providers::openai_compatible::OpenAiCompatibleProvider;
use llama_r::providers::opencode::OpenCodeProvider;
use llama_r::providers::LLMProvider;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

#[derive(Clone)]
struct RecordedRequest {
    path: String,
    headers: String,
    body: serde_json::Value,
}

/// A minimal HTTP/1.1 server that answers only the paths it has fixtures for.
/// Serving a 404 for unknown paths is deliberate: a broken adapter that posts
/// to the wrong URL fails loudly instead of quietly getting a 200.
struct FakeChatServer {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<RecordedRequest>>>,
    handle: JoinHandle<()>,
}

impl FakeChatServer {
    async fn start(replies: &[(&'static str, serde_json::Value)]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let replies: Vec<(String, serde_json::Value)> = replies
            .iter()
            .map(|(path, body)| ((*path).to_string(), body.clone()))
            .collect();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let sink = sink.clone();
                let replies = replies.clone();
                tokio::spawn(async move {
                    // Read headers, then the body announced by Content-Length.
                    // One request per connection is all a single JSON POST needs.
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    let (headers, body_bytes) = loop {
                        let read = match socket.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..read]);
                        if let Some(header_end) = find_header_end(&buf) {
                            let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
                            let content_length = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.trim()
                                        .eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().ok())?
                                })
                                .unwrap_or(0);
                            if buf.len() >= header_end + 4 + content_length {
                                let body =
                                    buf[header_end + 4..header_end + 4 + content_length].to_vec();
                                break (headers, body);
                            }
                        }
                    };

                    let path = headers
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or_default()
                        .to_string();
                    let body_json = serde_json::from_slice::<serde_json::Value>(&body_bytes)
                        .unwrap_or(serde_json::Value::Null);
                    sink.lock().unwrap().push(RecordedRequest {
                        path: path.clone(),
                        headers: headers.clone(),
                        body: body_json,
                    });

                    let payload = match replies.iter().find(|(candidate, _)| *candidate == path) {
                        Some((_, reply)) => serde_json::to_vec(reply).unwrap(),
                        None => b"no fixture for this path".to_vec(),
                    };
                    let status = if payload == b"no fixture for this path".to_vec() {
                        "HTTP/1.1 404 Not Found"
                    } else {
                        "HTTP/1.1 200 OK"
                    };
                    let response = format!(
                        "{status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&payload).await;
                    let _ = socket.flush().await;
                });
            }
        });

        Self { addr, seen, handle }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for FakeChatServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (found, value) = line.split_once(':')?;
        found
            .trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim())
    })
}

fn chat_request(content: &str) -> ChatRequest {
    ChatRequest {
        model: String::new(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: content.to_string(),
        }],
        stream: false,
    }
}

fn openai_chat_reply(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 1_700_000_000u64,
        "model": "openai/gpt-4o-mini",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }]
    })
}

fn opencode_reply(text: &str) -> serde_json::Value {
    // The documented envelope is data.text; the decoder also accepts a bare
    // text field, pinned by `opencode_accepts_a_legacy_text_envelope`.
    serde_json::json!({ "data": { "text": text } })
}

#[tokio::test]
async fn openrouter_sends_bearer_and_uses_v1_chat_completions() {
    let server =
        FakeChatServer::start(&[("/v1/chat/completions", openai_chat_reply("Hi there!"))]).await;
    let provider = OpenAiCompatibleProvider::new(
        server.url(),
        "sk-live-ABC123".to_string(),
        "openai/gpt-4o-mini".to_string(),
    );

    let response = provider.chat(chat_request("Hello")).await.unwrap();
    assert_eq!(response.message.content, "Hi there!");
    assert_eq!(response.message.role, "assistant");
    assert_eq!(response.model, "openai/gpt-4o-mini");
    assert!(response.done);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "exactly one HTTP request expected");
    let request = &requests[0];
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(
        header(&request.headers, "authorization"),
        Some("Bearer sk-live-ABC123")
    );
    // The key is in the header only: never in the request line (the URL).
    let request_line = request.headers.lines().next().unwrap();
    assert!(!request_line.contains("sk-live-ABC123"), "{request_line}");
    assert_eq!(request.body["model"], "openai/gpt-4o-mini");
    assert_eq!(request.body["messages"][0]["content"], "Hello");
    assert_eq!(request.body["stream"], false);
    assert!(!provider.get_base_url().contains("sk-live-ABC123"));
}

#[tokio::test]
async fn opencode_uses_generate_text_without_an_openai_path() {
    let server =
        FakeChatServer::start(&[("/api/experimental/generate/text", opencode_reply("Bonjour"))])
            .await;
    let provider = OpenCodeProvider::new(server.url(), None, "anthropic/claude".to_string());

    let response = provider.chat(chat_request("Hello")).await.unwrap();
    assert_eq!(response.message.content, "Bonjour");
    assert_eq!(response.message.role, "assistant");

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, "/api/experimental/generate/text");
    assert!(
        header(&request.headers, "authorization").is_none(),
        "no configured key means no auth header"
    );
    assert_eq!(request.body["model"], "anthropic/claude");
    assert_eq!(request.body["text"], "Hello");
}

#[tokio::test]
async fn a_stream_request_returns_unavailable_for_both_without_connecting() {
    let server = FakeChatServer::start(&[]).await;
    let url = server.url();

    let openrouter = OpenAiCompatibleProvider::new(url.clone(), "sk-key".to_string(), "m".into());
    let error = openrouter
        .chat_stream(chat_request("Hello"))
        .await
        .err()
        .expect("OpenRouter streaming must be unavailable");
    assert!(
        error
            .to_string()
            .contains("streaming is not supported by openrouter"),
        "{error}"
    );
    assert!(matches!(
        error.downcast_ref::<AppError>(),
        Some(AppError::Unavailable(_))
    ));

    let opencode = OpenCodeProvider::new(url, None, "m".into());
    let error = opencode
        .chat_stream(chat_request("Hello"))
        .await
        .err()
        .expect("OpenCode streaming must be unavailable");
    assert!(
        error
            .to_string()
            .contains("streaming is not supported by opencode"),
        "{error}"
    );
    assert!(matches!(
        error.downcast_ref::<AppError>(),
        Some(AppError::Unavailable(_))
    ));

    assert!(
        server.requests().is_empty(),
        "chat_stream must return before opening a connection"
    );
}

#[tokio::test]
async fn opencode_sends_basic_auth_when_a_password_is_configured() {
    let server =
        FakeChatServer::start(&[("/api/experimental/generate/text", opencode_reply("ok"))]).await;
    let provider = OpenCodeProvider::new(server.url(), Some("secret".to_string()), "m".into());

    provider.chat(chat_request("Hello")).await.unwrap();

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    // base64("opencode:secret") — basic auth, never Bearer.
    assert_eq!(
        header(&requests[0].headers, "authorization"),
        Some("Basic b3BlbmNvZGU6c2VjcmV0")
    );
}

#[tokio::test]
async fn openrouter_list_models_and_health_use_the_models_endpoint() {
    let server = FakeChatServer::start(&[(
        "/v1/models",
        serde_json::json!({ "data": [{"id": "gpt-4o"}, {"id": "gpt-4o-mini"}] }),
    )])
    .await;
    let provider = OpenAiCompatibleProvider::new(server.url(), "sk-key".to_string(), "m".into());

    provider.health_check().await.unwrap();
    let models = provider.list_models().await.unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.name.as_str())
            .collect::<Vec<_>>(),
        vec!["gpt-4o", "gpt-4o-mini"]
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.path, "/v1/models");
        assert_eq!(
            header(&request.headers, "authorization"),
            Some("Bearer sk-key")
        );
    }
}

#[tokio::test]
async fn opencode_list_models_and_health_use_documented_paths() {
    let server = FakeChatServer::start(&[
        (
            "/api/model",
            serde_json::json!([{"id": "anthropic/claude"}, {"id": "openai/gpt-4o"}]),
        ),
        ("/api/info", serde_json::json!({ "version": "1.0" })),
    ])
    .await;
    let provider = OpenCodeProvider::new(server.url(), None, "m".into());

    provider.health_check().await.unwrap();
    let models = provider.list_models().await.unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.name.as_str())
            .collect::<Vec<_>>(),
        vec!["anthropic/claude", "openai/gpt-4o"]
    );

    let requests = server.requests();
    let mut paths = requests
        .iter()
        .map(|request| request.path.as_str())
        .collect::<Vec<_>>();
    paths.sort_unstable();
    assert_eq!(paths, vec!["/api/info", "/api/model"]);
}

#[tokio::test]
async fn opencode_accepts_a_legacy_text_envelope() {
    let server = FakeChatServer::start(&[(
        "/api/experimental/generate/text",
        serde_json::json!({ "text": "legacy answer" }),
    )])
    .await;
    let provider = OpenCodeProvider::new(server.url(), None, "m".into());

    let response = provider.chat(chat_request("Hello")).await.unwrap();
    assert_eq!(response.message.content, "legacy answer");
}
