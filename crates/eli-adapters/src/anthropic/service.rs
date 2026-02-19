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
                                let prompt_delta =
                                    usage.prompt_tokens.saturating_sub(st.last_prompt_tokens);
                                let completion_delta = usage
                                    .completion_tokens
                                    .saturating_sub(st.last_completion_tokens);
                                st.last_prompt_tokens = usage.prompt_tokens;
                                st.last_completion_tokens = usage.completion_tokens;

                                if prompt_delta > 0 || completion_delta > 0 {
                                    return Some((
                                        Ok(ChatStreamEvent::Usage(eli_core::types::Usage {
                                            prompt_tokens: prompt_delta,
                                            completion_tokens: completion_delta,
                                            total_tokens: prompt_delta + completion_delta,
                                        })),
                                        st,
                                    ));
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
