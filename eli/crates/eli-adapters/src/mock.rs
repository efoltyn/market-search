use async_trait::async_trait;
use eli_core::adapter::{AdapterError, ChatStream, LlmAdapter};
use eli_core::types::{ChatRequest, ChatStreamEvent, ProviderKind, Role};
use futures::StreamExt;

type Result<T> = std::result::Result<T, AdapterError>;

pub struct MockAdapter {
    model: String,
}

impl MockAdapter {
    pub fn new(model: String) -> Self {
        Self { model }
    }
}

#[async_trait]
impl LlmAdapter for MockAdapter {
    fn provider(&self) -> ProviderKind {
        ProviderKind::Mock
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream> {
        let last_user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.clone())
            .unwrap_or_default();

        let response = format!(
            "eli(mock:{model}): {last_user}",
            model = self.model
        );
        let mut events = Vec::new();
        for chunk in response.as_bytes().chunks(16) {
            events.push(Ok(ChatStreamEvent::Delta(
                String::from_utf8_lossy(chunk).to_string(),
            )));
        }
        events.push(Ok(ChatStreamEvent::Done));
        Ok(futures::stream::iter(events).boxed())
    }
}

