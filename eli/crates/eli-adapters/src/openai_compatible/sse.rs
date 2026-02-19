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
