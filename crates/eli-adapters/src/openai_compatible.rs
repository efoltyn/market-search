use async_trait::async_trait;
use eli_core::adapter::{AdapterError, ChatStream, LlmAdapter};
use eli_core::types::{ChatMessage, ChatRequest, ChatStreamEvent, ProviderKind, ResponseFormat};
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::json;
use std::time::Duration;
use tracing::debug;

type Result<T> = std::result::Result<T, AdapterError>;

pub struct OpenAiCompatibleAdapter {
    provider: ProviderKind,
    model: String,
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl OpenAiCompatibleAdapter {
    pub fn new(
        provider: ProviderKind,
        model: String,
        base_url: String,
        api_key: String,
        timeout_secs: u64,
    ) -> Result<Self> {
        let timeout = Duration::from_secs(timeout_secs.max(1));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            // Avoid macOS SystemConfiguration proxy lookup crashes.
            // If proxy support is needed later, add explicit proxy config instead of system auto-detect.
            .no_proxy()
            .build()
            .map_err(|e| AdapterError::Http(e.to_string()))?;
        Ok(Self {
            provider,
            model,
            base_url,
            api_key: api_key.trim().to_string(),
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

    fn to_openai_message(msg: &ChatMessage) -> serde_json::Value {
        if !msg.images.is_empty() && msg.role == eli_core::types::Role::User {
            let mut content_parts = Vec::new();
            content_parts.push(json!({
                "type": "text",
                "text": msg.content
            }));

            for img in &msg.images {
                content_parts.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": img
                    }
                }));
            }

            let mut out = json!({
                "role": "user",
                "content": content_parts,
            });
            if let Some(name) = &msg.name {
                out["name"] = json!(name);
            }
            return out;
        }

        let (role, content) = match msg.role {
            eli_core::types::Role::System => ("system", msg.content.clone()),
            eli_core::types::Role::User => ("user", msg.content.clone()),
            eli_core::types::Role::Assistant => ("assistant", msg.content.clone()),
            eli_core::types::Role::Tool => ("user", format!("Observation: {}", msg.content)),
        };
        let mut out = json!({
            "role": role,
            "content": content,
        });
        if let Some(name) = &msg.name {
            out["name"] = json!(name);
        }
        out
    }

    fn openrouter_response_format() -> serde_json::Value {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "plan",
                "checklist",
                "focus",
                "status",
                "commands",
                "commands_parallel",
                "screen",
                "diffs",
                "notes",
                "subagents"
            ],
            "properties": {
                "plan": { "type": "string" },
                "checklist": { "type": "array", "items": { "type": "string" } },
                "focus": { "type": "string" },
                "status": { "type": "string", "enum": ["KEEP_WORKING", "DONE"] },
                "commands": { "type": "array", "items": { "type": "string" } },
                "commands_parallel": { "type": "boolean" },
                "screen": { "type": "array", "items": {} },
                "diffs": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "path": { "type": "string" },
                            "op": { "type": "string", "enum": ["create", "replace", "patch", "delete"] },
                            "before_sha256": { "type": "string" },
                            "after_text": { "type": "string" },
                            "patch": { "type": "string" }
                        },
                        "oneOf": [
                            { "required": ["path", "op", "after_text"], "properties": { "op": { "const": "create" } } },
                            { "required": ["path", "op", "after_text"], "properties": { "op": { "const": "replace" } } },
                            { "required": ["path", "op", "patch"], "properties": { "op": { "const": "patch" } } },
                            { "required": ["path", "op"], "properties": { "op": { "const": "delete" } } }
                        ]
                    }
                },
                "notes": { "type": "string" },
                "synthesis": {
                    "type": ["object", "null"],
                    "additionalProperties": false,
                    "required": ["summary", "answer", "next_steps"],
                    "properties": {
                        "summary": { "type": "array", "items": { "type": "string" } },
                        "answer": { "type": "string" },
                        "next_steps": { "type": "array", "items": { "type": "string" } }
                    }
                },
                "ask_user": { "type": ["string", "null"] },
                "subagents": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["name", "task"],
                        "properties": {
                            "name": { "type": "string" },
                            "task": { "type": "string" },
                            "model": { "type": ["string", "null"] },
                            "temperature": { "type": ["number", "null"] },
                            "max_tokens": { "type": ["integer", "null"] }
                        }
                    }
                }
            }
        });

        json!({
            "type": "json_schema",
            "json_schema": {
                "name": "eli_response",
                "description": "Eli tool contract response",
                "schema": schema,
                "strict": true
            }
        })
    }
}

