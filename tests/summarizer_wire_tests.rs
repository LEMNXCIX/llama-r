//! Wire coverage for the two non-Ollama conversation summarizers.
//!
//! Each test starts a real axum listener on an ephemeral port and drives the
//! `Summarizer` implementations from `src/adapters/history/summarizer.rs` over
//! HTTP. This pins the exact paths, auth headers and response shapes without a
//! live key, provider or decision model.

use axum::{
    body::to_bytes,
    extract::Request,
    http::{header::AUTHORIZATION, StatusCode},
    routing::post,
    Json, Router,
};
use llama_r::adapters::history::summarizer::{ChatCompletionsSummarizer, OpenCodeSummarizer};
use llama_r::ports::history::{ExportMessage, Summarizer};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

#[derive(Clone, Debug, Default)]
struct Captured {
    path: String,
    authorization: Option<String>,
    body: Value,
}

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Option<Captured>>>);

impl Recorder {
    fn get(&self) -> Captured {
        self.0
            .lock()
            .expect("recorder mutex")
            .clone()
            .expect("the fake server captured a request")
    }
}

struct FakeServer {
    url: String,
    recorder: Recorder,
    _task: JoinHandle<()>,
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self._task.abort();
    }
}

/// Serve one response on every supplied path, remembering the last request.
async fn fake_server(paths: &[&'static str], status: StatusCode, response: Value) -> FakeServer {
    let recorder = Recorder::default();
    let mut app = Router::new();
    for path in paths {
        let recorder = recorder.clone();
        let response = response.clone();
        app = app.route(
            path,
            post(move |req: Request| {
                let recorder = recorder.clone();
                let response = response.clone();
                async move {
                    let captured = Captured {
                        path: req.uri().path().to_string(),
                        authorization: req
                            .headers()
                            .get(AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string),
                        body: serde_json::from_slice(
                            &to_bytes(req.into_body(), 1_048_576)
                                .await
                                .expect("read body"),
                        )
                        .expect("JSON request body"),
                    };
                    *recorder.0.lock().expect("recorder mutex") = Some(captured);
                    (status, Json(response))
                }
            }),
        );
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral listener");
    let url = format!("http://{}", listener.local_addr().expect("local address"));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("fake server");
    });
    FakeServer {
        url,
        recorder,
        _task: task,
    }
}

fn messages() -> Vec<ExportMessage> {
    vec![
        ExportMessage {
            id: "1".into(),
            role: "user".into(),
            content: "¿Cuál es la política de reembolsos?".into(),
            created_at: 0,
        },
        ExportMessage {
            id: "2".into(),
            role: "assistant".into(),
            content: "24 horas.".into(),
            created_at: 1,
        },
    ]
}

#[tokio::test]
async fn chat_completions_summarizer_posts_bearer_and_parses_choice() {
    let server = fake_server(
        &["/api/v1/chat/completions"],
        StatusCode::OK,
        json!({ "choices": [{ "message": { "content": " resumen " } }] }),
    )
    .await;
    let summarizer = ChatCompletionsSummarizer::new(
        format!("{}/api/v1/", server.url),
        "dummy".to_string(),
        "m".to_string(),
    );

    assert_eq!(summarizer.summarize(&messages()).await.unwrap(), "resumen");

    let captured = server.recorder.get();
    // An `/api/v1` base is not stripped: the endpoint is appended to it.
    assert_eq!(captured.path, "/api/v1/chat/completions");
    assert_eq!(captured.authorization.as_deref(), Some("Bearer dummy"));
    assert_eq!(captured.body["model"].as_str(), Some("m"));
    assert_eq!(captured.body["stream"].as_bool(), Some(false));
    assert_eq!(captured.body["messages"][0]["role"].as_str(), Some("user"));
    let prompt = captured.body["messages"][0]["content"]
        .as_str()
        .expect("prompt string");
    assert!(prompt.contains("¿Cuál es la política de reembolsos?"));
    assert!(prompt.contains("assistant: 24 horas."));
}

#[tokio::test]
async fn chat_completions_summarizer_does_not_double_a_full_endpoint() {
    let server = fake_server(
        &["/api/v1/chat/completions"],
        StatusCode::OK,
        json!({ "choices": [{ "message": { "content": "resumen" } }] }),
    )
    .await;
    let summarizer = ChatCompletionsSummarizer::new(
        format!("{}/api/v1/chat/completions", server.url),
        "dummy".to_string(),
        "m".to_string(),
    );

    assert_eq!(summarizer.summarize(&messages()).await.unwrap(), "resumen");
    let captured = server.recorder.get();
    assert_eq!(captured.path, "/api/v1/chat/completions");
}

