pub async fn maybe_compact_memory(
    adapter: Arc<dyn LlmAdapter>,
    cfg: &ChatConfig,
    memory: &mut Memory,
) -> Result<Option<CompactionResult>> {
    if !cfg.compact {
        return Ok(None);
    }

    let trigger_tokens = cfg.resolved_compact_trigger_tokens();
    if let Some(token_trigger) = trigger_tokens {
        let estimated = estimate_prompt_tokens(&memory.context());
        if estimated < token_trigger {
            return Ok(None);
        }
    } else {
        let trigger = cfg.resolved_compact_trigger();
        if memory.len() < trigger {
            return Ok(None);
        }
    }

    let (older, keep_last, existing_summary) = {
        let total = memory.len();
        let mut keep_last = cfg.resolved_compact_keep().min(total);

        // Never drop the most recent message in the live chat loop; the caller expects a user/assistant
        // turn to remain in-context. (If the last message is enormous, handle it before inserting it.)
        if keep_last == 0 && total > 0 {
            keep_last = 1;
        }

        // Ensure there are actually older messages to summarize.
        if keep_last >= total {
            keep_last = total.saturating_sub(1);
        }

        let older = memory.older_messages(keep_last);
        if older.is_empty() {
            return Ok(None);
        }
        let existing_summary = memory.summary().map(|s| s.to_string());
        (older, keep_last, existing_summary)
    };

    let summary_model = cfg.resolved_summary_model().to_string();
    let dropped = older.len();
    let summary = summarize(adapter, summary_model, existing_summary, older).await?;
    memory.drop_older(keep_last);
    memory.set_summary(Some(summary.clone()));

    Ok(Some(CompactionResult { summary, dropped }))
}

pub async fn compact_memory_now(
    adapter: Arc<dyn LlmAdapter>,
    cfg: &ChatConfig,
    memory: &mut Memory,
) -> Result<Option<CompactionResult>> {
    let (older, keep_last, existing_summary) = {
        let total = memory.len();
        if total == 0 {
            return Ok(None);
        }

        let mut keep_last = cfg.resolved_compact_keep().min(total);
        if keep_last == 0 && total > 0 {
            keep_last = 1;
        }

        // Ensure there are actually older messages to summarize.
        if keep_last >= total {
            keep_last = total.saturating_sub(1);
        }

        let older = memory.older_messages(keep_last);
        if older.is_empty() {
            return Ok(None);
        }
        let existing_summary = memory.summary().map(|s| s.to_string());
        (older, keep_last, existing_summary)
    };

    let summary_model = cfg.resolved_summary_model().to_string();
    let dropped = older.len();
    let summary = summarize(adapter, summary_model, existing_summary, older).await?;
    memory.drop_older(keep_last);
    memory.set_summary(Some(summary.clone()));

    Ok(Some(CompactionResult { summary, dropped }))
}

pub async fn summarize_for_compaction(
    adapter: Arc<dyn LlmAdapter>,
    cfg: &ChatConfig,
    existing_summary: Option<String>,
    transcript: Vec<ChatMessage>,
) -> Result<String> {
    let summary_model = cfg.resolved_summary_model().to_string();
    summarize(adapter, summary_model, existing_summary, transcript).await
}

