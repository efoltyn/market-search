fn build_subagent_context(memory: &Memory) -> String {
    let mut out = String::new();
    if let Some(summary) = memory.summary() {
        out.push_str("Summary:\n");
        out.push_str(summary);
        out.push_str("\n\n");
    }

    let recent = memory.recent_messages(SUBAGENT_CONTEXT_MESSAGES);
    if !recent.is_empty() {
        out.push_str("Recent messages:\n");
        for msg in recent {
            let role = format_role(&msg);
            out.push_str(&format!(
                "{role}: {content}\n",
                role = role,
                content = msg.content
            ));
        }
    }

    truncate_chars(&out, SUMMARY_INPUT_MAX_CHARS)
}

async fn summarize(
    adapter: Arc<dyn LlmAdapter>,
    summary_model: String,
    existing_summary: Option<String>,
    messages: Vec<ChatMessage>,
) -> Result<String> {
    let mut existing_block = String::new();
    if let Some(summary) = existing_summary {
        existing_block.push_str("Existing summary:\n");
        existing_block.push_str(&summary);
        existing_block.push_str("\n\n");
    }

    let mut transcript_block = String::new();
    transcript_block.push_str("Transcript (most recent last):\n");
    for msg in messages {
        let role = format_role(&msg);
        transcript_block.push_str(&format!(
            "{role}: {content}\n",
            role = role,
            content = msg.content
        ));
    }

    let content = if existing_block.len() >= SUMMARY_INPUT_MAX_CHARS {
        truncate_chars(&existing_block, SUMMARY_INPUT_MAX_CHARS)
    } else {
        let remaining = SUMMARY_INPUT_MAX_CHARS.saturating_sub(existing_block.len());
        let tail = tail_chars(&transcript_block, remaining);
        format!("{existing_block}{tail}")
    };

    let req = ChatRequest {
        model: summary_model,
        messages: vec![
            ChatMessage::system(SUMMARY_SYSTEM_PROMPT),
            ChatMessage::user(content),
        ],
        temperature: Some(0.2),
        max_tokens: Some(SUMMARY_MAX_TOKENS),
        response_format: None,
        stream: false,
    };

    let out = adapter
        .chat(req)
        .await
        .map_err(|e| anyhow!(e))
        .context("summary chat")?;
    let summary = out.trim();
    if summary.is_empty() {
        return Err(anyhow!("summary empty"));
    }

    Ok(truncate_chars(summary, SUMMARY_OUTPUT_MAX_CHARS))
}

fn subagent_system_prompt(name: &str) -> String {
    format!(
        "You are Eli subagent: {name}. Provide concise, actionable output.\n\
No JSON, no filler. Use short bullets when helpful.",
        name = name
    )
}

fn format_role(msg: &ChatMessage) -> &'static str {
    match msg.role {
        crate::types::Role::System => "system",
        crate::types::Role::User => "user",
        crate::types::Role::Assistant => "assistant",
        crate::types::Role::Tool => "tool",
    }
}

fn truncate_chars(input: &str, max: usize) -> String {
    if input.len() <= max {
        return input.to_string();
    }
    let mut out = String::new();
    for (idx, ch) in input.char_indices() {
        if idx >= max {
            break;
        }
        out.push(ch);
    }
    let remaining = input.len().saturating_sub(out.len());
    format!("{out}... [truncated {remaining} bytes]")
}

fn tail_chars(input: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if input.len() <= max {
        return input.to_string();
    }
    let start_target = input.len().saturating_sub(max);
    let start = input
        .char_indices()
        .find(|(idx, _)| *idx >= start_target)
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    input[start..].to_string()
}

fn estimate_prompt_tokens(messages: &[ChatMessage]) -> usize {
    // Rough cross-provider token estimate:
    // - Most LLM tokenizers average ~4 bytes/token on English-ish text.
    // - We intentionally overcount slightly via a small per-message overhead.
    let mut bytes: usize = 0;
    for msg in messages {
        bytes = bytes.saturating_add(msg.content.as_bytes().len());
        bytes = bytes.saturating_add(16);
        for img in &msg.images {
            bytes = bytes.saturating_add(img.as_bytes().len());
        }
        if let Some(name) = &msg.name {
            bytes = bytes.saturating_add(name.as_bytes().len());
        }
    }
    (bytes + 3) / 4
}
