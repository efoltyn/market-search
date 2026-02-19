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
        if self.provider == ProviderKind::OpenRouter && Self::env_truthy("ELI_OPENROUTER_FORCE_FREE")
        {
            req.model = "openrouter/free".to_string();
        }
        let disable_stream_for_openrouter =
            self.provider == ProviderKind::OpenRouter && Self::env_truthy("ELI_OPENROUTER_NON_STREAM");
        if disable_stream_for_openrouter {
            req.stream = false;
        }

        let url = self.endpoint("chat/completions");
        let mut body = json!({
            "model": req.model,
            "messages": req.messages.iter().map(Self::to_openai_message).collect::<Vec<_>>(),
            "stream": req.stream,
            "temperature": req.temperature,
            "max_tokens": req.max_tokens,
        });
        if req.stream {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if self.provider == ProviderKind::OpenRouter
            && req.response_format == Some(ResponseFormat::EliContractJsonSchema)
        {
            body["response_format"] = Self::openrouter_response_format();
        }
        if self.provider == ProviderKind::OpenRouter {
            body["provider"] = self.openrouter_provider_preferences();
            if let Some(models) = self.openrouter_models_with_fallbacks(&req.model) {
                body["models"] = json!(models);
            }
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

        if !req.stream {
            let value: serde_json::Value = response
                .json()
                .await
                .map_err(|e| AdapterError::Json(e.to_string()))?;
            if let Some(err) = value.get("error") {
                return Err(AdapterError::Http(err.to_string()));
            }
            let content = Self::collect_openai_message_content(&value);
            if content.trim().is_empty() {
                let detail = Self::summarize_empty_assistant_payload(&value);
                return Err(AdapterError::StreamParse(format!(
                    "empty assistant message ({detail})"
                )));
            }
            let mut events: Vec<Result<ChatStreamEvent>> = vec![Ok(ChatStreamEvent::Delta(content))];
            if let Some(usage_value) = value.get("usage") {
                if let Ok(usage) = serde_json::from_value::<eli_core::types::Usage>(usage_value.clone()) {
                    events.push(Ok(ChatStreamEvent::Usage(usage)));
                }
            }
            events.push(Ok(ChatStreamEvent::Done));
            return Ok(futures::stream::iter(events).boxed());
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
