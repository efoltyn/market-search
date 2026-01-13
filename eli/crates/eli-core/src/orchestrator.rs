use crate::adapter::LlmAdapter;
use crate::config::ChatConfig;
use crate::contract::SubagentTask;
use crate::memory::Memory;
use crate::types::{ChatMessage, ChatRequest};
use anyhow::{anyhow, Context, Result};
use futures::stream::{self, StreamExt};
use std::sync::Arc;

const SUMMARY_SYSTEM_PROMPT: &str = "You compact Eli's memory. Produce a concise, durable summary.\n\
Include: decisions, constraints, files/paths touched, key commands/results, open questions, next steps.\n\
Plain text, tight bullets, no JSON.";
const SUMMARY_INPUT_MAX_CHARS: usize = 12_000;
const SUMMARY_OUTPUT_MAX_CHARS: usize = 4_000;
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

    let trigger = cfg.resolved_compact_trigger();
    let (older, keep_last, existing_summary) = {
        if memory.len() < trigger {
            return Ok(None);
        }
        let mut keep_last = cfg.resolved_compact_keep();
        if keep_last >= trigger {
            keep_last = trigger.saturating_sub(1).max(1);
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
    let mut transcript = String::new();
    if let Some(summary) = existing_summary {
        transcript.push_str("Existing summary:\n");
        transcript.push_str(&summary);
        transcript.push_str("\n\n");
    }

    transcript.push_str("Transcript:\n");
    for msg in messages {
        let role = format_role(&msg);
        transcript.push_str(&format!("{role}: {content}\n", role = role, content = msg.content));
    }

    let content = truncate_chars(&transcript, SUMMARY_INPUT_MAX_CHARS);

    let req = ChatRequest {
        model: summary_model,
        messages: vec![
            ChatMessage::system(SUMMARY_SYSTEM_PROMPT),
            ChatMessage::user(content),
        ],
        temperature: Some(0.2),
        max_tokens: Some(800),
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
