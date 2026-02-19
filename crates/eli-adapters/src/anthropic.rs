//! Anthropic Claude API adapter with streaming support.

use async_trait::async_trait;
use eli_core::adapter::{AdapterError, ChatStream, LlmAdapter};
use eli_core::types::{ChatMessage, ChatRequest, ChatStreamEvent, ProviderKind, Role};
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::json;
use std::time::Duration;
use tracing::debug;

type Result<T> = std::result::Result<T, AdapterError>;

const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 8192;

include!("anthropic/model.rs");
include!("anthropic/messages.rs");
include!("anthropic/service.rs");
include!("anthropic/sse.rs");
