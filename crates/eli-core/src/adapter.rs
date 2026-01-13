//! LLM Adapter trait - defined here to avoid circular dependencies.

use crate::types::{ChatRequest, ChatStreamEvent, ProviderKind};
use async_trait::async_trait;
use futures::stream::BoxStream;

pub type ChatStream = BoxStream<'static, Result<ChatStreamEvent, AdapterError>>;

#[derive(Debug)]
pub enum AdapterError {
    MissingApiKey(ProviderKind),
    UnsupportedProvider(ProviderKind),
    Http(String),
    Json(String),
    StreamParse(String),
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingApiKey(p) => write!(f, "missing API key for provider {}", p),
            Self::UnsupportedProvider(p) => write!(f, "unsupported provider {}", p),
            Self::Http(e) => write!(f, "http error: {}", e),
            Self::Json(e) => write!(f, "json error: {}", e),
            Self::StreamParse(e) => write!(f, "stream parse error: {}", e),
        }
    }
}

impl std::error::Error for AdapterError {}

#[async_trait]
pub trait LlmAdapter: Send + Sync {
    fn provider(&self) -> ProviderKind;
    fn model(&self) -> &str;

    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream, AdapterError>;

    async fn chat(&self, mut req: ChatRequest) -> Result<String, AdapterError> {
        use futures::StreamExt;
        req.stream = true;
        let mut stream = self.chat_stream(req).await?;
        let mut out = String::new();
        while let Some(item) = stream.next().await {
            match item? {
                ChatStreamEvent::Delta(delta) => out.push_str(&delta),
                ChatStreamEvent::Usage(_) => {}
                ChatStreamEvent::Done => break,
            }
        }
        Ok(out)
    }
}
