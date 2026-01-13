#![forbid(unsafe_code)]

mod anthropic;
mod factory;
mod mock;
mod ollama;
mod openai_compatible;

// Re-export from eli-core
pub use eli_core::adapter::{AdapterError, ChatStream, LlmAdapter};

pub use anthropic::AnthropicAdapter;
pub use factory::build_from_chat_config;
pub use mock::MockAdapter;
pub use ollama::OllamaAdapter;
pub use openai_compatible::OpenAiCompatibleAdapter;

