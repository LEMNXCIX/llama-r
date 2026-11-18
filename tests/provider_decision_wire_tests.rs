//! Wire-level validation of the decision and embedding adapters. A real
//! `TcpListener` records the actual HTTP request (path, headers and JSON body)
//! and answers with canned fixtures, so these tests pin what crosses the
//! socket rather than a struct an adapter built in memory.
//!
//! What they prove:
//! - OpenRouter posts decisions to `/api/alpha/decisions` with a Bearer key and
//!   maps `answers.<name>` into the port's `Choice`/`Noul` answers.
//! - OpenCode posts to `/api/experimental/generate/decisions` with the same
//!   body shape and a basic-auth password (none when unconfigured).
//! - A `refusal`, a missing question answer, or an unknown answer kind is an
//!   `AppError::Provider` — the ladder reads that as `RouterUnavailable`, never
//!   as a fabricated decision.
//! - OpenRouter embeddings send one batched `POST /api/v1/embeddings` with
//!   `input = texts` and read `data[i].embedding`.
//!
//! The decisions endpoints have no live server in this environment; replies
//! here are canned fixtures modeled on the d1 `/v1/systemone` envelope that
//! `src/adapters/router/fixtures/systemone_response.json` captured locally.

use llama_r::adapters::rag::gemini_embeddings::GeminiEmbeddings;
use llama_r::adapters::rag::openai_compatible_embeddings::OpenAiCompatibleEmbeddings;
use llama_r::adapters::rag::openrouter_embeddings::OpenRouterEmbeddings;
use llama_r::adapters::router::openai_decisions::OpenRouterRouter;
use llama_r::adapters::router::opencode_decisions::OpenCodeRouter;
use llama_r::error::AppError;
use llama_r::ports::rag::EmbeddingProvider;
use llama_r::ports::router::{Answer, DecisionRequest, DecisionRouter, Question};
use std::collections::BTreeMap;
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
struct FakeServer {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<RecordedRequest>>>,
    handle: JoinHandle<()>,
}