#[tokio::test]
async fn chat_completions_summarizer_joins_v1_and_bare_bases_once() {
    let server = fake_server(
        &["/v1/chat/completions"],
        StatusCode::OK,
        json!({ "choices": [{ "message": { "content": "resumen" } }] }),
    )
    .await;

    for base in [format!("{}/v1", server.url), server.url.clone()] {
        let summarizer = ChatCompletionsSummarizer::new(base, "dummy".to_string(), "m".to_string());
        assert_eq!(summarizer.summarize(&messages()).await.unwrap(), "resumen");
        assert_eq!(server.recorder.get().path, "/v1/chat/completions");
    }
}

#[tokio::test]
async fn open_code_summarizer_posts_model_ref_with_basic_auth() {
    let server = fake_server(
        &["/api/experimental/generate/text"],
        StatusCode::OK,
        json!({ "data": { "text": " resumen " } }),
    )
    .await;
    let summarizer = OpenCodeSummarizer::new(
        format!("{}/", server.url),
        Some("secret".to_string()),
        "m".to_string(),
    );

    assert_eq!(summarizer.summarize(&messages()).await.unwrap(), "resumen");

    let captured = server.recorder.get();
    assert_eq!(captured.path, "/api/experimental/generate/text");
    // base64("opencode:secret")
    assert_eq!(
        captured.authorization.as_deref(),
        Some("Basic b3BlbmNvZGU6c2VjcmV0")
    );
    assert_eq!(captured.body["model"]["id"].as_str(), Some("m"));
    assert_eq!(
        captured.body["model"]["providerID"].as_str(),
        Some("opencode")
    );
    assert_eq!(captured.body["model"]["modelID"].as_str(), Some("m"));
    let prompt = captured.body["prompt"].as_str().expect("prompt string");
    assert!(prompt.contains("¿Cuál es la política de reembolsos?"));
    assert!(prompt.contains("assistant: 24 horas."));
}

#[tokio::test]
async fn open_code_summarizer_accepts_legacy_text_envelope_without_password() {
    let server = fake_server(
        &["/api/experimental/generate/text"],
        StatusCode::OK,
        json!({ "text": "resumen legacy" }),
    )
    .await;
    let summarizer = OpenCodeSummarizer::new(server.url.clone(), None, "m".to_string());

    assert_eq!(
        summarizer.summarize(&messages()).await.unwrap(),
        "resumen legacy"
    );
    assert_eq!(server.recorder.get().authorization, None);
}

#[tokio::test]
async fn summarizers_return_empty_on_empty_content_and_error_without_leaking_key() {
    let chat = fake_server(
        &["/api/v1/chat/completions"],
        StatusCode::OK,
        json!({ "choices": [{ "message": { "content": "   " } }] }),
    )
    .await;
    let chat_summarizer = ChatCompletionsSummarizer::new(
        format!("{}/api/v1", chat.url),
        "k".to_string(),
        "m".to_string(),
    );
    assert_eq!(chat_summarizer.summarize(&messages()).await.unwrap(), "");

    let code = fake_server(
        &["/api/experimental/generate/text"],
        StatusCode::OK,
        json!({ "data": { "text": "  " } }),
    )
    .await;
    let code_summarizer = OpenCodeSummarizer::new(code.url.clone(), None, "m".to_string());
    assert_eq!(code_summarizer.summarize(&messages()).await.unwrap(), "");

    // A server that puts the secret in its error body must not leak it through
    // the summarizer's error string either.
    let bad = fake_server(
        &[
            "/api/v1/chat/completions",
            "/api/experimental/generate/text",
        ],
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "error": "super-secret-key-12345 super-secret-password-6789" }),
    )
    .await;
    let bad_chat = ChatCompletionsSummarizer::new(
        format!("{}/api/v1", bad.url),
        "super-secret-key-12345".to_string(),
        "m".to_string(),
    );
    let bad_code = OpenCodeSummarizer::new(
        bad.url.clone(),
        Some("super-secret-password-6789".to_string()),
        "m".to_string(),
    );
    for error in [
        bad_chat.summarize(&messages()).await.unwrap_err(),
        bad_code.summarize(&messages()).await.unwrap_err(),
    ] {
        assert!(
            !error.contains("super-secret"),
            "error leaked a secret: {error}"
        );
    }

    // Connection errors carry a URL, never an auth header or a password.
    let dead = "http://127.0.0.1:1";
    let dead_chat = ChatCompletionsSummarizer::new(
        format!("{dead}/api/v1"),
        "super-secret-key-12345".to_string(),
        "m".to_string(),
    );
    let dead_code = OpenCodeSummarizer::new(
        dead.to_string(),
        Some("super-secret-password-6789".to_string()),
        "m".to_string(),
    );
    for error in [
        dead_chat.summarize(&messages()).await.unwrap_err(),
        dead_code.summarize(&messages()).await.unwrap_err(),
    ] {
        assert!(
            !error.contains("super-secret"),
            "error leaked a secret: {error}"
        );
    }
}