#[async_trait]
impl LlmAdapter for OpenAiCompatibleAdapter {
    fn provider(&self) -> ProviderKind {
        self.provider
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn chat_stream(&self, mut req: ChatRequest) -> Result<ChatStream> {
        req.stream = true;
        if req.model.is_empty() {
            req.model = self.model.clone();
        }

        let url = self.endpoint("chat/completions");
        let mut body = json!({
            "model": req.model,
            "messages": req.messages.iter().map(Self::to_openai_message).collect::<Vec<_>>(),
            "stream": true,
            "stream_options": { "include_usage": true },
            "temperature": req.temperature,
            "max_tokens": req.max_tokens,
        });
        if self.provider == ProviderKind::OpenRouter
            && req.response_format == Some(ResponseFormat::EliContractJsonSchema)
        {
            body["response_format"] = Self::openrouter_response_format();
        }

        debug!(provider = %self.provider, url = %url, "sending openai-compatible streaming request");
        let mut request = self.client.post(url).bearer_auth(&self.api_key);
        // OpenRouter recommends (and sometimes requires) app identification headers.
        if self.provider == ProviderKind::OpenRouter {
            request = request
                .header("HTTP-Referer", "https://github.com/efoltyn/eli")
                .header("X-Title", "eli");
        }

        let response = request
            .json(&body)
            .send()
            .await
            .map_err(|e| AdapterError::Http(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let provider = self.provider.as_str();
            let mut msg = format!("HTTP status {status} for provider={provider}");
            if status.as_u16() == 401 {
                msg.push_str(" (unauthorized: check your API key)");
            }
            if !body.trim().is_empty() {
                msg.push_str(&format!(": {body}"));
            }
            return Err(AdapterError::Http(msg));
        }

        let byte_stream: BoxStream<'static, std::result::Result<Vec<u8>, reqwest::Error>> =
            response
                .bytes_stream()
                .map(|r| r.map(|b| b.to_vec()))
                .boxed();

        let state = SseState {
            stream: byte_stream,
            buffer: String::new(),
            done: false,
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
                    if data == "[DONE]" {
                        st.done = true;
                        return Some((Ok(ChatStreamEvent::Done), st));
                    }

                    match parse_openai_event(data) {
                        Ok(Some(event)) => return Some((Ok(event), st)),
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

struct SseState {
    stream: BoxStream<'static, std::result::Result<Vec<u8>, reqwest::Error>>,
    buffer: String,
    done: bool,
}

fn drain_next_sse_data(buffer: &mut String) -> Option<String> {
    let idx = buffer.find("\n\n")?;
    let event = buffer[..idx].to_string();
    buffer.drain(..idx + 2);

    let mut out = String::new();
    for line in event.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(rest.trim_start());
    }
    Some(out)
}

fn parse_openai_event(data: &str) -> Result<Option<ChatStreamEvent>> {
    let value: serde_json::Value =
        serde_json::from_str(data).map_err(|e| AdapterError::Json(e.to_string()))?;

    if let Some(error) = value.get("error") {
        return Err(AdapterError::StreamParse(error.to_string()));
    }

    if let Some(usage) = value.get("usage") {
        if let Ok(usage) = serde_json::from_value::<eli_core::types::Usage>(usage.clone()) {
            return Ok(Some(ChatStreamEvent::Usage(usage)));
        }
    }

    if let Some(delta) = value
        .pointer("/choices/0/delta/content")
        .and_then(|v| v.as_str())
    {
        if !delta.is_empty() {
            return Ok(Some(ChatStreamEvent::Delta(delta.to_string())));
        }
    }

    if value
        .pointer("/choices/0/finish_reason")
        .and_then(|v| v.as_str())
        .is_some()
    {
        // Some providers send usage in the same chunk as finish_reason, or after.
        // If we found usage above, we returned it. If not, it might be just finish_reason.
        return Ok(None);
    }

    Ok(None)
}
