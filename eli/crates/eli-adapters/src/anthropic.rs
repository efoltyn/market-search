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

pub struct AnthropicAdapter {
    model: String,
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl AnthropicAdapter {
    pub fn new(
        model: String,
        base_url: String,
        api_key: String,
        timeout_secs: u64,
    ) -> Result<Self> {
        let timeout = Duration::from_secs(timeout_secs.max(1));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            // Avoid macOS SystemConfiguration proxy lookup crashes.
            .no_proxy()
            .build()
            .map_err(|e| AdapterError::Http(e.to_string()))?;
        Ok(Self {
            model,
            base_url,
            api_key,
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

    /// Convert messages to Anthropic format.
    /// Returns (system_prompt, messages) tuple since Anthropic separates system from messages.
    fn convert_messages(messages: &[ChatMessage]) -> (Option<String>, Vec<serde_json::Value>) {
        let mut system_prompt = None;
        let mut converted = Vec::new();

        for msg in messages {
            match msg.role {
                Role::System => {
                    // Anthropic wants system as a separate top-level parameter
                    system_prompt = Some(msg.content.clone());
                }
                Role::User => {
                    converted.push(json!({
                        "role": "user",
                        "content": msg.content
                    }));
                }
                Role::Assistant => {
                    converted.push(json!({
                        "role": "assistant",
                        "content": msg.content
                    }));
                }
                Role::Tool => {
                    // Tool results are sent as user messages with observation prefix
                    converted.push(json!({
                        "role": "user",
                        "content": format!("Observation: {}", msg.content)
                    }));
                }
            }
        }

        // Anthropic requires alternating user/assistant messages
        // Merge consecutive messages of the same role
        let merged = merge_consecutive_roles(converted);

        (system_prompt, merged)
    }
}

/// Merge consecutive messages with the same role (Anthropic requirement)
fn merge_consecutive_roles(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut result: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");

        if let Some(last) = result.last_mut() {
            let last_role = last.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if last_role == role {
                // Merge content
                let last_content = last.get("content").and_then(|c| c.as_str()).unwrap_or("");
                *last = json!({
                    "role": role,
                    "content": format!("{}\n\n{}", last_content, content)
                });
                continue;
            }
        }
        result.push(msg);
    }

    result
}

#[async_trait]
impl LlmAdapter for AnthropicAdapter {
    fn provider(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn chat_stream(&self, mut req: ChatRequest) -> Result<ChatStream> {
        req.stream = true;
        if req.model.is_empty() {
            req.model = self.model.clone();
        }

        let url = self.endpoint("messages");
        let (system_prompt, messages) = Self::convert_messages(&req.messages);

        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            "stream": true,
        });

        if let Some(system) = system_prompt {
            body["system"] = json!(system);
        }

        if let Some(temp) = req.temperature {
            body["temperature"] = json!(temp);
        }

        debug!(provider = "anthropic", url = %url, "sending anthropic streaming request");

        let response = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AdapterError::Http(e.to_string()))?;

        // Check for error response
        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(AdapterError::StreamParse(format!(
                "Anthropic API error ({}): {}",
                status, error_text
            )));
        }

        let byte_stream: BoxStream<'static, std::result::Result<Vec<u8>, reqwest::Error>> =
            response
                .bytes_stream()
                .map(|r| r.map(|b| b.to_vec()))
                .boxed();

        let state = AnthropicSseState {
            stream: byte_stream,
            buffer: String::new(),
            done: false,
            last_prompt_tokens: 0,
            last_completion_tokens: 0,
        };

        Ok(futures::stream::unfold(state, |mut st| async move {
            loop {
                if st.done {
                    return None;
                }

                if let Some(data) = drain_next_sse_data(&mut st.buffer) {
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }

                    match parse_anthropic_event(data) {
                        Ok(Some(event)) => match event {
                            AnthropicEvent::Delta(text) => {
                                return Some((Ok(ChatStreamEvent::Delta(text)), st));
                            }
                            AnthropicEvent::Usage(usage) => {
                                let prompt_delta = usage.prompt_tokens.saturating_sub(st.last_prompt_tokens);
                                let completion_delta = usage.completion_tokens.saturating_sub(st.last_completion_tokens);
                                st.last_prompt_tokens = usage.prompt_tokens;
                                st.last_completion_tokens = usage.completion_tokens;
                                
                                if prompt_delta > 0 || completion_delta > 0 {
                                    return Some((Ok(ChatStreamEvent::Usage(eli_core::types::Usage {
                                        prompt_tokens: prompt_delta,
                                        completion_tokens: completion_delta,
                                        total_tokens: prompt_delta + completion_delta,
                                    })), st));
                                }
                                continue;
                            }
                            AnthropicEvent::Done => {
                                st.done = true;
                                return Some((Ok(ChatStreamEvent::Done), st));
                            }
                            AnthropicEvent::Continue => continue,
                        },
                        Ok(None) => continue,
                        Err(e) => {
                            st.done = true;
                            return Some((Err(e), st));
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

struct AnthropicSseState {
    stream: BoxStream<'static, std::result::Result<Vec<u8>, reqwest::Error>>,
    buffer: String,
    done: bool,
    last_prompt_tokens: u32,
    last_completion_tokens: u32,
}

enum AnthropicEvent {
    Delta(String),
    Usage(eli_core::types::Usage),
    Done,
    Continue,
}

fn drain_next_sse_data(buffer: &mut String) -> Option<String> {
    // Look for complete SSE event (ends with \n\n)
    let idx = buffer.find("\n\n")?;
    let event_block = buffer[..idx].to_string();
    buffer.drain(..idx + 2);

    // Parse event type and data
    let mut event_type = String::new();
    let mut data = String::new();

    for line in event_block.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }

    // Return data with event type prefix for parsing
    if !event_type.is_empty() {
        Some(format!("{}:{}", event_type, data))
    } else if !data.is_empty() {
        Some(data)
    } else {
        Some(String::new())
    }
}

fn parse_anthropic_event(data: &str) -> Result<Option<AnthropicEvent>> {
    // Split event type from data
    let (event_type, json_data) = if let Some(idx) = data.find(':') {
        let (evt, rest) = data.split_at(idx);
        (evt, rest.trim_start_matches(':'))
    } else {
        ("", data)
    };

    // Handle different event types
    match event_type {
        "message_start" => {
            if json_data.is_empty() {
                return Ok(Some(AnthropicEvent::Continue));
            }
            let value: serde_json::Value = serde_json::from_str(json_data)
                .map_err(|e| AdapterError::Json(e.to_string()))?;
            
            if let Some(usage_val) = value.get("message").and_then(|m| m.get("usage")) {
                let input = usage_val.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let output = usage_val.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                return Ok(Some(AnthropicEvent::Usage(eli_core::types::Usage {
                    prompt_tokens: input,
                    completion_tokens: output,
                    total_tokens: input + output,
                })));
            }
            Ok(Some(AnthropicEvent::Continue))
        }
        "content_block_start" | "ping" => {
            Ok(Some(AnthropicEvent::Continue))
        }
        "content_block_delta" => {
            // Parse the delta
            if json_data.is_empty() {
                return Ok(Some(AnthropicEvent::Continue));
            }
            let value: serde_json::Value = serde_json::from_str(json_data)
                .map_err(|e| AdapterError::Json(e.to_string()))?;

            // Extract text from delta
            if let Some(text) = value.pointer("/delta/text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    return Ok(Some(AnthropicEvent::Delta(text.to_string())));
                }
            }
            Ok(Some(AnthropicEvent::Continue))
        }
        "content_block_stop" => Ok(Some(AnthropicEvent::Continue)),
        "message_delta" => {
            if json_data.is_empty() {
                return Ok(Some(AnthropicEvent::Continue));
            }
            let value: serde_json::Value = serde_json::from_str(json_data)
                .map_err(|e| AdapterError::Json(e.to_string()))?;

            if let Some(usage_val) = value.get("usage") {
                let output = usage_val.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                return Ok(Some(AnthropicEvent::Usage(eli_core::types::Usage {
                    prompt_tokens: 0, 
                    completion_tokens: output,
                    total_tokens: output,
                })));
            }
            Ok(Some(AnthropicEvent::Continue))
        }
        "message_stop" => Ok(Some(AnthropicEvent::Done)),
        "error" => {
            if json_data.is_empty() {
                return Err(AdapterError::StreamParse(
                    "Unknown Anthropic error".to_string(),
                ));
            }
            let value: serde_json::Value = serde_json::from_str(json_data)
                .map_err(|e| AdapterError::Json(e.to_string()))?;
            let error_msg = value
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown error");
            Err(AdapterError::StreamParse(format!("Anthropic error: {}", error_msg)))
        }
        _ => {
            // Unknown event type, try to parse as JSON anyway
            if json_data.is_empty() {
                return Ok(None);
            }
            // Ignore unknown events
            Ok(Some(AnthropicEvent::Continue))
        }
    }
}