impl FakeServer {
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

impl Drop for FakeServer {
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

fn decision_request() -> DecisionRequest {
    DecisionRequest {
        state: "necesito cancelar el pedido 42".to_string(),
        questions: BTreeMap::from([
            (
                "agent".to_string(),
                Question::Choice {
                    instructions: "Which agent?".to_string(),
                    criteria: BTreeMap::from([(
                        "ops".to_string(),
                        "Pedidos y cancelaciones".to_string(),
                    )]),
                },
            ),
            (
                "specialized".to_string(),
                Question::Noul {
                    instructions: "Specialised?".to_string(),
                },
            ),
        ]),
    }
}

fn decisions_reply() -> serde_json::Value {
    // Canned: modeled on the d1 `/v1/systemone` envelope captured in
    // `src/adapters/router/fixtures/systemone_response.json`.
    serde_json::json!({
        "model": "meta-llama/llama-3.3-70b",
        "answers": {
            "agent": {
                "type": "choice",
                "choice": "ops",
                "confidence": 0.83,
                "probabilities": { "ops": 0.92, "general": 0.08 }
            },
            "specialized": {
                "type": "noul",
                "noul": 0.44
            }
        }
    })
}

fn assert_decoded_answers(response: &llama_r::ports::router::DecisionResponse) {
    assert_eq!(response.answers.len(), 2);
    match &response.answers["agent"] {
        Answer::Choice {
            key,
            confidence,
            probabilities,
        } => {
            assert_eq!(key, "ops");
            assert!((*confidence - 0.83).abs() < 1e-6, "{confidence}");
            assert!(
                (probabilities["ops"] - 0.92).abs() < 1e-6,
                "{probabilities:?}"
            );
            assert!(
                (probabilities["general"] - 0.08).abs() < 1e-6,
                "{probabilities:?}"
            );
        }
        other => panic!("agent must decode as Choice, got {other:?}"),
    }
    match &response.answers["specialized"] {
        Answer::Noul { value } => assert!((*value - 0.44).abs() < 1e-6, "{value}"),
        other => panic!("specialized must decode as Noul, got {other:?}"),
    }
}

fn assert_questions_wire(body: &serde_json::Value) {
    assert_eq!(body["model"], "decision-model");
    assert_eq!(body["state"], "necesito cancelar el pedido 42");
    assert_eq!(body["questions"]["agent"]["type"], "choice");
    assert_eq!(body["questions"]["agent"]["instructions"], "Which agent?");
    assert_eq!(
        body["questions"]["agent"]["criteria"]["ops"],
        "Pedidos y cancelaciones"
    );
    assert_eq!(body["questions"]["specialized"]["type"], "noul");
    assert_eq!(
        body["questions"]["specialized"]["instructions"],
        "Specialised?"
    );
    assert!(
        body["questions"]["specialized"].get("criteria").is_none(),
        "a noul must not gain a criteria field"
    );
}

#[tokio::test]
async fn openrouter_decisions_posts_to_alpha_decisions() {
    let server = FakeServer::start(&[("/api/alpha/decisions", decisions_reply())]).await;
    let router = OpenRouterRouter::with_endpoint(
        server.url(),
        "sk-live-ABC123".to_string(),
        "decision-model".to_string(),
        5_000,
    );

    let response = router.decide(decision_request()).await.unwrap();
    assert_decoded_answers(&response);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "exactly one HTTP request expected");
    let request = &requests[0];
    assert_eq!(request.path, "/api/alpha/decisions");
    assert_eq!(
        header(&request.headers, "authorization"),
        Some("Bearer sk-live-ABC123")
    );
    // The key is in the header only: never in the request line (the URL).
    let request_line = request.headers.lines().next().unwrap();
    assert!(!request_line.contains("sk-live-ABC123"), "{request_line}");
    assert!(!request.body.to_string().contains("sk-live-ABC123"));
    assert_questions_wire(&request.body);
}

#[tokio::test]
async fn opencode_decisions_posts_to_experimental_generate_decisions() {
    let server =
        FakeServer::start(&[("/api/experimental/generate/decisions", decisions_reply())]).await;
    let router = OpenCodeRouter::new(
        server.url(),
        Some("secret".to_string()),
        "decision-model".to_string(),
        5_000,
    );

    let response = router.decide(decision_request()).await.unwrap();
    assert_decoded_answers(&response);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, "/api/experimental/generate/decisions");
    // base64("opencode:secret") — basic auth, never Bearer.
    assert_eq!(
        header(&request.headers, "authorization"),
        Some("Basic b3BlbmNvZGU6c2VjcmV0")
    );
    let request_line = request.headers.lines().next().unwrap();
    assert!(!request_line.contains("secret"), "{request_line}");
    assert_questions_wire(&request.body);
}

#[tokio::test]
async fn opencode_decisions_without_password_sends_no_auth_header() {
    let server =
        FakeServer::start(&[("/api/experimental/generate/decisions", decisions_reply())]).await;
    let router = OpenCodeRouter::new(server.url(), None, "decision-model".to_string(), 5_000);

    router.decide(decision_request()).await.unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        header(&requests[0].headers, "authorization").is_none(),
        "no configured password means no auth header"
    );
}

