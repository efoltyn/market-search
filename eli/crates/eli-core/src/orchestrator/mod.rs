use crate::adapter::LlmAdapter;
use crate::config::ChatConfig;
use crate::contract::SubagentTask;
use crate::memory::Memory;
use crate::types::{ChatMessage, ChatRequest};
use anyhow::{anyhow, Context, Result};
use futures::stream::{self, StreamExt};
use std::sync::Arc;

pub const SUMMARY_SYSTEM_PROMPT: &str = "YOU ARE COMPACTING ELI'S MEMORY FOR A HANDOFF.\n\
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
pub const SUMMARY_INPUT_MAX_CHARS: usize = 200_000;
const SUMMARY_OUTPUT_MAX_CHARS: usize = 50_000;
pub const SUMMARY_MAX_TOKENS: u32 = 8192;
const SUBAGENT_CONTEXT_MESSAGES: usize = 6;

/// Preparation plan for streaming compact — split from apply so the TUI can stream in between.
pub struct CompactionPlan {
    pub older: Vec<ChatMessage>,
    pub keep_last: usize,
    pub existing_summary: Option<String>,
    pub dropped: usize,
}

/// Prepare a compaction plan without modifying memory.
pub fn plan_compact(cfg: &ChatConfig, memory: &Memory) -> Option<CompactionPlan> {
    let total = memory.len();
    if total == 0 {
        return None;
    }
    let mut keep_last = cfg.resolved_compact_keep().min(total);
    if keep_last == 0 {
        keep_last = 1;
    }
    if keep_last >= total {
        keep_last = total.saturating_sub(1);
    }
    let older = memory.older_messages(keep_last);
    if older.is_empty() {
        return None;
    }
    let existing_summary = memory.summary().map(|s| s.to_string());
    let dropped = older.len();
    Some(CompactionPlan {
        older,
        keep_last,
        existing_summary,
        dropped,
    })
}

/// Apply a completed compaction plan to memory.
pub fn apply_compact(memory: &mut Memory, plan: &CompactionPlan, summary: String) {
    memory.drop_older(plan.keep_last);
    memory.set_summary(Some(summary));
}

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

include!("compaction.rs");
include!("subagents.rs");
include!("helpers.rs");
