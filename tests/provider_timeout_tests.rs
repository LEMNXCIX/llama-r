use llama_r::providers::LLMProvider;
use std::time::{Duration, Instant};

#[tokio::test]
async fn ollama_health_fails_within_the_configured_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
    });
    let provider = llama_r::providers::ollama::OllamaProvider::with_timeout(
        format!("http://{addr}"),
        "unused".into(),
        Duration::from_millis(300),
    );
    let started = Instant::now();
    let err = provider.health_check().await.unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "health_check hung: {err}"
    );
}

#[tokio::test]
async fn openai_compatible_health_fails_within_the_configured_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
    });
    let provider = llama_r::providers::openai_compatible::OpenAiCompatibleProvider::with_timeout(
        format!("http://{addr}"),
        "key".into(),
        "m".into(),
        Duration::from_millis(300),
    );
    let started = Instant::now();
    let err = provider.health_check().await.unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "health_check hung: {err}"
    );
}

#[tokio::test]
async fn opencode_health_fails_within_the_configured_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
    });
    let provider = llama_r::providers::opencode::OpenCodeProvider::with_timeout(
        format!("http://{addr}"),
        None,
        "m".into(),
        Duration::from_millis(300),
    );
    let started = Instant::now();
    let err = provider.health_check().await.unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "health_check hung: {err}"
    );
}