#[tokio::test]
async fn a_refusal_answer_becomes_router_unavailable() {
    // The refusal must not be read as a DecisionAnswer or treated as a route.
    let reply = serde_json::json!({
        "answers": {
            "agent": { "type": "refusal", "refusal": "can't help" },
            "specialized": { "type": "noul", "noul": 0.44 }
        }
    });
    for path in [
        "/api/alpha/decisions",
        "/api/experimental/generate/decisions",
    ] {
        let server = FakeServer::start(&[(path, reply.clone())]).await;
        let openrouter = OpenRouterRouter::with_endpoint(
            server.url(),
            "sk-key".to_string(),
            "m".to_string(),
            5_000,
        );
        let opencode = OpenCodeRouter::new(server.url(), None, "m".to_string(), 5_000);

        for result in [
            openrouter.decide(decision_request()).await,
            opencode.decide(decision_request()).await,
        ] {
            match result {
                Err(AppError::Provider(message)) => {
                    assert!(!message.is_empty(), "{path}: empty provider error")
                }
                other => panic!("{path}: expected AppError::Provider, got {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn a_missing_or_unexpected_answer_becomes_router_unavailable() {
    let missing = serde_json::json!({
        "answers": { "agent": {
            "type": "choice", "choice": "ops", "confidence": 0.8,
            "probabilities": { "ops": 1.0 }
        } }
    });
    let unexpected_type = serde_json::json!({
        "answers": {
            "agent": {
                "type": "choice", "choice": "ops", "confidence": 0.8,
                "probabilities": { "ops": 1.0 }
            },
            "specialized": { "type": "score", "score": 2.0 },
            "extraneous": { "type": "noul", "noul": 0.5 }
        }
    });
    for reply in [missing, unexpected_type] {
        let server = FakeServer::start(&[("/api/alpha/decisions", reply)]).await;
        let router = OpenRouterRouter::with_endpoint(
            server.url(),
            "sk-key".to_string(),
            "m".to_string(),
            5_000,
        );
        match router.decide(decision_request()).await {
            Err(AppError::Provider(message)) => assert!(!message.is_empty()),
            other => panic!("expected AppError::Provider, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn openrouter_embeddings_sends_a_batched_input() {
    let reply = serde_json::json!({
        "model": "openai/text-embedding-3-small",
        "object": "list",
        "data": [
            { "object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3] },
            { "object": "embedding", "index": 1, "embedding": [0.4, 0.5, 0.6] }
        ]
    });
    let server = FakeServer::start(&[("/api/v1/embeddings", reply)]).await;
    let embeddings = OpenRouterEmbeddings::with_endpoint(
        server.url(),
        "sk-live-ABC123".to_string(),
        "openai/text-embedding-3-small".to_string(),
        3,
    );

    let vectors = embeddings
        .embed(&["refund policy".to_string(), "warranty".to_string()])
        .await
        .unwrap();
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0], vec![0.1, 0.2, 0.3]);
    assert_eq!(vectors[1], vec![0.4, 0.5, 0.6]);
    assert_eq!(embeddings.dimensions(), 3);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "must batch all texts in one request");
    let request = &requests[0];
    assert_eq!(request.path, "/api/v1/embeddings");
    assert_eq!(
        header(&request.headers, "authorization"),
        Some("Bearer sk-live-ABC123")
    );
    let request_line = request.headers.lines().next().unwrap();
    assert!(!request_line.contains("sk-live-ABC123"), "{request_line}");
    assert_eq!(request.body["model"], "openai/text-embedding-3-small");
    assert_eq!(request.body["input"][0], "refund policy");
    assert_eq!(request.body["input"][1], "warranty");
}

// --- Embeddings for the two direct providers that document an endpoint ------

#[tokio::test]
async fn openai_compatible_embeddings_posts_to_v1_embeddings() {
    let reply = serde_json::json!({
        "model": "text-embedding-3-small",
        "object": "list",
        "data": [
            { "object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3] },
            { "object": "embedding", "index": 1, "embedding": [0.4, 0.5, 0.6] }
        ]
    });
    let server = FakeServer::start(&[("/v1/embeddings", reply)]).await;
    let embeddings = OpenAiCompatibleEmbeddings::new(
        server.url(),
        "sk-openai-ABC123".to_string(),
        "text-embedding-3-small".to_string(),
        3,
    );

    let vectors = embeddings
        .embed(&["refund policy".to_string(), "warranty".to_string()])
        .await
        .unwrap();
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0], vec![0.1, 0.2, 0.3]);
    assert_eq!(vectors[1], vec![0.4, 0.5, 0.6]);
    assert_eq!(embeddings.dimensions(), 3);

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        1,
        "all texts must be batched in one request"
    );
    let request = &requests[0];
    assert_eq!(request.path, "/v1/embeddings");
    assert_eq!(
        header(&request.headers, "authorization"),
        Some("Bearer sk-openai-ABC123")
    );
    let request_line = request.headers.lines().next().unwrap();
    assert!(!request_line.contains("sk-openai-ABC123"), "{request_line}");
    assert!(!request.body.to_string().contains("sk-openai-ABC123"));
    assert_eq!(request.body["model"], "text-embedding-3-small");
    assert_eq!(request.body["input"][0], "refund policy");
    assert_eq!(request.body["input"][1], "warranty");
}

#[tokio::test]
async fn openai_compatible_embeddings_accept_a_base_url_ending_in_v1() {
    let reply = serde_json::json!({
        "data": [{ "object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3] }]
    });
    let server = FakeServer::start(&[("/v1/embeddings", reply)]).await;
    let embeddings = OpenAiCompatibleEmbeddings::new(
        format!("{}/v1", server.url()),
        "k".into(),
        "text-embedding-3-small".into(),
        3,
    );

    let vectors = embeddings.embed(&["text".to_string()]).await.unwrap();
    assert_eq!(vectors[0], vec![0.1, 0.2, 0.3]);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/embeddings");
}

#[tokio::test]
async fn gemini_embeddings_posts_to_batch_embed_contents_with_key_in_header() {
    let reply = serde_json::json!({
        "embeddings": [
            { "values": [0.1, 0.2, 0.3] },
            { "values": [0.4, 0.5, 0.6] }
        ]
    });
    let server =
        FakeServer::start(&[("/models/text-embedding-004:batchEmbedContents", reply)]).await;
    let embeddings = GeminiEmbeddings::new(
        Some(server.url()),
        "gem-key-ABC123".to_string(),
        "text-embedding-004".to_string(),
        3,
    );

    let vectors = embeddings
        .embed(&["refund policy".to_string(), "warranty".to_string()])
        .await
        .unwrap();
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0], vec![0.1, 0.2, 0.3]);
    assert_eq!(vectors[1], vec![0.4, 0.5, 0.6]);
    assert_eq!(embeddings.dimensions(), 3);

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        1,
        "all texts must be batched in one request"
    );
    let request = &requests[0];
    assert_eq!(
        request.path,
        "/models/text-embedding-004:batchEmbedContents"
    );
    assert_eq!(
        header(&request.headers, "x-goog-api-key"),
        Some("gem-key-ABC123")
    );
    let request_line = request.headers.lines().next().unwrap();
    assert!(!request_line.contains("gem-key-ABC123"), "{request_line}");
    assert_eq!(
        request.body["requests"][0]["model"],
        "models/text-embedding-004"
    );
    assert_eq!(
        request.body["requests"][0]["content"]["parts"][0]["text"],
        "refund policy"
    );
    assert_eq!(
        request.body["requests"][1]["content"]["parts"][0]["text"],
        "warranty"
    );
}

#[tokio::test]
async fn direct_embeddings_reject_a_dimension_mismatch_instead_of_resizing() {
    let openai_reply = serde_json::json!({
        "data": [{ "object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3] }]
    });
    let openai = FakeServer::start(&[("/v1/embeddings", openai_reply)]).await;
    let embeddings = OpenAiCompatibleEmbeddings::new(
        openai.url(),
        "k".into(),
        "text-embedding-3-small".into(),
        8,
    );
    let err = embeddings.embed(&["text".to_string()]).await.unwrap_err();
    assert!(err.contains("dimension mismatch"), "{err}");
    assert!(err.contains("got 3"), "{err}");

    let gemini_reply = serde_json::json!({ "embeddings": [{ "values": [0.1, 0.2, 0.3] }] });
    let gemini = FakeServer::start(&[(
        "/models/text-embedding-004:batchEmbedContents",
        gemini_reply,
    )])
    .await;
    let embeddings = GeminiEmbeddings::new(
        Some(gemini.url()),
        "k".into(),
        "text-embedding-004".into(),
        8,
    );
    let err = embeddings.embed(&["text".to_string()]).await.unwrap_err();
    assert!(err.contains("dimension mismatch"), "{err}");
    assert!(err.contains("got 3"), "{err}");
}
