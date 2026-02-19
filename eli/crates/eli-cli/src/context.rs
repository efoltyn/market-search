mod chat_ui;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use console::Term as ConsoleTerm;
use crossterm::cursor;
use crossterm::event::{
    self as ct_event, Event as CtEvent, KeyCode as CtKeyCode, KeyEventKind,
    KeyModifiers as CtKeyModifiers,
};
use crossterm::queue;
use crossterm::style::{
    Attribute, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{self};
use eli_core::config::{self, ApprovalMode, AutoMode, ConfigFile, DisplayMode, Paths, RunMode};
use eli_core::contract::{self, StepStatus};
use eli_core::diff::engine::UndoManager;
use eli_core::diff::engine::{DiffEngine, DiffResult};
use eli_core::executor::command_runner::{CommandResult, CommandRunner};
use eli_core::orchestrator::{
    compact_memory_now, maybe_compact_memory, run_subagents, SubagentResult,
};
use eli_core::persistence::{EventKind, SessionEvent, SessionStore};
use eli_core::types::{ChatMessage, ChatRequest, ProviderKind};
use eli_core::LlmAdapter;
use futures::StreamExt;
use itertools::Itertools;
use once_cell::sync::Lazy;
use proc_macro2::{Delimiter, Spacing, TokenStream as ProcTokenStream, TokenTree as ProcTokenTree};
use quote::quote;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::Widget;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use regex::Regex;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{
    Cmd, CompletionType, ConditionalEventHandler, Config, Context as RustyContext, Editor, Event,
    EventHandler, Helper, KeyCode, KeyEvent, Modifiers,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use termimad::MadSkin;
use textwrap::{wrap, Options as WrapOptions};
use tokio::process::Command as TokioCommand;
use tokio::time::{timeout as tokio_timeout, Duration as TokioDuration};
use tracing::{info, warn};
use unicode_width::UnicodeWidthStr;

const MODEL_HEALTH_PATH: &str = "eli_research/data/agent_model_health.json";
const MODEL_DISABLE_CONSECUTIVE_FAILURES: u32 = 3;
const MODEL_DISABLE_BASE_MINUTES: i64 = 10;
const MODEL_DISABLE_MAX_MINUTES: i64 = 180;
const MODEL_LIMIT_SIGNAL_COOLDOWN_SECS: i64 = 120;
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ModelHealthEntry {
    consecutive_failures: u32,
    last_error: Option<String>,
    last_seen_at: Option<String>,
    limit_until: Option<String>,
}

#[derive(Clone, Debug)]
struct ResearchArtifact {
    rel_path: String,
    title: String,
    status: String,
    created_utc: String,
    answer_hint: Option<String>,
}

#[derive(Clone, Serialize)]
struct ToolInfoArgCount {
    min: usize,
    max: usize,
}

#[derive(Clone, Serialize)]
struct ToolInfoArg {
    name: String,
    long: Option<String>,
    short: Option<String>,
    help: Option<String>,
    required: bool,
    value_type: String,
    num_args: Option<ToolInfoArgCount>,
    value_names: Option<Vec<String>>,
    possible_values: Option<Vec<String>>,
    default_values: Option<Vec<String>>,
}

#[derive(Clone, Serialize)]
struct ToolInfoSubcommand {
    name: String,
    about: Option<String>,
}

#[derive(Clone, Serialize)]
struct ToolInfoResponse {
    command: String,
    about: Option<String>,
    args: Vec<ToolInfoArg>,
    subcommands: Vec<ToolInfoSubcommand>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    available_subcommands: Option<Vec<ToolInfoSubcommand>>,
}

/// Runtime session state (not persisted to config)
struct SessionState {
    display_mode: DisplayMode,
    auto_mode: AutoMode,
    total_work_time: Duration,
    step_count: u32,
    prompt_queue: Vec<String>,
    input_buffer: String,
    cursor_pos: usize,
    prompt_history: Vec<String>,
    history_cursor: Option<usize>,
    recent_research: Vec<ResearchArtifact>,
    total_usage: eli_core::types::Usage,
    last_usage: Option<eli_core::types::Usage>,
}

const FOOTER_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

struct FooterUi {
    height: u16,
    active: bool,
    term_width: usize,
    term_height: usize,
}
