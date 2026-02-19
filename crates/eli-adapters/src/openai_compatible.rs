use async_trait::async_trait;
use eli_core::adapter::{AdapterError, ChatStream, LlmAdapter};
use eli_core::types::{ChatMessage, ChatRequest, ChatStreamEvent, ProviderKind, ResponseFormat};
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::json;
use std::time::Duration;
use tracing::debug;

type Result<T> = std::result::Result<T, AdapterError>;

include!("openai_compatible/model.rs");
include!("openai_compatible/service.rs");
include!("openai_compatible/sse.rs");
