use crate::adapter::LlmAdapter;
use crate::config::ChatConfig;
use crate::contract::SubagentTask;
use crate::memory::Memory;
use crate::types::{ChatMessage, ChatRequest};
use anyhow::{anyhow, Context, Result};
use futures::stream::{self, StreamExt};
use std::sync::Arc;

const SUMMARY_SYSTEM_PROMPT: &str = "YOU ARE COMPACTING ELI'S MEMORY FOR A HANDOFF.\n\
\n\
SUMMARIZE THE USER'S IDEA IN DEPTH. DO NOT OVER-COMPRESS.\n\
\n\
BE CLEAR WHERE YOU ARE LEAVING OFF.\n\
BE EXPLICIT ABOUT WHAT HAS ACTUALLY BEEN DONE VS WHAT IS NOT DONE YET.\n\
DO NOT IMPLY WORK HAPPENED IF IT DIDN'T.\n\
\n\
GIVE THE NEXT STEPS (CONCRETE, ACTIONABLE).\n\
BE CONFIDENT: PICK A DEFAULT PATH FORWARD INSTEAD OF LISTING OPTIONS.\n\
BE CRITICAL: STATE THE BIGGEST RISK/FAILURE MODE AND THE FASTEST WAY TO VERIFY OR DE-RISK IT.\n\
\n\
DO NOT ASK THE USER QUESTIONS.\n\
PLAIN TEXT ONLY. NO JSON.\n\
USE AS MUCH SPACE AS NEEDED UP TO THE LIMIT.";
const SUMMARY_INPUT_MAX_CHARS: usize = 200_000;
const SUMMARY_OUTPUT_MAX_CHARS: usize = 50_000;
const SUMMARY_MAX_TOKENS: u32 = 8192;
const SUBAGENT_CONTEXT_MESSAGES: usize = 6;

#[derive(Clone, Debug)]
pub struct CompactionResult {
    pub summary: String,
    pub dropped: usize,
}

#[derive(Clone, Debug)]
pub struct SubagentResult {
    pub name: String,
    pub output: String,
    pub error: Option<String>,
}

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

    Ok(Some(CompactionResult {
        summary,
        dropped,
    }))
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

pub async fn run_subagents(
    adapter: Arc<dyn LlmAdapter>,
    cfg: &ChatConfig,
    memory: &Memory,
    tasks: &[SubagentTask],
) -> Vec<SubagentResult> {
    if tasks.is_empty() {
        return Vec::new();
    }

    let context = build_subagent_context(memory);
    let cfg = cfg.clone();
    let tasks: Vec<SubagentTask> = tasks.to_vec();
    let max_parallel = cfg.resolved_parallel_subagents();

    let stream = stream::iter(tasks.into_iter().enumerate()).map(|(idx, task)| {
        let adapter = adapter.clone();
        let cfg = cfg.clone();
        let context = context.clone();
        async move {
            let res = run_one_subagent(adapter, cfg, task, context).await;
            (idx, res)
        }
    });

    let mut out: Vec<(usize, SubagentResult)> = stream.buffer_unordered(max_parallel).collect().await;
    out.sort_by_key(|(idx, _)| *idx);
    out.into_iter().map(|(_, result)| result).collect()
}

async fn run_one_subagent(
    adapter: Arc<dyn LlmAdapter>,
    cfg: ChatConfig,
    task: SubagentTask,
    context: String,
) -> SubagentResult {
    let name = task.name.trim().to_string();
    let task_text = task.task.trim().to_string();
    if name.is_empty() || task_text.is_empty() {
        return SubagentResult {
            name: if name.is_empty() { "subagent".to_string() } else { name },
            output: String::new(),
            error: Some("empty subagent name or task".to_string()),
        };
    }

    let prompt = format!(
        "Task:\n{task}\n\nContext:\n{context}",
        task = task_text,
        context = context
    );

    let req = ChatRequest {
        model: task.model.as_deref().unwrap_or(cfg.model.as_str()).to_string(),
        messages: vec![
            ChatMessage::system(subagent_system_prompt(&name)),
            ChatMessage::user(prompt),
        ],
        temperature: task.temperature.or(cfg.temperature).or(Some(0.2)),
        max_tokens: task.max_tokens.or(Some(800)),
        response_format: None,
        stream: false,
    };

    match adapter.chat(req).await {
        Ok(out) => SubagentResult {
            name,
            output: truncate_chars(out.trim(), SUMMARY_OUTPUT_MAX_CHARS),
            error: None,
        },
        Err(e) => SubagentResult {
            name,
            output: String::new(),
            error: Some(e.to_string()),
        },
    }
}

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
            out.push_str(&format!("{role}: {content}\n", role = role, content = msg.content));
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
