use crate::{AnthropicAdapter, MockAdapter, OllamaAdapter, OpenAiCompatibleAdapter};
use eli_core::adapter::{AdapterError, LlmAdapter};
use eli_core::config::ChatConfig;
use eli_core::types::ProviderKind;

type Result<T> = std::result::Result<T, AdapterError>;

pub fn build_from_chat_config(chat: &ChatConfig) -> Result<Box<dyn LlmAdapter>> {
    match chat.provider {
        ProviderKind::Mock => Ok(Box::new(MockAdapter::new(chat.model.clone()))),
        ProviderKind::OpenAI | ProviderKind::OpenRouter => {
            let api_key = chat
                .resolved_api_key()
                .ok_or(AdapterError::MissingApiKey(chat.provider))?;
            let base_url = chat
                .resolved_base_url()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            let adapter = OpenAiCompatibleAdapter::new(
                chat.provider,
                chat.model.clone(),
                base_url,
                api_key,
                chat.timeout_secs,
            )?;
            Ok(Box::new(adapter))
        }
        ProviderKind::Anthropic => {
            let api_key = chat
                .resolved_api_key()
                .ok_or(AdapterError::MissingApiKey(chat.provider))?;
            let base_url = chat
                .resolved_base_url()
                .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string());
            let adapter =
                AnthropicAdapter::new(chat.model.clone(), base_url, api_key, chat.timeout_secs)?;
            Ok(Box::new(adapter))
        }
        ProviderKind::Ollama => {
            let base_url = chat
                .resolved_base_url()
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            let adapter = OllamaAdapter::new(chat.model.clone(), base_url, chat.timeout_secs)?;
            Ok(Box::new(adapter))
        }
    }
}
