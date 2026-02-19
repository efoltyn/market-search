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

include!("compaction.rs");
include!("subagents.rs");
include!("helpers.rs");
