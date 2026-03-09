use crate::domain::models::{ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use async_trait::async_trait;
use std::error::Error;
use std::pin::Pin;
use tokio_stream::Stream;

pub mod ollama;

#[async_trait]
pub trait LLMProvider: Send + Sync {
    fn get_base_url(&self) -> String;
    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>>;
    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>>;
    async fn chat(
        &self,
        request: ChatRequest,
    ) -> Result<ChatResponse, Box<dyn Error + Send + Sync>>;

    // Returns a pinned boxed stream of events or an error
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>,
        Box<dyn Error + Send + Sync>,
    >;
}
