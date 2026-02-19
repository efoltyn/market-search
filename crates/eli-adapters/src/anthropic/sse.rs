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
            let value: serde_json::Value =
                serde_json::from_str(json_data).map_err(|e| AdapterError::Json(e.to_string()))?;

            if let Some(usage_val) = value.get("message").and_then(|m| m.get("usage")) {
                let input = usage_val
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let output = usage_val
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                return Ok(Some(AnthropicEvent::Usage(eli_core::types::Usage {
                    prompt_tokens: input,
                    completion_tokens: output,
                    total_tokens: input + output,
                })));
            }
            Ok(Some(AnthropicEvent::Continue))
        }
        "content_block_start" | "ping" => Ok(Some(AnthropicEvent::Continue)),
        "content_block_delta" => {
            // Parse the delta
            if json_data.is_empty() {
                return Ok(Some(AnthropicEvent::Continue));
            }
            let value: serde_json::Value =
                serde_json::from_str(json_data).map_err(|e| AdapterError::Json(e.to_string()))?;

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
            let value: serde_json::Value =
                serde_json::from_str(json_data).map_err(|e| AdapterError::Json(e.to_string()))?;

            if let Some(usage_val) = value.get("usage") {
                let output = usage_val
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
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
            let value: serde_json::Value =
                serde_json::from_str(json_data).map_err(|e| AdapterError::Json(e.to_string()))?;
            let error_msg = value
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown error");
            Err(AdapterError::StreamParse(format!(
                "Anthropic error: {}",
                error_msg
            )))
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
