//! Ollama local LLM adapter with streaming support.

use async_trait::async_trait;
use eli_core::adapter::{AdapterError, ChatStream, LlmAdapter};
use eli_core::types::{ChatMessage, ChatRequest, ChatStreamEvent, ProviderKind, Role};
use futures::stream::BoxStream;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::debug;

type Result<T> = std::result::Result<T, AdapterError>;

const DEFAULT_TIMEOUT_SECS: u64 = 300; // Ollama can be slow for local inference

pub struct OllamaAdapter {
    model: String,
    base_url: String,
    client: reqwest::Client,
}

impl OllamaAdapter {
    pub fn new(model: String, base_url: String, timeout_secs: u64) -> Result<Self> {
        let timeout = Duration::from_secs(timeout_secs.max(DEFAULT_TIMEOUT_SECS));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            // Avoid macOS SystemConfiguration proxy lookup crashes.
            .no_proxy()
            .build()
            .map_err(|e| AdapterError::Http(e.to_string()))?;
        Ok(Self {
            model,
            base_url,
            client,
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    fn convert_messages(messages: &[ChatMessage]) -> Vec<OllamaMessage> {
        messages
            .iter()
            .map(|msg| {
                let role = match msg.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "user", // Tool results as user messages
                };
                let content = if msg.role == Role::Tool {
                    format!("Observation: {}", msg.content)
                } else {
                    msg.content.clone()
                };
                OllamaMessage {
                    role: role.to_string(),
                    content,
                }
            })
            .collect()
    }
}

#[derive(Serialize)]
struct OllamaMessage {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
}

#[derive(Serialize)]
struct OllamaOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
}

#[derive(Deserialize)]
struct OllamaStreamResponse {
    message: Option<OllamaResponseMessage>,
    done: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct OllamaResponseMessage {
    content: String,
}

#[async_trait]
impl LlmAdapter for OllamaAdapter {
    fn provider(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn chat_stream(&self, mut req: ChatRequest) -> Result<ChatStream> {
        req.stream = true;
        if req.model.is_empty() {
            req.model = self.model.clone();
        }

        let url = self.endpoint("api/chat");
        let messages = Self::convert_messages(&req.messages);

        let options = if req.temperature.is_some() || req.max_tokens.is_some() {
            Some(OllamaOptions {
                temperature: req.temperature,
                num_predict: req.max_tokens,
            })
        } else {
            None
        };

        let body = OllamaChatRequest {
            model: req.model,
            messages,
            stream: true,
            options,
        };

        debug!(provider = "ollama", url = %url, "sending ollama streaming request");

        let response = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AdapterError::Http(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(AdapterError::StreamParse(format!(
                "Ollama API error ({}): {}",
                status, error_text
            )));
        }

        let byte_stream: BoxStream<'static, std::result::Result<Vec<u8>, reqwest::Error>> =
            response
                .bytes_stream()
                .map(|r| r.map(|b| b.to_vec()))
                .boxed();

        let state = OllamaSseState {
            stream: byte_stream,
            buffer: String::new(),
            done: false,
        };

        Ok(futures::stream::unfold(state, |mut st| async move {
            loop {
                if st.done {
                    return None;
                }

                // Ollama sends newline-delimited JSON
                if let Some(line_end) = st.buffer.find('\n') {
                    let line = st.buffer[..line_end].to_string();
                    st.buffer.drain(..line_end + 1);

                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }

                    match serde_json::from_str::<OllamaStreamResponse>(line) {
                        Ok(resp) => {
                            if let Some(error) = resp.error {
                                st.done = true;
                                return Some((
                                    Err(AdapterError::StreamParse(format!("Ollama error: {}", error))),
                                    st,
                                ));
                            }

                            if resp.done {
                                st.done = true;
                                return Some((Ok(ChatStreamEvent::Done), st));
                            }

                            if let Some(msg) = resp.message {
                                if !msg.content.is_empty() {
                                    return Some((Ok(ChatStreamEvent::Delta(msg.content)), st));
                                }
                            }
                            continue;
                        }
                        Err(e) => {
                            // Try to continue on parse errors for partial data
                            debug!(error = %e, line = %line, "ollama parse error, continuing");
                            continue;
                        }
                    }
                }

                match st.stream.next().await {
                    Some(Ok(chunk)) => {
                        st.buffer.push_str(&String::from_utf8_lossy(&chunk));
                    }
                    Some(Err(e)) => {
                        st.done = true;
                        return Some((Err(AdapterError::Http(e.to_string())), st));
                    }
                    None => {
                        st.done = true;
                        return Some((Ok(ChatStreamEvent::Done), st));
                    }
                }
            }
        })
        .boxed())
    }
}

struct OllamaSseState {
    stream: BoxStream<'static, std::result::Result<Vec<u8>, reqwest::Error>>,
    buffer: String,
    done: bool,
}

impl OllamaAdapter {
    /// List available models from Ollama
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let url = self.endpoint("api/tags");
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| AdapterError::Http(e.to_string()))?;

        if !response.status().is_success() {
            return Ok(Vec::new());
        }

        #[derive(Deserialize)]
        struct TagsResponse {
            models: Vec<ModelInfo>,
        }

        #[derive(Deserialize)]
        struct ModelInfo {
            name: String,
        }

        let tags: TagsResponse = response
            .json()
            .await
            .map_err(|e| AdapterError::Json(e.to_string()))?;
        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }
}
