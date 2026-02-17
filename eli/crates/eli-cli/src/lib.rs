#![forbid(unsafe_code)]

mod chat_ui;

use aho_corasick::{AhoCorasickBuilder, MatchKind};
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
use indexmap::IndexMap;
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
use regex::{Regex, RegexBuilder};
use regex_automata::{meta::Regex as MetaRegex, Anchored, Input as RegexInput};
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
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use termimad::MadSkin;
use textwrap::{wrap, Options as WrapOptions};
use tokio::process::Command as TokioCommand;
use tokio::time::{timeout as tokio_timeout, Duration as TokioDuration};
use tracing::{info, warn};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MODEL_HEALTH_PATH: &str = "eli_research/data/agent_model_health.json";
const MODEL_DISABLE_CONSECUTIVE_FAILURES: u32 = 3;
const REGEX_CAPTURE_CACHE_MAX: usize = 128;

static REGEX_CAPTURE_CACHE: Lazy<Mutex<IndexMap<String, Regex>>> =
    Lazy::new(|| Mutex::new(IndexMap::new()));

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ModelHealthEntry {
    consecutive_failures: u32,
    last_error: Option<String>,
    last_seen_at: Option<String>,
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

fn has_interactive_terminal() -> bool {
    if env_truthy("ELI_PLAIN_OUTPUT") || env_truthy("ELI_NO_FOOTER") {
        return false;
    }
    if let Ok(term) = std::env::var("TERM") {
        if term.eq_ignore_ascii_case("dumb") {
            return false;
        }
    }
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => {
            let v = value.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        }
        Err(_) => false,
    }
}

impl FooterUi {
    fn enable() -> Self {
        if !has_interactive_terminal() {
            let (w, h) = terminal_size();
            return Self {
                height: 3,
                active: false,
                term_width: w,
                term_height: h,
            };
        }

        terminal::enable_raw_mode().ok();
        let mut out = std::io::stdout();
        queue!(out, cursor::Hide).ok();
        out.flush().ok();
        let (w, h) = terminal_size();
        let mut this = Self {
            height: 3,
            active: true,
            term_width: w,
            term_height: h,
        };
        // Clear footer area before setting up scroll region
        this.clear_footer_rows(&mut out);
        this.apply_scroll_region();
        this
    }

    fn disable(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        // Clear footer area before resetting scroll region
        let mut out = std::io::stdout();
        self.clear_footer_rows(&mut out);
        self.reset_scroll_region();
        queue!(out, cursor::Show).ok();
        out.flush().ok();
        terminal::disable_raw_mode().ok();
    }

    fn clear_footer_rows(&self, out: &mut std::io::Stdout) {
        // Reset scroll region temporarily so we can write anywhere
        write!(out, "\x1b[r").ok();
        let footer_top = self.term_height.saturating_sub(self.height as usize);
        for row in footer_top..self.term_height {
            write!(out, "\x1b[{};1H\x1b[2K", row + 1).ok();
        }
        out.flush().ok();
    }

    fn apply_scroll_region(&mut self) {
        let bottom = self.term_height.saturating_sub(self.height as usize).max(1);
        let mut out = std::io::stdout();
        // DECSTBM: set scroll region to exclude footer rows.
        write!(out, "\x1b[1;{}r", bottom).ok();
        // Keep cursor in scrollable region.
        write!(out, "\x1b[{};1H", bottom).ok();
        out.flush().ok();
    }

    fn reset_scroll_region(&self) {
        let mut out = std::io::stdout();
        write!(out, "\x1b[r").ok();
        out.flush().ok();
    }

    fn render(&mut self, title: &str, input: &str, cursor_pos: usize) {
        if !self.active {
            return;
        }
        let (width, height) = terminal_size();
        if width != self.term_width || height != self.term_height {
            let mut out = std::io::stdout();

            // 1. Reset scroll region so we can clear anywhere
            write!(out, "\x1b[r").ok();

            // 2. Save cursor, clear from old footer to end of screen using ED command
            let old_footer_top = self.term_height.saturating_sub(self.height as usize);
            let new_footer_top = height.saturating_sub(self.height as usize);
            let clear_from = old_footer_top.min(new_footer_top);

            // Move to the earliest possible footer position and clear to end of screen
            write!(out, "\x1b[{};1H", clear_from + 1).ok(); // Move to row
            write!(out, "\x1b[J").ok(); // Clear from cursor to end of screen (ED0)
            out.flush().ok();

            // 3. Update dimensions and apply new scroll region
            self.term_width = width;
            self.term_height = height;
            self.apply_scroll_region();
        }
        let footer_top = height.saturating_sub(self.height as usize);
        let rect = Rect::new(0, 0, width as u16, self.height);
        let mut buf = Buffer::empty(rect);

        // TUI-style cursor rendering
        let inner_width = width.saturating_sub(4).max(1); // Account for borders and prompt
        let prompt = "› ";
        let cursor_pos = cursor_pos.min(input.len());
        let (before_cursor, after_cursor) = input.split_at(cursor_pos);

        // Get character at cursor (or space if at end)
        let cursor_char = after_cursor.chars().next().unwrap_or(' ');
        let rest = if after_cursor.len() > cursor_char.len_utf8() {
            &after_cursor[cursor_char.len_utf8()..]
        } else {
            ""
        };

        // Build styled line with block cursor
        let line = Line::from(vec![
            Span::styled(prompt, Style::default().fg(Color::Cyan)),
            Span::styled(before_cursor, Style::default().fg(Color::White)),
            Span::styled(
                cursor_char.to_string(),
                Style::default().fg(Color::Black).bg(Color::White),
            ),
            Span::styled(rest, Style::default().fg(Color::White)),
        ]);

        Clear.render(rect, &mut buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(Color::Cyan))
            .title_style(Style::new().fg(Color::Cyan))
            .title(title);
        let paragraph = Paragraph::new(line).block(block);
        paragraph.render(rect, &mut buf);

        let mut out = std::io::stdout();
        flush_buffer(&mut out, &buf, rect, footer_top as u16);
        let scroll_y = footer_top.saturating_sub(1);
        queue!(out, cursor::MoveTo(0, scroll_y as u16)).ok();
        out.flush().ok();
    }
}

impl Drop for FooterUi {
    fn drop(&mut self) {
        self.disable();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptMode {
    Ask,
    Plan,
    Auto,
}

fn prompt_mode(state: &SessionState, chat: &eli_core::config::ChatConfig) -> PromptMode {
    let _ = (state, chat);
    PromptMode::Auto
}

fn print_history_block(lines: Vec<String>) {
    use std::io::Write;

    let out = format_indented_block(&lines);
    if !out.is_empty() {
        print!("{}", out);
        std::io::stdout().flush().ok();
    }
}

fn print_history_line(line: String) {
    print_history_block(vec![line]);
}

fn apply_prompt_mode(
    _mode: PromptMode,
    state: &mut SessionState,
    chat: &mut eli_core::config::ChatConfig,
) {
    state.auto_mode = AutoMode::Autonomous;
    chat.approvals = ApprovalMode::Auto;
    chat.approvals_commands = None;
    chat.approvals_diffs = None;
    chat.auto_mode = state.auto_mode;
}

fn cycle_prompt_mode(state: &mut SessionState, chat: &mut eli_core::config::ChatConfig) {
    apply_prompt_mode(PromptMode::Auto, state, chat);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentProfile {
    Coding,
    Research,
}

impl SessionState {
    fn new(cfg: &eli_core::config::ChatConfig) -> Self {
        Self {
            display_mode: cfg.display_mode,
            auto_mode: cfg.auto_mode,
            total_work_time: Duration::ZERO,
            step_count: 0,
            prompt_queue: Vec::new(),
            input_buffer: String::new(),
            cursor_pos: 0,
            prompt_history: Vec::new(),
            history_cursor: None,
            recent_research: Vec::new(),
            total_usage: eli_core::types::Usage::default(),
            last_usage: None,
        }
    }

    fn queue_prompt(&mut self, prompt: String) {
        self.prompt_queue.push(prompt);
    }

    fn next_prompt(&mut self) -> Option<String> {
        if self.prompt_queue.is_empty() {
            None
        } else {
            Some(self.prompt_queue.remove(0))
        }
    }

    fn queue_len(&self) -> usize {
        self.prompt_queue.len()
    }

    fn load_recent_research(&mut self, project_root: &Path, max_items: usize) {
        self.recent_research = discover_recent_research(project_root, max_items);
    }

    fn record_research_report(&mut self, artifact: ResearchArtifact, max_items: usize) {
        // Deduplicate by path and keep newest first.
        self.recent_research
            .retain(|a| a.rel_path != artifact.rel_path);
        self.recent_research.insert(0, artifact);
        if self.recent_research.len() > max_items {
            self.recent_research.truncate(max_items);
        }
    }

    fn recent_research_context(&self, max_items: usize, max_chars: usize) -> Option<String> {
        if self.recent_research.is_empty() || max_items == 0 || max_chars == 0 {
            return None;
        }

        let mut out = String::new();
        out.push_str("RECENT_RESEARCH (open with `cat` if needed):\n");
        for (idx, a) in self.recent_research.iter().take(max_items).enumerate() {
            let status = if a.status.trim().is_empty() {
                "unknown"
            } else {
                a.status.trim()
            };
            out.push_str(&format!(
                "{}. {} — {} ({}, {})\n",
                idx + 1,
                a.rel_path,
                truncate(&a.title, 120),
                status,
                a.created_utc
            ));
            if idx == 0 {
                if let Some(hint) = &a.answer_hint {
                    let hint = hint.trim();
                    if !hint.is_empty() {
                        out.push_str(&format!("   last_answer: {}\n", truncate(hint, 220)));
                    }
                }
            }
        }

        Some(truncate(&out, max_chars))
    }
}

#[derive(Clone, Copy)]
struct SlashCommand {
    name: &'static str,
    desc: &'static str,
}

const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        desc: "show help",
    },
    SlashCommand {
        name: "/?",
        desc: "alias for /help",
    },
    SlashCommand {
        name: "/$",
        desc: "show cost/usage stats",
    },
    SlashCommand {
        name: "/brain",
        desc: "full output (tools, history, details)",
    },
    SlashCommand {
        name: "/debug",
        desc: "debug output (raw request/response + tool output + observation)",
    },
    SlashCommand {
        name: "/standard",
        desc: "brief output (recent stream, summary)",
    },
    SlashCommand {
        name: "/brief",
        desc: "alias for /standard",
    },
    SlashCommand {
        name: "/mode",
        desc: "set exec mode (read/work)",
    },
    SlashCommand {
        name: "/read",
        desc: "set exec mode to read",
    },
    SlashCommand {
        name: "/work",
        desc: "set exec mode to work",
    },
    SlashCommand {
        name: "/bot",
        desc: "work mode; cmds auto, diffs ask",
    },
    SlashCommand {
        name: "/yolo",
        desc: "work mode; auto approvals",
    },
    SlashCommand {
        name: "/model",
        desc: "set or show model for this session",
    },
    SlashCommand {
        name: "/models",
        desc: "show current model and usage",
    },
    SlashCommand {
        name: "/key",
        desc: "set API key for current provider",
    },
    SlashCommand {
        name: "/queue",
        desc: "show queued prompts",
    },
    SlashCommand {
        name: "/q",
        desc: "alias for /queue",
    },
    SlashCommand {
        name: "/clear-queue",
        desc: "clear queued prompts",
    },
    SlashCommand {
        name: "/cq",
        desc: "alias for /clear-queue",
    },
    SlashCommand {
        name: "/status",
        desc: "show current mode/stats",
    },
    SlashCommand {
        name: "/s",
        desc: "alias for /status",
    },
    SlashCommand {
        name: "/compact",
        desc: "summarize older context (reduce tokens)",
    },
    SlashCommand {
        name: "/reset",
        desc: "clear conversation",
    },
    SlashCommand {
        name: "/new",
        desc: "alias for /reset",
    },
    SlashCommand {
        name: "/tip",
        desc: "toggle tips (standard mode)",
    },
    SlashCommand {
        name: "/undo",
        desc: "undo last edit",
    },
    SlashCommand {
        name: "/exit",
        desc: "quit",
    },
    SlashCommand {
        name: "/quit",
        desc: "alias for /exit",
    },
];

#[derive(Clone, Default)]
struct SlashHelper {
    last_input_tokens: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Helper for SlashHelper {}
impl Highlighter for SlashHelper {}
impl Validator for SlashHelper {}

impl Hinter for SlashHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &RustyContext<'_>) -> Option<Self::Hint> {
        if pos < line.len() {
            return None;
        }

        if is_slash_command_context(line, pos) {
            let prefix = &line[..pos];
            if let Some(cmd) = SLASH_COMMANDS.iter().find(|c| c.name.starts_with(prefix)) {
                return Some(cmd.name[prefix.len()..].to_string());
            }
        }

        // Show token usage hint if present
        let tokens = self
            .last_input_tokens
            .load(std::sync::atomic::Ordering::Relaxed);
        if tokens > 0 {
            // "Input: ~X tokens"
            return Some(format!(
                "  {}Input: ~{} tokens{}",
                style::DARK_GRAY,
                tokens,
                style::RESET
            ));
        }

        None
    }
}

impl Completer for SlashHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &RustyContext<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before = &line[..pos];
        if !is_slash_command_context(line, pos) {
            return Ok((pos, Vec::new()));
        }
        let mut out = Vec::new();
        for cmd in SLASH_COMMANDS {
            if cmd.name.starts_with(before) {
                out.push(Pair {
                    display: format!("{:<14} {}", cmd.name, cmd.desc),
                    replacement: cmd.name.to_string(),
                });
            }
        }
        Ok((0, out))
    }
}

#[derive(Clone)]
struct SlashMenu {
    state: Arc<Mutex<SlashMenuState>>,
}

#[derive(Default)]
struct SlashMenuState {
    shown: bool,
}

impl SlashMenu {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(SlashMenuState::default())),
        }
    }

    fn reset(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.shown = false;
        }
    }

    fn show(&self) {
        let mut show = false;
        if let Ok(mut state) = self.state.lock() {
            if !state.shown {
                state.shown = true;
                show = true;
            }
        }
        if show {
            let lines = slash_menu_lines();
            let out = format_box_string(&lines);
            if !out.is_empty() {
                println!("{out}");
            }
        }
    }
}

#[derive(Clone, Copy)]
enum SlashNav {
    Next,
    Prev,
}

#[derive(Clone)]
struct SlashMenuHandler {
    menu: SlashMenu,
}

impl SlashMenuHandler {
    fn new(menu: SlashMenu) -> Self {
        Self { menu }
    }
}

impl ConditionalEventHandler for SlashMenuHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: usize,
        _positive: bool,
        ctx: &rustyline::EventContext,
    ) -> Option<Cmd> {
        if ctx.pos() == 0 && ctx.line().trim().is_empty() {
            self.menu.show();
        }
        None
    }
}

#[derive(Clone)]
struct SlashNavHandler {
    menu: SlashMenu,
    dir: SlashNav,
}

impl SlashNavHandler {
    fn new(menu: SlashMenu, dir: SlashNav) -> Self {
        Self { menu, dir }
    }
}

impl ConditionalEventHandler for SlashNavHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: usize,
        _positive: bool,
        ctx: &rustyline::EventContext,
    ) -> Option<Cmd> {
        if !is_slash_command_context(ctx.line(), ctx.pos()) {
            return None;
        }
        self.menu.show();
        match self.dir {
            SlashNav::Next => Some(Cmd::Complete),
            SlashNav::Prev => Some(Cmd::CompleteBackward),
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "eli", version, about = "Eli: a terminal CLI coding agent")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Command>,

    /// Provider: openrouter | openai | anthropic | ollama | mock
    #[arg(long, global = true)]
    provider: Option<String>,

    /// Model name (provider-specific)
    #[arg(long, global = true)]
    model: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Interactive setup - configure provider, model, and API key
    Setup,

    /// Create a default config file (if missing)
    Init,

    /// Print or set config values
    Config {
        /// Set a config value: provider, model, mem_steps, key, sec_user_agent, compact, compact_trigger, compact_keep, summary_model, parallel_commands, parallel_subagents, scrollback_max_lines
        #[arg(long)]
        set: Option<String>,

        /// Value to set
        #[arg(long)]
        value: Option<String>,
    },

    /// Emit JSON schema for a CLI subcommand (hidden)
    #[command(hide = true)]
    ToolInfo {
        /// Subcommand path (e.g., finance timeseries)
        #[arg(value_name = "PATH", num_args = 0..)]
        path: Vec<String>,
    },

    /// Chat in a readline loop (default)
    Chat,

    /// Chat in debug mode (raw request/response + full tool output + observation)
    Debug,

    /// Chat in raw mode (no extra dumps)
    Raw,

    /// One-shot quantitative research loop
    Research {
        /// Research question/prompt (quote it)
        query: String,
    },

    /// Launch the interactive chat UI (alias of default chat)
    Tui,

    /// Financial data tools (for raw time-series exploration)
    Finance {
        #[command(subcommand)]
        cmd: FinanceCommand,
    },

    /// Web tools (crawl, search, read)
    Web {
        #[command(subcommand)]
        cmd: WebCommand,
    },

    /// Run background-style Eli workers from natural language tasks.
    Agent {
        #[command(subcommand)]
        cmd: AgentCommand,
    },

    /// Parse Rust source into a structural map (functions, structs, enums, impls, traits).
    Code(CodeArgs),
}

#[derive(Subcommand, Debug)]
enum FinanceCommand {
    /// Fetch OHLCV time-series for one or more tickers.
    Timeseries(FinanceTimeseriesArgs),
    /// Fetch a point-in-time snapshot (market cap, shares, price, etc.) for one or more tickers.
    Snapshot(FinanceSnapshotArgs),
    /// Fetch quarterly financial statements (Income Statement, Balance Sheet, Cash Flow).
    Fundamentals(FinanceFundamentalsArgs),
    /// Search for ticker symbols or macro series IDs.
    Search(FinanceSearchArgs),
    /// Fetch recent SEC filings (8-K, 10-K, 10-Q) for a ticker.
    Filings(FinanceFilingsArgs),
    /// Alias for filings.
    Sec(FinanceFilingsArgs),
    /// Fetch news context for a specific ticker and date.
    News(FinanceNewsArgs),
    /// Fetch key macro economic indicators (CPI, Unemployment, GDP, etc).
    Macro(FinanceMacroArgs),
    /// Fetch earnings and macro release schedules (no-auth public endpoints).
    Schedule(FinanceScheduleArgs),
    /// Aggregate implied Fed policy trajectory from local prediction-market cache.
    RatePath(FinanceRatePathArgs),
    /// Fetch US treasury yield curve with key spreads.
    YieldCurve(FinanceYieldCurveArgs),
    /// Run a preset multi-tool macro dashboard.
    Dashboard(FinanceDashboardArgs),
    /// Latest spot prices from Pyth Hermes (REST).
    Prices(FinancePricesArgs),
    /// Prediction market discovery + pricing (Kalshi default; falls back to Polymarket).
    Odds(FinanceOddsArgs),
    /// Listed options chains with IV/skew summaries (Yahoo Finance).
    Options(FinanceOptionsArgs),
    /// Sync prediction markets (Kalshi + Polymarket) with rate limiting to local CSV cache.
    Sync(FinanceSyncArgs),
}

#[derive(Subcommand, Debug)]
enum WebCommand {
    /// Crawl a website and extract content from all discovered pages.
    Crawl(WebCrawlArgs),
    /// Search the web using DuckDuckGo.
    Search(WebSearchArgs),
    /// Read and extract content from a single URL.
    Read(WebReadArgs),
    /// Extract key facts from content (URL, file, or text).
    Extract(WebExtractArgs),
}

#[derive(Subcommand, Debug)]
enum AgentCommand {
    /// Run a single Eli worker from a natural-language task.
    Run(AgentRunArgs),
    /// Run many Eli workers in parallel from a task template and vars file.
    Fanout(AgentFanoutArgs),
    /// Chunk a large input and orchestrate map/reduce/critic swarm synthesis.
    Swarm(AgentSwarmArgs),
    /// Critique a lead thesis/report using worker fanout.
    Critique(AgentModeArgs),
    /// Find additional evidence for/against a thesis via worker fanout.
    Evidence(AgentModeArgs),
    /// Run competitive workers to find the best answer.
    Compete(AgentModeArgs),
    /// Run worker debate and synthesize consensus.
    Debate(AgentModeArgs),
}

#[derive(Debug, Serialize)]
struct RustFileSummary {
    items_total: usize,
    functions: usize,
    function_names: Vec<String>,
    structs: usize,
    struct_names: Vec<String>,
    enums: usize,
    enum_names: Vec<String>,
    impls: usize,
    impl_targets: Vec<String>,
    traits: usize,
    trait_names: Vec<String>,
    modules: usize,
    module_names: Vec<String>,
    uses: usize,
    use_paths: Vec<String>,
    consts: usize,
    const_names: Vec<String>,
    statics: usize,
    type_aliases: usize,
    type_alias_names: Vec<String>,
    macros: usize,
    others: usize,
}

#[derive(Debug, Serialize)]
struct RustNodeSummary {
    kind: String,
    ident: Option<String>,
}

#[derive(Copy, Clone, Debug, ValueEnum, Eq, PartialEq)]
enum CrawlViewMode {
    Summary,
    Raw,
    Path,
}

#[derive(Copy, Clone, Debug, ValueEnum, Eq, PartialEq)]
enum CrawlSaveMode {
    Auto,
    Off,
}

#[derive(clap::Args, Debug)]
struct AgentRunArgs {
    /// Natural-language task for the worker.
    #[arg(long)]
    task: String,

    /// Output file path (JSON).
    #[arg(long)]
    out: Option<PathBuf>,

    /// Comma-separated fallback models (used on worker failure).
    #[arg(long = "fallback-models", value_delimiter = ',')]
    fallback_models: Vec<String>,

    /// Max runtime budget per worker (milliseconds).
    #[arg(long = "max-ms", default_value_t = 45000)]
    max_ms: u64,

    /// Max total attempts per worker across primary + fallbacks.
    #[arg(long = "max-attempts", default_value_t = 4)]
    max_attempts: usize,
}

#[derive(clap::Args, Debug)]
struct AgentFanoutArgs {
    /// Task template. Use placeholders like {{ticker}} or {{stance}}.
    #[arg(long = "task-template")]
    task_template: String,

    /// JSON file containing an array of objects for template vars.
    #[arg(long)]
    vars: PathBuf,

    /// Optional shared artifact manifest path all workers should read first.
    #[arg(long = "shared-manifest")]
    shared_manifest: Option<PathBuf>,

    /// Max workers to run at once.
    #[arg(long, default_value = "4")]
    max_parallel: usize,

    /// Output file path (JSON).
    #[arg(long)]
    out: Option<PathBuf>,

    /// Comma-separated fallback models (used on worker failure).
    #[arg(long = "fallback-models", value_delimiter = ',')]
    fallback_models: Vec<String>,

    /// Max runtime budget per worker (milliseconds).
    #[arg(long = "max-ms", default_value_t = 45000)]
    max_ms: u64,

    /// Max total attempts per worker across primary + fallbacks.
    #[arg(long = "max-attempts", default_value_t = 4)]
    max_attempts: usize,
}

#[derive(clap::Args, Debug)]
struct AgentSwarmArgs {
    /// High-level goal for the swarm.
    #[arg(long)]
    task: String,

    /// Input file to process (txt/md/json/csv/ndjson/pdf).
    #[arg(long)]
    input: PathBuf,

    /// Optional explicit number of chunk workers (X swarms).
    #[arg(long)]
    chunks: Option<usize>,

    /// Approximate characters per chunk when --chunks is not provided.
    #[arg(long = "chunk-chars", default_value_t = 20_000)]
    chunk_chars: usize,

    /// Character overlap between chunks to reduce boundary loss.
    #[arg(long = "overlap-chars", default_value_t = 500)]
    overlap_chars: usize,

    /// Hard cap on produced chunks.
    #[arg(long = "max-chunks", default_value_t = 64)]
    max_chunks: usize,

    /// Max workers to run at once for map stage.
    #[arg(long, default_value = "4")]
    max_parallel: usize,

    /// Output file path (JSON).
    #[arg(long)]
    out: Option<PathBuf>,

    /// Comma-separated fallback models (used on worker failure).
    #[arg(long = "fallback-models", value_delimiter = ',')]
    fallback_models: Vec<String>,

    /// Max runtime budget per worker (milliseconds).
    #[arg(long = "max-ms", default_value_t = 120_000)]
    max_ms: u64,

    /// Max total attempts per worker across primary + fallbacks.
    #[arg(long = "max-attempts", default_value_t = 3)]
    max_attempts: usize,
}

#[derive(clap::Args, Debug)]
struct AgentModeArgs {
    /// User objective for this report mode.
    #[arg(long)]
    prompt: String,

    /// Optional lead report or thesis file path.
    #[arg(long)]
    lead: Option<PathBuf>,

    /// JSON file containing an array of worker objects (name/model/role/etc).
    #[arg(long)]
    vars: PathBuf,

    /// Optional shared artifact manifest path all workers should read first.
    #[arg(long = "shared-manifest")]
    shared_manifest: Option<PathBuf>,

    /// Allow workers to reference peer output in compete/debate modes.
    #[arg(long, default_value_t = false)]
    allow_cheat: bool,

    /// Max workers to run at once.
    #[arg(long, default_value = "4")]
    max_parallel: usize,

    /// Output file path (JSON).
    #[arg(long)]
    out: Option<PathBuf>,

    /// Comma-separated fallback models (used on worker failure).
    #[arg(long = "fallback-models", value_delimiter = ',')]
    fallback_models: Vec<String>,

    /// Max runtime budget per worker (milliseconds).
    #[arg(long = "max-ms", default_value_t = 120_000)]
    max_ms: u64,

    /// Max total attempts per worker across primary + fallbacks.
    #[arg(long = "max-attempts", default_value_t = 3)]
    max_attempts: usize,
}

#[derive(clap::Args, Debug)]
struct CodeArgs {
    /// Path to Rust source file to analyze.
    path: PathBuf,

    /// Also generate code (e.g., getter methods for structs).
    #[arg(long, default_value_t = false)]
    generate: bool,

    /// Optional output file for JSON response.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct WebCrawlArgs {
    /// URL to start crawling from.
    #[arg(long)]
    url: String,

    /// Maximum number of pages to crawl (default: 50).
    #[arg(long, default_value = "50")]
    max_pages: usize,

    /// Respect robots.txt (default: true).
    #[arg(long, default_value = "true")]
    respect_robots: bool,

    /// Include subdomains in crawl (default: false).
    #[arg(long, default_value = "false")]
    subdomains: bool,

    /// Crawl via sitemap discovery mode.
    #[arg(long, default_value = "false")]
    sitemap: bool,

    /// Smart crawl mode: HTTP first, render JS only when needed.
    #[arg(long, default_value = "false", conflicts_with = "sitemap")]
    smart: bool,

    /// Terminal output view.
    #[arg(long, value_enum, default_value_t = CrawlViewMode::Summary)]
    view: CrawlViewMode,

    /// Save policy when --out is not provided.
    #[arg(long, value_enum, default_value_t = CrawlSaveMode::Auto)]
    save: CrawlSaveMode,

    /// Output file path (JSON).
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct WebSearchArgs {
    /// Search query.
    #[arg(long)]
    query: String,

    /// Output file path (JSON).
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct WebReadArgs {
    /// URL to read content from.
    #[arg(long)]
    url: String,

    /// Output file path (JSON).
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct WebExtractArgs {
    /// URL to fetch and extract from.
    #[arg(long)]
    url: Option<String>,

    /// File path to extract from.
    #[arg(long)]
    file: Option<PathBuf>,

    /// Inline text to extract from (use heredoc for large content).
    #[arg(long)]
    text: Option<String>,

    /// Number of bullet points to extract (default: 10).
    #[arg(long, default_value = "10")]
    bullets: usize,

    /// Focus extraction on specific topic.
    #[arg(long)]
    focus: Option<String>,

    /// Output file path (JSON).
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
pub struct FinanceMacroArgs {
    /// Time range for calculating changes (e.g. 1y).
    #[arg(long, default_value = "1y")]
    pub range: String,
    /// Optional historical comparison date (YYYY-MM-DD).
    #[arg(long = "compare-to")]
    pub compare_to: Option<String>,
    /// Output format (json only).
    #[arg(long, default_value = "json")]
    pub format: String,
    /// Output file path.
    #[arg(short, long)]
    pub out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
pub struct FinanceScheduleArgs {
    /// Schedule kind: earnings | macro | all.
    #[arg(long, default_value = "all")]
    pub kind: String,
    /// Single date (YYYY-MM-DD). If set, overrides --from/--to.
    #[arg(long)]
    pub date: Option<String>,
    /// Start date (YYYY-MM-DD).
    #[arg(long = "from")]
    pub from: Option<String>,
    /// End date (YYYY-MM-DD).
    #[arg(long = "to")]
    pub to: Option<String>,
    /// Optional ticker filter for earnings rows (repeatable or comma-separated).
    #[arg(long, visible_alias = "tickers", value_delimiter = ',')]
    pub ticker: Vec<String>,
    /// Macro-only: keep only major US releases (CPI, PCE, GDP, jobs, FOMC, claims).
    #[arg(long, default_value_t = false)]
    pub major: bool,
    /// Output format (json only).
    #[arg(long, default_value = "json")]
    pub format: String,
    /// Write full JSON output to a file instead of stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
pub struct FinanceRatePathArgs {
    /// Optional cache directory for prediction-market CSVs.
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
    /// Source mode: auto | meeting | fallback.
    #[arg(long, default_value = "auto")]
    pub source_mode: String,
    /// Output format (json only).
    #[arg(long, default_value = "json")]
    pub format: String,
    /// Output file path.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
pub struct FinanceYieldCurveArgs {
    /// Optional comparison windows (comma-separated): 3mo,1y.
    #[arg(long, value_delimiter = ',')]
    pub compare: Vec<String>,
    /// Require all curve tenors (1mo..30y); fail if any are missing.
    #[arg(long, default_value_t = false)]
    pub strict: bool,
    /// Output format (json only).
    #[arg(long, default_value = "json")]
    pub format: String,
    /// Output file path.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
pub struct FinanceDashboardArgs {
    /// Dashboard preset (v1 supports: recession).
    #[arg(long)]
    pub preset: String,
    /// Optional per-section timeout budget in milliseconds.
    #[arg(long = "max-ms")]
    pub max_ms: Option<u64>,
    /// Output format (json only).
    #[arg(long, default_value = "json")]
    pub format: String,
    /// Output file path.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct FinanceNewsArgs {
    /// Ticker to search for.
    #[arg(long, visible_alias = "tickers")]
    ticker: String,

    /// Date of interest (YYYY-MM-DD).
    #[arg(long)]
    date: String,

    /// Write full JSON output to a file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct FinanceSnapshotArgs {
    /// Tickers to fetch (repeatable or comma-separated).
    #[arg(long, visible_alias = "ticker", value_delimiter = ',')]
    tickers: Vec<String>,

    /// Optional file with tickers (one per line).
    #[arg(long)]
    tickers_file: Option<PathBuf>,

    /// Data provider (mock | yahoo).
    #[arg(long, default_value = "yahoo")]
    provider: String,

    /// Optional trailing return windows (comma-separated): 1mo,3mo,6mo,1y.
    #[arg(long, value_delimiter = ',')]
    returns: Vec<String>,

    /// Output format (currently: json).
    #[arg(long, default_value = "json")]
    format: String,

    /// Write full JSON output to a file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct FinanceFundamentalsArgs {
    /// Ticker to fetch fundamentals for.
    #[arg(long, visible_alias = "tickers")]
    ticker: String,

    /// Output format (currently: json).
    #[arg(long, default_value = "json")]
    format: String,

    /// Write full JSON output to a file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct FinanceSearchArgs {
    /// Search query (e.g. "Apple" or "Inflation").
    #[arg(long)]
    query: String,

    /// Output format (currently: json).
    #[arg(long, default_value = "json")]
    format: String,

    /// Write full JSON output to a file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct FinancePricesArgs {
    /// Discover price feeds by query (e.g. "pepe").
    #[arg(long)]
    query: Option<String>,

    /// Asset type filter (e.g. crypto, equity, fx, metal, rates).
    #[arg(long)]
    asset_type: Option<String>,

    /// Explicit Pyth price feed IDs (repeatable or comma-separated).
    #[arg(long, value_delimiter = ',')]
    ids: Vec<String>,

    /// Auto-select the top ranked candidate when query matching is ambiguous.
    #[arg(long, default_value_t = false)]
    auto_select: bool,

    /// Output format (currently: json).
    #[arg(long, default_value = "json")]
    format: String,

    /// Write full JSON output to a file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct FinanceOddsArgs {
    #[command(subcommand)]
    action: Option<FinanceOddsAction>,

    /// Data source: kalshi (default), polymarket, or auto (kalshi then polymarket).
    #[arg(long)]
    provider: Option<String>,
    /// Kalshi series ticker.
    #[arg(long)]
    series: Option<String>,

    /// Event ticker.
    #[arg(long)]
    event: Option<String>,

    /// Market ticker.
    #[arg(long)]
    market: Option<String>,

    /// Filter by status (e.g. open).
    #[arg(long)]
    status: Option<String>,

    /// Page size limit.
    #[arg(long)]
    limit: Option<usize>,

    /// Pagination cursor.
    #[arg(long)]
    cursor: Option<String>,

    /// Max pages to fetch (Kalshi list endpoints).
    #[arg(long)]
    max_pages: Option<usize>,

    /// List series (Kalshi only).
    #[arg(long)]
    list_series: bool,

    /// List events.
    #[arg(long)]
    list_events: bool,

    /// List markets.
    #[arg(long)]
    list_markets: bool,

    /// List tags (Polymarket only).
    #[arg(long)]
    list_tags: bool,

    /// Category filter (Kalshi list endpoints).
    #[arg(long)]
    category: Option<String>,

    /// Case-insensitive literal substring match (titles/tickers/slugs).
    #[arg(long, alias = "query")]
    search: Option<String>,

    /// Optional country filter for local CSV search (v1: US only).
    #[arg(long)]
    country: Option<String>,

    /// Minimum market volume in USD (local CSV search).
    #[arg(long = "min-volume")]
    min_volume: Option<f64>,

    /// Return top N markets by volume (local CSV search).
    #[arg(long)]
    top: Option<usize>,
    /// Include compact ranking explanations in local CSV search output.
    #[arg(long, default_value_t = false)]
    explain: bool,

    /// Upgrade CSV search results to live API prices (fresh bid/ask/volume).
    #[arg(long, default_value_t = false)]
    live: bool,

    /// Include orderbook depth (heavier call; Polymarket orderbook supported).
    #[arg(long)]
    orderbook: bool,

    /// Orderbook depth (levels).
    #[arg(long)]
    depth: Option<usize>,

    /// Output format (currently: json).
    #[arg(long, default_value = "json")]
    format: String,

    /// Write full JSON output to a file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum FinanceOddsAction {
    /// Sync prediction markets (Kalshi + Polymarket) to a local CSV cache.
    Sync(FinanceSyncArgs),

    /// Print local cache paths for odds CSVs.
    Where(FinanceOddsWhereArgs),
}

#[derive(clap::Args, Debug)]
struct FinanceOddsWhereArgs {
    /// Override cache directory (defaults to the same cache used by `eli finance sync`).
    #[arg(long)]
    cache_dir: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct FinanceOptionsArgs {
    /// Underlying ticker (e.g. INTC).
    #[arg(long, visible_alias = "tickers")]
    ticker: String,

    /// Expiration date (YYYY-MM-DD). If omitted, uses the first available expiry.
    #[arg(long)]
    expiry: Option<String>,

    /// Filter: calls | puts | both (default: both).
    #[arg(long = "type", value_name = "calls|puts|both")]
    option_type: Option<String>,

    /// Only return strikes within this percentage of the underlying (e.g. 10 = +/-10%).
    #[arg(long = "near-money")]
    near_money: Option<f64>,

    /// Return summary metrics only (no full chain).
    #[arg(long)]
    summary: bool,

    /// List available expirations only.
    #[arg(long)]
    expirations: bool,

    /// Output format (currently: json).
    #[arg(long, default_value = "json")]
    format: String,

    /// Write full JSON output to a file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct FinanceSyncArgs {
    /// Sources to sync: kalshi, polymarket (comma-separated). Default: both.
    #[arg(long, value_delimiter = ',')]
    sources: Vec<String>,

    /// Max pages to fetch per source (default: 10).
    #[arg(long, default_value = "10")]
    max_pages: usize,

    /// Fail if pagination/coverage checks indicate incomplete source exhaustion.
    #[arg(long)]
    strict: bool,

    /// Cache directory for CSV files.
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Output format (currently: json).
    #[arg(long, default_value = "json")]
    format: String,

    /// Write full JSON output to a file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct FinanceFilingsArgs {
    /// Ticker to fetch filings for.
    #[arg(long, visible_alias = "tickers")]
    ticker: String,

    /// Form types to include (comma-separated), e.g. 8-K,10-K,10-Q. Defaults to 8-K,10-K,10-Q.
    #[arg(long, value_delimiter = ',')]
    forms: Vec<String>,

    /// Max number of filings to return.
    #[arg(long, default_value_t = 5)]
    limit: usize,

    /// Download primary documents, save to cache, and include a text excerpt inline.
    #[arg(long)]
    include_text: bool,

    /// Max chars for the inline excerpt (full text is still written to disk when --include-text is set).
    #[arg(long)]
    max_chars: Option<usize>,

    /// Override cache directory (defaults to Eli's cache dir).
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Output format (currently: json).
    #[arg(long, default_value = "json")]
    format: String,

    /// Write full JSON output to a file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct FinanceTimeseriesArgs {
    /// Tickers to fetch (repeatable or comma-separated).
    #[arg(long, visible_alias = "ticker", value_delimiter = ',')]
    tickers: Vec<String>,

    /// Optional file with tickers (one per line).
    #[arg(long)]
    tickers_file: Option<PathBuf>,

    /// Lookback range (e.g. 1d, 12mo, 5y).
    #[arg(long, default_value = "1y")]
    range: String,

    /// Candle size / sampling granularity (e.g. 10m, 1h, 1d, 1w, 1mo).
    #[arg(long, default_value = "1d")]
    granularity: String,

    /// Explicit window start (RFC3339 or YYYY-MM-DD). Must be used with --end.
    #[arg(long)]
    start: Option<String>,

    /// Explicit window end (RFC3339 or YYYY-MM-DD). Must be used with --start.
    #[arg(long)]
    end: Option<String>,

    /// End timestamp for the window (RFC3339). If you pass YYYY-MM-DD, it's treated as end-of-day UTC. Defaults to now (UTC).
    #[arg(long)]
    as_of: Option<String>,

    /// Data provider (auto | mock | yahoo | fred). "auto" tries Yahoo first, then FRED for failures.
    #[arg(long, default_value = "auto")]
    provider: String,

    /// Safety cap for points per ticker.
    #[arg(long)]
    max_points_per_ticker: Option<usize>,

    /// Override cache directory (defaults to Eli's cache dir).
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Output format (currently: json).
    #[arg(long, default_value = "json")]
    format: String,

    /// Write full JSON output to a file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "eli=warn,eli_cli=warn".to_string()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::try_parse()?;

    match cli.cmd {
        None => cmd_chat(cli.provider, cli.model, None).await,
        Some(Command::Setup) => cmd_setup().await,
        Some(Command::Init) => cmd_init().await,
        Some(Command::Config { set, value }) => cmd_config(set, value).await,
        Some(Command::ToolInfo { path }) => cmd_tool_info(path),
        Some(Command::Chat) => cmd_chat(cli.provider, cli.model, None).await,
        Some(Command::Debug) => cmd_chat(cli.provider, cli.model, Some(DisplayMode::Debug)).await,
        Some(Command::Raw) => cmd_chat(cli.provider, cli.model, Some(DisplayMode::Raw)).await,
        Some(Command::Research { query }) => cmd_research(query, cli.provider, cli.model).await,
        Some(Command::Tui) => cmd_tui(cli.provider, cli.model).await,
        Some(Command::Finance { cmd }) => cmd_finance(cmd).await,
        Some(Command::Web { cmd }) => cmd_web(cmd).await,
        Some(Command::Agent { cmd }) => cmd_agent(cmd, cli.provider, cli.model).await,
        Some(Command::Code(args)) => cmd_code(args).await,
    }
}

async fn cmd_research(
    query: String,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let paths = Paths::discover().context("discover paths")?;
    let mut cfg = config::load_or_create(&paths).context("load/create config")?;
    apply_overrides(&mut cfg, provider, model)?;

    // Research defaults: safe, autonomous, non-destructive.
    cfg.chat.mode = RunMode::Read;
    cfg.chat.approvals = ApprovalMode::Auto;
    cfg.chat.auto = true;
    // Force plain/non-footer output when external clients request it.
    if env_truthy("ELI_PLAIN_OUTPUT") || env_truthy("ELI_NO_FOOTER") {
        cfg.chat.display_mode = DisplayMode::Brain;
    }

    let adapter = eli_adapters::build_from_chat_config(&cfg.chat).context("build adapter")?;
    let adapter: Arc<dyn LlmAdapter> = Arc::from(adapter);

    let cwd = std::env::current_dir().context("get cwd")?;
    let project_root = cfg
        .chat
        .resolved_project_root(&cwd)
        .map_err(|e| anyhow::anyhow!(e))
        .context("resolve project root")?;

    ensure_eli_research_brain(&project_root).context("ensure eli_research/ELI.md")?;

    let diff_engine = DiffEngine::new(project_root.clone()).context("init diff engine")?;
    let command_runner = CommandRunner::new(
        cfg.chat.timeout_secs,
        cfg.chat.max_cmds,
        cfg.chat.parallel_commands,
        project_root.clone(),
    );

    let store = SessionStore::new(&paths);
    let session_id = uuid::Uuid::new_v4().to_string();

    // Ensure instincts directory exists
    let instincts_dir = project_root.join("instincts");
    if !instincts_dir.exists() {
        let _ = std::fs::create_dir_all(&instincts_dir);
    }

    info!(session_id = %session_id, provider = %cfg.chat.provider, model = %cfg.chat.model, "starting research");

    let mut memory = eli_core::memory::Memory::new(cfg.chat.mem_steps);
    memory.set_system(eli_core::contract::system_prompt());

    // Inject existing instincts into memory
    if instincts_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&instincts_dir) {
            for entry in entries.flatten() {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    let filename = entry.file_name().to_string_lossy().to_string();
                    memory.push(ChatMessage::system(format!(
                        "INSTINCT ({filename}):\n{content}"
                    )));
                }
            }
        }
    }
    let mut undo_stack: Vec<Vec<DiffResult>> = Vec::new();
    let mut state = SessionState::new(&cfg.chat);
    state.load_recent_research(&project_root, 12);
    if let Ok(agent_context) = std::env::var("ELI_AGENT_CONTEXT") {
        let ctx = agent_context.trim();
        if !ctx.is_empty() {
            memory.push(ChatMessage::system(format!(
                "AGENT EXECUTION CONTEXT:\n{ctx}"
            )));
        }
    }

    if is_trivial_query(&query) {
        let answer = "Hello. What should I focus on?";
        let assistant = serde_json::json!({
            "plan": format!(
                "MODE: READ | APPROVALS: AUTO | ROOT: {} | Trivial query detected; no tool calls needed.",
                project_root.display()
            ),
            "checklist": [],
            "focus": "Clarify user intent",
            "status": "DONE",
            "commands": [],
            "commands_parallel": false,
            "screen": [],
            "diffs": [],
            "subagents": [],
            "synthesis": {
                "summary": [],
                "answer": answer,
                "next_steps": []
            },
            "ask_user": "",
            "notes": answer
        })
        .to_string();

        store
            .append(
                &session_id,
                &SessionEvent {
                    ts: chrono::Utc::now(),
                    kind: EventKind::UserMessage {
                        content: query.clone(),
                    },
                },
            )
            .await?;
        store
            .append(
                &session_id,
                &SessionEvent {
                    ts: chrono::Utc::now(),
                    kind: EventKind::AssistantMessage { content: assistant },
                },
            )
            .await?;

        println!("{answer}");
        return Ok(());
    }

    if has_interactive_terminal() {
        print_banner(&cfg.chat, &project_root, &state);
    }

    run_agent_steps(
        &cfg.chat,
        adapter.clone(),
        &diff_engine,
        &command_runner,
        &store,
        &paths.data_dir,
        &session_id,
        &project_root,
        &mut memory,
        &mut undo_stack,
        &mut state,
        AgentProfile::Research,
        query,
        Vec::new(),
    )
    .await?;

    if has_interactive_terminal() && !env_truthy("ELI_PLAIN_OUTPUT") && !env_truthy("ELI_NO_FOOTER")
    {
        print_cost_stats(&state, &cfg.chat);
    }

    Ok(())
}

fn is_trivial_query(query: &str) -> bool {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return true;
    }
    matches!(
        q.as_str(),
        "hi" | "hello" | "hey" | "yo" | "sup" | "hola" | "good morning" | "good afternoon"
    )
}

fn is_quick_market_query(query: &str) -> bool {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return false;
    }
    q.contains("market today")
        || q.contains("what happened")
        || q.contains("what did you think")
        || q.contains("price of")
        || q.contains("stock price")
}

async fn cmd_finance(cmd: FinanceCommand) -> Result<()> {
    match cmd {
        FinanceCommand::Timeseries(args) => cmd_finance_timeseries(args).await,
        FinanceCommand::Snapshot(args) => cmd_finance_snapshot(args).await,
        FinanceCommand::Fundamentals(args) => cmd_finance_fundamentals(args).await,
        FinanceCommand::Search(args) => cmd_finance_search(args).await,
        FinanceCommand::Filings(args) | FinanceCommand::Sec(args) => {
            cmd_finance_filings(args).await
        }
        FinanceCommand::News(args) => cmd_finance_news(args).await,
        FinanceCommand::Macro(args) => cmd_finance_macro(args).await,
        FinanceCommand::Schedule(args) => cmd_finance_schedule(args).await,
        FinanceCommand::RatePath(args) => cmd_finance_rate_path(args).await,
        FinanceCommand::YieldCurve(args) => cmd_finance_yield_curve(args).await,
        FinanceCommand::Dashboard(args) => cmd_finance_dashboard(args).await,
        FinanceCommand::Prices(args) => cmd_finance_prices(args).await,
        FinanceCommand::Odds(args) => cmd_finance_odds(args).await,
        FinanceCommand::Options(args) => cmd_finance_options(args).await,
        FinanceCommand::Sync(args) => cmd_finance_sync(args).await,
    }
}

async fn cmd_web(cmd: WebCommand) -> Result<()> {
    match cmd {
        WebCommand::Crawl(args) => cmd_web_crawl(args).await,
        WebCommand::Search(args) => cmd_web_search(args).await,
        WebCommand::Read(args) => cmd_web_read(args).await,
        WebCommand::Extract(args) => cmd_web_extract(args).await,
    }
}


fn generate_struct_getters_quote(item: &syn::Item) -> Result<String> {
    let syn::Item::Struct(s) = item else {
        anyhow::bail!("template struct_getters expects a struct item");
    };

    let struct_ident = &s.ident;
    let fields_named = match &s.fields {
        syn::Fields::Named(named) => named,
        _ => anyhow::bail!("template struct_getters requires named fields"),
    };

    let mut methods = Vec::new();
    for f in &fields_named.named {
        let Some(field_ident) = &f.ident else {
            continue;
        };
        let field_ty = &f.ty;
        methods.push(quote! {
            pub fn #field_ident(&self) -> &#field_ty {
                &self.#field_ident
            }
        });
    }

    let tokens = quote! {
        impl #struct_ident {
            #(#methods)*
        }
    };
    Ok(tokens.to_string())
}

fn summarize_rust_file(file: &syn::File) -> RustFileSummary {
    let mut summary = RustFileSummary {
        items_total: file.items.len(),
        functions: 0,
        function_names: Vec::new(),
        structs: 0,
        struct_names: Vec::new(),
        enums: 0,
        enum_names: Vec::new(),
        impls: 0,
        impl_targets: Vec::new(),
        traits: 0,
        trait_names: Vec::new(),
        modules: 0,
        module_names: Vec::new(),
        uses: 0,
        use_paths: Vec::new(),
        consts: 0,
        const_names: Vec::new(),
        statics: 0,
        type_aliases: 0,
        type_alias_names: Vec::new(),
        macros: 0,
        others: 0,
    };

    for item in &file.items {
        match item {
            syn::Item::Fn(v) => {
                summary.functions += 1;
                summary.function_names.push(v.sig.ident.to_string());
            }
            syn::Item::Struct(v) => {
                summary.structs += 1;
                summary.struct_names.push(v.ident.to_string());
            }
            syn::Item::Enum(v) => {
                summary.enums += 1;
                summary.enum_names.push(v.ident.to_string());
            }
            syn::Item::Impl(v) => {
                summary.impls += 1;
                summary.impl_targets.push(format_impl_target(v));
            }
            syn::Item::Trait(v) => {
                summary.traits += 1;
                summary.trait_names.push(v.ident.to_string());
            }
            syn::Item::Mod(v) => {
                summary.modules += 1;
                summary.module_names.push(v.ident.to_string());
            }
            syn::Item::Use(v) => {
                summary.uses += 1;
                summary.use_paths.push(use_tree_to_string(&v.tree));
            }
            syn::Item::Const(v) => {
                summary.consts += 1;
                summary.const_names.push(v.ident.to_string());
            }
            syn::Item::Static(_) => summary.statics += 1,
            syn::Item::Type(v) => {
                summary.type_aliases += 1;
                summary.type_alias_names.push(v.ident.to_string());
            }
            syn::Item::Macro(_) => summary.macros += 1,
            _ => summary.others += 1,
        }
    }

    summary.function_names.sort();
    summary.struct_names.sort();
    summary.enum_names.sort();
    summary.impl_targets.sort();
    summary.trait_names.sort();
    summary.module_names.sort();
    summary.use_paths.sort();
    summary.const_names.sort();
    summary.type_alias_names.sort();

    summary
}

fn format_impl_target(item: &syn::ItemImpl) -> String {
    let self_ty = type_to_string(&item.self_ty);
    if let Some((_, trait_path, _)) = &item.trait_ {
        return format!("{} for {}", path_to_string(trait_path), self_ty);
    }
    format!("impl {}", self_ty)
}

fn type_to_string(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(t) => path_to_string(&t.path),
        syn::Type::Reference(t) => format!("&{}", type_to_string(&t.elem)),
        syn::Type::Slice(t) => format!("[{}]", type_to_string(&t.elem)),
        syn::Type::Array(t) => format!("[{}; _]", type_to_string(&t.elem)),
        syn::Type::Tuple(t) => {
            let parts = t.elems.iter().map(type_to_string).join(", ");
            format!("({parts})")
        }
        _ => "other".to_string(),
    }
}

fn path_to_string(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|seg| seg.ident.to_string())
        .join("::")
}

fn use_tree_to_string(tree: &syn::UseTree) -> String {
    match tree {
        syn::UseTree::Path(v) => format!("{}::{}", v.ident, use_tree_to_string(&v.tree)),
        syn::UseTree::Name(v) => v.ident.to_string(),
        syn::UseTree::Rename(v) => format!("{} as {}", v.ident, v.rename),
        syn::UseTree::Glob(_) => "*".to_string(),
        syn::UseTree::Group(v) => {
            let inner = v.items.iter().map(use_tree_to_string).join(", ");
            format!("{{{inner}}}")
        }
    }
}

fn summarize_rust_item(item: &syn::Item) -> RustNodeSummary {
    match item {
        syn::Item::Const(v) => RustNodeSummary {
            kind: "Const".to_string(),
            ident: Some(v.ident.to_string()),
        },
        syn::Item::Enum(v) => RustNodeSummary {
            kind: "Enum".to_string(),
            ident: Some(v.ident.to_string()),
        },
        syn::Item::Fn(v) => RustNodeSummary {
            kind: "Fn".to_string(),
            ident: Some(v.sig.ident.to_string()),
        },
        syn::Item::Impl(_) => RustNodeSummary {
            kind: "Impl".to_string(),
            ident: None,
        },
        syn::Item::Macro(_) => RustNodeSummary {
            kind: "Macro".to_string(),
            ident: None,
        },
        syn::Item::Mod(v) => RustNodeSummary {
            kind: "Mod".to_string(),
            ident: Some(v.ident.to_string()),
        },
        syn::Item::Static(v) => RustNodeSummary {
            kind: "Static".to_string(),
            ident: Some(v.ident.to_string()),
        },
        syn::Item::Struct(v) => RustNodeSummary {
            kind: "Struct".to_string(),
            ident: Some(v.ident.to_string()),
        },
        syn::Item::Trait(v) => RustNodeSummary {
            kind: "Trait".to_string(),
            ident: Some(v.ident.to_string()),
        },
        syn::Item::Type(v) => RustNodeSummary {
            kind: "Type".to_string(),
            ident: Some(v.ident.to_string()),
        },
        syn::Item::Union(v) => RustNodeSummary {
            kind: "Union".to_string(),
            ident: Some(v.ident.to_string()),
        },
        syn::Item::Use(_) => RustNodeSummary {
            kind: "Use".to_string(),
            ident: None,
        },
        _ => RustNodeSummary {
            kind: "Other".to_string(),
            ident: None,
        },
    }
}

async fn cmd_code(args: CodeArgs) -> Result<()> {
    let source_path = resolve_abs_path(&args.path);
    if !source_path.exists() {
        anyhow::bail!("path does not exist: {}", source_path.display());
    }
    if !source_path.is_file() {
        anyhow::bail!("path is not a file: {}", source_path.display());
    }

    let source = std::fs::read_to_string(&source_path)
        .with_context(|| format!("read {}", source_path.display()))?;
    let parsed = syn::parse_file(&source)
        .with_context(|| format!("parse rust file {}", source_path.display()))?;

    let summary = summarize_rust_file(&parsed);

    let generated = if args.generate {
        // Try to generate getters for each struct in the file
        let mut gen_parts = Vec::new();
        for item in &parsed.items {
            if let Ok(code) = generate_struct_getters_quote(item) {
                gen_parts.push(code);
            }
        }
        if gen_parts.is_empty() {
            None
        } else {
            Some(gen_parts.join("\n\n"))
        }
    } else {
        None
    };

    let resp = json!({
        "source_path": source_path.display().to_string(),
        "bytes": source.as_bytes().len(),
        "summary": summary,
        "generated": generated,
    });

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let json = serde_json::to_string_pretty(&resp).context("serialize code response")?;
        std::fs::write(&out_path, &json).context("write code --out")?;
        println!(
            "{{\"ok\":true,\"path\":{}}}",
            serde_json::to_string(&out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&resp).context("serialize code response")?
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentAttemptResult {
    model: String,
    status: String,
    duration_ms: u128,
    exit_code: Option<i32>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentWorkerResult {
    name: String,
    task: String,
    status: String,
    exit_code: Option<i32>,
    requested_model: Option<String>,
    used_model: Option<String>,
    attempted_models: Vec<String>,
    attempt_count: usize,
    attempts: Vec<AgentAttemptResult>,
    report_path: Option<String>,
    started_at: String,
    finished_at: String,
    duration_ms: u128,
    stdout_tail: String,
    stderr_tail: String,
}

#[derive(Debug, Clone, Serialize)]
struct AgentRunResponse {
    ok: bool,
    usable: bool,
    kind: String,
    saved_result_path: String,
    saved_manifest_path: String,
    artifact_paths: Vec<String>,
    worker: AgentWorkerResult,
}

#[derive(Debug, Clone, Serialize)]
struct AgentFanoutSummary {
    requested: usize,
    completed: usize,
    failed: usize,
    max_parallel: usize,
}

#[derive(Debug, Clone, Serialize)]
struct AgentFanoutResponse {
    ok: bool,
    usable: bool,
    kind: String,
    saved_result_path: String,
    saved_manifest_path: String,
    artifact_paths: Vec<String>,
    task_template: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    shared_manifest_path: Option<String>,
    summary: AgentFanoutSummary,
    workers: Vec<AgentWorkerResult>,
}

#[derive(Debug, Clone, Serialize)]
struct AgentSwarmSummary {
    requested_chunks: usize,
    generated_chunks: usize,
    map_completed: usize,
    map_failed: usize,
    max_parallel: usize,
}

#[derive(Debug, Clone, Serialize)]
struct AgentSwarmResponse {
    ok: bool,
    usable: bool,
    kind: String,
    saved_result_path: String,
    saved_manifest_path: String,
    artifact_paths: Vec<String>,
    task: String,
    input_path: String,
    chunk_manifest_path: String,
    map_manifest_path: String,
    summary: AgentSwarmSummary,
    map_workers: Vec<AgentWorkerResult>,
    reduce_worker: AgentWorkerResult,
    critic_worker: AgentWorkerResult,
    final_worker: AgentWorkerResult,
}

#[derive(Debug, Clone, Serialize)]
struct SwarmChunkInfo {
    index: usize,
    path: String,
    chars: usize,
}

#[derive(Debug, Clone)]
struct AgentWorkerSpec {
    name: String,
    task: String,
    provider: Option<String>,
    model: Option<String>,
    fallback_models: Vec<String>,
    max_ms: Option<u64>,
    max_attempts: Option<usize>,
}

struct DirectAgentOutcome {
    worker: AgentWorkerResult,
    artifact_paths: Vec<String>,
}

async fn cmd_agent(
    cmd: AgentCommand,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    match cmd {
        AgentCommand::Run(args) => cmd_agent_run(args, provider, model).await,
        AgentCommand::Fanout(args) => cmd_agent_fanout(args, provider, model).await,
        AgentCommand::Swarm(args) => cmd_agent_swarm(args, provider, model).await,
        AgentCommand::Critique(args) => cmd_agent_mode("critique", args, provider, model).await,
        AgentCommand::Evidence(args) => cmd_agent_mode("evidence", args, provider, model).await,
        AgentCommand::Compete(args) => cmd_agent_mode("compete", args, provider, model).await,
        AgentCommand::Debate(args) => cmd_agent_mode("debate", args, provider, model).await,
    }
}

async fn cmd_agent_mode(
    mode: &str,
    args: AgentModeArgs,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let lead_note = args
        .lead
        .as_ref()
        .map(|p| format!("Lead artifact: {}\n", resolve_abs_path(p).display()))
        .unwrap_or_default();
    let cheat_note = if args.allow_cheat {
        "Peer-aware mode: you may read peer reports from this run directory if present.\n"
    } else {
        "Independent mode: do not use peer reports as evidence.\n"
    };
    let style = match mode {
        "critique" => "Style objective: critique the lead answer and find weak points.",
        "evidence" => "Style objective: gather new supporting and rejecting evidence.",
        "compete" => "Style objective: compete for strongest answer quality.",
        "debate" => "Style objective: argue stance, rebut opposition, surface conditions.",
        _ => "Style objective: fulfill user prompt directly.",
    };
    let task_template = format!(
        "{style}\n{lead}{cheat}User objective:\n{objective}",
        style = style,
        lead = lead_note,
        cheat = cheat_note,
        objective = args.prompt
    );
    let fanout_args = AgentFanoutArgs {
        task_template,
        vars: args.vars,
        shared_manifest: args.shared_manifest,
        max_parallel: args.max_parallel,
        out: args.out,
        fallback_models: args.fallback_models,
        max_ms: args.max_ms,
        max_attempts: args.max_attempts,
    };
    cmd_agent_fanout(fanout_args, provider, model).await
}

async fn cmd_agent_run(
    args: AgentRunArgs,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let run_dir = resolve_agent_run_dir("run");
    let saved_result_path = run_dir.join("result.json");
    let saved_manifest_path = run_dir.join("manifest.json");
    let fallback_models = if args.fallback_models.is_empty() {
        default_agent_fallback_models()
    } else {
        args.fallback_models.clone()
    };
    let requested_provider = provider.clone();
    let requested_model = model.clone();
    let direct = try_agent_direct_route(
        "worker_01",
        &args.task,
        requested_provider.as_deref().unwrap_or("openrouter"),
        requested_model.as_deref(),
        &run_dir,
    )
    .await?;
    let (worker, artifact_paths) = if let Some(outcome) = direct {
        (outcome.worker, outcome.artifact_paths)
    } else {
        let worker = run_agent_worker(
            "worker_01".to_string(),
            args.task,
            provider,
            model,
            fallback_models,
            args.max_ms,
            args.max_attempts,
            run_dir.join("artifacts").join("worker_01"),
        )
        .await;
        let artifact_paths: Vec<String> = worker
            .report_path
            .as_ref()
            .map(|p| vec![p.clone()])
            .unwrap_or_default();
        (worker, artifact_paths)
    };

    let ok = worker.status == "done";
    let resp = AgentRunResponse {
        ok,
        usable: ok,
        kind: "agent_run".to_string(),
        saved_result_path: saved_result_path.display().to_string(),
        saved_manifest_path: saved_manifest_path.display().to_string(),
        artifact_paths: artifact_paths.clone(),
        worker,
    };
    persist_agent_response(&resp, "agent_run", &run_dir, &artifact_paths, args.out)
}

async fn cmd_agent_fanout(
    args: AgentFanoutArgs,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let run_dir = resolve_agent_run_dir("fanout");
    let saved_result_path = run_dir.join("result.json");
    let saved_manifest_path = run_dir.join("manifest.json");
    let base_fallback_models = if args.fallback_models.is_empty() {
        default_agent_fallback_models()
    } else {
        args.fallback_models.clone()
    };
    let specs = load_fanout_specs(&args.task_template, &args.vars)?;
    if specs.is_empty() {
        anyhow::bail!("--vars produced 0 workers; provide a non-empty array of objects");
    }
    let shared_manifest_path = if let Some(path) = args.shared_manifest.clone() {
        let redirected = redirect_finance_output(path);
        let abs = resolve_abs_path(&redirected);
        if !abs.exists() {
            anyhow::bail!("--shared-manifest path does not exist: {}", abs.display());
        }
        if !abs.is_file() {
            anyhow::bail!("--shared-manifest is not a file: {}", abs.display());
        }
        Some(abs)
    } else {
        None
    };

    let max_parallel = args.max_parallel.max(1);
    let provider = Arc::new(provider);
    let model = Arc::new(model);
    let base_fallback_models = Arc::new(base_fallback_models);
    let run_dir_arc = Arc::new(run_dir.clone());
    let shared_manifest_arc = Arc::new(shared_manifest_path.clone());

    let stream = futures::stream::iter(specs.into_iter().map(|spec| {
        let provider = provider.clone();
        let model = model.clone();
        let base_fallback_models = base_fallback_models.clone();
        let run_dir = run_dir_arc.clone();
        let shared_manifest = shared_manifest_arc.clone();
        async move {
            let worker_name = spec.name;
            let mut worker_task = spec.task;
            if let Some(path) = shared_manifest.as_ref().as_ref() {
                worker_task = prepend_shared_manifest_context(&worker_task, path);
            }
            let worker_provider = spec.provider.or_else(|| (*provider).clone());
            let worker_model = spec.model.or_else(|| (*model).clone());
            let worker_fallback = if spec.fallback_models.is_empty() {
                (*base_fallback_models).clone()
            } else {
                spec.fallback_models
            };
            let worker_artifact_dir = run_dir
                .join("artifacts")
                .join(sanitize_worker_name(&worker_name));
            run_agent_worker(
                worker_name,
                worker_task,
                worker_provider,
                worker_model,
                worker_fallback,
                spec.max_ms.unwrap_or(args.max_ms),
                spec.max_attempts.unwrap_or(args.max_attempts),
                worker_artifact_dir,
            )
            .await
        }
    }))
    .buffer_unordered(max_parallel);

    let mut workers: Vec<AgentWorkerResult> = stream.collect().await;
    workers.sort_by(|a, b| a.name.cmp(&b.name));
    let mut artifact_paths: Vec<String> = workers
        .iter()
        .filter_map(|w| w.report_path.clone())
        .collect();
    if let Some(path) = &shared_manifest_path {
        artifact_paths.push(path.display().to_string());
    }

    let completed = workers.iter().filter(|w| w.status == "done").count();
    let failed = workers.len().saturating_sub(completed);
    if let Ok(summary_path) = write_fanout_summary_artifact(&run_dir, &workers, completed, failed) {
        artifact_paths.push(summary_path);
    }
    if let Ok(report_path) = write_worker_compendium_markdown(
        &run_dir,
        "fanout_report.md",
        "Fanout Model Report",
        &workers,
    ) {
        artifact_paths.push(report_path);
    }
    if let Ok(report_path) = write_collaboration_draft_markdown(
        &run_dir,
        "fanout_collab.md",
        "Fanout Collaboration Draft",
        &workers,
    ) {
        artifact_paths.push(report_path);
    }
    let resp = AgentFanoutResponse {
        ok: failed == 0,
        usable: completed > 0,
        kind: "agent_fanout".to_string(),
        saved_result_path: saved_result_path.display().to_string(),
        saved_manifest_path: saved_manifest_path.display().to_string(),
        artifact_paths: artifact_paths.clone(),
        task_template: args.task_template,
        shared_manifest_path: shared_manifest_path.map(|p| p.display().to_string()),
        summary: AgentFanoutSummary {
            requested: workers.len(),
            completed,
            failed,
            max_parallel,
        },
        workers,
    };
    persist_agent_response(&resp, "agent_fanout", &run_dir, &artifact_paths, args.out)
}

async fn cmd_agent_swarm(
    args: AgentSwarmArgs,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let run_dir = resolve_agent_run_dir("swarm");
    let saved_result_path = run_dir.join("result.json");
    let saved_manifest_path = run_dir.join("manifest.json");
    let input_abs = resolve_abs_path(&args.input);
    if !input_abs.exists() {
        anyhow::bail!("--input does not exist: {}", input_abs.display());
    }
    if !input_abs.is_file() {
        anyhow::bail!("--input is not a file: {}", input_abs.display());
    }

    let fallback_models = if args.fallback_models.is_empty() {
        default_agent_fallback_models()
    } else {
        args.fallback_models.clone()
    };
    let input_text = load_swarm_input_text(&input_abs).await?;
    let chunk_texts = chunk_text_for_swarm(
        &input_text,
        args.chunks,
        args.chunk_chars,
        args.overlap_chars,
        args.max_chunks,
    );
    if chunk_texts.is_empty() {
        anyhow::bail!("input produced 0 chunks (input may be empty)");
    }

    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("create swarm run dir {}", run_dir.display()))?;
    let chunks_dir = run_dir.join("artifacts/chunks");
    std::fs::create_dir_all(&chunks_dir).ok();

    let mut chunk_infos = Vec::with_capacity(chunk_texts.len());
    let mut artifact_paths = Vec::new();
    for (idx, chunk) in chunk_texts.iter().enumerate() {
        let path = chunks_dir.join(format!("chunk_{:03}.txt", idx + 1));
        std::fs::write(&path, chunk).with_context(|| format!("write {}", path.display()))?;
        chunk_infos.push(SwarmChunkInfo {
            index: idx + 1,
            path: path.display().to_string(),
            chars: chunk.chars().count(),
        });
        artifact_paths.push(path.display().to_string());
    }

    let chunk_manifest = json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "input_path": input_abs.display().to_string(),
        "total_input_chars": input_text.chars().count(),
        "requested_chunks": args.chunks,
        "generated_chunks": chunk_infos.len(),
        "chunk_chars": args.chunk_chars,
        "overlap_chars": args.overlap_chars,
        "chunks": chunk_infos,
    });
    let chunk_manifest_path = run_dir.join("artifacts/chunk_manifest.json");
    std::fs::write(
        &chunk_manifest_path,
        serde_json::to_string_pretty(&chunk_manifest)?,
    )
    .context("write chunk manifest")?;
    let chunk_manifest_value =
        serde_json::to_value(&chunk_manifest).context("serialize chunk manifest for meta")?;
    write_shadow_meta_for_value(
        &chunk_manifest_path,
        &chunk_manifest_value,
        "agent.swarm",
        "agent_swarm:chunk_manifest",
    )
    .context("write chunk manifest sidecar")?;
    artifact_paths.push(chunk_manifest_path.display().to_string());

    let provider = Arc::new(provider);
    let model = Arc::new(model);
    let fallback_models = Arc::new(fallback_models);
    let run_dir_arc = Arc::new(run_dir.clone());
    let max_parallel = args.max_parallel.max(1);
    let max_ms = args.max_ms;
    let max_attempts = args.max_attempts;
    let task_goal = Arc::new(args.task.clone());

    let stream = futures::stream::iter(chunk_infos.iter().cloned().map(|chunk| {
        let provider = provider.clone();
        let model = model.clone();
        let fallback_models = fallback_models.clone();
        let run_dir = run_dir_arc.clone();
        let goal = task_goal.clone();
        async move {
            let worker_name = format!("map_{:03}", chunk.index);
            let worker_task = format!(
                "Swarm map worker {idx}.\nGoal:\n{goal}\n\nInput chunk path:\n{path}\n\nInstructions:\n- Read only this chunk.\n- Extract high-signal facts relevant to the goal.\n- Be concise and explicit about uncertainty.\n- Cite any file paths you create.",
                idx = chunk.index,
                goal = goal,
                path = chunk.path,
            );
            let worker_artifact_dir = run_dir.join("artifacts").join(&worker_name);
            run_agent_worker(
                worker_name,
                worker_task,
                (*provider).clone(),
                (*model).clone(),
                (*fallback_models).clone(),
                max_ms,
                max_attempts,
                worker_artifact_dir,
            )
            .await
        }
    }))
    .buffer_unordered(max_parallel);

    let mut map_workers: Vec<AgentWorkerResult> = stream.collect().await;
    map_workers.sort_by(|a, b| a.name.cmp(&b.name));
    artifact_paths.extend(map_workers.iter().filter_map(|w| w.report_path.clone()));
    let map_completed = map_workers.iter().filter(|w| w.status == "done").count();
    let map_failed = map_workers.len().saturating_sub(map_completed);

    let map_manifest = json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "task": args.task,
        "input_path": input_abs.display().to_string(),
        "chunk_manifest_path": chunk_manifest_path.display().to_string(),
        "summary": {
            "requested": map_workers.len(),
            "completed": map_completed,
            "failed": map_failed,
        },
        "workers": map_workers,
    });
    let map_manifest_path = run_dir.join("artifacts/map_manifest.json");
    std::fs::write(
        &map_manifest_path,
        serde_json::to_string_pretty(&map_manifest)?,
    )
    .context("write map manifest")?;
    let map_manifest_value =
        serde_json::to_value(&map_manifest).context("serialize map manifest for meta")?;
    write_shadow_meta_for_value(
        &map_manifest_path,
        &map_manifest_value,
        "agent.swarm",
        "agent_swarm:map_manifest",
    )
    .context("write map manifest sidecar")?;
    artifact_paths.push(map_manifest_path.display().to_string());

    let reduce_task = format!(
        "Swarm reduce stage.\nGoal:\n{goal}\n\nInputs:\n- Chunk manifest: {chunk_manifest}\n- Map manifest: {map_manifest}\n\nInstructions:\n- Read successful map reports.\n- Merge overlapping findings and resolve conflicts.\n- Produce a concise synthesis with confidence levels.",
        goal = args.task,
        chunk_manifest = chunk_manifest_path.display(),
        map_manifest = map_manifest_path.display(),
    );
    let reduce_worker = run_agent_worker(
        "reduce".to_string(),
        reduce_task,
        (*provider).clone(),
        (*model).clone(),
        (*fallback_models).clone(),
        max_ms,
        max_attempts,
        run_dir.join("artifacts/reduce"),
    )
    .await;
    if let Some(path) = &reduce_worker.report_path {
        artifact_paths.push(path.clone());
    }

    let critic_task = format!(
        "Swarm critic stage.\nGoal:\n{goal}\n\nInputs:\n- Map manifest: {map_manifest}\n- Reduce report: {reduce_report}\n\nInstructions:\n- Critique harshly and specifically.\n- Flag weak claims, missing evidence, and contradictions.\n- Provide corrective actions.",
        goal = args.task,
        map_manifest = map_manifest_path.display(),
        reduce_report = reduce_worker
            .report_path
            .clone()
            .unwrap_or_else(|| "<missing>".to_string()),
    );
    let critic_worker = run_agent_worker(
        "critic".to_string(),
        critic_task,
        (*provider).clone(),
        (*model).clone(),
        (*fallback_models).clone(),
        max_ms,
        max_attempts,
        run_dir.join("artifacts/critic"),
    )
    .await;
    if let Some(path) = &critic_worker.report_path {
        artifact_paths.push(path.clone());
    }

    let final_task = format!(
        "Swarm final stage.\nGoal:\n{goal}\n\nInputs:\n- Chunk manifest: {chunk_manifest}\n- Map manifest: {map_manifest}\n- Reduce report: {reduce_report}\n- Critic report: {critic_report}\n\nInstructions:\n- Produce final answer with evidence-weighted conclusions.\n- Incorporate valid critic feedback.\n- Be concise and explicit about uncertainty.",
        goal = args.task,
        chunk_manifest = chunk_manifest_path.display(),
        map_manifest = map_manifest_path.display(),
        reduce_report = reduce_worker
            .report_path
            .clone()
            .unwrap_or_else(|| "<missing>".to_string()),
        critic_report = critic_worker
            .report_path
            .clone()
            .unwrap_or_else(|| "<missing>".to_string()),
    );
    let final_worker = run_agent_worker(
        "final".to_string(),
        final_task,
        (*provider).clone(),
        (*model).clone(),
        (*fallback_models).clone(),
        max_ms,
        max_attempts,
        run_dir.join("artifacts/final"),
    )
    .await;
    if let Some(path) = &final_worker.report_path {
        artifact_paths.push(path.clone());
    }
    if let Ok(report_path) = write_swarm_markdown_report(
        &run_dir,
        &args.task,
        &chunk_manifest_path,
        &map_manifest_path,
        &map_workers,
        &reduce_worker,
        &critic_worker,
        &final_worker,
    ) {
        artifact_paths.push(report_path);
    }
    let mut collab_workers = map_workers.clone();
    collab_workers.push(reduce_worker.clone());
    collab_workers.push(critic_worker.clone());
    collab_workers.push(final_worker.clone());
    if let Ok(report_path) = write_collaboration_draft_markdown(
        &run_dir,
        "swarm_collab.md",
        "Swarm Collaboration Draft",
        &collab_workers,
    ) {
        artifact_paths.push(report_path);
    }

    artifact_paths.sort();
    artifact_paths.dedup();
    let resp = AgentSwarmResponse {
        ok: final_worker.status == "done",
        usable: final_worker.status == "done" || reduce_worker.status == "done",
        kind: "agent_swarm".to_string(),
        saved_result_path: saved_result_path.display().to_string(),
        saved_manifest_path: saved_manifest_path.display().to_string(),
        artifact_paths: artifact_paths.clone(),
        task: args.task,
        input_path: input_abs.display().to_string(),
        chunk_manifest_path: chunk_manifest_path.display().to_string(),
        map_manifest_path: map_manifest_path.display().to_string(),
        summary: AgentSwarmSummary {
            requested_chunks: args.chunks.unwrap_or(0),
            generated_chunks: chunk_texts.len(),
            map_completed,
            map_failed,
            max_parallel,
        },
        map_workers,
        reduce_worker,
        critic_worker,
        final_worker,
    };
    persist_agent_response(&resp, "agent_swarm", &run_dir, &artifact_paths, args.out)
}

async fn load_swarm_input_text(path: &Path) -> Result<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "pdf" {
        return extract_pdf_text_via_pdftotext(path).await;
    }
    std::fs::read_to_string(path).with_context(|| format!("read input text {}", path.display()))
}

async fn extract_pdf_text_via_pdftotext(path: &Path) -> Result<String> {
    let output = TokioCommand::new("pdftotext")
        .arg("-layout")
        .arg(path)
        .arg("-")
        .output()
        .await
        .with_context(|| {
            format!(
                "run pdftotext for {} (install poppler to enable PDF swarm input)",
                path.display()
            )
        })?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).to_string();
        anyhow::bail!("pdftotext failed for {}: {}", path.display(), err.trim());
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    if text.trim().is_empty() {
        anyhow::bail!("pdftotext produced empty output for {}", path.display());
    }
    Ok(text)
}

fn chunk_text_for_swarm(
    text: &str,
    chunk_count: Option<usize>,
    chunk_chars: usize,
    overlap_chars: usize,
    max_chunks: usize,
) -> Vec<String> {
    if text.trim().is_empty() || max_chunks == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    if total == 0 {
        return Vec::new();
    }
    let requested_chunks = chunk_count.unwrap_or(0).min(max_chunks);
    if requested_chunks > 0 {
        let mut out = Vec::with_capacity(requested_chunks);
        for i in 0..requested_chunks {
            let base_start = i * total / requested_chunks;
            let base_end = (i + 1) * total / requested_chunks;
            let start = if i == 0 {
                base_start
            } else {
                base_start.saturating_sub(overlap_chars)
            };
            let end = if i + 1 == requested_chunks {
                base_end
            } else {
                (base_end + overlap_chars).min(total)
            };
            if end > start {
                let chunk: String = chars[start..end].iter().collect();
                out.push(chunk);
            }
        }
        return out;
    }

    let target_size = chunk_chars.max(1);
    let overlap = overlap_chars.min(target_size.saturating_sub(1));
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < total && out.len() < max_chunks {
        let remaining_slots = max_chunks.saturating_sub(out.len());
        let mut end = (start + target_size).min(total);
        if remaining_slots <= 1 {
            end = total;
        }
        if end <= start {
            break;
        }
        let chunk: String = chars[start..end].iter().collect();
        out.push(chunk);
        if end >= total {
            break;
        }
        let mut next = end.saturating_sub(overlap);
        if next <= start {
            next = end;
        }
        start = next;
    }
    out
}

fn load_fanout_specs(task_template: &str, vars_path: &Path) -> Result<Vec<AgentWorkerSpec>> {
    let raw = std::fs::read_to_string(vars_path)
        .with_context(|| format!("read vars file {}", vars_path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse JSON {}", vars_path.display()))?;
    let rows = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("--vars file must be a JSON array of objects"))?;

    let mut out = Vec::with_capacity(rows.len());
    for (idx, row) in rows.iter().enumerate() {
        let obj = row.as_object().ok_or_else(|| {
            anyhow::anyhow!("--vars item {} must be an object; got {}", idx + 1, row)
        })?;
        let mut vars = obj.clone();
        let name = vars
            .remove("name")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| format!("worker_{:02}", idx + 1));
        let provider = vars
            .remove("provider")
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let model = vars
            .remove("model")
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let fallback_models = parse_fallback_models_value(vars.remove("fallback_models"));
        let max_ms = vars.remove("max_ms").and_then(|v| {
            if let Some(n) = v.as_u64() {
                Some(n)
            } else if let Some(s) = v.as_str() {
                s.trim().parse::<u64>().ok()
            } else {
                None
            }
        });
        let max_attempts = vars.remove("max_attempts").and_then(|v| {
            if let Some(n) = v.as_u64() {
                Some(n as usize)
            } else if let Some(s) = v.as_str() {
                s.trim().parse::<usize>().ok()
            } else {
                None
            }
        });
        let task = render_task_template(task_template, &vars);
        out.push(AgentWorkerSpec {
            name,
            task,
            provider,
            model,
            fallback_models,
            max_ms,
            max_attempts,
        });
    }
    Ok(out)
}

fn prepend_shared_manifest_context(task: &str, shared_manifest_path: &Path) -> String {
    format!(
        "Shared Artifact Contract:\n- Read manifest first: {path}\n- Use artifact paths + sidecars as ground truth (not prose).\n- If you create data artifacts, use Eli tools with --out auto and cite path + meta_path.\n\n{task}",
        path = shared_manifest_path.display(),
        task = task
    )
}

fn parse_fallback_models_value(value: Option<serde_json::Value>) -> Vec<String> {
    let Some(v) = value else {
        return Vec::new();
    };
    if let Some(arr) = v.as_array() {
        return arr
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Some(s) = v.as_str() {
        return s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect();
    }
    Vec::new()
}

fn default_agent_fallback_models() -> Vec<String> {
    vec!["openrouter/free".to_string()]
}

fn load_model_health() -> std::collections::BTreeMap<String, ModelHealthEntry> {
    let path = Path::new(MODEL_HEALTH_PATH);
    let Ok(raw) = std::fs::read_to_string(path) else {
        return std::collections::BTreeMap::new();
    };
    serde_json::from_str::<std::collections::BTreeMap<String, ModelHealthEntry>>(&raw)
        .unwrap_or_default()
}

fn save_model_health(health: &std::collections::BTreeMap<String, ModelHealthEntry>) {
    let path = Path::new(MODEL_HEALTH_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(raw) = serde_json::to_string_pretty(health) {
        let _ = std::fs::write(path, raw);
    }
}

fn is_model_temporarily_disabled(model: &str) -> bool {
    let health = load_model_health();
    let Some(entry) = health.get(model) else {
        return false;
    };
    if entry.consecutive_failures < MODEL_DISABLE_CONSECUTIVE_FAILURES {
        return false;
    }
    let Some(ts) = &entry.last_seen_at else {
        return true;
    };
    let Ok(last) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return true;
    };
    let age = chrono::Utc::now()
        .signed_duration_since(last.with_timezone(&chrono::Utc))
        .num_hours();
    age < 24
}

fn record_model_health_attempt(model: &str, ok: bool, err: Option<String>) {
    if model.trim().is_empty() {
        return;
    }
    let mut health = load_model_health();
    let now = chrono::Utc::now().to_rfc3339();
    let entry = health.entry(model.to_string()).or_default();
    entry.last_seen_at = Some(now);
    if ok {
        entry.consecutive_failures = 0;
        entry.last_error = None;
    } else {
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.last_error = err.map(|e| tail_chars(&e, 240));
    }
    save_model_health(&health);
}

fn is_transient_agent_failure(error_text: &str) -> bool {
    let e = error_text.to_ascii_lowercase();
    e.contains("empty assistant message")
        || e.contains("stream parse error")
        || e.contains("stream event")
        || e.contains("error decoding response body")
        || e.contains("timed out")
        || e.contains("http 5")
}

async fn try_agent_direct_route(
    worker_name: &str,
    task: &str,
    provider: &str,
    requested_model: Option<&str>,
    run_dir: &Path,
) -> Result<Option<DirectAgentOutcome>> {
    let lower = task.to_ascii_lowercase();
    let started_at = chrono::Utc::now();
    let t0 = Instant::now();
    let artifacts_dir = run_dir.join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).ok();

    if let Some(subject) = extract_price_subject(task) {
        let search = eli_core::finance::fetch_search(eli_core::finance::SearchRequest {
            query: subject.clone(),
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("direct route search")?;
        let symbol = search
            .results
            .iter()
            .find(|x| {
                x.asset_type
                    .as_ref()
                    .map(|t| {
                        let tt = t.to_ascii_uppercase();
                        tt == "EQUITY" || tt == "ETF" || tt == "INDEX"
                    })
                    .unwrap_or(true)
            })
            .map(|x| x.symbol.clone())
            .or_else(|| search.results.first().map(|x| x.symbol.clone()));
        let Some(symbol) = symbol else {
            return Ok(None);
        };
        let snapshot = eli_core::finance::fetch_snapshot(eli_core::finance::SnapshotRequest {
            tickers: vec![symbol.clone()],
            provider: eli_core::finance::ProviderKind::Yahoo,
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("direct route snapshot")?;
        let snap = match snapshot.snapshots.first() {
            Some(s) => s,
            None => return Ok(None),
        };
        let price = snap.current_price.or(snap.open).unwrap_or(0.0);
        let prev = snap.previous_close.unwrap_or(0.0);
        let pct = if prev > 0.0 {
            (price / prev - 1.0) * 100.0
        } else {
            0.0
        };
        let search_path = artifacts_dir.join("search.json");
        let snapshot_path = artifacts_dir.join("snapshot.json");
        std::fs::write(&search_path, serde_json::to_string_pretty(&search)?)
            .context("write direct search")?;
        std::fs::write(&snapshot_path, serde_json::to_string_pretty(&snapshot)?)
            .context("write direct snapshot")?;
        let search_meta_value =
            serde_json::to_value(&search).context("serialize direct search for meta")?;
        write_shadow_meta_for_value(
            &search_path,
            &search_meta_value,
            "agent.direct",
            "direct_route:price_lookup:search",
        )
        .context("write direct search sidecar")?;
        let snapshot_meta_value =
            serde_json::to_value(&snapshot).context("serialize direct snapshot for meta")?;
        write_shadow_meta_for_value(
            &snapshot_path,
            &snapshot_meta_value,
            "agent.direct",
            "direct_route:price_lookup:snapshot",
        )
        .context("write direct snapshot sidecar")?;
        let summary_path = artifacts_dir.join("summary.md");
        let summary = format!(
            "# Direct agent route\n\n- Task: {task}\n- Route: price_lookup\n- Symbol: {symbol}\n- Price: ${price:.4}\n- Prev close: ${prev:.4}\n- Change: {pct:.2}%\n"
        );
        std::fs::write(&summary_path, summary).context("write direct summary")?;
        let artifact_paths = vec![
            search_path.display().to_string(),
            snapshot_path.display().to_string(),
            summary_path.display().to_string(),
        ];
        let worker = AgentWorkerResult {
            name: worker_name.to_string(),
            task: task.to_string(),
            status: "done".to_string(),
            exit_code: Some(0),
            requested_model: requested_model.map(|s| s.to_string()),
            used_model: Some("direct-tools".to_string()),
            attempted_models: vec!["direct-tools".to_string()],
            attempt_count: 1,
            attempts: vec![AgentAttemptResult {
                model: "direct-tools".to_string(),
                status: "ok".to_string(),
                duration_ms: t0.elapsed().as_millis(),
                exit_code: Some(0),
                error: None,
            }],
            report_path: Some(summary_path.display().to_string()),
            started_at: started_at.to_rfc3339(),
            finished_at: chrono::Utc::now().to_rfc3339(),
            duration_ms: t0.elapsed().as_millis(),
            stdout_tail: format!("direct_route=price_lookup symbol={symbol} provider={provider}"),
            stderr_tail: String::new(),
        };
        return Ok(Some(DirectAgentOutcome {
            worker,
            artifact_paths,
        }));
    }

    let compare_tickers = extract_compare_tickers(task);
    if lower.contains("compare") && compare_tickers.len() >= 2 {
        let cache_dir = default_finance_cache_dir()?;
        let ts = eli_core::finance::fetch_timeseries(
            eli_core::finance::TimeseriesRequest {
                tickers: compare_tickers.clone(),
                range: eli_core::finance::Span::parse("1d")?,
                granularity: eli_core::finance::Span::parse("1h")?,
                as_of: None,
                provider: eli_core::finance::ProviderKind::Yahoo,
                max_points_per_ticker: None,
            },
            &cache_dir,
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("direct route timeseries")?;
        let mut returns = serde_json::Map::new();
        let mut best: Option<(String, f64)> = None;
        let mut worst: Option<(String, f64)> = None;
        for s in &ts.series {
            if s.candles.len() < 2 {
                continue;
            }
            let first = s.candles.first().map(|c| c.c).unwrap_or(0.0);
            let last = s.candles.last().map(|c| c.c).unwrap_or(0.0);
            if first <= 0.0 {
                continue;
            }
            let r = (last / first - 1.0) * 100.0;
            returns.insert(s.ticker.clone(), json!(r));
            if best.as_ref().map(|(_, v)| r > *v).unwrap_or(true) {
                best = Some((s.ticker.clone(), r));
            }
            if worst.as_ref().map(|(_, v)| r < *v).unwrap_or(true) {
                worst = Some((s.ticker.clone(), r));
            }
        }
        let ts_path = artifacts_dir.join("timeseries.json");
        let analysis_path = artifacts_dir.join("analysis.json");
        std::fs::write(&ts_path, serde_json::to_string_pretty(&ts)?).context("write direct ts")?;
        let analysis = json!({
            "tickers": compare_tickers,
            "returns_pct": returns,
            "strongest": best,
            "weakest": worst,
        });
        std::fs::write(&analysis_path, serde_json::to_string_pretty(&analysis)?)
            .context("write direct analysis")?;
        let ts_meta_value = serde_json::to_value(&ts).context("serialize direct ts for meta")?;
        write_shadow_meta_for_value(
            &ts_path,
            &ts_meta_value,
            "agent.direct",
            "direct_route:compare_tickers:timeseries",
        )
        .context("write direct timeseries sidecar")?;
        write_shadow_meta_for_value(
            &analysis_path,
            &analysis,
            "agent.direct",
            "direct_route:compare_tickers:analysis",
        )
        .context("write direct analysis sidecar")?;
        let summary_path = artifacts_dir.join("summary.md");
        std::fs::write(
            &summary_path,
            format!(
                "# Direct agent route\n\n- Task: {task}\n- Route: compare_tickers\n- Strongest: {:?}\n- Weakest: {:?}\n",
                best, worst
            ),
        )
        .context("write direct summary")?;
        let artifact_paths = vec![
            ts_path.display().to_string(),
            analysis_path.display().to_string(),
            summary_path.display().to_string(),
        ];
        let worker = AgentWorkerResult {
            name: worker_name.to_string(),
            task: task.to_string(),
            status: "done".to_string(),
            exit_code: Some(0),
            requested_model: requested_model.map(|s| s.to_string()),
            used_model: Some("direct-tools".to_string()),
            attempted_models: vec!["direct-tools".to_string()],
            attempt_count: 1,
            attempts: vec![AgentAttemptResult {
                model: "direct-tools".to_string(),
                status: "ok".to_string(),
                duration_ms: t0.elapsed().as_millis(),
                exit_code: Some(0),
                error: None,
            }],
            report_path: Some(summary_path.display().to_string()),
            started_at: started_at.to_rfc3339(),
            finished_at: chrono::Utc::now().to_rfc3339(),
            duration_ms: t0.elapsed().as_millis(),
            stdout_tail: format!(
                "direct_route=compare_tickers tickers={}",
                extract_compare_tickers(task).join(",")
            ),
            stderr_tail: String::new(),
        };
        return Ok(Some(DirectAgentOutcome {
            worker,
            artifact_paths,
        }));
    }

    if lower.contains("today") && lower.contains("what is going on with") {
        let ticker = extract_primary_ticker(task).unwrap_or_else(|| "SPY".to_string());
        let cache_dir = default_finance_cache_dir()?;
        let snapshot = eli_core::finance::fetch_snapshot(eli_core::finance::SnapshotRequest {
            tickers: vec![ticker.clone()],
            provider: eli_core::finance::ProviderKind::Yahoo,
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("direct route snapshot")?;
        let ts = eli_core::finance::fetch_timeseries(
            eli_core::finance::TimeseriesRequest {
                tickers: vec![ticker.clone()],
                range: eli_core::finance::Span::parse("1d")?,
                granularity: eli_core::finance::Span::parse("5min")?,
                as_of: None,
                provider: eli_core::finance::ProviderKind::Yahoo,
                max_points_per_ticker: None,
            },
            &cache_dir,
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("direct route timeseries")?;
        let mut latest = None;
        let mut intraday = None;
        let mut vs_prev = None;
        if let (Some(s), Some(sn)) = (ts.series.first(), snapshot.snapshots.first()) {
            if let Some(last) = s.candles.last() {
                latest = Some(last.c);
                if let Some(open) = sn.open {
                    if open > 0.0 {
                        intraday = Some((last.c / open - 1.0) * 100.0);
                    }
                }
                if let Some(prev) = sn.previous_close {
                    if prev > 0.0 {
                        vs_prev = Some((last.c / prev - 1.0) * 100.0);
                    }
                }
            }
        }
        let snap_path = artifacts_dir.join("snapshot.json");
        let ts_path = artifacts_dir.join("timeseries.json");
        let analysis_path = artifacts_dir.join("analysis.json");
        std::fs::write(&snap_path, serde_json::to_string_pretty(&snapshot)?)
            .context("write direct snapshot")?;
        std::fs::write(&ts_path, serde_json::to_string_pretty(&ts)?).context("write direct ts")?;
        let analysis = json!({
            "ticker": ticker,
            "latest": latest,
            "intraday_pct": intraday,
            "vs_prev_close_pct": vs_prev,
        });
        std::fs::write(&analysis_path, serde_json::to_string_pretty(&analysis)?)
            .context("write direct analysis")?;
        let snap_meta_value =
            serde_json::to_value(&snapshot).context("serialize direct snapshot for meta")?;
        write_shadow_meta_for_value(
            &snap_path,
            &snap_meta_value,
            "agent.direct",
            "direct_route:ticker_today:snapshot",
        )
        .context("write direct snapshot sidecar")?;
        let ts_meta_value = serde_json::to_value(&ts).context("serialize direct ts for meta")?;
        write_shadow_meta_for_value(
            &ts_path,
            &ts_meta_value,
            "agent.direct",
            "direct_route:ticker_today:timeseries",
        )
        .context("write direct timeseries sidecar")?;
        write_shadow_meta_for_value(
            &analysis_path,
            &analysis,
            "agent.direct",
            "direct_route:ticker_today:analysis",
        )
        .context("write direct analysis sidecar")?;
        let summary_path = artifacts_dir.join("summary.md");
        std::fs::write(
            &summary_path,
            format!(
                "# Direct agent route\n\n- Task: {task}\n- Route: ticker_today\n- Ticker: {ticker}\n- Latest: {:?}\n- Intraday %: {:?}\n- Vs prev close %: {:?}\n",
                latest, intraday, vs_prev
            ),
        )
        .context("write direct summary")?;
        let artifact_paths = vec![
            snap_path.display().to_string(),
            ts_path.display().to_string(),
            analysis_path.display().to_string(),
            summary_path.display().to_string(),
        ];
        let worker = AgentWorkerResult {
            name: worker_name.to_string(),
            task: task.to_string(),
            status: "done".to_string(),
            exit_code: Some(0),
            requested_model: requested_model.map(|s| s.to_string()),
            used_model: Some("direct-tools".to_string()),
            attempted_models: vec!["direct-tools".to_string()],
            attempt_count: 1,
            attempts: vec![AgentAttemptResult {
                model: "direct-tools".to_string(),
                status: "ok".to_string(),
                duration_ms: t0.elapsed().as_millis(),
                exit_code: Some(0),
                error: None,
            }],
            report_path: Some(summary_path.display().to_string()),
            started_at: started_at.to_rfc3339(),
            finished_at: chrono::Utc::now().to_rfc3339(),
            duration_ms: t0.elapsed().as_millis(),
            stdout_tail: format!("direct_route=ticker_today ticker={ticker} provider={provider}"),
            stderr_tail: String::new(),
        };
        return Ok(Some(DirectAgentOutcome {
            worker,
            artifact_paths,
        }));
    }

    Ok(None)
}

fn extract_price_subject(task: &str) -> Option<String> {
    let lower = task.to_ascii_lowercase();
    let idx = lower.find("price of")?;
    let mut rest = task[idx + "price of".len()..].trim().to_string();
    for marker in [" stock", " right now", " now", " today", "?"] {
        if let Some(i) = rest.to_ascii_lowercase().find(marker) {
            rest.truncate(i);
            break;
        }
    }
    let out = rest.trim();
    if out.is_empty() {
        None
    } else {
        Some(out.to_string())
    }
}

fn extract_primary_ticker(task: &str) -> Option<String> {
    extract_compare_tickers(task).into_iter().next()
}

fn extract_compare_tickers(task: &str) -> Vec<String> {
    let stop = [
        "what",
        "is",
        "going",
        "on",
        "with",
        "today",
        "and",
        "the",
        "me",
        "who",
        "strongest",
        "weakest",
        "stock",
        "price",
        "of",
        "right",
        "now",
        "compare",
        "tell",
    ];
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for raw in task.split(|c: char| !c.is_ascii_alphanumeric()) {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        let low = t.to_ascii_lowercase();
        if stop.contains(&low.as_str()) {
            continue;
        }
        if t.chars().all(|c| c.is_ascii_alphabetic()) && t.len() <= 5 {
            let upper = t.to_ascii_uppercase();
            if seen.insert(upper.clone()) {
                out.push(upper);
            }
        }
    }
    out
}

fn default_finance_cache_dir() -> Result<PathBuf> {
    if let Ok(paths) = Paths::discover() {
        paths.ensure_dirs().context("ensure dirs")?;
        return Ok(paths.cache_dir);
    }
    let tmp = std::env::temp_dir().join("eli_agent_cache");
    std::fs::create_dir_all(&tmp).ok();
    Ok(tmp)
}

fn render_task_template(
    template: &str,
    vars: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        let key = format!("{{{{{k}}}}}");
        let val = match v {
            serde_json::Value::String(s) => s.clone(),
            _ => v.to_string(),
        };
        out = out.replace(&key, &val);
    }
    out
}

async fn run_agent_worker(
    name: String,
    task: String,
    provider: Option<String>,
    model: Option<String>,
    fallback_models: Vec<String>,
    max_ms: u64,
    max_attempts: usize,
    artifact_dir: PathBuf,
) -> AgentWorkerResult {
    let started_at = chrono::Utc::now();
    let t0 = Instant::now();
    let mut status = "failed".to_string();
    let mut exit_code = None;
    let mut used_model = None;
    let requested_model = model.clone();
    let mut attempted_models: Vec<String> = Vec::new();
    let mut attempts: Vec<AgentAttemptResult> = Vec::new();
    let mut report_path: Option<String> = None;
    let mut stdout_tail = String::new();
    let mut stderr_tail = String::new();
    let provider_arg = provider.unwrap_or_else(|| "openrouter".to_string());
    std::fs::create_dir_all(&artifact_dir).ok();
    let agent_context = build_agent_worker_context(&artifact_dir);
    let artifact_dir_abs = if artifact_dir.is_absolute() {
        artifact_dir.clone()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&artifact_dir)
    };
    let mut model_attempts: Vec<Option<String>> = Vec::new();
    model_attempts.push(model.clone());
    for fm in fallback_models {
        if fm.trim().is_empty() {
            continue;
        }
        let f = Some(fm.trim().to_string());
        if !model_attempts.contains(&f) {
            model_attempts.push(f);
        }
    }
    if model_attempts.is_empty() {
        model_attempts.push(None);
    }
    model_attempts.retain(|candidate| {
        let label = candidate
            .clone()
            .unwrap_or_else(|| "<config-default>".to_string());
        !is_model_temporarily_disabled(&label)
    });
    if model_attempts.is_empty() {
        return AgentWorkerResult {
            name,
            task,
            status: "failed".to_string(),
            exit_code: Some(1),
            requested_model,
            used_model: None,
            attempt_count: 0,
            attempted_models,
            attempts,
            report_path,
            started_at: started_at.to_rfc3339(),
            finished_at: chrono::Utc::now().to_rfc3339(),
            duration_ms: t0.elapsed().as_millis(),
            stdout_tail,
            stderr_tail: "all candidate models are temporarily disabled due to repeated failures"
                .to_string(),
        };
    }
    let max_attempts = max_attempts.max(1);

    'model_loop: for candidate in model_attempts {
        if attempts.len() >= max_attempts {
            break;
        }
        let label = candidate
            .clone()
            .unwrap_or_else(|| "<config-default>".to_string());
        attempted_models.push(label.clone());
        let mut retries_left = 1usize;
        loop {
            if attempts.len() >= max_attempts {
                break 'model_loop;
            }
            let attempt_t0 = Instant::now();
            let run = async {
                let exe = std::env::current_exe().context("resolve current executable")?;
                let mut cmd = TokioCommand::new(exe);
                cmd.arg("--provider").arg(&provider_arg);
                if let Some(m) = &candidate {
                    cmd.arg("--model").arg(m);
                }
                cmd.arg("research")
                    .arg(&task)
                    .env("ELI_PLAIN_OUTPUT", "1")
                    .env("ELI_NO_FOOTER", "1")
                    .env("ELI_AGENT_RUN_DIR", artifact_dir_abs.display().to_string())
                    .env("ELI_AGENT_CONTEXT", &agent_context)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());

                let timeout_ms = max_ms.max(1);
                let output = tokio_timeout(TokioDuration::from_millis(timeout_ms), cmd.output())
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!("worker attempt timed out after {}ms", timeout_ms)
                    })?
                    .context("spawn worker command")?;
                exit_code = output.status.code();
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                stdout_tail = tail_chars(&stdout, 1800);
                stderr_tail = tail_chars(&stderr, 1800);
                report_path = extract_saved_report_path(&stdout)
                    .or_else(|| extract_saved_report_path(&stderr));
                let empty_assistant = stderr
                    .to_ascii_lowercase()
                    .contains("empty assistant message");
                let has_useful_output = report_path
                    .as_ref()
                    .map(|p| Path::new(p).exists())
                    .unwrap_or(false);
                if output.status.success() && !empty_assistant && has_useful_output {
                    if let Some(missing) = missing_data_sidecars(&artifact_dir_abs) {
                        let joined = missing
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        let gate_msg =
                            format!("schema gate failed: missing sidecars for {}", joined);
                        stderr_tail = if stderr_tail.trim().is_empty() {
                            gate_msg
                        } else {
                            format!("{stderr_tail}\n{gate_msg}")
                        };
                        exit_code = Some(2);
                        Ok(false)
                    } else {
                        status = "done".to_string();
                        used_model = Some(label.clone());
                        Ok::<bool, anyhow::Error>(true)
                    }
                } else {
                    Ok(false)
                }
            }
            .await;

            match run {
                Ok(true) => {
                    record_model_health_attempt(&label, true, None);
                    attempts.push(AgentAttemptResult {
                        model: label.clone(),
                        status: "ok".to_string(),
                        duration_ms: attempt_t0.elapsed().as_millis(),
                        exit_code,
                        error: None,
                    });
                    break 'model_loop;
                }
                Ok(false) => {
                    let err = if stderr_tail.trim().is_empty() {
                        "unspecified failure".to_string()
                    } else {
                        tail_chars(&stderr_tail, 300)
                    };
                    record_model_health_attempt(&label, false, Some(err.clone()));
                    let transient = is_transient_agent_failure(&err);
                    attempts.push(AgentAttemptResult {
                        model: label.clone(),
                        status: "failed".to_string(),
                        duration_ms: attempt_t0.elapsed().as_millis(),
                        exit_code,
                        error: Some(err),
                    });
                    if transient && retries_left > 0 {
                        retries_left -= 1;
                        continue;
                    }
                    break;
                }
                Err(err) => {
                    let err_text = format!("{err:#}");
                    record_model_health_attempt(&label, false, Some(err_text.clone()));
                    let transient = is_transient_agent_failure(&err_text);
                    stderr_tail = if stderr_tail.is_empty() {
                        format!("worker runtime error: {err_text}")
                    } else {
                        format!("{stderr_tail}\nworker runtime error: {err_text}")
                    };
                    attempts.push(AgentAttemptResult {
                        model: label.clone(),
                        status: "error".to_string(),
                        duration_ms: attempt_t0.elapsed().as_millis(),
                        exit_code,
                        error: Some(err_text),
                    });
                    if transient && retries_left > 0 {
                        retries_left -= 1;
                        continue;
                    }
                    break;
                }
            }
        }
    }

    AgentWorkerResult {
        name,
        task,
        status,
        exit_code,
        requested_model,
        used_model,
        attempt_count: attempts.len(),
        attempted_models,
        attempts,
        report_path,
        started_at: started_at.to_rfc3339(),
        finished_at: chrono::Utc::now().to_rfc3339(),
        duration_ms: t0.elapsed().as_millis(),
        stdout_tail,
        stderr_tail,
    }
}

fn missing_data_sidecars(artifact_dir: &Path) -> Option<Vec<PathBuf>> {
    let data_files = collect_data_artifact_files(artifact_dir);
    if data_files.is_empty() {
        return None;
    }
    let missing = data_files
        .into_iter()
        .filter(|p| !eli_core::meta::sidecar_path_for(p).exists())
        .collect::<Vec<_>>();
    if missing.is_empty() {
        None
    } else {
        Some(missing)
    }
}

fn collect_data_artifact_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.exists() {
        return out;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            if is_data_artifact_path(&path) {
                out.push(path);
            }
        }
    }
    out
}

fn is_data_artifact_path(path: &Path) -> bool {
    let display = path.display().to_string().to_ascii_lowercase();
    if display.ends_with(".meta.json") {
        return false;
    }
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("json" | "csv" | "ndjson" | "parquet")
    )
}

fn extract_saved_report_path(text: &str) -> Option<String> {
    for line in text.lines().rev() {
        if let Some(pos) = line.find("saved:") {
            let raw = line[pos + "saved:".len()..].trim();
            let raw = raw.trim_start_matches('(').trim_end_matches(')');
            if !raw.is_empty() {
                return Some(raw.to_string());
            }
        }
    }
    None
}

fn sanitize_worker_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.trim_matches('_').is_empty() {
        "worker".to_string()
    } else {
        out
    }
}

fn build_agent_worker_context(artifact_dir: &Path) -> String {
    let abs = if artifact_dir.is_absolute() {
        artifact_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(artifact_dir)
    };
    format!(
        "- Save machine-readable outputs under: {dir}\n- Use `--out auto` for tool calls to get programmatic, context-rich filenames. Do not rely on `eli_research/data/.last_tool_output.json` across multiple tool calls.\n- If running Python, prefer a heredoc script (`python3 << 'EOF'`) over fragile nested-quote one-liners.\n- In your final synthesis.answer, cite exact output file path(s) you created.\n",
        dir = abs.display()
    )
}

fn tail_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut chars = input.chars().rev().take(max_chars).collect::<Vec<char>>();
    chars.reverse();
    chars.into_iter().collect()
}

fn resolve_agent_run_dir(kind: &str) -> PathBuf {
    let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let id = uuid::Uuid::new_v4().to_string();
    let short = &id[..8];
    Path::new("eli_research/data/agent_runs").join(format!("{kind}_{stamp}_{short}"))
}

fn persist_agent_response<T: Serialize>(
    full_value: &T,
    kind: &str,
    run_dir: &Path,
    artifact_paths: &[String],
    out_copy: Option<PathBuf>,
) -> Result<()> {
    let full_json = serde_json::to_string_pretty(full_value).context("serialize response")?;
    std::fs::create_dir_all(run_dir)
        .with_context(|| format!("create agent run dir {}", run_dir.display()))?;
    let result_path = run_dir.join("result.json");
    let manifest_path = run_dir.join("manifest.json");
    std::fs::write(&result_path, &full_json).context("write result file")?;

    let manifest = json!({
        "kind": kind,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "result_path": result_path.display().to_string(),
        "artifact_paths": artifact_paths,
    });
    let manifest_json = serde_json::to_string_pretty(&manifest).context("serialize manifest")?;
    std::fs::write(&manifest_path, manifest_json).context("write manifest file")?;

    if let Some(path) = out_copy {
        let out_path = redirect_finance_output(path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, &full_json).context("write --out copy")?;
    }
    println!("{full_json}");
    Ok(())
}

fn write_fanout_summary_artifact(
    run_dir: &Path,
    workers: &[AgentWorkerResult],
    completed: usize,
    failed: usize,
) -> Result<String> {
    let artifacts_dir = run_dir.join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).ok();
    let path = artifacts_dir.join("fanout_summary.json");
    let successful_reports: Vec<serde_json::Value> = workers
        .iter()
        .filter(|w| w.status == "done")
        .map(|w| {
            json!({
                "name": w.name,
                "used_model": w.used_model,
                "report_path": w.report_path,
                "duration_ms": w.duration_ms,
            })
        })
        .collect();
    let failed_workers: Vec<serde_json::Value> = workers
        .iter()
        .filter(|w| w.status != "done")
        .map(|w| {
            json!({
                "name": w.name,
                "requested_model": w.requested_model,
                "attempts": w.attempts,
                "stderr_tail": w.stderr_tail,
            })
        })
        .collect();
    let summary = json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "completed": completed,
        "failed": failed,
        "successful_reports": successful_reports,
        "failed_workers": failed_workers,
    });
    std::fs::write(&path, serde_json::to_string_pretty(&summary)?)
        .context("write fanout summary artifact")?;
    Ok(path.display().to_string())
}

fn write_swarm_markdown_report(
    run_dir: &Path,
    task: &str,
    chunk_manifest_path: &Path,
    map_manifest_path: &Path,
    map_workers: &[AgentWorkerResult],
    reduce_worker: &AgentWorkerResult,
    critic_worker: &AgentWorkerResult,
    final_worker: &AgentWorkerResult,
) -> Result<String> {
    let artifacts_dir = run_dir.join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).ok();
    let path = artifacts_dir.join("swarm_report.md");
    let mut md = String::new();
    md.push_str("# Swarm Model Report\n\n");
    md.push_str(&format!("- Task: `{}`\n", task));
    md.push_str(&format!(
        "- Chunk manifest: `{}`\n",
        chunk_manifest_path.display()
    ));
    md.push_str(&format!(
        "- Map manifest: `{}`\n\n",
        map_manifest_path.display()
    ));
    md.push_str("## Map Workers\n\n");
    md.push_str(&render_worker_sections_markdown(map_workers));
    md.push_str("\n## Stage Workers\n\n");
    md.push_str(&render_worker_sections_markdown(&[
        reduce_worker.clone(),
        critic_worker.clone(),
        final_worker.clone(),
    ]));
    std::fs::write(&path, md).context("write swarm report markdown")?;
    Ok(path.display().to_string())
}

fn write_worker_compendium_markdown(
    run_dir: &Path,
    filename: &str,
    title: &str,
    workers: &[AgentWorkerResult],
) -> Result<String> {
    let artifacts_dir = run_dir.join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).ok();
    let path = artifacts_dir.join(filename);
    let mut md = String::new();
    md.push_str(&format!("# {}\n\n", title));
    md.push_str(&render_worker_sections_markdown(workers));
    std::fs::write(&path, md).context("write worker compendium markdown")?;
    Ok(path.display().to_string())
}

fn write_collaboration_draft_markdown(
    run_dir: &Path,
    filename: &str,
    title: &str,
    workers: &[AgentWorkerResult],
) -> Result<String> {
    let artifacts_dir = run_dir.join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).ok();
    let path = artifacts_dir.join(filename);
    let mut md = String::new();
    md.push_str(&format!("# {}\n\n", title));
    md.push_str(
        "This is an append-only shared draft. Contradictions are preserved intentionally.\n\n",
    );
    md.push_str("## Contributions\n\n");
    for worker in workers {
        let model = worker
            .used_model
            .clone()
            .or_else(|| worker.requested_model.clone())
            .unwrap_or_else(|| "<unknown>".to_string());
        md.push_str(&format!(
            "### {} ({})\n\n- Status: `{}`\n- Duration: `{}` ms\n",
            worker.name, model, worker.status, worker.duration_ms
        ));
        if let Some(path) = &worker.report_path {
            md.push_str(&format!("- Source report: `{}`\n\n", path));
            if let Ok(raw) = std::fs::read_to_string(path) {
                let body =
                    extract_answer_markdown_block(&raw).unwrap_or_else(|| tail_chars(&raw, 1800));
                md.push_str(body.trim());
                md.push_str("\n\n");
            } else {
                md.push_str("_No readable report content._\n\n");
            }
        } else {
            md.push_str("_No report produced._\n\n");
        }
    }
    std::fs::write(&path, md).context("write collaboration draft markdown")?;
    Ok(path.display().to_string())
}

fn render_worker_sections_markdown(workers: &[AgentWorkerResult]) -> String {
    let mut out = String::new();
    for worker in workers {
        let model = worker
            .used_model
            .clone()
            .or_else(|| worker.requested_model.clone())
            .unwrap_or_else(|| "<unknown>".to_string());
        out.push_str(&format!("### {} ({})\n\n", worker.name, model));
        out.push_str(&format!(
            "- Status: `{}`\n- Duration: `{}` ms\n",
            worker.status, worker.duration_ms
        ));
        if let Some(path) = &worker.report_path {
            out.push_str(&format!("- Report: `{}`\n\n", path));
            match std::fs::read_to_string(path) {
                Ok(raw) => {
                    let body = extract_answer_markdown_block(&raw)
                        .unwrap_or_else(|| tail_chars(&raw, 1500));
                    out.push_str("```markdown\n");
                    out.push_str(body.trim());
                    out.push_str("\n```\n\n");
                }
                Err(_) => {
                    out.push_str("_Unable to read report file._\n\n");
                }
            }
        } else {
            out.push_str("_No report produced._\n\n");
        }
    }
    out
}

fn extract_answer_markdown_block(md: &str) -> Option<String> {
    let answer_header = "## Answer";
    let start = md.find(answer_header)?;
    let after = &md[start + answer_header.len()..];
    let mut end_idx = after.len();
    if let Some(pos) = after.find("\n## ") {
        end_idx = pos;
    }
    Some(after[..end_idx].trim().to_string())
}

async fn cmd_web_crawl(args: WebCrawlArgs) -> Result<()> {
    let url_for_meta = args.url.clone();
    let req = eli_core::web::CrawlRequest {
        url: args.url,
        max_pages: Some(args.max_pages),
        respect_robots: args.respect_robots,
        include_subdomains: args.subdomains,
        include_sitemap: args.sitemap,
        smart_mode: args.smart,
    };

    let resp = eli_core::web::crawl_website(req)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
        .context("crawl website")?;

    let mut write_result: Option<MetaWriteResult> = None;
    if let Some(out_path) = args.out.clone() {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "web.crawl",
            &[
                format!("url={url_for_meta}"),
                format!("sitemap={}", args.sitemap),
                format!("smart={}", args.smart),
                format!("view={}", format!("{:?}", args.view).to_ascii_lowercase()),
                format!("save={}", format!("{:?}", args.save).to_ascii_lowercase()),
            ],
        )?;
        write_result = Some(wr);
    } else if args.save == CrawlSaveMode::Auto {
        let wr = write_json_out_with_meta(
            PathBuf::from("eli_research/data/auto.json"),
            &resp,
            "web.crawl",
            &[
                format!("url={url_for_meta}"),
                format!("sitemap={}", args.sitemap),
                format!("smart={}", args.smart),
                format!("view={}", format!("{:?}", args.view).to_ascii_lowercase()),
                "save=auto".to_string(),
            ],
        )?;
        write_result = Some(wr);
    }

    match args.view {
        CrawlViewMode::Raw => {
            let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
            println!("{json}");
        }
        CrawlViewMode::Summary => {
            print_crawl_summary(&resp, write_result.as_ref());
        }
        CrawlViewMode::Path => {
            if let Some(wr) = write_result.as_ref() {
                println!(
                    "{{\"ok\":true,\"path\":{},\"meta_path\":{},\"pages_crawled\":{},\"crawl_mode\":{}}}",
                    serde_json::to_string(&wr.out_path.display().to_string())
                        .unwrap_or_else(|_| "\"\"".to_string()),
                    serde_json::to_string(&wr.meta_path.display().to_string())
                        .unwrap_or_else(|_| "\"\"".to_string()),
                    resp.pages_crawled,
                    serde_json::to_string(&resp.crawl_mode)
                        .unwrap_or_else(|_| "\"\"".to_string()),
                );
            } else {
                println!(
                    "{{\"ok\":true,\"saved\":false,\"pages_crawled\":{},\"crawl_mode\":{}}}",
                    resp.pages_crawled,
                    serde_json::to_string(&resp.crawl_mode).unwrap_or_else(|_| "\"\"".to_string()),
                );
            }
        }
    }

    if args.out.is_some() && args.view != CrawlViewMode::Path {
        if let Some(wr) = write_result.as_ref() {
            println!(
                "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
                serde_json::to_string(&wr.out_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(&wr.meta_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
            );
        }
    } else if args.view != CrawlViewMode::Path {
        if let Some(wr) = write_result {
            println!(
                "{{\"saved\":true,\"path\":{},\"meta_path\":{}}}",
                serde_json::to_string(&wr.out_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(&wr.meta_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
            );
        }
    }

    Ok(())
}

fn print_crawl_summary(resp: &eli_core::web::CrawlResponse, wr: Option<&MetaWriteResult>) {
    println!(
        "crawl mode={} pages={} duration_ms={}",
        resp.crawl_mode, resp.pages_crawled, resp.duration_ms
    );
    if resp.pages.is_empty() {
        println!("pages: none");
    } else {
        println!("top pages:");
        for page in resp.pages.iter().take(5) {
            let title = page.title.as_deref().unwrap_or("(untitled)");
            let snippet = page
                .text_preview
                .split_whitespace()
                .take(24)
                .collect::<Vec<_>>()
                .join(" ");
            println!("- {} | {}", title, page.url);
            if !snippet.is_empty() {
                println!("  {}", snippet);
            }
        }
        if resp.pages.len() > 5 {
            println!("... {} more pages", resp.pages.len().saturating_sub(5));
        }
    }
    if let Some(wr) = wr {
        println!(
            "saved raw={} meta={}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
    }
}

async fn cmd_web_search(args: WebSearchArgs) -> Result<()> {
    let hits = eli_core::web::providers::general::search_general(&args.query)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
        .context("web search")?;

    let resp = eli_core::web::WebSearchResponse { results: hits.clone(), hits };
    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "web.search",
            &[format!("query={}", args.query)],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_web_read(args: WebReadArgs) -> Result<()> {
    let article = eli_core::web::providers::read::read_url(&args.url)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
        .context("read url")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &article,
            "web.read",
            &[format!("url={}", args.url)],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&article).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_web_extract(args: WebExtractArgs) -> Result<()> {
    let resp = if let Some(url) = args.url {
        eli_core::extraction::extract_from_url(&url, args.bullets, args.focus)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
            .context("extract from url")?
    } else if let Some(file) = args.file {
        eli_core::extraction::extract_from_file(&file, args.bullets, args.focus)
            .map_err(|e| anyhow::anyhow!("{}", e))
            .context("extract from file")?
    } else if let Some(text) = args.text {
        let req = eli_core::extraction::ExtractRequest {
            content: text,
            source: "inline".to_string(),
            bullets: args.bullets,
            focus: args.focus,
        };
        eli_core::extraction::extract_facts(req)
            .map_err(|e| anyhow::anyhow!("{}", e))
            .context("extract from text")?
    } else {
        anyhow::bail!("must provide --url, --file, or --text");
    };

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "web.extract",
            &[format!("bullets={}", args.bullets)],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

/// Redirect JSON output files to eli_research/data/ if they're in the project root.
fn redirect_finance_output(path: std::path::PathBuf) -> std::path::PathBuf {
    // Only redirect if it's a bare filename (no directory component)
    if path
        .parent()
        .map(|p| p == std::path::Path::new("") || p == std::path::Path::new("."))
        .unwrap_or(true)
    {
        if let Some(filename) = path.file_name() {
            let target = std::path::Path::new("eli_research/data").join(filename);
            // Ensure directory exists
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            return target;
        }
    }
    path
}

fn is_auto_out_path(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    stem.eq_ignore_ascii_case("auto")
}

fn canonical_span_token(raw: &str) -> String {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return String::new();
    }
    let mut digits = String::new();
    let mut unit = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else if !ch.is_whitespace() {
            unit.push(ch);
        }
    }
    if digits.is_empty() {
        return normalize_name_token(&s, true, 16);
    }
    let suffix = match unit.as_str() {
        "y" | "yr" | "yrs" | "year" | "years" => "YR",
        "mo" | "mon" | "month" | "months" => "MO",
        "w" | "wk" | "week" | "weeks" => "W",
        "d" | "day" | "days" => "D",
        "h" | "hr" | "hour" | "hours" => "H",
        "m" | "min" | "mins" | "minute" | "minutes" => "MIN",
        other => return normalize_name_token(&format!("{digits}{other}"), true, 16),
    };
    format!("{digits}{suffix}")
}

fn normalize_name_token(raw: &str, uppercase: bool, max_len: usize) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        let c = if uppercase {
            ch.to_ascii_uppercase()
        } else {
            ch
        };
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if c == '_' || c == '-' {
            out.push('_');
        }
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    let out = out.trim_matches('_').to_string();
    if out.chars().count() > max_len {
        out.chars().take(max_len).collect()
    } else {
        out
    }
}

fn parse_kv_args(args: &[String]) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for arg in args {
        if let Some((k, v)) = arg.split_once('=') {
            let key = normalize_name_token(k, false, 48).to_ascii_lowercase();
            out.insert(key, v.to_string());
        }
    }
    out
}

fn tickers_from_payload(value: &serde_json::Value) -> Vec<String> {
    let mut tickers = Vec::new();
    if let Some(arr) = value.get("tickers").and_then(|v| v.as_array()) {
        for t in arr.iter().filter_map(|v| v.as_str()) {
            let tok = normalize_name_token(t, true, 12);
            if !tok.is_empty() {
                tickers.push(tok);
            }
        }
    }
    if tickers.is_empty() {
        if let Some(t) = value.get("ticker").and_then(|v| v.as_str()) {
            let tok = normalize_name_token(t, true, 12);
            if !tok.is_empty() {
                tickers.push(tok);
            }
        }
    }
    if tickers.is_empty() {
        if let Some(series) = value.get("series").and_then(|v| v.as_array()) {
            for row in series {
                if let Some(t) = row.get("ticker").and_then(|v| v.as_str()) {
                    let tok = normalize_name_token(t, true, 12);
                    if !tok.is_empty() {
                        tickers.push(tok);
                    }
                }
            }
        }
    }
    if tickers.is_empty() {
        if let Some(snaps) = value.get("snapshots").and_then(|v| v.as_array()) {
            for row in snaps {
                if let Some(t) = row.get("ticker").and_then(|v| v.as_str()) {
                    let tok = normalize_name_token(t, true, 12);
                    if !tok.is_empty() {
                        tickers.push(tok);
                    }
                }
            }
        }
    }
    tickers.sort();
    tickers.dedup();
    tickers
}

fn tool_prefix(tool_name: &str) -> String {
    match tool_name {
        "finance.timeseries" => "TIMESERIES".to_string(),
        "finance.snapshot" => "SNAPSHOT".to_string(),
        "finance.odds" => "ODDS".to_string(),
        "finance.sync" => "SYNC".to_string(),
        "finance.options" => "OPTIONS".to_string(),
        "finance.prices" => "PRICES".to_string(),
        "finance.news" => "NEWS".to_string(),
        "finance.fundamentals" => "FUNDAMENTALS".to_string(),
        "finance.filings" => "FILINGS".to_string(),
        "finance.search" => "SEARCH".to_string(),
        "finance.macro" => "MACRO".to_string(),
        "finance.schedule" => "SCHEDULE".to_string(),
        "web.search" => "WEBSEARCH".to_string(),
        "web.read" => "WEBREAD".to_string(),
        "web.crawl" => "WEBCRAWL".to_string(),
        "web.extract" => "WEBEXTRACT".to_string(),
        _ => normalize_name_token(tool_name, true, 20),
    }
}

fn build_programmatic_dataset_stem(
    tool_name: &str,
    value: &serde_json::Value,
    args: &[String],
    stamp: &str,
) -> String {
    let kv = parse_kv_args(args);
    let mut parts = vec![tool_prefix(tool_name)];

    let tickers = tickers_from_payload(value);
    if !tickers.is_empty() {
        parts.extend(tickers);
    }

    if let Some(range) = kv.get("range") {
        let tok = canonical_span_token(range);
        if !tok.is_empty() {
            parts.push(tok);
        }
    }
    if let Some(granularity) = kv.get("granularity") {
        let tok = canonical_span_token(granularity);
        if !tok.is_empty() {
            parts.push(tok);
        }
    }

    let provider = value
        .get("provider")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| kv.get("provider").cloned());
    if let Some(provider) = provider {
        let tok = normalize_name_token(&provider, true, 12);
        if !tok.is_empty() {
            parts.push(tok);
        }
    }

    if let Some(status) = kv.get("status") {
        let tok = normalize_name_token(status, true, 12);
        if !tok.is_empty() {
            parts.push(tok);
        }
    }

    parts.push(stamp.to_string());
    let mut stem = parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    if stem.chars().count() > 220 {
        stem = stem.chars().take(220).collect();
    }
    stem
}

fn resolve_programmatic_out_path(
    requested: PathBuf,
    tool_name: &str,
    value: &serde_json::Value,
    args: &[String],
) -> PathBuf {
    let requested = redirect_finance_output(requested);
    if !is_auto_out_path(&requested) {
        return requested;
    }
    let parent = requested.parent().and_then(|p| {
        if p == Path::new("") || p == Path::new(".") {
            None
        } else {
            Some(p.to_path_buf())
        }
    });
    let dir = parent.unwrap_or_else(|| PathBuf::from("eli_research/data"));
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();
    let stem = build_programmatic_dataset_stem(tool_name, value, args, &stamp);
    dir.join(format!("{stem}.json"))
}

#[derive(Clone, Debug)]
struct MetaWriteResult {
    out_path: PathBuf,
    meta_path: PathBuf,
}

fn resolve_abs_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(path)
}

fn write_json_out_with_meta<T: Serialize>(
    out_path: PathBuf,
    payload: &T,
    tool_name: &str,
    args: &[String],
) -> Result<MetaWriteResult> {
    let value = serde_json::to_value(payload).context("serialize response to value")?;
    let out_path = resolve_programmatic_out_path(out_path, tool_name, &value, args);
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let json = serde_json::to_string_pretty(payload).context("serialize response")?;
    std::fs::write(&out_path, &json).context("write --out")?;

    let abs = resolve_abs_path(&out_path);
    let ctx = eli_core::meta::MetaContext {
        source_path: Some(abs),
        source_kind: eli_core::meta::SourceKind::Json,
        source_size_bytes: Some(json.as_bytes().len() as u64),
        provenance: Some(eli_core::meta::MetaProvenance {
            tool: Some(tool_name.to_string()),
            command: Some(tool_name.to_string()),
            args: args.to_vec(),
            origin_query: None,
        }),
    };
    let meta = eli_core::meta::build_json_meta(&value, ctx);
    let meta_path =
        eli_core::meta::write_sidecar(&meta, &out_path).context("write sidecar meta")?;
    Ok(MetaWriteResult {
        out_path,
        meta_path,
    })
}

fn prediction_markets_path_for_output(out_path: &Path) -> PathBuf {
    let parent = out_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut existing: Vec<PathBuf> = std::fs::read_dir(parent)
        .ok()
        .into_iter()
        .flat_map(|rd| rd.filter_map(|e| e.ok()))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    n.starts_with("prediction_markets_")
                        && n.ends_with(".json")
                        && !n.ends_with(".meta.json")
                })
                .unwrap_or(false)
        })
        .collect();
    existing.sort();
    if let Some(last) = existing.pop() {
        return last;
    }
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    parent.join(format!("prediction_markets_{stamp}.json"))
}

fn push_unique_items(
    dst: &mut Vec<serde_json::Value>,
    src: &[serde_json::Value],
    key_fn: impl Fn(&serde_json::Value) -> Option<String>,
) {
    let mut seen: std::collections::HashSet<String> = dst.iter().filter_map(&key_fn).collect();
    for item in src {
        if let Some(key) = key_fn(item) {
            if seen.insert(key) {
                dst.push(item.clone());
            }
        } else {
            dst.push(item.clone());
        }
    }
}

fn parse_json_array_field(
    root: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Vec<serde_json::Value> {
    root.get(key)
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

fn update_prediction_markets(
    prediction_markets_path: &Path,
    req: &eli_core::finance::OddsRequest,
    resp: &eli_core::finance::OddsResponse,
    source_file: Option<&Path>,
) -> Result<()> {
    let mut bundle: serde_json::Map<String, serde_json::Value> = if prediction_markets_path.exists()
    {
        let raw = std::fs::read_to_string(prediction_markets_path).unwrap_or_default();
        serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    } else {
        serde_json::Map::new()
    };

    let resp_value = serde_json::to_value(resp).context("serialize odds response for bundle")?;
    let resp_obj = resp_value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("odds response was not an object"))?;

    let mut query_entry = serde_json::Map::new();
    query_entry.insert(
        "recorded_at".to_string(),
        serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
    );
    query_entry.insert(
        "provider".to_string(),
        req.provider
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone()))
            .unwrap_or(serde_json::Value::Null),
    );
    query_entry.insert(
        "search".to_string(),
        req.search
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone()))
            .unwrap_or(serde_json::Value::Null),
    );
    query_entry.insert(
        "series_ticker".to_string(),
        req.series_ticker
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone()))
            .unwrap_or(serde_json::Value::Null),
    );
    query_entry.insert(
        "event_ticker".to_string(),
        req.event_ticker
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone()))
            .unwrap_or(serde_json::Value::Null),
    );
    query_entry.insert(
        "market_ticker".to_string(),
        req.market_ticker
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone()))
            .unwrap_or(serde_json::Value::Null),
    );
    query_entry.insert(
        "status".to_string(),
        req.status
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone()))
            .unwrap_or(serde_json::Value::Null),
    );
    query_entry.insert(
        "source_file".to_string(),
        source_file
            .map(|p| serde_json::Value::String(resolve_abs_path(p).display().to_string()))
            .unwrap_or(serde_json::Value::Null),
    );

    let mut queries = parse_json_array_field(&bundle, "queries");
    queries.push(serde_json::Value::Object(query_entry));
    if queries.len() > 500 {
        let drop_n = queries.len().saturating_sub(500);
        queries.drain(0..drop_n);
    }
    bundle.insert("queries".to_string(), serde_json::Value::Array(queries));

    let mut available_markets = parse_json_array_field(&bundle, "available_markets");
    let new_available_markets = parse_json_array_field(&resp_obj, "available_markets");
    push_unique_items(&mut available_markets, &new_available_markets, |v| {
        let obj = v.as_object()?;
        obj.get("market_id")
            .and_then(|x| x.as_str())
            .map(|s| format!("id:{s}"))
            .or_else(|| {
                obj.get("ticker")
                    .and_then(|x| x.as_str())
                    .map(|s| format!("ticker:{s}"))
            })
    });
    bundle.insert(
        "available_markets".to_string(),
        serde_json::Value::Array(available_markets),
    );

    let mut markets = parse_json_array_field(&bundle, "markets");
    let new_markets = parse_json_array_field(&resp_obj, "markets");
    push_unique_items(&mut markets, &new_markets, |v| {
        let obj = v.as_object()?;
        obj.get("market_id")
            .and_then(|x| x.as_str())
            .map(|s| format!("id:{s}"))
            .or_else(|| {
                obj.get("ticker")
                    .and_then(|x| x.as_str())
                    .map(|s| format!("ticker:{s}"))
            })
    });
    bundle.insert("markets".to_string(), serde_json::Value::Array(markets));

    let mut available_events = parse_json_array_field(&bundle, "available_events");
    let new_available_events = parse_json_array_field(&resp_obj, "available_events");
    push_unique_items(&mut available_events, &new_available_events, |v| {
        let obj = v.as_object()?;
        obj.get("event_id")
            .and_then(|x| x.as_str())
            .map(|s| format!("id:{s}"))
            .or_else(|| {
                obj.get("ticker")
                    .and_then(|x| x.as_str())
                    .map(|s| format!("ticker:{s}"))
            })
    });
    bundle.insert(
        "available_events".to_string(),
        serde_json::Value::Array(available_events),
    );

    let mut events = parse_json_array_field(&bundle, "events");
    let new_events = parse_json_array_field(&resp_obj, "events");
    push_unique_items(&mut events, &new_events, |v| {
        let obj = v.as_object()?;
        obj.get("event_id")
            .and_then(|x| x.as_str())
            .map(|s| format!("id:{s}"))
            .or_else(|| {
                obj.get("ticker")
                    .and_then(|x| x.as_str())
                    .map(|s| format!("ticker:{s}"))
            })
    });
    bundle.insert("events".to_string(), serde_json::Value::Array(events));

    let mut sources = parse_json_array_field(&bundle, "sources");
    let new_sources = parse_json_array_field(&resp_obj, "sources");
    push_unique_items(&mut sources, &new_sources, |v| {
        let obj = v.as_object()?;
        obj.get("source")
            .and_then(|x| x.as_str())
            .map(|s| format!("source:{s}"))
    });
    bundle.insert("sources".to_string(), serde_json::Value::Array(sources));

    if let Some(semantics) = resp_obj.get("field_semantics") {
        bundle.insert("field_semantics".to_string(), semantics.clone());
    }

    bundle.insert(
        "bundle_type".to_string(),
        serde_json::Value::String("eli_finance_prediction_markets".to_string()),
    );
    bundle.insert(
        "updated_at".to_string(),
        serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
    );
    let query_count = bundle
        .get("queries")
        .and_then(|v| v.as_array())
        .map(|v| v.len())
        .unwrap_or(0);
    bundle.insert(
        "query_count".to_string(),
        serde_json::Value::Number(serde_json::Number::from(query_count as u64)),
    );

    if let Some(parent) = prediction_markets_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_string_pretty(&serde_json::Value::Object(bundle))
        .context("serialize odds bundle")?;
    std::fs::write(prediction_markets_path, json).context("write prediction markets")?;
    Ok(())
}

fn write_shadow_meta_for_value(
    target_data_path: &Path,
    value: &serde_json::Value,
    tool_name: &str,
    command: &str,
) -> Result<PathBuf> {
    let raw = serde_json::to_string(value).unwrap_or_default();
    let ctx = eli_core::meta::MetaContext {
        source_path: Some(resolve_abs_path(target_data_path)),
        source_kind: eli_core::meta::SourceKind::Json,
        source_size_bytes: Some(raw.as_bytes().len() as u64),
        provenance: Some(eli_core::meta::MetaProvenance {
            tool: Some(tool_name.to_string()),
            command: Some(command.to_string()),
            args: Vec::new(),
            origin_query: None,
        }),
    };
    let meta = eli_core::meta::build_json_meta(value, ctx);
    eli_core::meta::write_sidecar(&meta, target_data_path).context("write implicit sidecar meta")
}

fn schema_pattern_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let meta = eli_core::meta::build_json_meta(
        value,
        eli_core::meta::MetaContext {
            source_kind: eli_core::meta::SourceKind::Json,
            ..Default::default()
        },
    );
    let root = format!("schema_root={}", meta.schema_tree.kind);
    let path_count = format!("schema_paths={}", meta.path_index.len());
    let nullable = format!("nullable_fields={}", meta.vitals.nullable_paths);
    vec![root, path_count, nullable]
}

async fn cmd_finance_macro(args: FinanceMacroArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let range = if args.range.is_empty() {
        None
    } else {
        match eli_core::finance::Span::parse(&args.range) {
            Ok(s) => Some(s),
            Err(e) => anyhow::bail!("invalid --range '{}': {}", args.range, e),
        }
    };

    let compare_to = if let Some(raw) = args.compare_to.as_deref() {
        Some(
            chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
                .map_err(|_| anyhow::anyhow!("invalid --compare-to '{raw}' (expected YYYY-MM-DD)"))?,
        )
    } else {
        None
    };

    let req = eli_core::finance::MacroRequest { range, compare_to };
    let resp = eli_core::finance::fetch_macro(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch macro")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.macro",
            &[format!("range={}", args.range)],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_schedule(args: FinanceScheduleArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let kind = match args.kind.trim().to_ascii_lowercase().as_str() {
        "earnings" => eli_core::finance::ScheduleKind::Earnings,
        "macro" => eli_core::finance::ScheduleKind::Macro,
        "all" => eli_core::finance::ScheduleKind::All,
        other => anyhow::bail!("unsupported --kind '{other}' (supported: earnings, macro, all)"),
    };

    let (start_date, end_date) = if let Some(date) = args.date {
        if args.from.is_some() || args.to.is_some() {
            anyhow::bail!("use either --date or --from/--to");
        }
        (date.clone(), date)
    } else {
        let start = args
            .from
            .ok_or_else(|| anyhow::anyhow!("missing --date or --from"))?;
        let end = args.to.unwrap_or_else(|| start.clone());
        (start, end)
    };

    let req = eli_core::finance::ScheduleRequest {
        kind,
        start_date,
        end_date,
        tickers: args.ticker,
        major_only: args.major,
    };

    let resp = eli_core::finance::fetch_schedule(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch schedule")?;
    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.schedule",
            &[format!("kind={}", args.kind)],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_rate_path(args: FinanceRatePathArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let source_mode = match args.source_mode.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(eli_core::finance::RatePathSourceMode::Auto),
        "meeting" => Some(eli_core::finance::RatePathSourceMode::Meeting),
        "fallback" => Some(eli_core::finance::RatePathSourceMode::Fallback),
        other => anyhow::bail!(
            "unsupported --source-mode '{other}' (supported: auto, meeting, fallback)"
        ),
    };
    let req = eli_core::finance::RatePathRequest {
        cache_dir: args.cache_dir.map(|p| p.to_string_lossy().to_string()),
        source_mode,
    };
    let resp = eli_core::finance::fetch_rate_path(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch rate path")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.rate_path", &[])?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_yield_curve(args: FinanceYieldCurveArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let mut compare_3mo = false;
    let mut compare_1y = false;
    for item in &args.compare {
        match item.trim().to_ascii_lowercase().as_str() {
            "" => {}
            "3mo" => compare_3mo = true,
            "1y" => compare_1y = true,
            other => anyhow::bail!("invalid --compare value '{other}' (supported: 3mo,1y)"),
        }
    }

    let req = eli_core::finance::YieldCurveRequest {
        compare_3mo,
        compare_1y,
        strict: args.strict,
    };
    let resp = eli_core::finance::fetch_yield_curve(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch yield curve")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.yield_curve", &[])?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_dashboard(args: FinanceDashboardArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let req = eli_core::finance::DashboardRequest {
        preset: args.preset.clone(),
        max_ms: args.max_ms,
    };
    let resp = eli_core::finance::fetch_dashboard(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch dashboard")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.dashboard",
            &[format!("preset={}", args.preset)],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_prices(args: FinancePricesArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let ids_for_meta = args.ids.clone();
    let req = eli_core::finance::PricesRequest {
        query: args.query,
        asset_type: args.asset_type,
        ids: args.ids,
        auto_select: args.auto_select,
    };

    let resp = eli_core::finance::fetch_prices(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch prices")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.prices", &ids_for_meta)?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_odds(args: FinanceOddsArgs) -> Result<()> {
    if let Some(action) = args.action {
        match action {
            FinanceOddsAction::Sync(sync_args) => return cmd_finance_sync(sync_args).await,
            FinanceOddsAction::Where(where_args) => return cmd_finance_odds_where(where_args),
        }
    }

    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    // When --search is provided alone (no --list-events/--list-markets/ticker),
    // search the local CSV cache from `eli finance sync` instead of hitting the API.
    let has_search = args.search.is_some();
    let has_list_or_ticker = args.list_events
        || args.list_markets
        || args.list_series
        || args.list_tags
        || args.series.is_some()
        || args.event.is_some()
        || args.market.is_some();

    if has_search && !has_list_or_ticker {
        // Check if local CSV cache exists
        let cache_dir = directories::ProjectDirs::from("", "", "eli")
            .map(|d| d.cache_dir().join("odds"))
            .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"));
        let csv_path = cache_dir.join("all_markets.csv");

        if csv_path.exists() && !args.live {
            // CSV exists — search locally (instant, no API calls)
            return cmd_finance_odds_search_csv(
                args.search.as_deref().unwrap_or(""),
                args.limit,
                args.country.as_deref(),
                args.min_volume,
                args.top,
                args.explain,
            );
        }

        if csv_path.exists() && args.live {
            // CSV exists + --live: search CSV for tickers, then fetch fresh prices
            return cmd_finance_odds_search_live(
                args.search.as_deref().unwrap_or(""),
                &csv_path,
                args.limit,
                args.country.as_deref(),
                args.min_volume,
                args.top,
                args.explain,
            )
            .await;
        }

        // No CSV — fall back to live API search (Kalshi events → markets)
        eprintln!(
            "no local CSV cache; falling back to live API search for {:?}",
            args.search.as_deref().unwrap_or("")
        );
        return cmd_finance_odds_search_live_no_csv(
            args.search.as_deref().unwrap_or(""),
            args.limit,
            args.top,
        )
        .await;
    }

    let provider = args
        .provider
        .as_ref()
        .map(|s| s.trim().to_ascii_lowercase());
    let provider = match provider {
        None => None,
        Some(p) if p.is_empty() => None,
        Some(p) => match p.as_str() {
            "kalshi" | "polymarket" | "auto" => Some(p),
            other => anyhow::bail!(
                "unsupported --provider '{other}' (supported: kalshi, polymarket, auto)"
            ),
        },
    };

    let req = eli_core::finance::OddsRequest {
        provider,
        disable_kalshi: false,
        series_ticker: args.series,
        event_ticker: args.event,
        market_ticker: args.market,
        status: args.status,
        limit: args.limit,
        cursor: args.cursor,
        max_pages: args.max_pages,
        include_orderbook: args.orderbook,
        orderbook_depth: args.depth,
        list_series: args.list_series,
        list_events: args.list_events,
        list_markets: args.list_markets,
        list_tags: args.list_tags,
        category: args.category,
        search: args.search,
    };

    let resp = eli_core::finance::fetch_odds(req.clone())
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch odds")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.odds",
            &[format!(
                "provider={}",
                args.provider.clone().unwrap_or_default()
            )],
        )?;
        let prediction_markets_path = prediction_markets_path_for_output(&wr.out_path);
        update_prediction_markets(&prediction_markets_path, &req, &resp, Some(&wr.out_path))
            .context("update prediction markets")?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{},\"prediction_markets_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&prediction_markets_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string())
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

/// Search the local prediction market CSV cache (from `eli finance sync`).
/// Returns matching markets as JSON, sorted by volume descending.
fn cmd_finance_odds_search_csv(
    query: &str,
    limit: Option<usize>,
    country: Option<&str>,
    min_volume_usd: Option<f64>,
    top: Option<usize>,
    explain: bool,
) -> Result<()> {
    #[derive(Deserialize)]
    struct OddsCsvRow {
        source: String,
        ticker: String,
        title: String,
        event_ticker: String,
        yes_price: String,
        volume: String,
        status: String,
        probability: String,
        category: String,
        topic: String,
    }

    fn contains_keyword(haystack: &str, keyword: &str) -> bool {
        if keyword.contains(' ') || keyword.contains('.') {
            return haystack.contains(keyword);
        }
        haystack
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|tok| tok == keyword)
    }

    fn find_us_hints(row: &OddsCsvRow) -> Vec<String> {
        let text = format!(
            "{} {} {} {} {}",
            row.title, row.event_ticker, row.category, row.topic, row.ticker
        )
        .to_ascii_lowercase();
        let keywords = [
            "us",
            "u.s.",
            "united states",
            "american",
            "nfp",
            "nonfarm payrolls",
            "fomc",
            "federal reserve",
            "cpi",
            "pce",
            "gdpnow",
        ];
        keywords
            .iter()
            .filter(|k| contains_keyword(&text, k))
            .map(|k| k.to_string())
            .collect()
    }

    fn compile_term_patterns(terms: &[String]) -> Vec<(String, regex::Regex)> {
        terms
            .iter()
            .filter_map(|t| {
                regex::Regex::new(&format!(r"(?i)\b{}\b", regex::escape(t)))
                    .ok()
                    .map(|re| (t.clone(), re))
            })
            .collect()
    }

    fn compute_match_terms(text: &str, term_patterns: &[(String, regex::Regex)]) -> Vec<String> {
        term_patterns
            .iter()
            .filter_map(|(term, re)| re.is_match(text).then_some(term.clone()))
            .collect()
    }

    fn has_phrase_match(text: &str, phrase_pattern: &Option<regex::Regex>) -> bool {
        phrase_pattern.as_ref().is_some_and(|re| re.is_match(text))
    }

    fn compute_match_score(row: &OddsCsvRow, query: &str, matched_terms: &[String], volume_usd: f64) -> i64 {
        let q = query.to_ascii_lowercase();
        let title = row.title.to_ascii_lowercase();
        let ticker = row.ticker.to_ascii_lowercase();
        let event = row.event_ticker.to_ascii_lowercase();
        let category = row.category.to_ascii_lowercase();
        let topic = row.topic.to_ascii_lowercase();

        let mut score = 0.0f64;
        if !q.is_empty() && title.contains(&q) {
            score += 30.0;
        }
        for t in matched_terms {
            if title.contains(t) {
                score += 10.0;
            }
            if ticker.contains(t) || event.contains(t) {
                score += 6.0;
            }
            if category.contains(t) || topic.contains(t) {
                score += 4.0;
            }
        }
        score += (matched_terms.len() as f64) * 8.0;
        score += (volume_usd.max(0.0) + 1.0).log10() * 3.0;
        score.round() as i64
    }

    fn explain_reasons(
        row: &OddsCsvRow,
        query: &str,
        matched_terms: &[String],
        volume_usd: f64,
    ) -> Vec<String> {
        let mut reasons = Vec::new();
        let query_l = query.to_ascii_lowercase();
        let title_l = row.title.to_ascii_lowercase();
        if !query_l.is_empty() && title_l.contains(&query_l) {
            reasons.push("title contains full query".to_string());
        }
        if !matched_terms.is_empty() {
            reasons.push(format!("matched terms: {}", matched_terms.join(", ")));
        }
        reasons.push(format!("volume_usd={volume_usd:.2}"));
        reasons.truncate(3);
        reasons
    }

    let cache_dir = directories::ProjectDirs::from("", "", "eli")
        .map(|d| d.cache_dir().join("odds"))
        .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"));

    let csv_path = cache_dir.join("all_markets.csv");
    if !csv_path.exists() {
        anyhow::bail!(
            "no local prediction market cache found at {}. Run `eli finance sync` first.",
            csv_path.display()
        );
    }

    let query = query.trim();
    if query.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }
    let query_lower = query.to_ascii_lowercase();
    let terms: Vec<String> = query_lower
        .split_whitespace()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if terms.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }
    let term_patterns = compile_term_patterns(&terms);
    let phrase_pattern = {
        let q = query.trim();
        if q.is_empty() {
            None
        } else {
            regex::Regex::new(&format!(r"(?i)\b{}\b", regex::escape(q))).ok()
        }
    };

    let country_normalized = country.map(|c| c.trim().to_ascii_uppercase());
    if let Some(ref c) = country_normalized {
        if c != "US" {
            anyhow::bail!("unsupported --country '{c}' (v1 supports: US)");
        }
    }

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(&csv_path)
        .with_context(|| format!("open {}", csv_path.display()))?;

    let mut matches: Vec<serde_json::Value> = Vec::new();
    let mut loose_matches: Vec<serde_json::Value> = Vec::new();
    let mut semantic_query_expanded = false;
    let federal_reserve_policy_query = query_lower.contains("federal reserve");
    let fed_context_terms = [
        "fed",
        "federal reserve",
        "fomc",
        "fed funds",
        "interest rate",
    ];
    let policy_action_terms = [
        "rate", "cut", "cuts", "hike", "hikes", "hold", "decrease", "increase", "bps",
        "basis point", "decision", "meeting",
    ];
    for row in rdr.deserialize::<OddsCsvRow>() {
        let row = match row {
            Ok(r) => r,
            Err(_) => continue,
        };
        let searchable = format!(
            "{} {} {} {} {} {}",
            row.source, row.ticker, row.title, row.event_ticker, row.category, row.topic
        );
        let searchable_lower = searchable.to_ascii_lowercase();
        let mut matched_terms = compute_match_terms(&searchable, &term_patterns);
        let mut expanded_match = false;
        let has_fed_context = fed_context_terms.iter().any(|t| searchable_lower.contains(t));
        let has_policy_action = policy_action_terms.iter().any(|t| searchable_lower.contains(t));
        let policy_like = has_fed_context && has_policy_action;
        if matched_terms.is_empty() {
            if federal_reserve_policy_query && policy_like {
                matched_terms.push("fed_policy_expanded".to_string());
                semantic_query_expanded = true;
                expanded_match = true;
            } else {
                continue;
            }
        }
        if terms.len() >= 2 {
            let matched_unique: std::collections::BTreeSet<String> =
                matched_terms.iter().cloned().collect();
            if matched_unique.len() < 2
                && !has_phrase_match(&searchable, &phrase_pattern)
                && !expanded_match
            {
                continue;
            }
        }

        let country_hints = find_us_hints(&row);
        if country_normalized.as_deref() == Some("US") && country_hints.is_empty() {
            continue;
        }

        let vol_cents: f64 = row.volume.trim().parse().unwrap_or(0.0);
        let volume_usd = vol_cents / 100.0;
        if let Some(min_usd) = min_volume_usd {
            if volume_usd < min_usd {
                continue;
            }
        }

        let yes_price: f64 = row.yes_price.trim().parse().unwrap_or(0.0);
        let prob: f64 = row.probability.trim().parse().unwrap_or(0.0);
        let phrase_boost = if has_phrase_match(&searchable, &phrase_pattern) {
            7
        } else {
            0
        };
        let match_score =
            compute_match_score(&row, query, &matched_terms, volume_usd) + phrase_boost;
        let mut row_json = serde_json::json!({
            "source": row.source,
            "ticker": row.ticker,
            "title": row.title,
            "event_ticker": row.event_ticker,
            "yes_price": yes_price,
            "volume": vol_cents,
            "volume_usd": volume_usd,
            "status": row.status,
            "probability": prob,
            "category": row.category,
            "topic": row.topic,
            "match_score": match_score,
            "match_terms": matched_terms,
            "country_hints": country_hints,
        });
        if explain {
            let matched_terms_vec: Vec<String> = row_json["match_terms"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            row_json["explain"] = serde_json::json!({
                "reasons": explain_reasons(&row, query, &matched_terms_vec, volume_usd)
            });
        }
        if federal_reserve_policy_query && !policy_like {
            loose_matches.push(row_json);
            continue;
        }
        matches.push(row_json);
    }

    let mut semantic_filter_relaxed = false;
    if federal_reserve_policy_query && matches.is_empty() && !loose_matches.is_empty() {
        semantic_filter_relaxed = true;
        matches = loose_matches;
    }

    // Keep breadth by default; rank by relevance first, then liquidity.
    matches.sort_by(|a, b| {
        let sa = a["match_score"].as_i64().unwrap_or(0);
        let sb = b["match_score"].as_i64().unwrap_or(0);
        sb.cmp(&sa).then_with(|| {
            let va = a["volume_usd"].as_f64().unwrap_or(0.0);
            let vb = b["volume_usd"].as_f64().unwrap_or(0.0);
            vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
        })
    });

    let total_matches = matches.len();
    let final_limit = top.or(limit).unwrap_or(25);
    matches.truncate(final_limit);

    let resp = serde_json::json!({
        "query": query,
        "source": "local_csv_cache",
        "csv_path": csv_path.display().to_string(),
        "total_matches": total_matches,
        "returned_matches": matches.len(),
        "limit": final_limit,
        "country": country_normalized,
        "min_volume_usd": min_volume_usd,
        "top": top,
        "semantic_filter_relaxed": semantic_filter_relaxed,
        "semantic_query_expanded": semantic_query_expanded,
        "markets": matches,
    });

    let json = serde_json::to_string_pretty(&resp).context("serialize search results")?;
    println!("{json}");
    Ok(())
}

/// No CSV available — fall back to live API: search Kalshi events, then fetch markets
/// for matched events. Also queries Polymarket. Returns combined results.
async fn cmd_finance_odds_search_live_no_csv(
    query: &str,
    limit: Option<usize>,
    top: Option<usize>,
) -> Result<()> {
    let query = query.trim();
    if query.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }

    // Step 1: Search Kalshi events
    let kalshi_events_req = eli_core::finance::OddsRequest {
        provider: Some("kalshi".to_string()),
        disable_kalshi: false,
        series_ticker: None,
        event_ticker: None,
        market_ticker: None,
        status: Some("open".to_string()),
        limit: Some(200),
        cursor: None,
        max_pages: Some(3),
        include_orderbook: false,
        orderbook_depth: None,
        list_series: false,
        list_events: true,
        list_markets: false,
        list_tags: false,
        category: None,
        search: Some(query.to_string()),
    };

    // Step 2: Search Polymarket events
    let poly_events_req = eli_core::finance::OddsRequest {
        provider: Some("polymarket".to_string()),
        ..kalshi_events_req.clone()
    };

    let (kalshi_resp, poly_resp) = tokio::join!(
        eli_core::finance::fetch_odds(kalshi_events_req),
        eli_core::finance::fetch_odds(poly_events_req),
    );

    let mut all_events: Vec<serde_json::Value> = Vec::new();

    // Collect Kalshi events
    if let Ok(resp) = &kalshi_resp {
        if let Some(events) = &resp.available_events {
            for e in events {
                all_events.push(serde_json::json!({
                    "source": "kalshi",
                    "event_ticker": e.ticker,
                    "title": e.title,
                    "category": e.category,
                    "series_ticker": e.series_ticker,
                }));
            }
        }
    }

    // Collect Polymarket events
    if let Ok(resp) = &poly_resp {
        if let Some(events) = &resp.available_events {
            for e in events {
                all_events.push(serde_json::json!({
                    "source": "polymarket",
                    "event_ticker": e.ticker,
                    "title": e.title,
                    "category": e.category,
                    "slug": e.slug,
                }));
            }
        }
    }

    // Step 3: For the top events, fetch their markets with live prices
    let final_limit = top.or(limit).unwrap_or(10);
    all_events.truncate(final_limit.min(15)); // cap at 15 events to avoid rate limits

    let mut live_markets: Vec<serde_json::Value> = Vec::new();

    for event_json in &all_events {
        let source = event_json["source"].as_str().unwrap_or("");
        let event_ticker = event_json["event_ticker"].as_str().unwrap_or("");
        if event_ticker.is_empty() {
            continue;
        }

        let market_req = eli_core::finance::OddsRequest {
            provider: Some(source.to_string()),
            disable_kalshi: source == "polymarket",
            series_ticker: None,
            event_ticker: Some(event_ticker.to_string()),
            market_ticker: None,
            status: None,
            limit: None,
            cursor: None,
            max_pages: None,
            include_orderbook: false,
            orderbook_depth: None,
            list_series: false,
            list_events: false,
            list_markets: false,
            list_tags: false,
            category: None,
            search: None,
        };

        if let Ok(resp) = eli_core::finance::fetch_odds(market_req).await {
            for m in &resp.markets {
                live_markets.push(serde_json::json!({
                    "source": source,
                    "ticker": m.ticker,
                    "title": m.title,
                    "event_ticker": m.event_ticker,
                    "yes_price": m.yes_price,
                    "yes_bid": m.yes_bid,
                    "yes_ask": m.yes_ask,
                    "volume": m.volume,
                    "volume_usd": m.volume.map(|v| v as f64 / 100.0),
                    "status": m.status,
                    "probability": m.probability_yes,
                }));
            }
        }
    }

    let resp = serde_json::json!({
        "query": query,
        "source": "live_api",
        "note": "no local CSV cache; results fetched from live Kalshi + Polymarket APIs",
        "events_found": all_events.len(),
        "events": all_events,
        "markets": live_markets,
        "total_markets": live_markets.len(),
    });

    let json = serde_json::to_string_pretty(&resp).context("serialize live search results")?;
    println!("{json}");
    Ok(())
}

/// CSV exists + --live: search CSV for event tickers, then fetch fresh prices from API.
async fn cmd_finance_odds_search_live(
    query: &str,
    csv_path: &std::path::Path,
    limit: Option<usize>,
    country: Option<&str>,
    min_volume_usd: Option<f64>,
    top: Option<usize>,
    explain: bool,
) -> Result<()> {
    // First, run the normal CSV search to find matching events
    // We read the CSV and extract unique event_tickers + source
    let query_trimmed = query.trim();
    if query_trimmed.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }
    let query_lower = query_trimmed.to_ascii_lowercase();
    let terms: Vec<String> = query_lower
        .split_whitespace()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if terms.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }

    // Build regex patterns for matching
    let term_patterns: Vec<(String, regex::Regex)> = terms
        .iter()
        .filter_map(|t| {
            regex::Regex::new(&format!(r"(?i)\b{}\b", regex::escape(t)))
                .ok()
                .map(|re| (t.clone(), re))
        })
        .collect();

    #[derive(serde::Deserialize)]
    struct OddsCsvRow {
        source: String,
        ticker: String,
        title: String,
        event_ticker: String,
        yes_price: String,
        volume: String,
        status: String,
        probability: String,
        category: String,
        topic: String,
    }

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(csv_path)
        .with_context(|| format!("open {}", csv_path.display()))?;

    // Find unique event_tickers that match the query
    let mut event_map: std::collections::BTreeMap<String, (String, String, f64)> =
        std::collections::BTreeMap::new(); // event_ticker -> (source, category, total_volume)

    for row in rdr.deserialize::<OddsCsvRow>() {
        let row = match row {
            Ok(r) => r,
            Err(_) => continue,
        };
        let searchable = format!(
            "{} {} {} {} {} {}",
            row.source, row.ticker, row.title, row.event_ticker, row.category, row.topic
        );
        let matched: Vec<String> = term_patterns
            .iter()
            .filter_map(|(term, re)| re.is_match(&searchable).then_some(term.clone()))
            .collect();
        if matched.is_empty() {
            continue;
        }
        if terms.len() >= 2 {
            let unique: std::collections::BTreeSet<&String> = matched.iter().collect();
            if unique.len() < 2 {
                continue;
            }
        }

        // Apply country filter
        if let Some("US") = country {
            let text = format!(
                "{} {} {} {}",
                row.title, row.event_ticker, row.category, row.ticker
            )
            .to_ascii_lowercase();
            let us_terms = [
                "us", "u.s.", "united states", "american", "fomc", "federal reserve", "cpi", "pce",
            ];
            if !us_terms.iter().any(|t| text.contains(t)) {
                continue;
            }
        }

        let vol_cents: f64 = row.volume.trim().parse().unwrap_or(0.0);
        let volume_usd = vol_cents / 100.0;
        if let Some(min_usd) = min_volume_usd {
            if volume_usd < min_usd {
                continue;
            }
        }

        let entry = event_map
            .entry(row.event_ticker.clone())
            .or_insert_with(|| (row.source.clone(), row.category.clone(), 0.0));
        entry.2 += volume_usd;
    }

    if event_map.is_empty() {
        let resp = serde_json::json!({
            "query": query_trimmed,
            "source": "live_api_via_csv",
            "note": "no matching events found in CSV cache",
            "events_found": 0,
            "events": [],
            "markets": [],
            "total_markets": 0,
        });
        let json = serde_json::to_string_pretty(&resp).context("serialize")?;
        println!("{json}");
        return Ok(());
    }

    // Sort events by total volume, take top N
    let mut events_sorted: Vec<(String, String, String, f64)> = event_map
        .into_iter()
        .map(|(ticker, (source, cat, vol))| (ticker, source, cat, vol))
        .collect();
    events_sorted.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    let final_limit = top.or(limit).unwrap_or(10).min(15);
    events_sorted.truncate(final_limit);

    // Fetch fresh prices for each event
    let mut all_events: Vec<serde_json::Value> = Vec::new();
    let mut all_markets: Vec<serde_json::Value> = Vec::new();

    for (event_ticker, source, category, csv_volume) in &events_sorted {
        let market_req = eli_core::finance::OddsRequest {
            provider: Some(source.clone()),
            disable_kalshi: source == "polymarket",
            series_ticker: None,
            event_ticker: Some(event_ticker.clone()),
            market_ticker: None,
            status: None,
            limit: None,
            cursor: None,
            max_pages: None,
            include_orderbook: false,
            orderbook_depth: None,
            list_series: false,
            list_events: false,
            list_markets: false,
            list_tags: false,
            category: None,
            search: None,
        };

        match eli_core::finance::fetch_odds(market_req).await {
            Ok(resp) => {
                let event_title = resp
                    .events
                    .first()
                    .map(|e| e.title.clone())
                    .unwrap_or_default();
                let mut event_markets: Vec<serde_json::Value> = Vec::new();
                for m in &resp.markets {
                    let mkt_json = serde_json::json!({
                        "source": source,
                        "ticker": m.ticker,
                        "title": m.title,
                        "event_ticker": m.event_ticker,
                        "yes_price": m.yes_price,
                        "yes_bid": m.yes_bid,
                        "yes_ask": m.yes_ask,
                        "volume": m.volume,
                        "volume_usd": m.volume.map(|v| v as f64 / 100.0),
                        "status": m.status,
                        "probability": m.probability_yes,
                    });
                    all_markets.push(mkt_json.clone());
                    event_markets.push(mkt_json);
                }
                all_events.push(serde_json::json!({
                    "event_ticker": event_ticker,
                    "title": event_title,
                    "source": source,
                    "category": category,
                    "csv_volume_usd": csv_volume,
                    "markets": event_markets,
                }));
            }
            Err(e) => {
                all_events.push(serde_json::json!({
                    "event_ticker": event_ticker,
                    "source": source,
                    "category": category,
                    "csv_volume_usd": csv_volume,
                    "error": format!("{e}"),
                    "markets": [],
                }));
            }
        }
    }

    let resp = serde_json::json!({
        "query": query_trimmed,
        "source": "live_api_via_csv",
        "note": "CSV used for discovery, live API used for fresh prices",
        "events_found": all_events.len(),
        "events": all_events,
        "total_markets": all_markets.len(),
    });

    let json = serde_json::to_string_pretty(&resp).context("serialize live search results")?;
    println!("{json}");
    Ok(())
}

fn cmd_finance_odds_where(args: FinanceOddsWhereArgs) -> Result<()> {
    #[derive(Serialize)]
    struct OddsIdLanguageKalshi {
        /// Kalshi identifiers are human-readable tickers.
        ///
        /// - event_ticker: groups a set of related markets (e.g. `KXFED-26MAR`)
        /// - market_ticker: a specific market within the event (e.g. `KXFED-26MAR-T3.50`)
        event_ticker_example: String,
        market_ticker_example: String,
    }

    #[derive(Serialize)]
    struct OddsIdLanguagePolymarket {
        /// Polymarket markets have numeric IDs.
        ///
        /// - market_id: numeric market id (e.g. `609655`)
        /// - event_id: numeric event id (sometimes also exposed as a slug/ticker in search results)
        market_id_example: String,
        event_id_example: String,
        event_slug_example: String,
    }

    #[derive(Serialize)]
    struct OddsIdLanguage {
        kalshi: OddsIdLanguageKalshi,
        polymarket: OddsIdLanguagePolymarket,
    }

    #[derive(Serialize)]
    struct OddsWhereResponse {
        cache_dir: String,
        kalshi_csv_path: String,
        polymarket_csv_path: String,
        merged_csv_path: String,
        csv_schema: Vec<&'static str>,
        id_language: OddsIdLanguage,
    }

    let cache_dir = args.cache_dir.unwrap_or_else(|| {
        directories::ProjectDirs::from("", "", "eli")
            .map(|d| d.cache_dir().join("odds"))
            .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"))
    });

    let resp = OddsWhereResponse {
        cache_dir: cache_dir.display().to_string(),
        kalshi_csv_path: cache_dir.join("kalshi_markets.csv").display().to_string(),
        polymarket_csv_path: cache_dir
            .join("polymarket_markets.csv")
            .display()
            .to_string(),
        merged_csv_path: cache_dir.join("all_markets.csv").display().to_string(),
        csv_schema: vec![
            "source",
            "ticker",
            "title",
            "event_ticker",
            "yes_price",
            "volume",
            "status",
            "probability",
            "category",
            "topic",
        ],
        id_language: OddsIdLanguage {
            kalshi: OddsIdLanguageKalshi {
                event_ticker_example: "KXFED-26MAR".to_string(),
                market_ticker_example: "KXFED-26MAR-T3.50".to_string(),
            },
            polymarket: OddsIdLanguagePolymarket {
                market_id_example: "609655".to_string(),
                event_id_example: "48802".to_string(),
                event_slug_example: "us-recession-by-end-of-2026".to_string(),
            },
        },
    };

    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

async fn cmd_finance_options(args: FinanceOptionsArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    if args.summary && args.expirations {
        anyhow::bail!("use only one of --summary or --expirations");
    }

    let option_type = match args
        .option_type
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
    {
        None => None,
        Some(t) if t == "both" || t.is_empty() => None,
        Some(t) if t == "calls" || t == "puts" => Some(t),
        Some(other) => anyhow::bail!("invalid --type '{other}' (expected calls|puts|both)"),
    };

    let ticker_for_meta = args.ticker.clone();
    let req = eli_core::finance::OptionsRequest {
        ticker: args.ticker,
        expiry: args.expiry,
        option_type,
        near_money_pct: args.near_money,
        summary_only: args.summary,
        list_expirations: args.expirations,
        multi_expiry: false,
        num_expiries: None,
    };

    let resp = eli_core::finance::fetch_options(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch options")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.options",
            &[format!("ticker={ticker_for_meta}")],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_sync(args: FinanceSyncArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let sources = if args.sources.is_empty() {
        None
    } else {
        Some(args.sources)
    };

    let req = eli_core::finance::OddsSyncRequest {
        sources,
        cache_dir: args.cache_dir.map(|p| p.to_string_lossy().to_string()),
        max_pages: Some(args.max_pages),
        strict: args.strict,
    };

    let resp = eli_core::finance::sync_odds(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("sync prediction markets")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.sync",
            &[format!("max_pages={}", args.max_pages)],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_news(args: FinanceNewsArgs) -> Result<()> {
    let ticker_for_meta = args.ticker.clone();
    let req = eli_core::finance::NewsRequest {
        ticker: args.ticker,
        date: args.date,
    };

    let resp = eli_core::finance::fetch_news(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.news",
            &[format!("ticker={ticker_for_meta}")],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp)?;
    println!("{}", json);
    Ok(())
}

async fn cmd_finance_snapshot(args: FinanceSnapshotArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }
    let return_windows = parse_snapshot_return_windows(&args.returns)?;

    let mut tickers = args.tickers;
    if let Some(path) = args.tickers_file {
        let raw = std::fs::read_to_string(&path).context("read tickers_file")?;
        for line in raw.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            tickers.push(t.to_string());
        }
    }

    let provider = match args.provider.trim().to_ascii_lowercase().as_str() {
        "mock" => eli_core::finance::ProviderKind::Mock,
        "yahoo" => eli_core::finance::ProviderKind::Yahoo,
        other => anyhow::bail!("unsupported --provider '{other}' (supported: mock, yahoo)"),
    };

    let req = eli_core::finance::SnapshotRequest { tickers, provider };
    let mut resp = eli_core::finance::fetch_snapshot(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch snapshot")?;

    if !return_windows.is_empty()
        && matches!(resp.provider, eli_core::finance::ProviderKind::Yahoo)
        && !resp.tickers.is_empty()
    {
        let longest = return_windows
            .iter()
            .max_by_key(|(_, span)| span.approx_duration().num_seconds())
            .map(|(_, span)| *span)
            .unwrap_or(eli_core::finance::Span {
                n: 1,
                unit: eli_core::finance::SpanUnit::Month,
            });
        let fetch_range = padded_snapshot_return_fetch_range(longest);

        let cache_dir = default_finance_cache_dir()?;
        let ts_req = eli_core::finance::TimeseriesRequest {
            tickers: resp.tickers.clone(),
            range: fetch_range,
            granularity: eli_core::finance::Span {
                n: 1,
                unit: eli_core::finance::SpanUnit::Day,
            },
            as_of: Some(resp.generated_at),
            provider: eli_core::finance::ProviderKind::Yahoo,
            max_points_per_ticker: None,
        };
        if let Ok(ts_resp) = eli_core::finance::fetch_timeseries(ts_req, &cache_dir).await {
            let mut trailing: std::collections::BTreeMap<String, std::collections::BTreeMap<String, f64>> =
                std::collections::BTreeMap::new();
            for series in ts_resp.series {
                let Some(latest) = series.candles.last() else {
                    continue;
                };
                if latest.c <= 0.0 {
                    continue;
                }
                let mut per_period = std::collections::BTreeMap::new();
                for (label, span) in &return_windows {
                    let target = latest.t - span.approx_duration();
                    if let Some(anchor) = series.candles.iter().rev().find(|c| c.t <= target) {
                        if anchor.c > 0.0 {
                            per_period.insert(label.clone(), (latest.c / anchor.c) - 1.0);
                        }
                    }
                }
                if !per_period.is_empty() {
                    trailing.insert(series.ticker.clone(), per_period);
                }
            }
            if !trailing.is_empty() {
                resp.trailing_returns = Some(trailing);
            }
        }
    }

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.snapshot",
            &[format!("provider={}", args.provider)],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

fn padded_snapshot_return_fetch_range(longest: eli_core::finance::Span) -> eli_core::finance::Span {
    match longest.unit {
        eli_core::finance::SpanUnit::Year => eli_core::finance::Span {
            // Add one month of padding so 1y anchors can land on a prior trading day.
            n: longest.n.saturating_mul(12).saturating_add(1),
            unit: eli_core::finance::SpanUnit::Month,
        },
        eli_core::finance::SpanUnit::Month => eli_core::finance::Span {
            // One month of lookback padding is enough for month-based trailing windows.
            n: longest.n.saturating_add(1),
            unit: eli_core::finance::SpanUnit::Month,
        },
        _ => longest,
    }
}

fn parse_snapshot_return_windows(
    raw_windows: &[String],
) -> Result<Vec<(String, eli_core::finance::Span)>> {
    let mut out: Vec<(String, eli_core::finance::Span)> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for w in raw_windows {
        let normalized = w.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            continue;
        }
        if !seen.insert(normalized.clone()) {
            continue;
        }
        let span = match normalized.as_str() {
            "1mo" => eli_core::finance::Span {
                n: 1,
                unit: eli_core::finance::SpanUnit::Month,
            },
            "3mo" => eli_core::finance::Span {
                n: 3,
                unit: eli_core::finance::SpanUnit::Month,
            },
            "6mo" => eli_core::finance::Span {
                n: 6,
                unit: eli_core::finance::SpanUnit::Month,
            },
            "1y" => eli_core::finance::Span {
                n: 1,
                unit: eli_core::finance::SpanUnit::Year,
            },
            other => {
                anyhow::bail!(
                    "invalid --returns window '{other}' (supported: 1mo,3mo,6mo,1y)"
                )
            }
        };
        out.push((normalized, span));
    }
    Ok(out)
}

async fn cmd_finance_fundamentals(args: FinanceFundamentalsArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let ticker_for_meta = args.ticker.clone();
    let req = eli_core::finance::FundamentalsRequest {
        ticker: args.ticker,
    };
    let resp = eli_core::finance::fetch_fundamentals(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch fundamentals")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.fundamentals",
            &[format!("ticker={ticker_for_meta}")],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_search(args: FinanceSearchArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let query_for_meta = args.query.clone();
    let req = eli_core::finance::SearchRequest { query: args.query };
    let resp = eli_core::finance::fetch_search(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch search")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.search",
            &[format!("query={query_for_meta}")],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_filings(args: FinanceFilingsArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let cache_dir = if let Some(path) = args.cache_dir {
        path
    } else {
        let paths = Paths::discover().context("discover paths")?;
        paths.ensure_dirs().context("ensure dirs")?;
        paths.cache_dir
    };

    let paths = Paths::discover().ok();
    let config = if let Some(p) = paths {
        config::load_or_default(&p).ok()
    } else {
        None
    };

    let user_agent = config.and_then(|c| c.chat.sec_user_agent);

    let ticker_for_meta = args.ticker.clone();
    let req = eli_core::finance::FilingsRequest {
        ticker: args.ticker,
        forms: args.forms,
        limit: Some(args.limit),
        include_text: args.include_text,
        max_chars: args.max_chars,
        user_agent,
    };

    let resp = eli_core::finance::fetch_filings(req, &cache_dir)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch filings")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.filings",
            &[format!("ticker={ticker_for_meta}")],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_timeseries(args: FinanceTimeseriesArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let mut tickers = args.tickers;
    if let Some(path) = args.tickers_file {
        let raw = std::fs::read_to_string(&path).context("read tickers_file")?;
        for line in raw.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            tickers.push(t.to_string());
        }
    }

    let mut range = eli_core::finance::Span::parse(&args.range)
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse --range")?;
    let granularity = eli_core::finance::Span::parse(&args.granularity)
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse --granularity")?;

    let mut as_of = match args.as_of.as_ref() {
        Some(raw) => Some(
            eli_core::finance::parse_as_of(raw)
                .map_err(|e| anyhow::anyhow!(e))
                .context("parse --as-of")?,
        ),
        None => None,
    };

    match (args.start.as_deref(), args.end.as_deref()) {
        (Some(start_raw), Some(end_raw)) => {
            if args.as_of.is_some() {
                anyhow::bail!("--as-of cannot be combined with --start/--end");
            }
            let start_dt = parse_window_start(start_raw).context("parse --start")?;
            let end_dt = eli_core::finance::parse_as_of(end_raw)
                .map_err(|e| anyhow::anyhow!(e))
                .context("parse --end")?;
            if end_dt <= start_dt {
                anyhow::bail!("--end must be strictly after --start");
            }
            let delta = end_dt.signed_duration_since(start_dt);
            let minutes = ((delta.num_seconds() + 59) / 60).max(1);
            range = eli_core::finance::Span::parse(&format!("{minutes}min"))
                .map_err(|e| anyhow::anyhow!(e))
                .context("derive range from --start/--end")?;
            as_of = Some(end_dt);
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("--start and --end must be used together");
        }
        (None, None) => {}
    }

    let provider_str = args.provider.trim().to_ascii_lowercase();
    let is_auto = provider_str == "auto";
    let provider = match provider_str.as_str() {
        "auto" | "yahoo" => eli_core::finance::ProviderKind::Yahoo,
        "mock" => eli_core::finance::ProviderKind::Mock,
        "fred" => eli_core::finance::ProviderKind::Fred,
        other => anyhow::bail!("unsupported --provider '{other}' (supported: auto, mock, yahoo, fred)"),
    };

    let cache_dir = if let Some(path) = args.cache_dir {
        path
    } else {
        let paths = Paths::discover().context("discover paths")?;
        paths.ensure_dirs().context("ensure dirs")?;
        paths.cache_dir
    };

    let req = eli_core::finance::TimeseriesRequest {
        tickers: tickers.clone(),
        range,
        granularity,
        as_of,
        provider,
        max_points_per_ticker: args.max_points_per_ticker,
    };

    let mut resp = eli_core::finance::fetch_timeseries(req, &cache_dir)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch timeseries")?;

    // Auto-fallback: if in auto mode and Yahoo returned errors, retry failed tickers with FRED.
    // Also re-fetch valid Yahoo tickers individually so their data isn't lost (the core drops
    // all series when any ticker fails).
    let auto_fallback_needed = is_auto
        && (resp.series.is_empty()
            || resp.status.as_deref() == Some("error")
            || resp.errors.as_ref().map(|e| !e.is_empty()).unwrap_or(false));
    if auto_fallback_needed {
        let failed_tickers: Vec<String> = resp
            .errors
            .as_ref()
            .map(|errs| errs.iter().map(|e| e.ticker.clone()).collect())
            .unwrap_or_default();
        let valid_tickers: Vec<String> = resp
            .valid_tickers
            .clone()
            .unwrap_or_default();

        let mut merged_series = Vec::new();
        let mut remaining_errors = Vec::new();

        // Re-fetch valid Yahoo tickers (core dropped their data on partial failure)
        for t in &valid_tickers {
            let re_req = eli_core::finance::TimeseriesRequest {
                tickers: vec![t.clone()],
                range,
                granularity,
                as_of,
                provider: eli_core::finance::ProviderKind::Yahoo,
                max_points_per_ticker: args.max_points_per_ticker,
            };
            if let Ok(re_resp) = eli_core::finance::fetch_timeseries(re_req, &cache_dir).await {
                merged_series.extend(re_resp.series);
            }
        }

        // Retry failed tickers with FRED
        if !failed_tickers.is_empty() {
            let fred_req = eli_core::finance::TimeseriesRequest {
                tickers: failed_tickers.clone(),
                range,
                granularity,
                as_of,
                provider: eli_core::finance::ProviderKind::Fred,
                max_points_per_ticker: args.max_points_per_ticker,
            };
            match eli_core::finance::fetch_timeseries(fred_req, &cache_dir).await {
                Ok(fred_resp) => {
                    let fred_ok: std::collections::HashSet<String> =
                        fred_resp.series.iter().map(|s| s.ticker.clone()).collect();
                    merged_series.extend(fred_resp.series);
                    // Collect tickers that failed both Yahoo AND FRED
                    for t in &failed_tickers {
                        if !fred_ok.contains(t) {
                            remaining_errors.push(eli_core::finance::TimeseriesError {
                                ticker: t.clone(),
                                stage: Some("auto-fallback".to_string()),
                                message: format!("failed on both Yahoo and FRED"),
                            });
                        }
                    }
                }
                Err(_) => {
                    // FRED also failed entirely — keep original errors
                    if let Some(errs) = &resp.errors {
                        remaining_errors.extend(errs.iter().cloned());
                    }
                }
            }
        }

        if !merged_series.is_empty() {
            // If all data came from FRED (no Yahoo successes), label provider as fred;
            // if mixed, label as yahoo (primary) — the data speaks for itself.
            if valid_tickers.is_empty() {
                resp.provider = eli_core::finance::ProviderKind::Fred;
            }
            resp.series = merged_series;
            resp.status = if remaining_errors.is_empty() { None } else { Some("partial".to_string()) };
            resp.error = None;
            resp.errors = if remaining_errors.is_empty() { None } else { Some(remaining_errors) };
            resp.valid_tickers = None;
        }
    }

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.timeseries",
            &[
                format!("range={}", args.range),
                format!("granularity={}", args.granularity),
            ],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{},\"cache\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&resp.cache).unwrap_or_else(|_| "null".to_string())
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

fn parse_window_start(raw: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    let s = raw.trim();
    if s.is_empty() {
        anyhow::bail!("empty start value");
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&chrono::Utc));
    }
    let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("invalid start '{raw}' (use YYYY-MM-DD or RFC3339)"))?;
    let naive = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow::anyhow!("invalid start date '{raw}'"))?;
    Ok(chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
        naive,
        chrono::Utc,
    ))
}

async fn cmd_setup() -> Result<()> {
    use std::io::Write;
    let paths = Paths::discover().context("discover paths")?;
    paths.ensure_dirs().context("ensure config dirs")?;
    let mut cfg = config::load_or_default(&paths).context("load config")?;

    println!("=== Eli Setup ===\n");

    // Provider selection
    println!("Select provider:");
    println!("  1) anthropic  - Claude models (recommended)");
    println!("  2) openai     - GPT models");
    println!("  3) openrouter - Multiple providers via OpenRouter");
    println!("  4) ollama     - Local models (no API key needed)");
    print!("\nChoice [1-4]: ");
    std::io::stdout().flush().ok();

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("read provider choice")?;
    let provider = match input.trim() {
        "1" | "anthropic" => ProviderKind::Anthropic,
        "2" | "openai" => ProviderKind::OpenAI,
        "3" | "openrouter" => ProviderKind::OpenRouter,
        "4" | "ollama" => ProviderKind::Ollama,
        _ => {
            println!("Invalid choice, defaulting to anthropic");
            ProviderKind::Anthropic
        }
    };
    cfg.chat.provider = provider;

    // Model selection with smart defaults
    let default_model = match provider {
        ProviderKind::Anthropic => "claude-sonnet-4-20250514",
        ProviderKind::OpenAI => "gpt-4o",
        ProviderKind::OpenRouter => "mistralai/devstral-2512:free",
        ProviderKind::Ollama => "llama3.2",
        ProviderKind::Mock => "mock",
    };

    print!("\nModel [{}]: ", default_model);
    std::io::stdout().flush().ok();
    input.clear();
    std::io::stdin()
        .read_line(&mut input)
        .context("read model")?;
    let model = input.trim();
    cfg.chat.model = if model.is_empty() {
        default_model.to_string()
    } else {
        model.to_string()
    };

    // API key (skip for Ollama)
    if provider != ProviderKind::Ollama {
        print!("\nAPI Key: ");
        std::io::stdout().flush().ok();
        input.clear();
        std::io::stdin()
            .read_line(&mut input)
            .context("read api key")?;
        let key = input.trim().to_string();

        if !key.is_empty() {
            match provider {
                ProviderKind::Anthropic => cfg.chat.anthropic_api_key = Some(key),
                ProviderKind::OpenAI => cfg.chat.openai_api_key = Some(key),
                ProviderKind::OpenRouter => cfg.chat.openrouter_api_key = Some(key),
                _ => {} // Should not happen
            }
        }
    }

    // Save config
    config::save(&paths, &cfg).context("save config")?;

    println!("\n=== Configuration saved! ===");
    println!("Config file: {}", paths.config_file().display());
    println!("Provider: {}", cfg.chat.provider);
    println!("Model: {}", cfg.chat.model);
    println!("\nJust run 'eli' to start chatting!");

    Ok(())
}

async fn cmd_init() -> Result<()> {
    let paths = Paths::discover().context("discover paths")?;
    let cfg = config::load_or_create(&paths).context("load/create config")?;
    println!("Config file: {}", paths.config_file().display());
    println!(
        "{}",
        toml::to_string_pretty(&cfg).context("serialize config")?
    );
    Ok(())
}

async fn cmd_config(set: Option<String>, value: Option<String>) -> Result<()> {
    let paths = Paths::discover().context("discover paths")?;

    // If setting a value
    if let Some(key) = set {
        let val = value.unwrap_or_default();
        let mut cfg = config::load_or_create(&paths).context("load config")?;

        match key.to_lowercase().as_str() {
            "provider" => {
                cfg.chat.provider = val
                    .parse::<ProviderKind>()
                    .map_err(|e| anyhow::anyhow!(e))
                    .context("invalid provider")?;
                println!("Set provider = {}", cfg.chat.provider);
            }
            "model" => {
                cfg.chat.model = val.clone();
                println!("Set model = {}", val);
            }
            "mem_steps" | "memory" | "mem" => {
                cfg.chat.mem_steps = val.parse::<usize>().context("mem_steps must be a number")?;
                println!("Set mem_steps = {}", cfg.chat.mem_steps);
            }
            "key" | "api_key" | "apikey" => {
                match cfg.chat.provider {
                    ProviderKind::Anthropic => cfg.chat.anthropic_api_key = Some(val.clone()),
                    ProviderKind::OpenAI => cfg.chat.openai_api_key = Some(val.clone()),
                    ProviderKind::OpenRouter => cfg.chat.openrouter_api_key = Some(val.clone()),
                    _ => {} // Should not happen
                }
                println!("Set API key for {}", cfg.chat.provider);
            }
            "anthropic_key" | "anthropic_api_key" => {
                cfg.chat.anthropic_api_key = Some(val.clone());
                println!("Set anthropic_api_key");
            }
            "openai_key" | "openai_api_key" => {
                cfg.chat.openai_api_key = Some(val.clone());
                println!("Set openai_api_key");
            }
            "openrouter_key" | "openrouter_api_key" => {
                cfg.chat.openrouter_api_key = Some(val.clone());
                println!("Set openrouter_api_key");
            }
            "sec_user_agent" | "sec_ua" => {
                cfg.chat.sec_user_agent = Some(val.clone());
                println!("Set sec_user_agent = {}", val);
            }
            "compact" => {
                cfg.chat.compact = parse_bool(&val)?;
                println!("Set compact = {}", cfg.chat.compact);
            }
            "compact_trigger" => {
                cfg.chat.compact_trigger = Some(
                    val.parse::<usize>()
                        .context("compact_trigger must be a number")?,
                );
                println!(
                    "Set compact_trigger = {}",
                    cfg.chat.compact_trigger.unwrap_or(0)
                );
            }
            "compact_keep" => {
                cfg.chat.compact_keep = Some(
                    val.parse::<usize>()
                        .context("compact_keep must be a number")?,
                );
                println!("Set compact_keep = {}", cfg.chat.compact_keep.unwrap_or(0));
            }
            "summary_model" => {
                cfg.chat.summary_model = if val.trim().is_empty() {
                    None
                } else {
                    Some(val.clone())
                };
                println!(
                    "Set summary_model = {}",
                    cfg.chat
                        .summary_model
                        .clone()
                        .unwrap_or_else(|| "none".to_string())
                );
            }
            "parallel_commands" | "parallel_cmds" => {
                cfg.chat.parallel_commands = val
                    .parse::<u32>()
                    .context("parallel_commands must be a number")?;
                println!("Set parallel_commands = {}", cfg.chat.parallel_commands);
            }
            "parallel_subagents" | "parallel_agents" => {
                cfg.chat.parallel_subagents = val
                    .parse::<u32>()
                    .context("parallel_subagents must be a number")?;
                println!("Set parallel_subagents = {}", cfg.chat.parallel_subagents);
            }
            "scrollback_max_lines" | "scrollback" => {
                cfg.chat.scrollback_max_lines = val
                    .parse::<usize>()
                    .context("scrollback_max_lines must be a number")?;
                println!(
                    "Set scrollback_max_lines = {}",
                    cfg.chat.scrollback_max_lines
                );
            }
            other => {
                anyhow::bail!("Unknown config key: {}. Valid keys: provider, model, mem_steps, key, anthropic_key, openai_key, openrouter_key, sec_user_agent, compact, compact_trigger, compact_keep, summary_model, parallel_commands, parallel_subagents, scrollback_max_lines", other);
            }
        }

        config::save(&paths, &cfg).context("save config")?;
        return Ok(());
    }

    // Otherwise, print current config
    let cfg = config::load_or_default(&paths).context("load config")?;
    println!("Config file: {}", paths.config_file().display());
    println!(
        "{}",
        toml::to_string_pretty(&cfg).context("serialize config")?
    );
    Ok(())
}

fn build_tool_info(path: &[String]) -> ToolInfoResponse {
    use clap::{ArgAction, ValueHint};

    let mut cmd = Cli::command();
    let mut full_path = vec![cmd.get_name().to_string()];
    let mut missing: Option<String> = None;

    for seg in path {
        let next = cmd
            .get_subcommands()
            .find(|c| c.get_name() == seg.as_str())
            .cloned();
        if let Some(sub) = next {
            cmd = sub;
            full_path.push(seg.clone());
        } else {
            missing = Some(seg.clone());
            break;
        }
    }

    let args: Vec<ToolInfoArg> = cmd
        .get_arguments()
        .map(|arg| {
            let num_args = arg.get_num_args().map(|range| ToolInfoArgCount {
                min: range.min_values(),
                max: range.max_values(),
            });

            let value_names = arg
                .get_value_names()
                .map(|names| names.iter().map(|n| n.to_string()).collect::<Vec<_>>());

            let possible_values = arg
                .get_value_parser()
                .possible_values()
                .map(|vals| vals.map(|v| v.get_name().to_string()).collect::<Vec<_>>());

            let default_values = arg
                .get_default_values()
                .iter()
                .map(|v| v.to_string_lossy().to_string())
                .collect::<Vec<_>>();
            let default_values = if default_values.is_empty() {
                None
            } else {
                Some(default_values)
            };

            let action = arg.get_action();
            let mut value_type = if matches!(*action, ArgAction::SetTrue | ArgAction::SetFalse) {
                "bool".to_string()
            } else if matches!(*action, ArgAction::Count) {
                "count".to_string()
            } else if possible_values.is_some() {
                "enum".to_string()
            } else {
                "string".to_string()
            };

            let type_id = arg.get_value_parser().type_id();
            if value_type == "string" {
                if type_id == std::any::TypeId::of::<bool>() {
                    value_type = "bool".to_string();
                } else if type_id == std::any::TypeId::of::<std::path::PathBuf>() {
                    value_type = "path".to_string();
                } else if type_id == std::any::TypeId::of::<usize>()
                    || type_id == std::any::TypeId::of::<u64>()
                    || type_id == std::any::TypeId::of::<u32>()
                    || type_id == std::any::TypeId::of::<u16>()
                    || type_id == std::any::TypeId::of::<u8>()
                    || type_id == std::any::TypeId::of::<i64>()
                    || type_id == std::any::TypeId::of::<i32>()
                    || type_id == std::any::TypeId::of::<i16>()
                    || type_id == std::any::TypeId::of::<i8>()
                    || type_id == std::any::TypeId::of::<f64>()
                    || type_id == std::any::TypeId::of::<f32>()
                {
                    value_type = "number".to_string();
                }
            }

            if let ValueHint::FilePath | ValueHint::DirPath | ValueHint::ExecutablePath =
                arg.get_value_hint()
            {
                value_type = "path".to_string();
            }

            ToolInfoArg {
                name: arg.get_id().to_string(),
                long: arg.get_long().map(|s| s.to_string()),
                short: arg.get_short().map(|c| c.to_string()),
                help: arg.get_help().map(|s| s.to_string()),
                required: arg.is_required_set(),
                value_type,
                num_args,
                value_names,
                possible_values,
                default_values,
            }
        })
        .collect();

    let subcommands: Vec<ToolInfoSubcommand> = cmd
        .get_subcommands()
        .map(|sub| ToolInfoSubcommand {
            name: sub.get_name().to_string(),
            about: sub.get_about().map(|s| s.to_string()),
        })
        .collect();

    let (error, available_subcommands) = if let Some(missing) = missing {
        (
            Some(format!("unknown subcommand '{missing}'")),
            Some(subcommands.clone()),
        )
    } else {
        (None, None)
    };

    ToolInfoResponse {
        command: full_path.join(" "),
        about: cmd.get_about().map(|s| s.to_string()),
        args,
        subcommands,
        error,
        available_subcommands,
    }
}

fn cmd_tool_info(path: Vec<String>) -> Result<()> {
    let resp = build_tool_info(&path);

    let json = serde_json::to_string_pretty(&resp).context("serialize tool-info")?;
    println!("{json}");
    Ok(())
}

/// Run chat in TUI mode (alternate screen, no ghost issues)
async fn run_chat_tui(
    cfg: &mut ConfigFile,
    adapter: Arc<dyn LlmAdapter>,
    diff_engine: &DiffEngine,
    command_runner: &CommandRunner,
    store: &SessionStore,
    paths: &Paths,
    session_id: &str,
    project_root: &Path,
    memory: &mut eli_core::memory::Memory,
    undo_stack: &mut Vec<Vec<DiffResult>>,
) -> Result<()> {
    use chat_ui::{ChatTerminal, ChatUi, PromptMode as TuiPromptMode};
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    let mut ui = ChatUi::new();
    ui.prompt_mode = TuiPromptMode::Auto;
    ui.scrollback_max_lines = cfg.chat.scrollback_max_lines;
    let mut terminal = ChatTerminal::new().context("create TUI terminal")?;

    // Map TUI prompt mode to config
    cfg.chat.approvals = ApprovalMode::Auto;
    cfg.chat.auto_mode = AutoMode::Autonomous;
    let apply_tui_mode = |_mode: TuiPromptMode, cfg: &mut ConfigFile| {
        cfg.chat.approvals = ApprovalMode::Auto;
        cfg.chat.auto_mode = AutoMode::Autonomous;
    };

    let task_start = Instant::now();

    loop {
        // Update spinner and elapsed time
        ui.tick_spinner();
        ui.elapsed_secs = task_start.elapsed().as_secs();

        // Render
        terminal.draw(&mut ui)?;

        // Poll for events
        if let Some(event) = terminal.poll_event(Duration::from_millis(50))? {
            match event {
                Event::Paste(text) => {
                    ui.handle_paste(&text);
                    continue;
                }
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Press {
                        if key.code == KeyCode::Char('o')
                            && key.modifiers.contains(KeyModifiers::ALT)
                        {
                            let enabled = ui.toggle_tool_output();
                            ui.add_message(
                                "System",
                                if enabled {
                                    "Tool output: shown."
                                } else {
                                    "Tool output: hidden (commands still shown)."
                                },
                            );
                            continue;
                        }
                        if let Some(input) = ui.handle_key(key.code, key.modifiers) {
                            let trimmed = input.trim();

                            // Handle slash commands
                            if trimmed == "/exit" || trimmed == "/quit" {
                                break;
                            }
                            if trimmed == "/help" {
                                ui.add_message(
                                    "System",
                                    "Commands: /exit, /help, /model, /compact, /reset, /copy, /status, /output\n/copy [scope] [> file] - Copy session: all, last, user, tools, N, -data\n/output - Toggle full tool stdout/stderr\nKeys: Esc interrupt, ↑↓ history, PgUp/PgDn scroll, Opt+O output toggle",
                                );
                                continue;
                            }
                            if trimmed == "/output" {
                                let enabled = ui.toggle_tool_output();
                                ui.add_message(
                                    "System",
                                    if enabled {
                                        "Tool output: shown."
                                    } else {
                                        "Tool output: hidden (commands still shown)."
                                    },
                                );
                                continue;
                            }
                            if trimmed == "/model" || trimmed.starts_with("/model ") {
                                let model = trimmed.strip_prefix("/model").unwrap_or("").trim();
                                if model.is_empty() {
                                    ui.add_message("System", &format!("model: {}", cfg.chat.model));
                                } else {
                                    cfg.chat.model = model.to_string();
                                    ui.add_message(
                                        "System",
                                        &format!("(model: {})", cfg.chat.model),
                                    );
                                }
                                continue;
                            }
                            if trimmed == "/models" {
                                ui.add_message(
                                    "System",
                                    &format!("model: {}\nset with: /model <name>", cfg.chat.model),
                                );
                                continue;
                            }
                            if trimmed == "/compact" {
                                match compact_memory_now(adapter.clone(), &cfg.chat, memory).await {
                                    Ok(Some(compaction)) => {
                                        let note = format!(
                                            "memory_compaction: dropped {} messages\n{}",
                                            compaction.dropped, compaction.summary
                                        );
                                        let brain_entry = format!(
                                            "\n### {} (session {})\n{}\n",
                                            chrono::Utc::now().to_rfc3339(),
                                            session_id,
                                            note
                                        );
                                        if let Err(e) = append_eli_brain(project_root, &brain_entry)
                                        {
                                            ui.add_message(
                                                "System",
                                                &format!(
                                                    "(compacted, but failed to write brain: {e})"
                                                ),
                                            );
                                        } else {
                                            ui.add_message(
                                                "System",
                                                &format!(
                                                    "memory: compacted ({} msgs)",
                                                    compaction.dropped
                                                ),
                                            );
                                        }
                                        store
                                            .append(
                                                session_id,
                                                &SessionEvent {
                                                    ts: chrono::Utc::now(),
                                                    kind: EventKind::Note { content: note },
                                                },
                                            )
                                            .await
                                            .ok();
                                    }
                                    Ok(None) => ui.add_message("System", "(nothing to compact)"),
                                    Err(e) => {
                                        ui.add_message("Error", &format!("compact failed: {e}"))
                                    }
                                }
                                continue;
                            }
                            if trimmed == "/tip" {
                                ui.show_tips = !ui.show_tips;
                                ui.add_message(
                                    "System",
                                    if ui.show_tips {
                                        "Tips shown."
                                    } else {
                                        "Tips hidden."
                                    },
                                );
                                continue;
                            }
                            if trimmed == "/brain" || trimmed == "/debug" || trimmed == "/raw" {
                                // Can't switch rendering modes mid-session
                                ui.add_message("System", &format!(
                                    "Can't switch to {} mode mid-session. Exit and run: eli chat --display {}",
                                    trimmed.trim_start_matches('/'),
                                    trimmed.trim_start_matches('/')
                                ));
                                continue;
                            }
                            if trimmed == "/standard" {
                                ui.add_message("System", "Already in standard (TUI) mode.");
                                continue;
                            }
                            if trimmed == "/status" || trimmed == "/s" {
                                ui.add_message(
                                    "System",
                                    &format!(
                                        "Mode: AUTO | Tokens: {} | Time: {}s",
                                        ui.total_tokens, ui.elapsed_secs
                                    ),
                                );
                                continue;
                            }
                            if trimmed == "/copy" || trimmed.starts_with("/copy ") {
                                let args = trimmed.strip_prefix("/copy").unwrap_or("").trim();
                                let result = execute_copy_command(args, memory, project_root).await;
                                match result {
                                    Ok(msg) => ui.add_message("System", &msg),
                                    Err(e) => ui.add_message("Error", &format!("copy failed: {e}")),
                                }
                                continue;
                            }
                            if trimmed == "/clear" || trimmed == "/reset" || trimmed == "/new" {
                                ui.messages.clear();
                                ui.add_message("System", "Conversation cleared.");
                                // Reset memory by creating fresh one with same system prompt
                                *memory = eli_core::memory::Memory::new(cfg.chat.mem_steps);
                                memory.set_system(eli_core::contract::system_prompt());
                                ui.total_tokens = 0;
                                ui.clear_sources();
                                continue;
                            }
                            if trimmed.is_empty() {
                                continue;
                            }

                            // Regular input - run agent
                            apply_tui_mode(ui.prompt_mode, cfg);
                            ui.add_message("You", trimmed);
                            store
                                .append(
                                    session_id,
                                    &SessionEvent {
                                        ts: chrono::Utc::now(),
                                        kind: EventKind::UserMessage {
                                            content: trimmed.to_string(),
                                        },
                                    },
                                )
                                .await
                                .ok();
                            ui.is_processing = true;
                            ui.clear_sources();

                            // Render processing state
                            terminal.draw(&mut ui)?;

                            let (clean_prompt, images) = process_input_for_images(trimmed);

                            // Run the agent (single unified persona)
                            let result = run_agent_tui(
                                &cfg.chat,
                                adapter.clone(),
                                diff_engine,
                                command_runner,
                                store,
                                &paths.data_dir,
                                session_id,
                                project_root,
                                memory,
                                undo_stack,
                                &mut ui,
                                &mut terminal,
                                AgentProfile::Coding,
                                clean_prompt,
                                images,
                            )
                            .await;

                            ui.is_processing = false;

                            if let Err(e) = result {
                                let msg = format!("{:?}", e);
                                ui.add_message("Error", &msg);
                                store
                                    .append(
                                        session_id,
                                        &SessionEvent {
                                            ts: chrono::Utc::now(),
                                            kind: EventKind::Note { content: msg },
                                        },
                                    )
                                    .await
                                    .ok();
                            }

                            while let Some(queued) = ui.pop_queued() {
                                let trimmed = queued.trim();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                apply_tui_mode(ui.prompt_mode, cfg);
                                ui.add_message("You", trimmed);
                                store
                                    .append(
                                        session_id,
                                        &SessionEvent {
                                            ts: chrono::Utc::now(),
                                            kind: EventKind::UserMessage {
                                                content: trimmed.to_string(),
                                            },
                                        },
                                    )
                                    .await
                                    .ok();
                                ui.is_processing = true;
                                ui.clear_sources();
                                terminal.draw(&mut ui)?;

                                let (clean_prompt, images) = process_input_for_images(trimmed);
                                let queued_result = run_agent_tui(
                                    &cfg.chat,
                                    adapter.clone(),
                                    diff_engine,
                                    command_runner,
                                    store,
                                    &paths.data_dir,
                                    session_id,
                                    project_root,
                                    memory,
                                    undo_stack,
                                    &mut ui,
                                    &mut terminal,
                                    AgentProfile::Coding,
                                    clean_prompt,
                                    images,
                                )
                                .await;

                                ui.is_processing = false;
                                if let Err(e) = queued_result {
                                    let msg = format!("{:?}", e);
                                    ui.add_message("Error", &msg);
                                    store
                                        .append(
                                            session_id,
                                            &SessionEvent {
                                                ts: chrono::Utc::now(),
                                                kind: EventKind::Note { content: msg },
                                            },
                                        )
                                        .await
                                        .ok();
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if ui.should_quit {
            break;
        }
    }

    Ok(())
}

/// Run agent steps with TUI output
async fn run_agent_tui(
    chat: &eli_core::config::ChatConfig,
    adapter: Arc<dyn LlmAdapter>,
    _diff_engine: &DiffEngine,
    command_runner: &CommandRunner,
    store: &SessionStore,
    _data_dir: &Path,
    session_id: &str,
    _project_root: &Path,
    memory: &mut eli_core::memory::Memory,
    _undo_stack: &mut Vec<Vec<DiffResult>>,
    ui: &mut chat_ui::ChatUi,
    terminal: &mut chat_ui::ChatTerminal,
    _profile: AgentProfile,
    initial_message: String,
    images: Vec<String>,
) -> Result<()> {
    use crossterm::event as ct_event;
    use crossterm::event::{
        Event as CtEvent, KeyCode as CtKeyCode, KeyEventKind as CtKeyEventKind,
        KeyModifiers as CtKeyModifiers,
    };
    use eli_core::types::ChatStreamEvent;
    use futures::StreamExt;

    let max_iters = if chat.auto { chat.max_auto.max(1) } else { 1 };
    let mut current_message = initial_message.clone();
    let mut current_images = images;
    let mut invalid_format_retries: u8 = 0;

    for step in 1..=max_iters {
        // Update UI
        ui.tick_spinner();
        terminal.draw(ui)?;

        // Add message to memory
        if !current_images.is_empty() {
            memory.push(ChatMessage::user_with_images(
                current_message.clone(),
                current_images.clone(),
            ));
            current_images.clear();
        } else {
            memory.push(ChatMessage::user(current_message.clone()));
        }

        // Build request
        let req = ChatRequest {
            messages: memory.context(),
            model: chat.model.clone(),
            max_tokens: chat.max_tokens,
            temperature: chat.temperature,
            response_format: None,
            stream: true,
        };

        // Stream response (spinner in title shows we're working)
        terminal.draw(ui)?;

        let mut stream = adapter.chat_stream(req).await.context("start stream")?;
        let mut full_response = String::new();
        let mut interrupted = false;

        let check_interrupt = |ui: &mut chat_ui::ChatUi| -> bool {
            if ui.interrupt_requested {
                ui.interrupt_requested = false;
                return true;
            }
            while ct_event::poll(Duration::from_millis(0)).unwrap_or(false) {
                let Ok(ev) = ct_event::read() else {
                    continue;
                };
                match ev {
                    CtEvent::Key(key) => {
                        if key.kind != CtKeyEventKind::Press {
                            continue;
                        }
                        if key.code == CtKeyCode::Esc {
                            return true;
                        }
                        if key.code == CtKeyCode::Char('o')
                            && key.modifiers.contains(CtKeyModifiers::ALT)
                        {
                            let enabled = ui.toggle_tool_output();
                            ui.add_message(
                                "System",
                                if enabled {
                                    "Tool output: shown."
                                } else {
                                    "Tool output: hidden (commands still shown)."
                                },
                            );
                            continue;
                        }
                        if let Some(input) = ui.handle_key(key.code, key.modifiers) {
                            let trimmed = input.trim();
                            if trimmed.eq_ignore_ascii_case("/exit")
                                || trimmed.eq_ignore_ascii_case("/quit")
                            {
                                ui.should_quit = true;
                                return true;
                            }
                            if !trimmed.is_empty() {
                                ui.queue_prompt(trimmed.to_string());
                            }
                        }
                    }
                    CtEvent::Paste(text) => {
                        ui.handle_paste(&text);
                    }
                    _ => {}
                }
            }
            false
        };

        loop {
            tokio::select! {
                maybe_ev = stream.next() => {
                    match maybe_ev {
                        Some(Ok(ChatStreamEvent::Delta(text))) => {
                            full_response.push_str(&text);
                        }
                        Some(Ok(ChatStreamEvent::Usage(usage))) => {
                            ui.total_tokens = ui.total_tokens.saturating_add(usage.total_tokens);
                        }
                        Some(Ok(ChatStreamEvent::Done)) => break,
                        Some(Err(e)) => {
                            let msg = format!("Stream error: {:?}", e);
                            ui.add_message("Error", &msg);
                            store
                                .append(
                                    session_id,
                                    &SessionEvent {
                                        ts: chrono::Utc::now(),
                                        kind: EventKind::Note { content: msg },
                                    },
                                )
                                .await
                                .ok();
                            break;
                        }
                        None => break,
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }

            if check_interrupt(ui) {
                interrupted = true;
                break;
            }

            // Update UI periodically
            ui.tick_spinner();
            terminal.draw(ui)?;
        }

        if interrupted {
            ui.add_message("System", "(interrupted)");
            store
                .append(
                    session_id,
                    &SessionEvent {
                        ts: chrono::Utc::now(),
                        kind: EventKind::Note {
                            content: "(interrupted)".to_string(),
                        },
                    },
                )
                .await
                .ok();
            return Ok(());
        }

        // Parse response
        let model = match contract::validate_model_response(&full_response) {
            Ok(m) => {
                invalid_format_retries = 0;
                m
            }
            Err(e) => {
                let msg = format!("Invalid response: {}", e);
                ui.add_message("Error", &msg);
                store
                    .append(
                        session_id,
                        &SessionEvent {
                            ts: chrono::Utc::now(),
                            kind: EventKind::Note { content: msg },
                        },
                    )
                    .await
                    .ok();
                invalid_format_retries = invalid_format_retries.saturating_add(1);
                if invalid_format_retries >= 3 {
                    ui.add_message(
                        "Error",
                        &format!(
                            "Too many invalid JSON responses ({}). Stopping this run.",
                            invalid_format_retries
                        ),
                    );
                    break;
                }
                let format_error = format!(
                    "FORMAT ERROR: Your previous response was invalid ({e}). Return ONLY one strict JSON object matching the Eli contract. No prose, no markdown, no <tool_call> tags."
                );
                memory.push(ChatMessage::tool(format_error.clone(), "eli.format"));
                store
                    .append(
                        session_id,
                        &SessionEvent {
                            ts: chrono::Utc::now(),
                            kind: EventKind::Note {
                                content: format_error.clone(),
                            },
                        },
                    )
                    .await
                    .ok();
                current_message = format_error;
                continue;
            }
        };
        let canonical = serde_json::to_string_pretty(&model).unwrap_or(full_response.clone());

        // Add response to memory
        memory.push(ChatMessage::assistant(canonical.clone()));
        store
            .append(
                session_id,
                &SessionEvent {
                    ts: chrono::Utc::now(),
                    kind: EventKind::AssistantMessage { content: canonical },
                },
            )
            .await
            .ok();

        // Show progress while working; only show final answer on DONE.
        let display_text: Option<String> = match model.status {
            StepStatus::Done => {
                if let Some(synthesis) = &model.synthesis {
                    if !synthesis.answer.trim().is_empty() {
                        Some(synthesis.answer.trim().to_string())
                    } else if !model.notes.trim().is_empty() {
                        Some(model.notes.trim().to_string())
                    } else {
                        None
                    }
                } else if !model.notes.trim().is_empty() {
                    Some(model.notes.trim().to_string())
                } else {
                    None
                }
            }
            StepStatus::KeepWorking => {
                let summary = model
                    .synthesis
                    .as_ref()
                    .map(|s| {
                        s.summary
                            .iter()
                            .map(|x| x.trim())
                            .filter(|x| !x.is_empty())
                            .take(3)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    })
                    .filter(|s| !s.trim().is_empty());
                summary.or_else(|| {
                    if !model.notes.trim().is_empty() {
                        Some(model.notes.trim().to_string())
                    } else {
                        None
                    }
                })
            }
        };
        if matches!(model.status, StepStatus::Done) {
            ui.clear_tool_messages();
        }
        if let Some(text) = &display_text {
            let role = if matches!(model.status, StepStatus::KeepWorking) {
                "Progress"
            } else {
                "Eli"
            };
            ui.add_message(role, text);
        }

        // Execute commands if any
        if !model.commands.is_empty() && !matches!(chat.mode, RunMode::Read) {
            let mut command_results: Vec<CommandResult> = Vec::new();

            for cmd in &model.commands {
                store
                    .append(
                        session_id,
                        &SessionEvent {
                            ts: chrono::Utc::now(),
                            kind: EventKind::Note {
                                content: format!("$ {}", cmd),
                            },
                        },
                    )
                    .await
                    .ok();
                ui.last_tool_ok = None;
                ui.last_tool_summary = Some(format!("$ {}", truncate_line(cmd, 110)));
                ui.add_message("Tool", &format!("$ {}", cmd));
                terminal.draw(ui)?;

                let results = command_runner.run_commands(&[cmd.clone()]).await;

                for r in &results {
                    let line = tui_tool_status_summary(r);
                    ui.last_tool_ok = Some(r.returncode == 0);
                    ui.last_tool_summary = Some(line.clone());
                    if ui.show_tool_output {
                        ui.add_message("Tool", &format_tui_tool_output(r));
                    }
                    store
                        .append(
                            session_id,
                            &SessionEvent {
                                ts: chrono::Utc::now(),
                                kind: EventKind::Note { content: line },
                            },
                        )
                        .await
                        .ok();

                    // Infer sources (never invent a generic "eli finance" source)
                    for source in infer_sources(cmd, &r.stdout) {
                        ui.add_source(source);
                    }
                }
                command_results.extend(results);
                terminal.draw(ui)?;
            }

            if !command_results.is_empty() {
                command_results = augment_tool_errors(command_results);
                let command_results_for_llm =
                    shadow_large_tool_outputs(_project_root, session_id, step, &command_results);
                let observation =
                    build_observation(false, false, false, &[], &command_results_for_llm);
                memory.push(ChatMessage::tool(observation.clone(), "eli"));
                store
                    .append(
                        session_id,
                        &SessionEvent {
                            ts: chrono::Utc::now(),
                            kind: EventKind::Note {
                                content: observation,
                            },
                        },
                    )
                    .await
                    .ok();
            }
        }

        // Check if done
        if matches!(model.status, StepStatus::Done) {
            ui.last_tool_ok = None;
            ui.last_tool_summary = None;
            ui.clear_tool_messages();
            break;
        }

        // Continue with KEEP WORKING
        current_message = "KEEP WORKING".to_string();
    }

    Ok(())
}

fn tui_tool_status_summary(result: &CommandResult) -> String {
    let cmd = truncate_line(result.command.trim(), 68);
    if result.returncode == 0 {
        let digest = truncate_line(&build_command_digest(result), 120);
        return format!("{cmd} · {digest}");
    }

    let mut reason = result
        .stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| format!("returncode={}", result.returncode));
    reason = truncate_line(&reason, 120);
    format!("{cmd} · {reason}")
}

fn format_tui_tool_output(result: &CommandResult) -> String {
    let mut out = String::new();
    out.push_str(&format!("exit={}\n", result.returncode));

    if !result.stdout.trim().is_empty() {
        out.push_str("stdout:\n");
        out.push_str(&result.stdout);
        if !result.stdout.ends_with('\n') {
            out.push('\n');
        }
    }

    if !result.stderr.trim().is_empty() {
        out.push_str("stderr:\n");
        out.push_str(&result.stderr);
        if !result.stderr.ends_with('\n') {
            out.push('\n');
        }
    }

    if out.trim().is_empty() {
        out.push_str("(no output)");
    }

    out
}

async fn cmd_chat(
    provider: Option<String>,
    model: Option<String>,
    display_override: Option<DisplayMode>,
) -> Result<()> {
    let paths = Paths::discover().context("discover paths")?;
    let mut cfg = config::load_or_create(&paths).context("load/create config")?;
    apply_overrides(&mut cfg, provider, model)?;
    ensure_tui_default_model(&mut cfg.chat);
    cfg.chat.approvals = ApprovalMode::Auto;
    cfg.chat.approvals_commands = None;
    cfg.chat.approvals_diffs = None;
    cfg.chat.auto_mode = AutoMode::Autonomous;
    if let Some(mode) = display_override {
        cfg.chat.display_mode = mode;
    }

    let adapter = eli_adapters::build_from_chat_config(&cfg.chat).context("build adapter")?;
    let mut adapter: Arc<dyn LlmAdapter> = Arc::from(adapter);

    let cwd = std::env::current_dir().context("get cwd")?;
    let project_root = cfg
        .chat
        .resolved_project_root(&cwd)
        .map_err(|e| anyhow::anyhow!(e))
        .context("resolve project root")?;

    let diff_engine = DiffEngine::new(project_root.clone()).context("init diff engine")?;
    let command_runner = CommandRunner::new(
        cfg.chat.timeout_secs,
        cfg.chat.max_cmds,
        cfg.chat.parallel_commands,
        project_root.clone(),
    );

    let store = SessionStore::new(&paths);
    let session_id = uuid::Uuid::new_v4().to_string();
    info!(session_id = %session_id, provider = %cfg.chat.provider, model = %cfg.chat.model, "starting chat");

    let rl_config = Config::builder()
        .completion_type(CompletionType::Circular)
        .build();
    let shared_input_tokens = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut editor: Editor<SlashHelper, DefaultHistory> =
        Editor::with_config(rl_config).context("init readline")?;
    editor.set_helper(Some(SlashHelper {
        last_input_tokens: shared_input_tokens.clone(),
    }));
    let slash_menu = SlashMenu::new();
    editor.bind_sequence(
        KeyEvent::from('/'),
        EventHandler::Conditional(Box::new(SlashMenuHandler::new(slash_menu.clone()))),
    );
    editor.bind_sequence(
        KeyEvent(KeyCode::Down, Modifiers::NONE),
        EventHandler::Conditional(Box::new(SlashNavHandler::new(
            slash_menu.clone(),
            SlashNav::Next,
        ))),
    );
    editor.bind_sequence(
        KeyEvent(KeyCode::Up, Modifiers::NONE),
        EventHandler::Conditional(Box::new(SlashNavHandler::new(
            slash_menu.clone(),
            SlashNav::Prev,
        ))),
    );
    let mut memory = eli_core::memory::Memory::new(cfg.chat.mem_steps);
    memory.set_system(eli_core::contract::system_prompt());
    ensure_eli_research_brain(&project_root).context("ensure eli_research/ELI.md")?;
    let mut undo_stack: Vec<Vec<DiffResult>> = Vec::new();
    let mut state = SessionState::new(&cfg.chat);
    state.load_recent_research(&project_root, 12);
    let force_plain_prompt = matches!(cfg.chat.display_mode, DisplayMode::Debug);

    // Use TUI mode for Standard display mode (alternate screen, no ghost issues)
    if matches!(cfg.chat.display_mode, DisplayMode::Standard) {
        return run_chat_tui(
            &mut cfg,
            adapter,
            &diff_engine,
            &command_runner,
            &store,
            &paths,
            &session_id,
            &project_root,
            &mut memory,
            &mut undo_stack,
        )
        .await;
    }

    // Non-TUI modes (Brain/Debug/Raw) use the old CLI approach
    if has_interactive_terminal() {
        print_banner(&cfg.chat, &project_root, &state);
    }

    loop {
        // Show queue status if there are queued prompts
        let queue_len = state.prompt_queue.len();

        // Update token hint for the upcoming prompt
        if let Some(usage) = &state.last_usage {
            shared_input_tokens.store(
                usage.prompt_tokens as usize,
                std::sync::atomic::Ordering::Relaxed,
            );
        }

        let (line, from_boxed_prompt) = if let Some(queued) = state.next_prompt() {
            print_history_line(format!("{}›{} {}", style::CYAN, style::RESET, queued));
            (queued, false)
        } else if matches!(state.display_mode, DisplayMode::Standard) && !force_plain_prompt {
            let Some(line) =
                read_line_boxed(&mut state, &mut cfg.chat, queue_len).context("boxed prompt")?
            else {
                break;
            };
            (line, true)
        } else {
            let prompt_prefix = if force_plain_prompt {
                "› ".to_string()
            } else if queue_len > 0 {
                format!("[{}Q] › ", queue_len)
            } else {
                "› ".to_string()
            };

            slash_menu.reset();

            let res = editor.readline_with_initial(&prompt_prefix, (&state.input_buffer, ""));
            state.input_buffer.clear();

            let line = match res {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => {
                    println!();
                    continue;
                }
                Err(ReadlineError::Eof) => break,
                Err(e) => return Err(e).context("readline failed"),
            };
            (line, false)
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if from_boxed_prompt {
            print_history_line(format!("{}›{} {}", style::CYAN, style::RESET, trimmed));
        }
        state.prompt_history.push(trimmed.to_string());
        editor.add_history_entry(trimmed).ok();

        // Slash commands
        if trimmed == "/exit" || trimmed == "/quit" {
            break;
        }
        if trimmed == "/queue" || trimmed == "/q" {
            if state.prompt_queue.is_empty() {
                println!("(queue empty)");
            } else {
                println!("Queue:");
                for (i, p) in state.prompt_queue.iter().enumerate() {
                    println!("  {}. {}", i + 1, p);
                }
            }
            continue;
        }
        if trimmed.starts_with("/q ") || trimmed.starts_with("/queue ") {
            let rest = trimmed.splitn(2, ' ').nth(1).unwrap_or("");
            if !rest.is_empty() {
                state.queue_prompt(rest.to_string());
                println!("(added to queue: position {})", state.queue_len());
            }
            continue;
        }
        if trimmed == "/clear-queue" || trimmed == "/cq" {
            state.prompt_queue.clear();
            println!("(queue cleared)");
            continue;
        }
        if trimmed == "/compact" {
            match compact_memory_now(adapter.clone(), &cfg.chat, &mut memory).await {
                Ok(Some(compaction)) => {
                    let note = format!(
                        "memory_compaction: dropped {} messages\n{}",
                        compaction.dropped, compaction.summary
                    );
                    let brain_entry = format!(
                        "\n### {} (session {})\n{}\n",
                        chrono::Utc::now().to_rfc3339(),
                        session_id,
                        note
                    );
                    if let Err(e) = append_eli_brain(&project_root, &brain_entry) {
                        println!("(compacted, but failed to write brain: {e})");
                    } else {
                        println!("memory: compacted ({} msgs)", compaction.dropped);
                    }
                    store
                        .append(
                            &session_id,
                            &SessionEvent {
                                ts: chrono::Utc::now(),
                                kind: EventKind::Note { content: note },
                            },
                        )
                        .await
                        .ok();
                }
                Ok(None) => println!("(nothing to compact)"),
                Err(e) => println!("(compact failed: {e})"),
            }
            continue;
        }
        if trimmed == "/tip" {
            println!("(tips are only shown in standard TUI mode)");
            continue;
        }
        if trimmed == "/reset" || trimmed == "/new" {
            memory = eli_core::memory::Memory::new(cfg.chat.mem_steps);
            memory.set_system(eli_core::contract::system_prompt());
            ensure_eli_research_brain(&project_root).ok();
            state.total_work_time = Duration::ZERO;
            state.step_count = 0;
            state.total_usage = eli_core::types::Usage::default();
            state.last_usage = None;
            println!("(reset)");
            continue;
        }
        if trimmed == "/brain" {
            state.display_mode = DisplayMode::Brain;
            println!("(brain mode: full output)");
            continue;
        }
        if trimmed == "/debug" {
            state.display_mode = DisplayMode::Debug;
            println!("(debug mode: raw request/response + tool output + observation)");
            continue;
        }
        if trimmed == "/standard" || trimmed == "/brief" {
            state.display_mode = DisplayMode::Standard;
            println!("(standard mode: brief output)");
            continue;
        }
        if trimmed == "/read" {
            cfg.chat.mode = RunMode::Read;
            println!("(exec mode: read)");
            continue;
        }
        if trimmed == "/work" {
            cfg.chat.mode = RunMode::Work;
            println!("(exec mode: work)");
            continue;
        }
        if trimmed == "/bot" {
            cfg.chat.mode = RunMode::Work;
            cfg.chat.approvals = ApprovalMode::Auto;
            cfg.chat.approvals_commands = Some(ApprovalMode::Auto);
            cfg.chat.approvals_diffs = Some(ApprovalMode::Ask);
            println!(
                "(bot: exec=work, approvals={})",
                format_approvals_display(&cfg.chat)
            );
            continue;
        }
        if trimmed == "/yolo" {
            cfg.chat.mode = RunMode::Work;
            cfg.chat.approvals = ApprovalMode::Auto;
            cfg.chat.approvals_commands = None;
            cfg.chat.approvals_diffs = None;
            println!(
                "(yolo: exec=work, approvals={})",
                format_approvals_display(&cfg.chat)
            );
            continue;
        }
        if trimmed == "/mode" || trimmed.starts_with("/mode ") {
            let mode = trimmed
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_ascii_lowercase();
            if mode.is_empty() {
                println!("exec mode: {}", format_mode(cfg.chat.mode));
            } else if mode == "read" {
                cfg.chat.mode = RunMode::Read;
                println!("(exec mode: read)");
            } else if mode == "work" {
                cfg.chat.mode = RunMode::Work;
                println!("(exec mode: work)");
            } else {
                println!("(mode must be read or work)");
            }
            continue;
        }
        if trimmed == "/model" || trimmed.starts_with("/model ") {
            let model = trimmed.strip_prefix("/model").unwrap_or("").trim();
            if model.is_empty() {
                print_history_block(vec![format!("model: {}", cfg.chat.model)]);
            } else {
                cfg.chat.model = model.to_string();
                print_history_block(vec![format!("(model: {})", cfg.chat.model)]);
            }
            continue;
        }
        if trimmed == "/models" {
            print_history_block(vec![
                format!("model: {}", cfg.chat.model),
                "set with: /model <name>".to_string(),
            ]);
            continue;
        }
        if trimmed == "/key" || trimmed.starts_with("/key ") {
            let key = trimmed.strip_prefix("/key").unwrap_or("").trim();
            if key.is_empty() {
                println!("usage: /key <api-key>");
                continue;
            }
            match cfg.chat.provider {
                ProviderKind::Anthropic => cfg.chat.anthropic_api_key = Some(key.to_string()),
                ProviderKind::OpenAI => cfg.chat.openai_api_key = Some(key.to_string()),
                ProviderKind::OpenRouter => cfg.chat.openrouter_api_key = Some(key.to_string()),
                ProviderKind::Ollama | ProviderKind::Mock => {
                    println!("(no API key needed for {})", cfg.chat.provider);
                    continue;
                }
            }
            adapter = Arc::from(
                eli_adapters::build_from_chat_config(&cfg.chat).context("build adapter")?,
            );
            println!("(api key set for {} - session only)", cfg.chat.provider);
            continue;
        }
        if trimmed == "/status" || trimmed == "/s" {
            print_mode_status(&state, &cfg.chat);
            print_cost_stats(&state, &cfg.chat);
            continue;
        }
        if trimmed == "/$" {
            print_cost_stats(&state, &cfg.chat);
            continue;
        }
        if trimmed == "/help" || trimmed == "/?" {
            print_help();
            continue;
        }
        if trimmed == "/undo" {
            perform_undo(&mut undo_stack, &mut memory, &store, &session_id).await?;
            continue;
        }

        // Queue prompt with + prefix (e.g., "+fix the bug" queues it)
        if trimmed.starts_with('+') {
            let queued = trimmed[1..].trim().to_string();
            if !queued.is_empty() {
                state.queue_prompt(queued);
                println!("(queued, {} in queue)", state.queue_len());
            }
            continue;
        }

        // Process images
        let (clean_prompt, images) = process_input_for_images(trimmed);
        // Run agent for this prompt (single unified persona)
        run_agent_steps(
            &cfg.chat,
            adapter.clone(),
            &diff_engine,
            &command_runner,
            &store,
            &paths.data_dir,
            &session_id,
            &project_root,
            &mut memory,
            &mut undo_stack,
            &mut state,
            AgentProfile::Coding,
            clean_prompt,
            images,
        )
        .await?;

        // Process queue automatically
        while let Some(queued_prompt) = state.next_prompt() {
            print_history_line(format!(
                "{}›{} {}",
                style::CYAN,
                style::RESET,
                queued_prompt
            ));
            // Queue currently supports text only (no image dragging into queue command yet,
            // though one could theoretically type the path, but process_input_for_images handles paths in string)
            let (q_clean, q_images) = process_input_for_images(&queued_prompt);

            run_agent_steps(
                &cfg.chat,
                adapter.clone(),
                &diff_engine,
                &command_runner,
                &store,
                &paths.data_dir,
                &session_id,
                &project_root,
                &mut memory,
                &mut undo_stack,
                &mut state,
                AgentProfile::Coding,
                q_clean,
                q_images,
            )
            .await?;
        }
    }

    Ok(())
}

fn read_line_boxed(
    state: &mut SessionState,
    chat: &mut eli_core::config::ChatConfig,
    queue_len: usize,
) -> Result<Option<String>> {
    let mut input_buffer = std::mem::take(&mut state.input_buffer);
    let mut cursor_pos = state.cursor_pos.min(input_buffer.len());
    let mut history_cursor = state.history_cursor;

    let start = Instant::now();
    let mut spinner_idx = 0usize;
    let mut last_anim = Instant::now();
    let mut footer = FooterUi::enable();
    let mut esc_armed = false;
    let mut esc_deadline = Instant::now();

    let render = |footer: &mut FooterUi,
                  spinner_idx: usize,
                  input_buffer: &str,
                  cursor_pos: usize,
                  state: &SessionState,
                  chat: &eli_core::config::ChatConfig| {
        let title = footer_title(
            "ready",
            spinner_idx,
            queue_len,
            start.elapsed(),
            state.total_usage.total_tokens,
            Some(prompt_mode(state, chat)),
        );
        footer.render(&title, input_buffer, cursor_pos);
    };

    render(
        &mut footer,
        spinner_idx,
        &input_buffer,
        cursor_pos,
        state,
        chat,
    );

    let maybe_line = loop {
        if esc_armed && Instant::now() > esc_deadline {
            esc_armed = false;
        }
        if last_anim.elapsed() > Duration::from_millis(120) {
            spinner_idx = (spinner_idx + 1) % FOOTER_SPINNER.len();
            render(
                &mut footer,
                spinner_idx,
                &input_buffer,
                cursor_pos,
                state,
                chat,
            );
            last_anim = Instant::now();
        }

        if !ct_event::poll(Duration::from_millis(40)).unwrap_or(false) {
            continue;
        }

        let event = match ct_event::read() {
            Ok(ev) => ev,
            Err(_) => continue,
        };

        match event {
            CtEvent::Resize(_, _) => {
                render(
                    &mut footer,
                    spinner_idx,
                    &input_buffer,
                    cursor_pos,
                    state,
                    chat,
                );
                continue;
            }
            CtEvent::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                if key.modifiers.contains(CtKeyModifiers::CONTROL) {
                    match key.code {
                        CtKeyCode::Char('c') => {
                            input_buffer.clear();
                            cursor_pos = 0;
                            history_cursor = None;
                            esc_armed = false;
                            break Some(String::new());
                        }
                        CtKeyCode::Char('d') => {
                            break None;
                        }
                        _ => {}
                    }
                }

                match key.code {
                    CtKeyCode::Char(c) => {
                        history_cursor = None;
                        input_buffer.insert(cursor_pos, c);
                        cursor_pos += 1;
                        esc_armed = false;
                    }
                    CtKeyCode::Backspace => {
                        history_cursor = None;
                        if cursor_pos > 0 {
                            cursor_pos -= 1;
                            input_buffer.remove(cursor_pos);
                        }
                        esc_armed = false;
                    }
                    CtKeyCode::Delete => {
                        history_cursor = None;
                        if cursor_pos < input_buffer.len() {
                            input_buffer.remove(cursor_pos);
                        }
                        esc_armed = false;
                    }
                    CtKeyCode::Left => {
                        if cursor_pos > 0 {
                            cursor_pos -= 1;
                        }
                        esc_armed = false;
                    }
                    CtKeyCode::Right => {
                        if cursor_pos < input_buffer.len() {
                            cursor_pos += 1;
                        }
                        esc_armed = false;
                    }
                    CtKeyCode::Home => {
                        cursor_pos = 0;
                        esc_armed = false;
                    }
                    CtKeyCode::End => {
                        cursor_pos = input_buffer.len();
                        esc_armed = false;
                    }
                    CtKeyCode::Up => {
                        let Some(last_idx) = state.prompt_history.len().checked_sub(1) else {
                            continue;
                        };
                        let next = match history_cursor {
                            None => Some(last_idx),
                            Some(idx) => idx.checked_sub(1),
                        };
                        if let Some(idx) = next {
                            history_cursor = Some(idx);
                            input_buffer = state.prompt_history[idx].clone();
                            cursor_pos = input_buffer.len(); // Cursor at end after history load
                        }
                        esc_armed = false;
                    }
                    CtKeyCode::Down => {
                        let Some(idx) = history_cursor else {
                            continue;
                        };
                        let next = idx.saturating_add(1);
                        if next >= state.prompt_history.len() {
                            history_cursor = None;
                            input_buffer.clear();
                            cursor_pos = 0;
                        } else {
                            history_cursor = Some(next);
                            input_buffer = state.prompt_history[next].clone();
                            cursor_pos = input_buffer.len(); // Cursor at end after history load
                        }
                        esc_armed = false;
                    }
                    CtKeyCode::Esc => {
                        if !esc_armed {
                            esc_armed = true;
                            esc_deadline = Instant::now() + Duration::from_millis(800);
                        } else {
                            history_cursor = None;
                            input_buffer.clear();
                            cursor_pos = 0;
                            esc_armed = false;
                        }
                    }
                    CtKeyCode::Enter => {
                        let line = input_buffer.clone();
                        history_cursor = None;
                        input_buffer.clear();
                        cursor_pos = 0;
                        esc_armed = false;
                        break Some(line);
                    }
                    _ => {}
                }

                render(
                    &mut footer,
                    spinner_idx,
                    &input_buffer,
                    cursor_pos,
                    state,
                    chat,
                );
            }
            _ => {}
        }
    };

    state.input_buffer = input_buffer;
    state.cursor_pos = cursor_pos;
    state.history_cursor = history_cursor;

    Ok(maybe_line)
}

fn print_mode_status(state: &SessionState, chat: &eli_core::config::ChatConfig) {
    let display = match state.display_mode {
        DisplayMode::Standard => "standard",
        DisplayMode::Brain => "brain",
        DisplayMode::Debug => "debug",
        DisplayMode::Raw => "raw",
    };
    let agent = "autonomous (locked)";
    let exec = format_mode(chat.mode);
    let approvals = format_approvals_display(chat);
    let auto_run = if chat.auto { "on" } else { "off" };
    let time = format_duration(state.total_work_time);

    let body = format!(
        "display: {display}\nagent: {agent}\nexec: {exec}\napprovals: {approvals}\nauto-run: {auto_run}\nsteps: {}\ntime: {time}",
        state.step_count
    );
    println!("{}", render_ratatui_panel("status", &body));
}

fn print_help() {
    use style::*;

    let lines = vec![
        format!("{}{}Commands{}", BOLD, CYAN, RESET),
        String::new(),
        format!("{}Display{}", PURPLE, RESET),
        format!(
            "  {}/brain{}      full output (tools, history, details)",
            WHITE, RESET
        ),
        format!(
            "  {}/debug{}      debug output (raw request/response + tool output + observation)",
            WHITE, RESET
        ),
        format!(
            "  {}/standard{}   brief output (recent stream, summary)",
            WHITE, RESET
        ),
        String::new(),
        format!("{}Execution{}", PURPLE, RESET),
        format!("  {}/mode{}       set exec mode (read/work)", WHITE, RESET),
        format!("  {}/read{}       set exec mode to read", WHITE, RESET),
        format!("  {}/work{}       set exec mode to work", WHITE, RESET),
        format!("  {}/bot{}        work; cmds auto, diffs ask", WHITE, RESET),
        format!("  {}/yolo{}       work; auto approvals", WHITE, RESET),
        String::new(),
        format!("{}Configuration{}", PURPLE, RESET),
        format!(
            "  {}/model{}      set or show model for this session",
            WHITE, RESET
        ),
        format!(
            "  {}/key{}        set API key for current provider",
            WHITE, RESET
        ),
        String::new(),
        format!("{}Queue{}", PURPLE, RESET),
        format!("  {}/queue /q{}   show queued prompts", WHITE, RESET),
        format!("  {}/cq{}         clear queue", WHITE, RESET),
        format!("  {}+<prompt>{}   queue a prompt for later", WHITE, RESET),
        String::new(),
        format!("{}Keyboard{}", PURPLE, RESET),
        format!(
            "  {}Esc{}         interrupt current run (standard mode)",
            WHITE, RESET
        ),
        format!(
            "  {}Esc Esc{}     clear input (standard mode)",
            WHITE, RESET
        ),
        format!(
            "  {}Ctrl+C{}      clear input (standard mode)",
            WHITE, RESET
        ),
        format!("  {}Ctrl+D{}      quit (standard mode)", WHITE, RESET),
        String::new(),
        format!("{}Session{}", PURPLE, RESET),
        format!("  {}/status /s{}  show current mode/stats", WHITE, RESET),
        format!(
            "  {}/compact{}    summarize older context (reduce tokens)",
            WHITE, RESET
        ),
        format!("  {}/reset{}      clear conversation", WHITE, RESET),
        format!("  {}/new{}        alias for /reset", WHITE, RESET),
        format!(
            "  {}/tip{}        toggle tips (standard mode)",
            WHITE, RESET
        ),
        format!("  {}/undo{}       undo last edit", WHITE, RESET),
        format!("  {}/exit{}       quit", WHITE, RESET),
    ];

    let out = format_indented_block(&lines);
    println!("{}", out);
}

async fn perform_undo(
    undo_stack: &mut Vec<Vec<DiffResult>>,
    memory: &mut eli_core::memory::Memory,
    store: &SessionStore,
    session_id: &str,
) -> Result<()> {
    let Some(last) = undo_stack.pop() else {
        println!("(nothing to undo)");
        return Ok(());
    };

    let messages = UndoManager::undo_step(&last);
    if messages.is_empty() {
        println!("(nothing to undo)");
        return Ok(());
    }

    for msg in &messages {
        println!("{msg}");
    }

    let observation = format!("undo:\n{}", messages.join("\n"));
    memory.push(ChatMessage::tool(observation.clone(), "eli"));
    store
        .append(
            session_id,
            &SessionEvent {
                ts: chrono::Utc::now(),
                kind: EventKind::Note {
                    content: observation,
                },
            },
        )
        .await
        .ok();

    Ok(())
}

fn ensure_eli_research_brain(project_root: &Path) -> Result<PathBuf> {
    let dir = project_root.join("eli_research");
    std::fs::create_dir_all(&dir).context("create eli_research dir")?;

    let brain = dir.join("ELI.md");
    const PINNED_START: &str = "<!-- ELI_PINNED_START -->";
    const PINNED_END: &str = "<!-- ELI_PINNED_END -->";

    let pinned_block = format!(
        "{PINNED_START}\n\
## Default Research Flow\n\
- If ticker/company is ambiguous: `eli finance search --query <name>`\n\
- Start with price/volume: `eli finance timeseries` (zoom out, then zoom in). Identify key move dates.\n\
- Only then pull catalysts: `eli finance news --date YYYY-MM-DD` / `eli finance filings` for those key dates. News only matters if it moved price.\n\
- If the user mentions specific dates/days, include them (or ask 1 clarification).\n\
{PINNED_END}\n\
\n\
<!-- Append-only log below (eli writes here). -->\n"
    );

    if brain.exists() {
        // Ensure a pinned instructions section exists (like CLAUDE.md), without clobbering existing notes.
        let content = std::fs::read_to_string(&brain).unwrap_or_default();
        if content.contains(PINNED_START) && content.contains(PINNED_END) {
            return Ok(brain);
        }
        let mut out = String::new();
        out.push_str(&pinned_block);
        if !content.trim().is_empty() {
            out.push_str("\n");
            out.push_str(&content);
        }
        std::fs::write(&brain, out).context("seed eli_research/ELI.md")?;
        return Ok(brain);
    }

    std::fs::write(&brain, pinned_block).context("create eli_research/ELI.md")?;
    Ok(brain)
}

fn read_eli_brain_tail(project_root: &Path, max_chars: usize) -> Result<Option<String>> {
    const MAX_LOG_ENTRIES: usize = 5;
    const LOG_MARKER: &str = "<!-- Append-only log below (eli writes here). -->";

    let brain = ensure_eli_research_brain(project_root)?;
    let content = std::fs::read_to_string(&brain).context("read eli_research/ELI.md")?;
    if content.trim().is_empty() {
        return Ok(None);
    }

    let log_slice = if let Some(idx) = content.find(LOG_MARKER) {
        &content[idx + LOG_MARKER.len()..]
    } else {
        content.as_str()
    };

    let mut entries: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in log_slice.lines() {
        if line.starts_with("### ") {
            if !current.is_empty() {
                entries.push(current.join("\n"));
                current.clear();
            }
            current.push(line.to_string());
        } else if !current.is_empty() {
            current.push(line.to_string());
        }
    }
    if !current.is_empty() {
        entries.push(current.join("\n"));
    }

    if entries.is_empty() {
        return Ok(None);
    }

    let start = entries.len().saturating_sub(MAX_LOG_ENTRIES);
    let mut recent = entries[start..].join("\n\n");
    recent = recent.trim().to_string();
    if recent.is_empty() {
        return Ok(None);
    }

    if max_chars == 0 {
        return Ok(Some(recent));
    }

    let total = recent.chars().count();
    if total <= max_chars {
        return Ok(Some(recent));
    }

    let tail: String = recent.chars().skip(total - max_chars).collect();
    Ok(Some(format!("…\n{tail}")))
}

fn read_eli_brain_pinned(project_root: &Path, max_chars: usize) -> Result<Option<String>> {
    const PINNED_START: &str = "<!-- ELI_PINNED_START -->";
    const PINNED_END: &str = "<!-- ELI_PINNED_END -->";

    let brain = ensure_eli_research_brain(project_root)?;
    let content = std::fs::read_to_string(&brain).context("read eli_research/ELI.md")?;

    let Some(start) = content.find(PINNED_START) else {
        return Ok(None);
    };
    let after_start = &content[start + PINNED_START.len()..];
    let Some(end_rel) = after_start.find(PINNED_END) else {
        return Ok(None);
    };
    let pinned = after_start[..end_rel].trim();
    if pinned.is_empty() {
        return Ok(None);
    }

    if max_chars == 0 {
        return Ok(Some(pinned.to_string()));
    }

    let total = pinned.chars().count();
    if total <= max_chars {
        return Ok(Some(pinned.to_string()));
    }

    let truncated: String = pinned.chars().take(max_chars).collect();
    Ok(Some(format!("{truncated}…")))
}

fn read_eli_brain_context(
    project_root: &Path,
    pinned_max: usize,
    tail_max: usize,
) -> Result<Option<String>> {
    let pinned = match read_eli_brain_pinned(project_root, pinned_max) {
        Ok(v) => v,
        Err(e) => {
            warn!("eli brain: failed to read pinned (ignored): {e}");
            None
        }
    };
    let tail = match read_eli_brain_tail(project_root, tail_max) {
        Ok(v) => v,
        Err(e) => {
            warn!("eli brain: failed to read tail (ignored): {e}");
            None
        }
    };

    match (pinned, tail) {
        (None, None) => Ok(None),
        (Some(pinned), None) => Ok(Some(format!("ELI.md (pinned):\n{pinned}"))),
        (None, Some(tail)) => Ok(Some(format!("ELI.md (recent):\n{tail}"))),
        (Some(pinned), Some(tail)) => Ok(Some(format!(
            "ELI.md (pinned):\n{pinned}\n\nELI.md (recent):\n{tail}"
        ))),
    }
}

fn append_eli_brain(project_root: &Path, entry: &str) -> Result<()> {
    let brain = ensure_eli_research_brain(project_root)?;

    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&brain)
        .context("open eli_research/ELI.md")?;

    use std::io::Write;
    f.write_all(entry.as_bytes())
        .context("append eli_research/ELI.md")?;
    if !entry.ends_with('\n') {
        f.write_all(b"\n")
            .context("append newline to eli_research/ELI.md")?;
    }
    Ok(())
}

/// Execute /copy command - query session state and copy to clipboard or file
async fn execute_copy_command(
    args: &str,
    memory: &eli_core::memory::Memory,
    project_root: &Path,
) -> Result<String> {
    use eli_core::types::Role;

    // Parse arguments
    let parts: Vec<&str> = args.split_whitespace().collect();

    // Check for file output: /copy all > file.md
    let (scope_parts, output_file) = if let Some(idx) = parts.iter().position(|&p| p == ">") {
        let (scope, rest) = parts.split_at(idx);
        let file = rest.get(1).map(|s| s.to_string());
        (scope.to_vec(), file)
    } else {
        (parts, None)
    };

    // Parse scope
    let scope = scope_parts.first().copied().unwrap_or("");

    // Check for filters (-data, -raw, -meta)
    let exclude_data = scope_parts.iter().any(|&p| p == "-data");
    let exclude_meta = scope_parts.iter().any(|&p| p == "-meta");

    // Get messages from memory
    let messages = memory.context();

    // Filter by scope
    let filtered: Vec<_> = match scope {
        "" | "last" => {
            // Last assistant response
            messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .into_iter()
                .collect()
        }
        "all" => {
            // All non-system messages
            messages.iter().filter(|m| m.role != Role::System).collect()
        }
        "user" => messages.iter().filter(|m| m.role == Role::User).collect(),
        "assistant" => messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .collect(),
        "tools" => messages.iter().filter(|m| m.role == Role::Tool).collect(),
        n if n.parse::<usize>().is_ok() => {
            // Last N turns (user + assistant pairs)
            let n: usize = n.parse().unwrap();
            let non_system: Vec<_> = messages.iter().filter(|m| m.role != Role::System).collect();
            non_system
                .into_iter()
                .rev()
                .take(n * 2)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        }
        _ => {
            return Err(anyhow::anyhow!(
                "unknown scope '{}'. Use: all, last, user, assistant, tools, or N",
                scope
            ));
        }
    };

    if filtered.is_empty() {
        return Ok("Nothing to copy.".to_string());
    }

    // Format as markdown
    let mut output = String::new();
    for msg in filtered {
        let role_str = match msg.role {
            Role::User => "## User",
            Role::Assistant => "## Assistant",
            Role::Tool => &format!("### Tool: {}", msg.name.as_deref().unwrap_or("unknown")),
            Role::System => continue, // Skip system messages
        };

        output.push_str(role_str);
        output.push_str("\n\n");

        let content = if exclude_data && msg.content.len() > 2000 && msg.role == Role::Tool {
            format!(
                "[output: {} chars, omitted with -data]\n",
                msg.content.len()
            )
        } else {
            msg.content.clone()
        };

        output.push_str(&content);
        output.push_str("\n\n");
    }

    let char_count = output.len();

    // Output to file or clipboard
    if let Some(file_path) = output_file {
        let full_path = project_root.join(&file_path);
        std::fs::write(&full_path, &output)
            .with_context(|| format!("write to {}", full_path.display()))?;
        Ok(format!("Copied {} chars to {}", char_count, file_path))
    } else {
        // Copy to clipboard
        eli_screen::clipboard_set(&output)
            .await
            .map_err(|e| anyhow::anyhow!("clipboard: {}", e))?;
        Ok(format!("Copied {} chars to clipboard", char_count))
    }
}

fn slugify_for_filename(input: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut last_was_sep = false;

    for ch in input.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_was_sep = false;
        } else if matches!(
            c,
            ' ' | '-' | '_' | '.' | '/' | '\\' | ':' | ';' | ',' | '|'
        ) {
            if !out.is_empty() && !last_was_sep {
                out.push('_');
                last_was_sep = true;
            }
        }

        if max_len > 0 && out.len() >= max_len {
            break;
        }
    }

    while out.ends_with('_') {
        out.pop();
    }

    out
}

fn write_research_report_md(
    project_root: &Path,
    session_id: &str,
    chat: &eli_core::config::ChatConfig,
    prompt: &str,
    synthesis: Option<&eli_core::contract::Synthesis>,
    status: &str,
    partial_output: Option<&str>,
) -> Result<Option<PathBuf>> {
    let dir = project_root.join("eli_research");
    std::fs::create_dir_all(&dir).context("create eli_research dir")?;
    let _ = ensure_eli_research_brain(project_root)?;

    let now = chrono::Utc::now();
    let ts = now.format("%Y%m%d_%H%M%S").to_string();
    let session_short: String = session_id.chars().take(8).collect();

    let prompt_clean = strip_agent_context_block(prompt).trim();
    let title = if prompt_clean.is_empty() {
        prompt.trim()
    } else {
        prompt_clean
    };
    let title = if title.is_empty() { "Research" } else { title };
    let title_line = truncate(title, 120);

    let slug = slugify_for_filename(title, 60);
    let filename = if slug.is_empty() {
        format!("research_{ts}_{session_short}.md")
    } else {
        format!("research_{ts}_{slug}_{session_short}.md")
    };
    let path = dir.join(filename);

    let mut md = String::new();
    md.push_str(&format!("# {title_line}\n\n"));
    md.push_str(&format!("- Date (UTC): {}\n", now.to_rfc3339()));
    md.push_str(&format!("- Session: `{session_id}`\n"));
    md.push_str(&format!("- Provider: `{}`\n", chat.provider));
    md.push_str(&format!("- Model: `{}`\n", chat.model));
    md.push_str(&format!("- Status: {status}\n\n"));

    md.push_str("## Prompt\n");
    md.push_str("```\n");
    md.push_str(title.trim());
    md.push_str("\n```\n\n");

    if let Some(s) = synthesis {
        if !s.summary.is_empty() {
            md.push_str("## Summary\n");
            for item in &s.summary {
                let item = item.trim();
                if !item.is_empty() {
                    md.push_str("- ");
                    md.push_str(item);
                    md.push('\n');
                }
            }
            md.push('\n');
        }

        if !s.answer.trim().is_empty() {
            md.push_str("## Answer\n\n");
            md.push_str(s.answer.trim());
            md.push_str("\n\n");
        }

        if !s.next_steps.is_empty() {
            md.push_str("## Next Steps\n");
            for item in &s.next_steps {
                let item = item.trim();
                if !item.is_empty() {
                    md.push_str("- ");
                    md.push_str(item);
                    md.push('\n');
                }
            }
            md.push('\n');
        }
    }

    if let Some(partial) = partial_output {
        let partial = partial.trim();
        if !partial.is_empty() {
            md.push_str("## Partial Output\n");
            md.push_str("```\n");
            md.push_str(partial);
            md.push_str("\n```\n");
        }
    }

    std::fs::write(&path, md).context("write research report")?;
    Ok(Some(path))
}

fn strip_agent_context_block(prompt: &str) -> &str {
    let marker = "[ELI_AGENT_CONTEXT]";
    if let Some(idx) = prompt.find(marker) {
        return &prompt[..idx];
    }
    prompt
}

fn slash_menu_lines() -> Vec<String> {
    use style::*;

    let mut lines = Vec::new();
    lines.push(format!(
        "{}{}Slash Commands{} {}(↑/↓ to cycle){}",
        BOLD, CYAN, RESET, GRAY, RESET
    ));
    lines.push(String::new());
    for cmd in SLASH_COMMANDS {
        lines.push(format!(
            "{}{:<14}{} {}{}{}",
            WHITE, cmd.name, RESET, GRAY, cmd.desc, RESET
        ));
    }
    lines
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

async fn cmd_tui(provider: Option<String>, model: Option<String>) -> Result<()> {
    // Keep `eli tui` as an explicit entrypoint, but route to the same UI as `eli`/`eli chat`.
    cmd_chat(provider, model, None).await
}

fn apply_overrides(
    cfg: &mut ConfigFile,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    if let Some(provider) = provider {
        cfg.chat.provider = provider
            .parse::<ProviderKind>()
            .map_err(|e| anyhow::anyhow!(e))
            .context("parse provider")?;
    }
    if let Some(model) = model {
        cfg.chat.model = model;
    }
    Ok(())
}

use base64::Engine;

fn ensure_tui_default_model(chat: &mut eli_core::config::ChatConfig) {
    let model = chat.model.trim();
    if model.is_empty() || model.eq_ignore_ascii_case("test") {
        chat.model = config::DEFAULT_OPENROUTER_MODEL.to_string();
    }
}

fn debug_print_request(req: &ChatRequest) {
    println!("\n=== REQUEST ===");
    match serde_json::to_string_pretty(req) {
        Ok(json) => println!("{json}"),
        Err(err) => println!("(failed to serialize request: {err})"),
    }
    println!("\n=== END REQUEST ===");
}

fn process_input_for_images(input: &str) -> (String, Vec<String>) {
    let mut clean_words = Vec::new();
    let mut images = Vec::new();

    for word in input.split_whitespace() {
        let path = Path::new(word);
        if path.exists() && path.is_file() {
            if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                let ext = ext.to_lowercase();
                if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif") {
                    if let Ok(bytes) = std::fs::read(path) {
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        let mime = match ext.as_str() {
                            "png" => "image/png",
                            "jpg" | "jpeg" => "image/jpeg",
                            "webp" => "image/webp",
                            "gif" => "image/gif",
                            _ => "application/octet-stream",
                        };
                        images.push(format!("data:{};base64,{}", mime, b64));
                        continue; // Consumed as image
                    }
                }
            }
        }
        clean_words.push(word);
    }

    (clean_words.join(" "), images)
}

async fn run_agent_steps(
    chat: &eli_core::config::ChatConfig,
    adapter: Arc<dyn LlmAdapter>,
    diff_engine: &DiffEngine,
    command_runner: &CommandRunner,
    store: &SessionStore,
    data_dir: &Path,
    session_id: &str,
    project_root: &Path,
    memory: &mut eli_core::memory::Memory,
    undo_stack: &mut Vec<Vec<DiffResult>>,
    state: &mut SessionState,
    profile: AgentProfile,
    initial_user_message: String,
    initial_images: Vec<String>,
) -> Result<()> {
    let trajectory_logger = eli_core::trajectory::TrajectoryLogger::new(data_dir.to_path_buf());

    let max_iters = if chat.auto { chat.max_auto.max(1) } else { 1 };
    let task_start = Instant::now();
    let debug = matches!(state.display_mode, DisplayMode::Debug)
        || matches!(chat.display_mode, DisplayMode::Debug);
    let brief = matches!(state.display_mode, DisplayMode::Standard)
        && !matches!(chat.display_mode, DisplayMode::Debug)
        && has_interactive_terminal();
    let machine_stream = env_truthy("ELI_PLAIN_OUTPUT")
        || env_truthy("ELI_NO_FOOTER")
        || !has_interactive_terminal();
    let emit_cli_chrome = !machine_stream || debug;
    let mut footer: Option<FooterUi> = None;
    let mut spinner_idx = 0usize;
    let mut last_anim = Instant::now();
    let synthesis_title = format_synthesis_title(&initial_user_message);
    let mut task_had_actions = false;
    let mut task_insights: Vec<String> = Vec::new();
    let mut saw_finance_timeseries = false;
    let mut saw_finance_snapshot = false;
    let mut plan_confirmed = !matches!(state.auto_mode, AutoMode::Plan);
    let mut current_message = initial_user_message;
    let mut current_images = initial_images;
    let root_prompt = current_message.clone();
    let mut invalid_format_retries: u8 = 0;
    let mut last_keep_working_signature: Option<String> = None;
    let mut repeated_keep_working_count: u32 = 0;
    let mut last_focus_notes_signature: Option<String> = None;
    let mut repeated_focus_notes_count: u32 = 0;
    let quick_query_mode = profile == AgentProfile::Research && is_quick_market_query(&root_prompt);
    let mut forced_finalize_sent = false;

    for step in 1..=max_iters {
        let step_start = Instant::now();
        state.step_count += 1;
        let mut step_observation: Option<String> = None;

        // Sequence fix: only push "KEEP WORKING" if the last message wasn't a tool observation.
        // This avoids double-user messages which crash some providers.
        let skip_keep_working = step > 1
            && current_message == "KEEP WORKING"
            && memory.last_role() == Some(eli_core::types::Role::Tool);

        if !skip_keep_working {
            store
                .append(
                    session_id,
                    &SessionEvent {
                        ts: chrono::Utc::now(),
                        kind: EventKind::UserMessage {
                            content: current_message.clone(),
                        },
                    },
                )
                .await
                .ok();

            if !current_images.is_empty() {
                memory.push(ChatMessage::user_with_images(
                    current_message.clone(),
                    current_images.clone(),
                ));
                if !brief {
                    println!("(attached {} images)", current_images.len());
                }
                // Clear images after first use so we don't re-send them in loop unless intended
                current_images.clear();
            } else {
                memory.push(ChatMessage::user(current_message.clone()));
            }
        }

        if let Ok(Some(compaction)) = maybe_compact_memory(adapter.clone(), chat, memory).await {
            let note = format!(
                "memory_compaction: dropped {} messages\n{}",
                compaction.dropped, compaction.summary
            );
            let brain_entry = format!(
                "\n### {} (session {})\n{}\n",
                chrono::Utc::now().to_rfc3339(),
                session_id,
                note
            );
            if let Err(e) = append_eli_brain(project_root, &brain_entry) {
                warn!("eli brain: failed to persist compaction (ignored): {e}");
            }
            store
                .append(
                    session_id,
                    &SessionEvent {
                        ts: chrono::Utc::now(),
                        kind: EventKind::Note {
                            content: note.clone(),
                        },
                    },
                )
                .await
                .ok();
            if !brief {
                println!("memory: compacted ({} msgs)", compaction.dropped);
            }
        }

        let mut messages = memory.context();
        if let Ok(Some(ctx)) = read_eli_brain_context(project_root, 2_000, 6_000) {
            insert_system_context_before_conversation(&mut messages, ChatMessage::system(ctx));
        }
        let trajectory_input = messages.clone();

        let req = ChatRequest {
            model: chat.model.clone(),
            messages,
            temperature: chat.temperature,
            max_tokens: chat.max_tokens,
            response_format: None,
            stream: true,
        };

        if debug {
            debug_print_request(&req);
        }

        use std::io::Write;
        let mut out = String::new();
        let mut interrupted = false;
        let mut interrupted_by_esc = false;

        let connect_start = Instant::now();
        if brief {
            if footer.is_none() {
                footer = Some(FooterUi::enable());
            }
            render_footer(
                &mut footer,
                "connecting",
                spinner_idx,
                connect_start.elapsed(),
                state,
                None,
            );
        }

        let stream_opt = if brief {
            let mut fut = Box::pin(adapter.chat_stream(req));
            loop {
                let changed =
                    drain_run_key_events(state, &mut interrupted, &mut interrupted_by_esc);
                if brief && (last_anim.elapsed() > Duration::from_millis(120) || changed) {
                    if last_anim.elapsed() > Duration::from_millis(120) {
                        spinner_idx = (spinner_idx + 1) % FOOTER_SPINNER.len();
                        last_anim = Instant::now();
                    }
                    render_footer(
                        &mut footer,
                        "connecting",
                        spinner_idx,
                        connect_start.elapsed(),
                        state,
                        None,
                    );
                }
                if interrupted {
                    break None;
                }
                tokio::select! {
                    res = &mut fut => break Some(res.context("chat_stream")?),
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
        } else {
            Some(adapter.chat_stream(req).await.context("chat_stream")?)
        };

        if let Some(mut stream) = stream_opt {
            let thinking_start = Instant::now();
            if brief {
                render_footer(
                    &mut footer,
                    "thinking",
                    spinner_idx,
                    thinking_start.elapsed(),
                    state,
                    None,
                );
            }

            loop {
                tokio::select! {
                    maybe_ev = stream.next() => {
                        let Some(ev) = maybe_ev else { break; };
                        match ev.context("stream event")? {
                            eli_core::types::ChatStreamEvent::Delta(delta) => {
                                out.push_str(&delta);
                            }
                            eli_core::types::ChatStreamEvent::Usage(usage) => {
                                state.last_usage = Some(usage.clone());
                                state.total_usage.prompt_tokens += usage.prompt_tokens;
                                state.total_usage.completion_tokens += usage.completion_tokens;
                                state.total_usage.total_tokens += usage.total_tokens;
                            }
                            eli_core::types::ChatStreamEvent::Done => break,
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }

                let changed =
                    drain_run_key_events(state, &mut interrupted, &mut interrupted_by_esc);
                if interrupted {
                    break;
                }

                if brief && (last_anim.elapsed() > Duration::from_millis(120) || changed) {
                    if last_anim.elapsed() > Duration::from_millis(120) {
                        spinner_idx = (spinner_idx + 1) % FOOTER_SPINNER.len();
                        last_anim = Instant::now();
                    }
                    render_footer(
                        &mut footer,
                        "thinking",
                        spinner_idx,
                        thinking_start.elapsed(),
                        state,
                        None,
                    );
                }
            }
        }

        if brief && interrupted_by_esc {
            let mut armed = false;
            let mut deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if !ct_event::poll(Duration::from_millis(60)).unwrap_or(false) {
                    continue;
                }
                let Ok(CtEvent::Key(key)) = ct_event::read() else {
                    continue;
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                match key.code {
                    CtKeyCode::Esc => {
                        if !armed {
                            armed = true;
                            print!(
                                "\r\x1b[K  {}!{} press {}Esc{} again to clear input",
                                style::YELLOW,
                                style::RESET,
                                style::WHITE,
                                style::RESET
                            );
                            std::io::stdout().flush().ok();
                            deadline = Instant::now() + Duration::from_secs(2);
                        } else {
                            state.input_buffer.clear();
                            state.cursor_pos = 0;
                            break;
                        }
                    }
                    CtKeyCode::Char(c) => {
                        state.input_buffer.insert(state.cursor_pos, c);
                        state.cursor_pos += 1;
                        break;
                    }
                    CtKeyCode::Backspace => {
                        if state.cursor_pos > 0 {
                            state.cursor_pos -= 1;
                            state.input_buffer.remove(state.cursor_pos);
                        }
                        break;
                    }
                    _ => break,
                }
            }
            print!("\r\x1b[K");
            std::io::stdout().flush().ok();
        }

        if interrupted {
            println!("(interrupted)");
            if profile == AgentProfile::Research {
                match write_research_report_md(
                    project_root,
                    session_id,
                    chat,
                    &root_prompt,
                    None,
                    "interrupted",
                    Some(&out),
                ) {
                    Ok(Some(path)) => {
                        let rel = path.strip_prefix(project_root).unwrap_or(&path);
                        if brief {
                            println!("  saved: {}", rel.display());
                        } else {
                            println!("(saved: {})", rel.display());
                        }

                        let note = format!(
                            "research_report_saved: {}\nstatus: interrupted\ntitle: {}",
                            rel.display(),
                            truncate(&root_prompt, 120)
                        );
                        memory.push(ChatMessage::tool(note.clone(), "eli.research"));
                        store
                            .append(
                                session_id,
                                &SessionEvent {
                                    ts: chrono::Utc::now(),
                                    kind: EventKind::Note { content: note },
                                },
                            )
                            .await
                            .ok();

                        state.record_research_report(
                            ResearchArtifact {
                                rel_path: rel.display().to_string(),
                                title: root_prompt.clone(),
                                status: "interrupted".to_string(),
                                created_utc: chrono::Utc::now().to_rfc3339(),
                                answer_hint: None,
                            },
                            24,
                        );

                        let brain_entry = format!(
                            "\n### {} (session {})\n- Research saved: {} (interrupted)\n",
                            chrono::Utc::now().to_rfc3339(),
                            session_id,
                            rel.display()
                        );
                        if let Err(e) = append_eli_brain(project_root, &brain_entry) {
                            warn!("eli brain: failed to persist research pointer (ignored): {e}");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => warn!("failed to write interrupted research report (ignored): {e}"),
                }
            }
            break;
        }

        if out.trim().is_empty() {
            warn!("empty assistant message");
            break;
        }

        if debug {
            println!("\n=== RAW MODEL OUTPUT ===");
            print!("{}", out);
            if !out.ends_with('\n') {
                println!();
            }
            println!("=== END RAW MODEL OUTPUT ===");
        }

        let model = match contract::validate_model_response(&out) {
            Ok(m) => {
                invalid_format_retries = 0;
                m
            }
            Err(e) => {
                if emit_cli_chrome {
                    println!("eli: invalid response ({})", e);
                }
                if !brief && emit_cli_chrome {
                    println!("{}", out);
                }
                invalid_format_retries = invalid_format_retries.saturating_add(1);
                if invalid_format_retries >= 3 {
                    if emit_cli_chrome {
                        println!(
                            "eli: stopping after {} invalid-format responses",
                            invalid_format_retries
                        );
                    }
                    break;
                }

                current_message = format!(
                    "FORMAT ERROR: Your previous response was invalid ({e}). Return ONLY one strict JSON object matching the Eli contract. No prose, no markdown, no <tool_call> tags."
                );
                current_images.clear();
                continue;
            }
        };
        let canonical = serde_json::to_string_pretty(&model).unwrap_or(out.clone());

        memory.push(ChatMessage::assistant(canonical.clone()));
        store
            .append(
                session_id,
                &SessionEvent {
                    ts: chrono::Utc::now(),
                    kind: EventKind::AssistantMessage { content: canonical },
                },
            )
            .await
            .ok();

        // Track step time
        let step_elapsed = step_start.elapsed();

        // Print step summary (brief vs full)
        if brief {
            if step == 1 {
                // Force a scroll line so the first prompt is not overwritten.
                print_history_line(String::new());
            }
            print_step_summary_brief(step, step_elapsed, &model);
            render_footer(
                &mut footer,
                "ready",
                spinner_idx,
                Duration::ZERO,
                state,
                None,
            );
        } else if emit_cli_chrome {
            print_step_summary(step, &model);
        }

        let mut read_mode = matches!(chat.mode, RunMode::Read);
        let mut approvals_ask_commands =
            matches!(chat.resolved_command_approvals(), ApprovalMode::Ask);
        let mut approvals_ask_diffs = matches!(chat.resolved_diff_approvals(), ApprovalMode::Ask);
        let (plan_mode, plan_approvals) = parse_plan_controls(&model.plan);
        if matches!(plan_mode, Some(RunMode::Read)) {
            read_mode = true;
        }
        if matches!(plan_approvals, Some(ApprovalMode::Ask)) {
            approvals_ask_commands = true;
            approvals_ask_diffs = true;
        }

        let wants_user_input = model
            .ask_user
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);

        let has_actions =
            !model.commands.is_empty() || !model.diffs.is_empty() || !model.subagents.is_empty();

        // Anti-loop guard: if the model repeats identical KEEP_WORKING command sets,
        // force a synthesis pivot instead of running the same tools indefinitely.
        if matches!(model.status, StepStatus::KeepWorking)
            && !model.commands.is_empty()
            && model.diffs.is_empty()
            && model.subagents.is_empty()
        {
            let signature = model
                .commands
                .iter()
                .map(|c| c.trim())
                .collect::<Vec<_>>()
                .join("\n");
            if last_keep_working_signature
                .as_deref()
                .map(|s| s == signature)
                .unwrap_or(false)
            {
                repeated_keep_working_count = repeated_keep_working_count.saturating_add(1);
            } else {
                last_keep_working_signature = Some(signature);
                repeated_keep_working_count = 0;
            }

            if repeated_keep_working_count >= 2 {
                let loop_note = "loop_guard: repeated identical KEEP_WORKING commands detected; forcing synthesis without additional tool calls.".to_string();
                memory.push(ChatMessage::tool(loop_note.clone(), "eli.loop_guard"));
                store
                    .append(
                        session_id,
                        &SessionEvent {
                            ts: chrono::Utc::now(),
                            kind: EventKind::Note { content: loop_note },
                        },
                    )
                    .await
                    .ok();
                current_message = "LOOP_GUARD: You have repeated identical commands multiple times. Do not run more commands. Use existing tool outputs and return status DONE with a concise answer.".to_string();
                current_images.clear();
                continue;
            }
        } else {
            last_keep_working_signature = None;
            repeated_keep_working_count = 0;
        }

        if matches!(model.status, StepStatus::KeepWorking) {
            let focus_notes_sig = format!(
                "{}|{}",
                model.focus.trim().to_ascii_lowercase(),
                model.notes.trim().to_ascii_lowercase()
            );
            if last_focus_notes_signature
                .as_deref()
                .map(|s| s == focus_notes_sig)
                .unwrap_or(false)
            {
                repeated_focus_notes_count = repeated_focus_notes_count.saturating_add(1);
            } else {
                last_focus_notes_signature = Some(focus_notes_sig);
                repeated_focus_notes_count = 0;
            }

            if repeated_focus_notes_count >= 4 {
                let loop_note = "loop_guard: repeated KEEP_WORKING focus/notes detected; forcing final synthesis.".to_string();
                memory.push(ChatMessage::tool(loop_note.clone(), "eli.loop_guard"));
                store
                    .append(
                        session_id,
                        &SessionEvent {
                            ts: chrono::Utc::now(),
                            kind: EventKind::Note { content: loop_note },
                        },
                    )
                    .await
                    .ok();
                current_message = "LOOP_GUARD: You are repeating the same focus/notes. Stop running tools and return status DONE with the best concise answer from existing evidence.".to_string();
                current_images.clear();
                continue;
            }
        } else {
            last_focus_notes_signature = None;
            repeated_focus_notes_count = 0;
        }

        if debug {
            println!("\n=== TOOL CALL ATTEMPTED ===");
            if model.commands.is_empty()
                && model.diffs.is_empty()
                && model.subagents.is_empty()
                && model.screen.is_empty()
            {
                println!("(none)");
            } else {
                if !model.commands.is_empty() {
                    println!("commands:");
                    for cmd in &model.commands {
                        println!("  $ {}", cmd);
                    }
                }
                if !model.diffs.is_empty() {
                    println!("diffs: {}", model.diffs.len());
                    for diff in &model.diffs {
                        println!("  {:?} {}", diff.op, diff.path);
                    }
                }
                if !model.subagents.is_empty() {
                    println!("subagents: {}", model.subagents.len());
                    for agent in &model.subagents {
                        println!(
                            "  {} (model: {})",
                            agent.name,
                            agent.model.as_deref().unwrap_or("default")
                        );
                    }
                }
                if !model.screen.is_empty() {
                    println!("screen actions: {}", model.screen.len());
                }
            }
        }

        if matches!(state.auto_mode, AutoMode::Plan)
            && !plan_confirmed
            && !wants_user_input
            && !model.plan.trim().is_empty()
            && (has_actions || matches!(model.status, StepStatus::KeepWorking))
        {
            if brief {
                footer.take();
            }

            println!(
                "\n{}[PLAN]{} \n{}\n",
                style::BLUE,
                style::RESET,
                model.plan.trim_end()
            );

            use std::io::Write;
            print!(
                "{}?{} Confirm plan (Enter = proceed, type = critique): ",
                style::YELLOW,
                style::RESET
            );
            std::io::stdout().flush().ok();

            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .context("read plan confirmation input")?;
            let critique = input.trim();

            if !critique.is_empty() {
                current_message = critique.to_string();
                current_images.clear();
                continue;
            }

            plan_confirmed = true;

            if !has_actions {
                current_message = "Plan approved. Proceed with execution.".to_string();
                continue;
            }
        }

        let mut diff_results: Vec<DiffResult> = Vec::new();
        let mut command_results: Vec<CommandResult> = Vec::new();
        if !wants_user_input {
            if !model.diffs.is_empty() {
                if read_mode {
                    // READ mode: allow ONLY creation of NEW files.
                    for diff in &model.diffs {
                        let is_create = matches!(diff.op, contract::DiffOp::Create);
                        let res = diff_engine.apply_diff(diff, !is_create);
                        diff_results.push(res);
                    }
                    if emit_cli_chrome {
                        print_diff_results(&diff_results, true, brief);
                    }
                    let actual_changes: Vec<_> = diff_results
                        .iter()
                        .filter(|r| !r.preview && r.success)
                        .cloned()
                        .collect();
                    if !actual_changes.is_empty() {
                        undo_stack.push(actual_changes);
                    }
                } else {
                    let apply = if approvals_ask_diffs {
                        if brief {
                            footer.take();
                        }
                        let ans = confirm("Apply diffs?")?;
                        ans
                    } else {
                        true
                    };
                    diff_results = diff_engine.apply_diffs(&model.diffs, !apply);
                    if emit_cli_chrome {
                        print_diff_results(&diff_results, !apply, brief);
                    }
                    if apply {
                        undo_stack.push(diff_results.clone());
                    }
                }
            }

            if !model.commands.is_empty() {
                if read_mode {
                    // READ mode: allow all commands as requested by user
                    let parallelism = if model.commands_parallel {
                        chat.resolved_parallel_commands()
                    } else {
                        1
                    };
                    if brief {
                        let exec_start = Instant::now();
                        render_footer(
                            &mut footer,
                            "exec",
                            spinner_idx,
                            exec_start.elapsed(),
                            state,
                            None,
                        );

                        let mut fut = Box::pin(run_commands_with_policy(
                            profile,
                            command_runner,
                            &model.commands,
                            parallelism,
                        ));
                        loop {
                            tokio::select! {
                                res = &mut fut => {
                                    command_results = res;
                                    break;
                                }
                                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                            }
                            let changed = drain_run_key_events_queue_only(state);
                            if last_anim.elapsed() > Duration::from_millis(120) || changed {
                                if last_anim.elapsed() > Duration::from_millis(120) {
                                    spinner_idx = (spinner_idx + 1) % FOOTER_SPINNER.len();
                                    last_anim = Instant::now();
                                }
                                render_footer(
                                    &mut footer,
                                    "exec",
                                    spinner_idx,
                                    exec_start.elapsed(),
                                    state,
                                    None,
                                );
                            }
                        }
                    } else {
                        command_results = run_commands_with_policy(
                            profile,
                            command_runner,
                            &model.commands,
                            parallelism,
                        )
                        .await;
                    }
                } else {
                    let run = if approvals_ask_commands {
                        if brief {
                            footer.take();
                        }
                        let ans = confirm("Run commands?")?;
                        ans
                    } else {
                        true
                    };
                    if run {
                        let parallelism = if model.commands_parallel {
                            chat.resolved_parallel_commands()
                        } else {
                            1
                        };
                        if brief {
                            let exec_start = Instant::now();
                            render_footer(
                                &mut footer,
                                "exec",
                                spinner_idx,
                                exec_start.elapsed(),
                                state,
                                None,
                            );

                            let mut fut = Box::pin(run_commands_with_policy(
                                profile,
                                command_runner,
                                &model.commands,
                                parallelism,
                            ));
                            loop {
                                tokio::select! {
                                    res = &mut fut => {
                                        command_results = res;
                                        break;
                                    }
                                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                                }
                                let changed = drain_run_key_events_queue_only(state);
                                if last_anim.elapsed() > Duration::from_millis(120) || changed {
                                    if last_anim.elapsed() > Duration::from_millis(120) {
                                        spinner_idx = (spinner_idx + 1) % FOOTER_SPINNER.len();
                                        last_anim = Instant::now();
                                    }
                                    render_footer(
                                        &mut footer,
                                        "exec",
                                        spinner_idx,
                                        exec_start.elapsed(),
                                        state,
                                        None,
                                    );
                                }
                            }
                        } else {
                            command_results = run_commands_with_policy(
                                profile,
                                command_runner,
                                &model.commands,
                                parallelism,
                            )
                            .await;
                        }
                    } else {
                        command_results = model
                            .commands
                            .iter()
                            .map(|cmd| CommandResult {
                                command: cmd.clone(),
                                returncode: -1,
                                stdout: String::new(),
                                stderr: "Skipped (approvals_cmds=ask)".to_string(),
                                duration_ms: 0,
                                allowed: false,
                                deny_reason: Some("approvals_cmds=ask".to_string()),
                            })
                            .collect();
                    }
                }
            }

            if profile == AgentProfile::Research {
                if command_results.iter().any(|r| {
                    r.allowed
                        && r.returncode == 0
                        && r.command.trim_start().starts_with("eli finance timeseries")
                }) {
                    saw_finance_timeseries = true;
                }
                if command_results.iter().any(|r| {
                    r.allowed
                        && r.returncode == 0
                        && r.command.trim_start().starts_with("eli finance snapshot")
                }) {
                    saw_finance_snapshot = true;
                }
            }

            if !command_results.is_empty() {
                command_results = augment_tool_errors(command_results);
            }

            let insight = extract_insight(&command_results, &diff_results);
            if let Some(ref line) = insight {
                if task_insights.last().map(|s| s != line).unwrap_or(true) {
                    if task_insights.len() < 6 {
                        task_insights.push(line.to_string());
                    }
                }
            }

            if !command_results.is_empty() {
                if emit_cli_chrome {
                    if debug {
                        print_tool_results_debug(&command_results);
                    } else {
                        print_command_results(
                            &command_results,
                            brief,
                            matches!(state.display_mode, DisplayMode::Brain),
                        );
                    }
                }
                if brief && emit_cli_chrome {
                    render_footer(
                        &mut footer,
                        "ready",
                        spinner_idx,
                        Duration::ZERO,
                        state,
                        None,
                    );
                }
            }

            if !model.screen.is_empty() && !read_mode && !brief && emit_cli_chrome {
                print_screen_results(&model.screen).await;
            }

            let command_results_for_llm =
                shadow_large_tool_outputs(project_root, session_id, step, &command_results);

            if !diff_results.is_empty() || !command_results.is_empty() || !model.screen.is_empty() {
                task_had_actions = true;
                let observation = build_observation(
                    read_mode,
                    approvals_ask_commands,
                    approvals_ask_diffs,
                    &diff_results,
                    &command_results_for_llm,
                );
                if debug {
                    println!("\n=== OBSERVATION INJECTED (eli) ===");
                    print!("{}", observation);
                    if !observation.ends_with('\n') {
                        println!();
                    }
                    println!("=== END OBSERVATION INJECTED (eli) ===");
                }
                step_observation = Some(observation.clone());
                memory.push(ChatMessage::tool(observation.clone(), "eli"));
                store
                    .append(
                        session_id,
                        &SessionEvent {
                            ts: chrono::Utc::now(),
                            kind: EventKind::Note {
                                content: observation,
                            },
                        },
                    )
                    .await
                    .ok();
            }
        }

        let subagent_results = if wants_user_input || model.subagents.is_empty() {
            Vec::new()
        } else if brief {
            let agents_start = Instant::now();
            render_footer(
                &mut footer,
                "agents",
                spinner_idx,
                agents_start.elapsed(),
                state,
                None,
            );

            let mut fut = Box::pin(run_subagents(
                adapter.clone(),
                chat,
                memory,
                &model.subagents,
            ));
            let results = loop {
                tokio::select! {
                    res = &mut fut => {
                        break res;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
                let changed = drain_run_key_events_queue_only(state);
                if last_anim.elapsed() > Duration::from_millis(120) || changed {
                    if last_anim.elapsed() > Duration::from_millis(120) {
                        spinner_idx = (spinner_idx + 1) % FOOTER_SPINNER.len();
                        last_anim = Instant::now();
                    }
                    render_footer(
                        &mut footer,
                        "agents",
                        spinner_idx,
                        agents_start.elapsed(),
                        state,
                        None,
                    );
                }
            };
            results
        } else {
            run_subagents(adapter.clone(), chat, memory, &model.subagents).await
        };
        if !subagent_results.is_empty() {
            task_had_actions = true;
            if !brief && emit_cli_chrome {
                print_subagent_results(&subagent_results);
            } else if brief {
                println!("  subagents: {} completed", subagent_results.len());
            }
            if brief {
                render_footer(
                    &mut footer,
                    "ready",
                    spinner_idx,
                    Duration::ZERO,
                    state,
                    None,
                );
            }
            let observation = build_subagent_observation(&subagent_results);
            if debug {
                println!("\n=== OBSERVATION INJECTED (eli.subagents) ===");
                print!("{}", observation);
                if !observation.ends_with('\n') {
                    println!();
                }
                println!("=== END OBSERVATION INJECTED (eli.subagents) ===");
            }
            if let Some(ref mut existing) = step_observation {
                existing.push_str("\n");
                existing.push_str(&observation);
            } else {
                step_observation = Some(observation.clone());
            }
            memory.push(ChatMessage::tool(observation.clone(), "eli.subagents"));
            store
                .append(
                    session_id,
                    &SessionEvent {
                        ts: chrono::Utc::now(),
                        kind: EventKind::Note {
                            content: observation,
                        },
                    },
                )
                .await
                .ok();
        }

        // Capture trajectory
        let _ = trajectory_logger
            .append(&eli_core::trajectory::TrajectoryStep {
                session_id: session_id.to_string(),
                step_index: step as usize,
                timestamp: chrono::Utc::now(),
                input_messages: trajectory_input,
                model_output_raw: out.clone(),
                observation: step_observation,
                usage: state.last_usage.clone(),
            })
            .await;

        match model.status {
            StepStatus::Done => {
                let show_wrap_up = task_had_actions || step > 1;

                let mut fallback = None;
                let synthesis = model
                    .synthesis
                    .as_ref()
                    .filter(|s| synthesis_has_content(s))
                    .or_else(|| {
                        fallback = build_fallback_synthesis(&task_insights, model.notes.trim());
                        fallback.as_ref()
                    });

                if !wants_user_input {
                    if let Some(synthesis) = synthesis {
                        // Only show synthesis box if there's substantial content beyond the step summary
                        if show_wrap_up
                            || !synthesis.summary.is_empty()
                            || !synthesis.next_steps.is_empty()
                        {
                            if emit_cli_chrome {
                                print_synthesis_box(&synthesis_title, synthesis);
                            }
                        }
                        // Skip print_answer_line - step summary already showed the answer
                    }
                    // Skip print_answer_line for notes - step summary already showed them
                }

                if profile == AgentProfile::Research {
                    let status = if wants_user_input {
                        "needs_user_input"
                    } else {
                        "done"
                    };
                    let partial = if synthesis.is_some() {
                        None
                    } else {
                        Some(model.notes.as_str())
                    };

                    match write_research_report_md(
                        project_root,
                        session_id,
                        chat,
                        &root_prompt,
                        synthesis,
                        status,
                        partial,
                    ) {
                        Ok(Some(path)) => {
                            let rel = path.strip_prefix(project_root).unwrap_or(&path);
                            if brief {
                                println!("  saved: {}", rel.display());
                            } else {
                                println!("(saved: {})", rel.display());
                            }

                            let note = format!(
                                "research_report_saved: {}\nstatus: {}\ntitle: {}",
                                rel.display(),
                                status,
                                truncate(&root_prompt, 120)
                            );
                            memory.push(ChatMessage::tool(note.clone(), "eli.research"));
                            store
                                .append(
                                    session_id,
                                    &SessionEvent {
                                        ts: chrono::Utc::now(),
                                        kind: EventKind::Note { content: note },
                                    },
                                )
                                .await
                                .ok();

                            state.record_research_report(
                                ResearchArtifact {
                                    rel_path: rel.display().to_string(),
                                    title: root_prompt.clone(),
                                    status: status.to_string(),
                                    created_utc: chrono::Utc::now().to_rfc3339(),
                                    answer_hint: synthesis
                                        .map(|s| s.answer.clone())
                                        .filter(|s| !s.trim().is_empty()),
                                },
                                24,
                            );

                            let brain_entry = format!(
                                "\n### {} (session {})\n- Research saved: {} ({})\n",
                                chrono::Utc::now().to_rfc3339(),
                                session_id,
                                rel.display(),
                                status
                            );
                            if let Err(e) = append_eli_brain(project_root, &brain_entry) {
                                warn!(
                                    "eli brain: failed to persist research pointer (ignored): {e}"
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(e) => warn!("failed to write research report (ignored): {e}"),
                    }
                }

                // Show final summary for brief mode
                let task_elapsed = task_start.elapsed();
                state.total_work_time += task_elapsed;
                if brief && step > 1 && emit_cli_chrome {
                    println!(
                        "\n{}✓{} done in {} ({} steps)",
                        style::GREEN,
                        style::RESET,
                        format_duration(task_elapsed),
                        step
                    );
                }
                break;
            }
            StepStatus::KeepWorking => {
                if quick_query_mode && !forced_finalize_sent && step >= 6 {
                    forced_finalize_sent = true;
                    current_message = "FINALIZE NOW: You have enough evidence for this quick market question. Do not run more tools. Return status DONE with a concise answer and optional brief summary. For market direction, do NOT use open-vs-previous-close; only report intraday direction from timeseries first-to-latest prices, or state that direction is unavailable.".to_string();
                    current_images.clear();
                    continue;
                }
                if step == max_iters {
                    if emit_cli_chrome {
                        println!("(stopped: max autonomous steps reached)");
                    }
                    if profile == AgentProfile::Research {
                        let synthesis = model
                            .synthesis
                            .as_ref()
                            .filter(|s| synthesis_has_content(s));
                        match write_research_report_md(
                            project_root,
                            session_id,
                            chat,
                            &root_prompt,
                            synthesis,
                            "stopped_max_steps",
                            Some(model.notes.as_str()),
                        ) {
                            Ok(Some(path)) => {
                                let rel = path.strip_prefix(project_root).unwrap_or(&path);
                                if brief {
                                    println!("  saved: {}", rel.display());
                                } else {
                                    println!("(saved: {})", rel.display());
                                }

                                let note = format!(
                                    "research_report_saved: {}\nstatus: stopped_max_steps\ntitle: {}",
                                    rel.display(),
                                    truncate(&root_prompt, 120)
                                );
                                memory.push(ChatMessage::tool(note.clone(), "eli.research"));
                                store
                                    .append(
                                        session_id,
                                        &SessionEvent {
                                            ts: chrono::Utc::now(),
                                            kind: EventKind::Note { content: note },
                                        },
                                    )
                                    .await
                                    .ok();

                                state.record_research_report(
                                    ResearchArtifact {
                                        rel_path: rel.display().to_string(),
                                        title: root_prompt.clone(),
                                        status: "stopped_max_steps".to_string(),
                                        created_utc: chrono::Utc::now().to_rfc3339(),
                                        answer_hint: synthesis
                                            .map(|s| s.answer.clone())
                                            .filter(|s| !s.trim().is_empty()),
                                    },
                                    24,
                                );

                                let brain_entry = format!(
                                    "\n### {} (session {})\n- Research saved: {} (stopped_max_steps)\n",
                                    chrono::Utc::now().to_rfc3339(),
                                    session_id,
                                    rel.display()
                                );
                                if let Err(e) = append_eli_brain(project_root, &brain_entry) {
                                    warn!("eli brain: failed to persist research pointer (ignored): {e}");
                                }
                            }
                            Ok(None) => {}
                            Err(e) => warn!("failed to write research report (ignored): {e}"),
                        }
                    }
                }
            }
        }

        if !chat.auto {
            let task_elapsed = task_start.elapsed();
            state.total_work_time += task_elapsed;
            break;
        }

        if let Some(ask) = model.ask_user {
            if !ask.trim().is_empty() {
                if brief {
                    footer.take();
                }
                let (msg, imgs) = prompt_user(ask.trim())?;
                current_message = msg;
                current_images = imgs;
                continue;
            }
        }

        current_message = "KEEP WORKING".to_string();
    }

    if brief {
        footer.take();
    }

    Ok(())
}

fn print_banner(chat: &eli_core::config::ChatConfig, project_root: &Path, _state: &SessionState) {
    use style::*;

    let model = truncate_middle(&chat.model, 60);
    let root = format_root_path(project_root);
    // ASCII art logo with monochrome gradient (white → gray)
    println!(
        r#"
{W1}{BOLD}  ███████╗██╗     ██╗{RESET}
{W2}{BOLD}  ██╔════╝██║     ██║{RESET}     {WHITE}financial coding agent{RESET}
{W3}{BOLD}  █████╗  ██║     ██║{RESET}     {GRAY}v0.1.0{RESET}
{W4}{BOLD}  ██╔══╝  ██║     ██║{RESET}
{W5}{BOLD}  ███████╗███████╗██║{RESET}
{W6}{BOLD}  ╚══════╝╚══════╝╚═╝{RESET}
"#,
        W1 = "\x1b[38;5;255m", // bright white
        W2 = "\x1b[38;5;252m", // light gray
        W3 = "\x1b[38;5;249m", // medium light
        W4 = "\x1b[38;5;246m", // medium gray
        W5 = "\x1b[38;5;243m", // darker gray
        W6 = "\x1b[38;5;240m", // dark gray
    );

    println!("{}({} / {}){}", GRAY, chat.provider, model, RESET);
    println!("{}cwd{} {}", GRAY, RESET, root);
    println!("{}Auto mode. /help for commands.{}", DARK_GRAY, RESET);
    println!();
}

fn print_step_summary(step: u32, model: &eli_core::contract::ModelResponse) {
    use style::*;

    let mut lines = Vec::new();
    if !model.notes.trim().is_empty() {
        lines.push(format!(
            "{}eli[{}]{} {}",
            CYAN,
            step,
            RESET,
            model.notes.trim()
        ));
    }

    let mut plan_lines = model.plan.lines();
    if let Some(first) = plan_lines.next() {
        if !first.trim().is_empty() {
            lines.push(format!("{}→{} plan: {}", PURPLE, RESET, first.trim()));
        }
    }
    if let Some(second) = plan_lines.next() {
        if !second.trim().is_empty() {
            lines.push(format!("{}→{} next: {}", BLUE, RESET, second.trim()));
        }
    }

    if !model.focus.trim().is_empty() {
        lines.push(format!(
            "{}◆{} focus: {}",
            YELLOW,
            RESET,
            model.focus.trim()
        ));
    }

    if !model.checklist.is_empty() {
        lines.push(format!("{}checklist:{}", GRAY, RESET));
        for item in model.checklist.iter().take(4) {
            if !item.trim().is_empty() {
                lines.push(format!("  {}•{} {}", GREEN, RESET, item.trim()));
            }
        }
        if model.checklist.len() > 4 {
            lines.push(format!(
                "  {}... +{} more{}",
                DARK_GRAY,
                model.checklist.len() - 4,
                RESET
            ));
        }
    }

    let status = match model.status {
        StepStatus::KeepWorking => format!("{}● keep_working{}", YELLOW, RESET),
        StepStatus::Done => format!("{}✓ done{}", GREEN, RESET),
    };
    lines.push(format!("status: {}", status));

    let out = format_indented_block(&lines);
    println!("{}", out);
}

/// Brief step summary for standard mode - one line
fn print_step_summary_brief(
    _step: u32,
    elapsed: Duration,
    model: &eli_core::contract::ModelResponse,
) {
    let _ = elapsed;
    match model.status {
        StepStatus::KeepWorking => {
            // Show focus/plan when still working
            let focus = if model.focus.trim().is_empty() {
                model.notes.lines().next().unwrap_or("").trim()
            } else {
                model.focus.trim()
            };
            if focus.is_empty() {
                return;
            }
            print_history_line(format!("→ {}", focus));
        }
        StepStatus::Done => {
            // Show the actual response/answer unboxed
            let answer = model
                .synthesis
                .as_ref()
                .map(|s| s.answer.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| model.notes.trim());
            if answer.is_empty() {
                return;
            }

            print_history_line(String::new());
            print_markdown(answer);
        }
    };
}

fn extract_insight(
    command_results: &[CommandResult],
    diff_results: &[DiffResult],
) -> Option<String> {
    for result in command_results {
        if let Some(line) = result.stdout.lines().find(|l| !l.trim().is_empty()) {
            return Some(truncate_line(line.trim(), 120));
        }
    }

    if let Some(diff) = diff_results.first() {
        let detail = format!("{} {}", diff.op, diff.path);
        return Some(truncate_line(&detail, 120));
    }

    None
}

fn build_command_digest(result: &CommandResult) -> String {
    let stdout = result.stdout.trim();
    let stdout_bytes = result.stdout.as_bytes().len();
    let stderr_bytes = result.stderr.as_bytes().len();

    if result.returncode != 0 {
        return format!(
            "returncode={} stdout_bytes={} stderr_bytes={}",
            result.returncode, stdout_bytes, stderr_bytes
        );
    }

    if stdout.is_empty() {
        return format!(
            "returncode={} stdout_bytes={} stderr_bytes={}",
            result.returncode, stdout_bytes, stderr_bytes
        );
    }

    if stdout.starts_with("[OUTPUT SUPPRESSED]") {
        let mut parts: Vec<String> = Vec::new();
        if let Some(saved_to) = stdout
            .split("saved_to=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
        {
            parts.push(format!("saved_to={saved_to}"));
        }
        if let Some(bytes) = stdout
            .split('(')
            .nth(1)
            .and_then(|s| s.split(" bytes").next())
        {
            if bytes.chars().all(|c| c.is_ascii_digit()) {
                parts.push(format!("bytes={bytes}"));
            }
        }
        if let Some(points) = stdout
            .split("Data points: ")
            .nth(1)
            .and_then(|s| s.split('.').next())
        {
            let points = points.trim();
            if !points.is_empty() && points.chars().all(|c| c.is_ascii_digit()) {
                parts.push(format!("data_points={points}"));
            }
        }
        if parts.is_empty() {
            parts.push(format!("stdout_bytes={stdout_bytes}"));
        }
        return parts.join(" ");
    }

    if let Some(value) = extract_json_from_stdout(stdout) {
        if let Some(file_digest) = digest_from_ok_path_json(&result.command, &value) {
            return file_digest;
        }
        return digest_from_json_for_command(&result.command, &value, stdout_bytes);
    }

    let lines = stdout.lines().count();
    format!("stdout_bytes={} lines={}", stdout_bytes, lines)
}

fn digest_from_ok_path_json(command: &str, value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let ok = obj.get("ok")?.as_bool()?;
    if !ok {
        return None;
    }
    let path = obj.get("path")?.as_str()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let nested = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    let mut digest = digest_from_json_for_command(command, &nested, raw.as_bytes().len());
    if !digest.contains("saved_to=") {
        digest = format!("saved_to={} {}", path, digest);
    }
    Some(digest)
}

fn digest_from_json_for_command(command: &str, value: &serde_json::Value, bytes: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("bytes={bytes}"));

    match value {
        serde_json::Value::Array(items) => {
            parts.push(format!("items={}", items.len()));
        }
        serde_json::Value::Object(map) => {
            let mut array_parts: Vec<String> = Vec::new();
            for (key, val) in map.iter() {
                if let serde_json::Value::Array(items) = val {
                    array_parts.push(format!("{key}={}", items.len()));
                }
            }
            if !array_parts.is_empty() {
                array_parts.truncate(4);
                parts.extend(array_parts);
            } else {
                parts.push(format!("keys={}", map.len()));
            }
            if let Some(ts) = map
                .get("generated_at")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
            {
                parts.push(format!("generated_at={ts}"));
            } else if let Some(ts) = map
                .get("fetched_at")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
            {
                parts.push(format!("fetched_at={ts}"));
            }
        }
        _ => {}
    }

    let command_parts = command_summary_parts(command, value, 5);
    parts.extend(command_parts);
    parts.join(" ")
}

fn command_summary_parts(
    command: &str,
    value: &serde_json::Value,
    max_parts: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    let path = extract_eli_tool_path(command).unwrap_or_default();
    if path.len() >= 2 && path[0] == "finance" && path[1] == "timeseries" {
        out.extend(timeseries_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "snapshot" {
        out.extend(snapshot_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "fundamentals" {
        out.extend(fundamentals_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && (path[1] == "filings" || path[1] == "sec")
    {
        out.extend(filings_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "news" {
        out.extend(news_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "macro" {
        out.extend(macro_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "schedule" {
        out.extend(schedule_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "prices" {
        out.extend(prices_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "odds" {
        out.extend(odds_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "options" {
        out.extend(options_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "search" {
        out.extend(search_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "sync" {
        out.extend(sync_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "web" && path[1] == "search" {
        out.extend(web_search_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "web" && path[1] == "read" {
        out.extend(web_read_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "web" && path[1] == "extract" {
        out.extend(web_extract_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "web" && path[1] == "crawl" {
        out.extend(web_crawl_summary_parts(value));
    }
    out.truncate(max_parts);
    out
}

fn command_summary_lines(
    command: &str,
    value: &serde_json::Value,
    max_lines: usize,
) -> Vec<String> {
    let parts = command_summary_parts(command, value, max_lines);
    parts
        .into_iter()
        .map(|p| format!("insight: {p}"))
        .collect::<Vec<_>>()
}

fn fmt_pct(v: f64) -> String {
    format!("{:.2}%", v * 100.0)
}

fn top_two_by_abs_change(entries: &[(String, f64)]) -> Option<(String, f64, String, f64)> {
    if entries.len() < 2 {
        return None;
    }
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let lo = sorted.last()?.clone();
    let hi = sorted.first()?.clone();
    Some((hi.0, hi.1, lo.0, lo.1))
}

fn timeseries_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let series = map
        .get("series")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let series_count = series.len();
    let total_points: usize = series
        .iter()
        .filter_map(|s| s.get("candles").and_then(|v| v.as_array()).map(|a| a.len()))
        .sum();
    if series_count > 0 {
        out.push(format!("series={series_count}"));
    }
    if total_points > 0 {
        out.push(format!("points={total_points}"));
    }

    if let Some(stats) = map
        .get("analytics")
        .and_then(|a| a.get("stats"))
        .and_then(|v| v.as_object())
    {
        let mut returns: Vec<(String, f64)> = Vec::new();
        let mut vols: Vec<(String, f64)> = Vec::new();
        let mut sharpes: Vec<(String, f64)> = Vec::new();
        for (ticker, statv) in stats {
            let Some(stat) = statv.as_object() else {
                continue;
            };
            if let Some(r) = stat
                .get("total_return")
                .and_then(|v| v.as_f64())
                .or_else(|| stat.get("return_total").and_then(|v| v.as_f64()))
            {
                returns.push((ticker.clone(), r));
            }
            if let Some(v) = stat
                .get("annualized_vol")
                .and_then(|v| v.as_f64())
                .or_else(|| stat.get("vol_annualized").and_then(|v| v.as_f64()))
            {
                vols.push((ticker.clone(), v));
            }
            if let Some(s) = stat
                .get("sharpe_ratio")
                .and_then(|v| v.as_f64())
                .or_else(|| stat.get("sharpe").and_then(|v| v.as_f64()))
            {
                sharpes.push((ticker.clone(), s));
            }
        }
        if let Some((hi_t, hi_v, lo_t, lo_v)) = top_two_by_abs_change(&returns) {
            out.push(format!("best_return={hi_t}:{}", fmt_pct(hi_v)));
            out.push(format!("worst_return={lo_t}:{}", fmt_pct(lo_v)));
        }
        if !vols.is_empty() {
            vols.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            out.push(format!("highest_vol={}:{}", vols[0].0, fmt_pct(vols[0].1)));
        }
        if !sharpes.is_empty() {
            sharpes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            out.push(format!("best_sharpe={}:{:.2}", sharpes[0].0, sharpes[0].1));
        }
    }

    if let Some(cm) = map
        .get("analytics")
        .and_then(|a| a.get("correlation_matrix"))
        .and_then(|v| v.as_object())
    {
        let mut pairs: Vec<(String, String, f64)> = Vec::new();
        for (a, rowv) in cm {
            let Some(row) = rowv.as_object() else {
                continue;
            };
            for (b, cv) in row {
                if a >= b {
                    continue;
                }
                if let Some(c) = cv.as_f64() {
                    pairs.push((a.clone(), b.clone(), c));
                }
            }
        }
        if !pairs.is_empty() {
            let avg = pairs.iter().map(|p| p.2).sum::<f64>() / pairs.len() as f64;
            pairs.sort_by(|x, y| x.2.partial_cmp(&y.2).unwrap_or(std::cmp::Ordering::Equal));
            let low = &pairs[0];
            let high = &pairs[pairs.len() - 1];
            out.push(format!("corr_avg={avg:.3}"));
            out.push(format!("corr_max={}-{}:{:.3}", high.0, high.1, high.2));
            out.push(format!("corr_min={}-{}:{:.3}", low.0, low.1, low.2));
        }
    }
    out
}

fn snapshot_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if let Some(weights) = map
        .get("analytics")
        .and_then(|a| a.get("market_cap_weights"))
        .and_then(|v| v.as_object())
    {
        let mut w: Vec<(String, f64)> = weights
            .iter()
            .filter_map(|(k, v)| v.as_f64().map(|x| (k.clone(), x)))
            .collect();
        if !w.is_empty() {
            w.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let top3 = w.iter().take(3).map(|x| x.1).sum::<f64>();
            out.push(format!("top_weight={}:{}", w[0].0, fmt_pct(w[0].1)));
            out.push(format!("top3_weight={}", fmt_pct(top3)));
        }
    }
    if let Some(snaps) = map.get("snapshots").and_then(|v| v.as_array()) {
        let mut caps: Vec<(String, f64)> = Vec::new();
        for s in snaps {
            let t = s.get("ticker").and_then(|v| v.as_str()).unwrap_or("?");
            if let Some(cap) = s.get("market_cap").and_then(|v| v.as_f64()) {
                caps.push((t.to_string(), cap));
            }
        }
        if !caps.is_empty() {
            caps.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            out.push(format!("largest_cap={}", caps[0].0));
        }
    }
    out
}

fn fundamentals_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let Some(first) = map
        .get("statements")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
    else {
        return out;
    };
    let rev = first.get("total_revenue").and_then(|v| v.as_f64());
    let gross = first.get("gross_profit").and_then(|v| v.as_f64());
    let op = first.get("operating_income").and_then(|v| v.as_f64());
    let fcf = first.get("free_cash_flow").and_then(|v| v.as_f64());
    let cash = first.get("cash_and_equivalents").and_then(|v| v.as_f64());
    let debt = first.get("total_debt").and_then(|v| v.as_f64());
    if let (Some(g), Some(r)) = (gross, rev) {
        out.push(format!("gross_margin={}", fmt_pct(g / r)));
    }
    if let (Some(o), Some(r)) = (op, rev) {
        out.push(format!("op_margin={}", fmt_pct(o / r)));
    }
    if let (Some(f), Some(r)) = (fcf, rev) {
        out.push(format!("fcf_margin={}", fmt_pct(f / r)));
    }
    if let (Some(c), Some(d)) = (cash, debt) {
        out.push(format!("net_cash={:.1}B", (c - d) / 1e9));
    }
    out
}

fn filings_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let Some(filings) = map.get("filings").and_then(|v| v.as_array()) else {
        return out;
    };
    out.push(format!("filings={}", filings.len()));
    if let Some(first) = filings.first() {
        let form = first.get("form").and_then(|v| v.as_str()).unwrap_or("?");
        let date = first
            .get("filing_date")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        out.push(format!("latest={form}@{date}"));
    }
    out
}

fn news_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let Some(news) = map.get("news").and_then(|v| v.as_array()) else {
        return out;
    };
    out.push(format!("articles={}", news.len()));
    if let Some(first) = news.first() {
        let t = first.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if !t.is_empty() {
            out.push(format!("top_headline={}", truncate_line(t, 50)));
        }
    }
    out
}

fn macro_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let Some(ind) = map.get("indicators").and_then(|v| v.as_array()) else {
        return out;
    };
    let mut changes: Vec<(String, f64)> = Vec::new();
    for x in ind {
        let s = x.get("symbol").and_then(|v| v.as_str()).unwrap_or("?");
        if let Some(c) = x.get("change_1y").and_then(|v| v.as_f64()) {
            changes.push((s.to_string(), c));
        }
    }
    out.push(format!("indicators={}", ind.len()));
    if !changes.is_empty() {
        changes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let hi = &changes[0];
        let lo = &changes[changes.len() - 1];
        out.push(format!("max_1y_change={}:{}", hi.0, fmt_pct(hi.1)));
        out.push(format!("min_1y_change={}:{}", lo.0, fmt_pct(lo.1)));
    }
    out
}

fn schedule_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let earnings = map
        .get("earnings")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let macro_n = map
        .get("macro")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    out.push(format!("earnings={earnings}"));
    out.push(format!("macro={macro_n}"));
    if let Some(arr) = map.get("earnings").and_then(|v| v.as_array()) {
        let mut pre = 0usize;
        for e in arr {
            if e.get("time").and_then(|v| v.as_str()) == Some("pre-market") {
                pre += 1;
            }
        }
        if !arr.is_empty() {
            out.push(format!(
                "premarket_share={:.1}%",
                100.0 * pre as f64 / arr.len() as f64
            ));
        }
    }
    out
}

fn prices_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if let Some(status) = map.get("status").and_then(|v| v.as_str()) {
        out.push(format!("status={status}"));
    }
    if let Some(prices) = map.get("prices").and_then(|v| v.as_array()) {
        out.push(format!("prices={}", prices.len()));
    }
    if let Some(cands) = map
        .get("disambiguation")
        .and_then(|d| d.get("candidates"))
        .and_then(|v| v.as_array())
    {
        out.push(format!("candidates={}", cands.len()));
    }
    out
}

fn odds_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if let Some(sem) = map.get("field_semantics").and_then(|v| v.as_object()) {
        let prob = sem
            .get("probability_scale")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let yes_units = sem
            .get("yes_price_units")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let vol_units = sem
            .get("volume_units")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        out.push(format!(
            "semantics=probability:{prob},yes_price:{yes_units},volume:{vol_units}"
        ));
    }
    let Some(markets) = map.get("markets").and_then(|v| v.as_array()) else {
        return out;
    };
    out.push(format!("markets={}", markets.len()));
    let mut probs = Vec::new();
    let mut spreads = Vec::new();
    let mut top_vol: Option<(String, f64)> = None;
    let mut open_n = 0usize;
    for m in markets {
        if m.get("status").and_then(|v| v.as_str()) == Some("open") {
            open_n += 1;
        }
        if let Some(p) = m.get("probability_yes").and_then(|v| v.as_f64()) {
            probs.push(p);
        }
        let bid = m.get("yes_bid").and_then(|v| v.as_f64());
        let ask = m.get("yes_ask").and_then(|v| v.as_f64());
        if let (Some(b), Some(a)) = (bid, ask) {
            spreads.push(a - b);
        }
        if let Some(vol) = m.get("volume").and_then(|v| v.as_f64()) {
            let t = m
                .get("ticker")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            match &top_vol {
                Some((_, best)) if vol <= *best => {}
                _ => top_vol = Some((t, vol)),
            }
        }
    }
    if !markets.is_empty() {
        out.push(format!(
            "open_share={:.1}%",
            100.0 * open_n as f64 / markets.len() as f64
        ));
    }
    if !probs.is_empty() {
        let avg = probs.iter().sum::<f64>() / probs.len() as f64;
        out.push(format!("prob_yes_avg={avg:.3}"));
    }
    if !spreads.is_empty() {
        let avg = spreads.iter().sum::<f64>() / spreads.len() as f64;
        let max = spreads
            .iter()
            .copied()
            .fold(f64::MIN, |a, b| if b > a { b } else { a });
        out.push(format!("spread_avg={avg:.3}"));
        out.push(format!("spread_max={max:.3}"));
    }
    if let Some((ticker, vol)) = top_vol {
        out.push(format!("top_volume={ticker}:{vol:.0}"));
    }
    out
}

fn options_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let metrics = map.get("metrics").and_then(|v| v.as_object());
    if let Some(m) = metrics {
        if let Some(v) = m.get("put_call_ratio_volume").and_then(|v| v.as_f64()) {
            out.push(format!("pcr_vol={v:.2}"));
        }
        if let Some(v) = m.get("put_call_ratio_oi").and_then(|v| v.as_f64()) {
            out.push(format!("pcr_oi={v:.2}"));
        }
        if let Some(v) = m.get("atm_iv_call").and_then(|v| v.as_f64()) {
            out.push(format!("atm_iv_call={:.2}%", v));
        }
        if let Some(v) = m.get("atm_iv_put").and_then(|v| v.as_f64()) {
            out.push(format!("atm_iv_put={:.2}%", v));
        }
        if let Some(v) = m.get("max_pain").and_then(|v| v.as_f64()) {
            out.push(format!("max_pain={v:.2}"));
        }
    }
    out
}

fn search_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let results = map
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    out.push(format!("results={}", results.len()));
    if let Some(first) = results.first() {
        let sym = first.get("symbol").and_then(|v| v.as_str()).unwrap_or("?");
        let typ = first
            .get("asset_type")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        out.push(format!("top_match={sym}:{typ}"));
    }
    if let Some(ms) = map.get("macro_suggestions").and_then(|v| v.as_array()) {
        out.push(format!("macro_suggestions={}", ms.len()));
    }
    out
}

fn sync_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if let Some(n) = map.get("total_markets").and_then(|v| v.as_i64()) {
        out.push(format!("total_markets={n}"));
    }
    if let Some(n) = map.get("total_events").and_then(|v| v.as_i64()) {
        out.push(format!("total_events={n}"));
    }
    if let Some(sources) = map.get("sources").and_then(|v| v.as_array()) {
        let ok = sources
            .iter()
            .filter(|s| s.get("ok").and_then(|v| v.as_bool()) == Some(true))
            .count();
        out.push(format!("sources_ok={ok}/{}", sources.len()));
    }
    if let Some(analysis) = map.get("analysis").and_then(|v| v.as_object()) {
        if let Some(v_cents) = analysis.get("total_volume").and_then(|v| v.as_i64()) {
            out.push(format!("total_volume_cents={v_cents}"));
            out.push(format!("total_volume_usd={:.2}", v_cents as f64 / 100.0));
        }
        if let Some(v_pct) = analysis
            .get("extreme_prob_volume_share_pct")
            .and_then(|v| v.as_f64())
        {
            out.push(format!("extreme_prob_volume_share_pct={v_pct:.2}"));
        }
    }
    out
}

fn web_search_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let hits = map
        .get("hits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    out.push(format!("hits={}", hits.len()));
    if let Some(first) = hits.first() {
        if let Some(title) = first.get("title").and_then(|v| v.as_str()) {
            out.push(format!("top_hit={}", truncate_line(title, 45)));
        }
        if let Some(url) = first.get("url").and_then(|v| v.as_str()) {
            out.push(format!("top_domain={}", domain_of(url)));
        }
    }
    out
}

fn web_read_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if let Some(title) = map.get("title").and_then(|v| v.as_str()) {
        out.push(format!("title={}", truncate_line(title, 45)));
    }
    if let Some(text) = map.get("text").and_then(|v| v.as_str()) {
        out.push(format!("chars={}", text.chars().count()));
        out.push(format!("words={}", text.split_whitespace().count()));
    }
    out
}

fn web_extract_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if let Some(n) = map.get("word_count").and_then(|v| v.as_i64()) {
        out.push(format!("word_count={n}"));
    }
    if let Some(b) = map.get("bullets").and_then(|v| v.as_array()) {
        out.push(format!("bullets={}", b.len()));
    }
    out
}

fn web_crawl_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if let Some(n) = map.get("pages_crawled").and_then(|v| v.as_i64()) {
        out.push(format!("pages={n}"));
    }
    if let Some(ms) = map.get("duration_ms").and_then(|v| v.as_i64()) {
        out.push(format!("duration_ms={ms}"));
    }
    if let Some(pages) = map
        .get("pages")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
    {
        if let Some(title) = pages.get("title").and_then(|v| v.as_str()) {
            out.push(format!("first_title={}", truncate_line(title, 45)));
        }
    }
    out
}

fn domain_of(url: &str) -> String {
    let mut s = url;
    if let Some(x) = s.strip_prefix("https://") {
        s = x;
    } else if let Some(x) = s.strip_prefix("http://") {
        s = x;
    }
    s.split('/').next().unwrap_or(s).to_string()
}

fn synthesis_has_content(synthesis: &eli_core::contract::Synthesis) -> bool {
    !synthesis.summary.is_empty()
        || !synthesis.next_steps.is_empty()
        || !synthesis.answer.trim().is_empty()
}

fn format_synthesis_title(_user_message: &str) -> String {
    String::new()
}

fn print_markdown(text: &str) {
    let skin = MadSkin::default();
    skin.print_text(text);
}

fn print_synthesis_box(title: &str, synthesis: &eli_core::contract::Synthesis) {
    use style::*;

    let mut lines = Vec::new();
    // Header removed as per user request ("eli" name gone)
    if !title.trim().is_empty() {
        lines.push(format!("{}{}{}", GRAY, title, RESET));
    }

    let answer_text = synthesis.answer.trim();
    let mut seen = std::collections::HashSet::new();
    let summary: Vec<String> = synthesis
        .summary
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter(|s| !summary_repeats_answer(s, answer_text))
        .filter(|s| seen.insert(s.to_string()))
        .take(3)
        .map(|s| format!("{}•{} {}", GREEN, RESET, s))
        .collect();
    if !summary.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(summary);
    }

    if !answer_text.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!("{}◆{} {}", CYAN, RESET, answer_text));
    }

    let next_steps: Vec<String> = synthesis
        .next_steps
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .take(3)
        .map(|s| s.to_string())
        .collect();
    if !next_steps.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!("{}next steps:{}", PURPLE, RESET));
        for (idx, step) in next_steps.iter().enumerate() {
            lines.push(format!("{}{}. {}{}", BLUE, idx + 1, RESET, step));
        }
    }

    if lines.len() > 1 {
        let out = format_indented_block(&lines);
        println!("{}", out);
    }
}

fn normalize_for_dedupe(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn summary_repeats_answer(summary: &str, answer: &str) -> bool {
    if answer.trim().is_empty() {
        return false;
    }
    let s = normalize_for_dedupe(summary);
    let a = normalize_for_dedupe(answer);
    if s.len() < 16 || a.len() < 16 {
        return false;
    }
    a.contains(&s) || s.contains(&a)
}

fn build_fallback_synthesis(
    insights: &[String],
    answer: &str,
) -> Option<eli_core::contract::Synthesis> {
    let summary: Vec<String> = insights
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .take(5)
        .map(|s| s.to_string())
        .collect();
    let answer = answer.trim();
    if summary.is_empty() && answer.is_empty() {
        return None;
    }
    Some(eli_core::contract::Synthesis {
        summary,
        answer: answer.to_string(),
        next_steps: Vec::new(),
    })
}

fn print_subagent_results(results: &[SubagentResult]) {
    use style::*;

    if results.is_empty() {
        return;
    }
    let mut lines = Vec::new();
    lines.push(format!("{}{}subagents{}", BOLD, PURPLE, RESET));
    for result in results {
        if let Some(err) = &result.error {
            lines.push(format!(
                "{}✗{} {}: {}error{} {}",
                RED, RESET, result.name, RED, RESET, err
            ));
            continue;
        }
        if result.output.trim().is_empty() {
            lines.push(format!(
                "{}✓{} {}: {}(no output){}",
                GREEN, RESET, result.name, GRAY, RESET
            ));
            continue;
        }
        lines.push(format!("{}✓{} {}:{}", GREEN, RESET, result.name, RESET));
        for line in result.output.lines().take(6) {
            if !line.trim().is_empty() {
                lines.push(format!("  {}{}{}", GRAY, line.trim(), RESET));
            }
        }
    }
    let out = format_indented_block(&lines);
    println!("{}", out);
}

fn build_subagent_observation(results: &[SubagentResult]) -> String {
    let mut out = String::from("subagents:\n");
    for result in results {
        out.push_str(&format!("- {}\n", result.name));
        if let Some(err) = &result.error {
            out.push_str(&format!("  error: {err}\n"));
            continue;
        }
        if result.output.trim().is_empty() {
            out.push_str("  (no output)\n");
            continue;
        }
        for line in result.output.lines() {
            if line.trim().is_empty() {
                continue;
            }
            out.push_str(&format!("  {line}\n", line = line.trim()));
        }
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════════════
// STYLING CONSTANTS
// ═══════════════════════════════════════════════════════════════════════════════

#[allow(dead_code)]
mod style {
    // Box drawing chars (rounded)
    pub const TL: &str = "╭"; // top-left
    pub const TR: &str = "╮"; // top-right
    pub const BL: &str = "╰"; // bottom-left
    pub const BR: &str = "╯"; // bottom-right
    pub const H: &str = "─"; // horizontal
    pub const V: &str = "│"; // vertical

    // Colors (ANSI 256 / RGB where supported)
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";

    // Gradient palette for eli branding
    pub const CYAN: &str = "\x1b[38;5;51m"; // bright cyan
    pub const BLUE: &str = "\x1b[38;5;39m"; // bright blue
    pub const PURPLE: &str = "\x1b[38;5;141m"; // lavender
    pub const PINK: &str = "\x1b[38;5;213m"; // pink
    pub const GREEN: &str = "\x1b[38;5;120m"; // mint green
    pub const YELLOW: &str = "\x1b[38;5;227m"; // soft yellow
    pub const ORANGE: &str = "\x1b[38;5;215m"; // peach
    pub const RED: &str = "\x1b[38;5;203m"; // coral red
    pub const GRAY: &str = "\x1b[38;5;245m"; // medium gray
    pub const DARK_GRAY: &str = "\x1b[38;5;238m"; // dark gray
    pub const WHITE: &str = "\x1b[38;5;255m"; // bright white

    // Semantic colors
    pub const SUCCESS: &str = "\x1b[38;5;120m"; // mint
    pub const ERROR: &str = "\x1b[38;5;203m"; // coral
    pub const WARN: &str = "\x1b[38;5;215m"; // peach
    pub const INFO: &str = "\x1b[38;5;111m"; // soft blue
    pub const MUTED: &str = "\x1b[38;5;245m"; // gray

    // Spinner frames handled by indicatif (no manual frames here).
}

fn split_leading_spaces(s: &str) -> (String, &str) {
    let count = s.chars().take_while(|c| *c == ' ').count();
    let (indent, rest) = s.split_at(count);
    (indent.to_string(), rest)
}

fn split_bullet_prefix(s: &str) -> (String, String) {
    let candidates = ["- ", "* ", "• ", "=> ", "→ "];
    for cand in candidates {
        if s.starts_with(cand) {
            return (cand.to_string(), s[cand.len()..].to_string());
        }
    }
    if let Some(pos) = s.find(". ") {
        if s[..pos].chars().all(|c| c.is_ascii_digit()) {
            return (s[..pos + 2].to_string(), s[pos + 2..].to_string());
        }
    }
    (String::new(), s.to_string())
}

fn format_box_string(lines: &[String]) -> String {
    format_indented_block(lines)
}

fn format_indented_block(lines: &[String]) -> String {
    if lines.is_empty() {
        return String::new();
    }

    let (term_width, _term_height) = terminal_size();
    if term_width < 20 {
        return lines.join("\n");
    }

    let term_width = term_width.min(140);
    let max_content_width = term_width.saturating_sub(1).max(1);
    let mut wrapped_lines = Vec::new();
    for line in lines {
        let clean = strip_ansi(line);
        if clean.trim().is_empty() {
            wrapped_lines.push(String::new());
            continue;
        }

        let (indent, rest) = split_leading_spaces(&clean);
        let (prefix, content) = split_bullet_prefix(rest);
        let full = format!("{prefix}{content}");
        let subsequent_indent = if prefix.is_empty() {
            indent.clone()
        } else {
            format!("{}{}", indent, " ".repeat(prefix.width()))
        };

        let options = WrapOptions::new(max_content_width)
            .break_words(true)
            .initial_indent(&indent)
            .subsequent_indent(&subsequent_indent);
        let wrapped = wrap(&full, &options);
        for line in wrapped {
            wrapped_lines.push(line.into_owned());
        }
    }

    let mut out = wrapped_lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn tail_to_width(input: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut width = 0usize;
    for ch in input.chars().rev() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > max_width {
            break;
        }
        out.insert(0, ch);
        width += w;
    }
    out
}

fn flush_buffer(out: &mut std::io::Stdout, buf: &Buffer, rect: Rect, top: u16) {
    let mut current_style = Style::default();
    for y in 0..rect.height {
        queue!(out, cursor::MoveTo(0, top + y)).ok();
        for x in 0..rect.width {
            let cell = buf.get(x, y);
            let cell_style = cell.style();
            if cell_style != current_style {
                apply_style(out, cell_style);
                current_style = cell_style;
            }
            queue!(out, crossterm::style::Print(cell.symbol())).ok();
        }
        queue!(out, SetAttribute(Attribute::Reset), ResetColor).ok();
        current_style = Style::default();
    }
}

fn apply_style(out: &mut std::io::Stdout, style: Style) {
    queue!(out, SetAttribute(Attribute::Reset), ResetColor).ok();
    if let Some(fg) = style.fg {
        queue!(out, SetForegroundColor(map_color(fg))).ok();
    }
    if let Some(bg) = style.bg {
        queue!(out, SetBackgroundColor(map_color(bg))).ok();
    }
    let mods = style.add_modifier;
    if mods.contains(Modifier::BOLD) {
        queue!(out, SetAttribute(Attribute::Bold)).ok();
    }
    if mods.contains(Modifier::DIM) {
        queue!(out, SetAttribute(Attribute::Dim)).ok();
    }
    if mods.contains(Modifier::ITALIC) {
        queue!(out, SetAttribute(Attribute::Italic)).ok();
    }
    if mods.contains(Modifier::UNDERLINED) {
        queue!(out, SetAttribute(Attribute::Underlined)).ok();
    }
    if mods.contains(Modifier::REVERSED) {
        queue!(out, SetAttribute(Attribute::Reverse)).ok();
    }
    if mods.contains(Modifier::HIDDEN) {
        queue!(out, SetAttribute(Attribute::Hidden)).ok();
    }
    if mods.contains(Modifier::CROSSED_OUT) {
        queue!(out, SetAttribute(Attribute::CrossedOut)).ok();
    }
    if mods.contains(Modifier::SLOW_BLINK) {
        queue!(out, SetAttribute(Attribute::SlowBlink)).ok();
    }
    if mods.contains(Modifier::RAPID_BLINK) {
        queue!(out, SetAttribute(Attribute::RapidBlink)).ok();
    }
}

fn map_color(color: Color) -> crossterm::style::Color {
    match color {
        Color::Reset => crossterm::style::Color::Reset,
        Color::Black => crossterm::style::Color::Black,
        Color::Red => crossterm::style::Color::DarkRed,
        Color::Green => crossterm::style::Color::DarkGreen,
        Color::Yellow => crossterm::style::Color::DarkYellow,
        Color::Blue => crossterm::style::Color::DarkBlue,
        Color::Magenta => crossterm::style::Color::DarkMagenta,
        Color::Cyan => crossterm::style::Color::DarkCyan,
        Color::Gray => crossterm::style::Color::Grey,
        Color::DarkGray => crossterm::style::Color::DarkGrey,
        Color::LightRed => crossterm::style::Color::Red,
        Color::LightGreen => crossterm::style::Color::Green,
        Color::LightYellow => crossterm::style::Color::Yellow,
        Color::LightBlue => crossterm::style::Color::Blue,
        Color::LightMagenta => crossterm::style::Color::Magenta,
        Color::LightCyan => crossterm::style::Color::Cyan,
        Color::White => crossterm::style::Color::White,
        Color::Indexed(idx) => crossterm::style::Color::AnsiValue(idx),
        Color::Rgb(r, g, b) => crossterm::style::Color::Rgb { r, g, b },
    }
}

fn footer_title(
    phase: &str,
    spinner_idx: usize,
    queue_len: usize,
    elapsed: Duration,
    total_tokens: u32,
    mode: Option<PromptMode>,
) -> String {
    let spinner = FOOTER_SPINNER[spinner_idx % FOOTER_SPINNER.len()];
    let queue_indicator = if queue_len > 0 {
        format!(" [{}Q]", queue_len)
    } else {
        String::new()
    };
    let mode_chip = match mode {
        Some(PromptMode::Ask) => " [ASK]",
        Some(PromptMode::Plan) => " [PLAN]",
        Some(PromptMode::Auto) => " [AUTO]",
        None => "",
    };
    format!(
        "{spinner} {phase}{queue_indicator}{mode_chip} [{}s] {total_tokens} tokens",
        elapsed.as_secs()
    )
}

fn render_footer(
    footer: &mut Option<FooterUi>,
    phase: &str,
    spinner_idx: usize,
    elapsed: Duration,
    state: &SessionState,
    mode: Option<PromptMode>,
) {
    if footer.is_none() {
        *footer = Some(FooterUi::enable());
    }
    if let Some(footer) = footer.as_mut() {
        let title = footer_title(
            phase,
            spinner_idx,
            state.queue_len(),
            elapsed,
            state.total_usage.total_tokens,
            mode,
        );
        footer.render(&title, &state.input_buffer, state.cursor_pos);
    }
}

fn drain_run_key_events(
    state: &mut SessionState,
    interrupted: &mut bool,
    interrupted_by_esc: &mut bool,
) -> bool {
    let mut changed = false;
    while ct_event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let Ok(ev) = ct_event::read() else {
            continue;
        };
        match ev {
            CtEvent::Resize(_, _) => {
                changed = true;
            }
            CtEvent::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    CtKeyCode::Char(c) => {
                        state.input_buffer.insert(state.cursor_pos, c);
                        state.cursor_pos += 1;
                        changed = true;
                    }
                    CtKeyCode::Backspace => {
                        if state.cursor_pos > 0 {
                            state.cursor_pos -= 1;
                            state.input_buffer.remove(state.cursor_pos);
                            changed = true;
                        }
                    }
                    CtKeyCode::Delete => {
                        if state.cursor_pos < state.input_buffer.len() {
                            state.input_buffer.remove(state.cursor_pos);
                            changed = true;
                        }
                    }
                    CtKeyCode::Left => {
                        if state.cursor_pos > 0 {
                            state.cursor_pos -= 1;
                            changed = true;
                        }
                    }
                    CtKeyCode::Right => {
                        if state.cursor_pos < state.input_buffer.len() {
                            state.cursor_pos += 1;
                            changed = true;
                        }
                    }
                    CtKeyCode::Home => {
                        state.cursor_pos = 0;
                        changed = true;
                    }
                    CtKeyCode::End => {
                        state.cursor_pos = state.input_buffer.len();
                        changed = true;
                    }
                    CtKeyCode::Enter => {
                        let trimmed = state.input_buffer.trim().to_string();
                        if !trimmed.is_empty() {
                            if trimmed == "/stop" || trimmed == "/interrupt" {
                                *interrupted = true;
                                state.input_buffer.clear();
                                state.cursor_pos = 0;
                                changed = true;
                                break;
                            }
                            print_history_line(format!(
                                "{}›{} {}",
                                style::CYAN,
                                style::RESET,
                                trimmed
                            ));
                            state.queue_prompt(trimmed.clone());
                            state.prompt_history.push(trimmed);
                            state.input_buffer.clear();
                            state.cursor_pos = 0;
                            changed = true;
                        }
                    }
                    CtKeyCode::Esc => {
                        *interrupted = true;
                        *interrupted_by_esc = true;
                        changed = true;
                        break;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    changed
}

fn drain_run_key_events_queue_only(state: &mut SessionState) -> bool {
    let mut changed = false;
    while ct_event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let Ok(ev) = ct_event::read() else {
            continue;
        };
        match ev {
            CtEvent::Resize(_, _) => {
                changed = true;
            }
            CtEvent::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    CtKeyCode::Char(c) => {
                        state.input_buffer.insert(state.cursor_pos, c);
                        state.cursor_pos += 1;
                        changed = true;
                    }
                    CtKeyCode::Backspace => {
                        if state.cursor_pos > 0 {
                            state.cursor_pos -= 1;
                            state.input_buffer.remove(state.cursor_pos);
                            changed = true;
                        }
                    }
                    CtKeyCode::Delete => {
                        if state.cursor_pos < state.input_buffer.len() {
                            state.input_buffer.remove(state.cursor_pos);
                            changed = true;
                        }
                    }
                    CtKeyCode::Left => {
                        if state.cursor_pos > 0 {
                            state.cursor_pos -= 1;
                            changed = true;
                        }
                    }
                    CtKeyCode::Right => {
                        if state.cursor_pos < state.input_buffer.len() {
                            state.cursor_pos += 1;
                            changed = true;
                        }
                    }
                    CtKeyCode::Home => {
                        state.cursor_pos = 0;
                        changed = true;
                    }
                    CtKeyCode::End => {
                        state.cursor_pos = state.input_buffer.len();
                        changed = true;
                    }
                    CtKeyCode::Enter => {
                        let trimmed = state.input_buffer.trim().to_string();
                        if !trimmed.is_empty() {
                            print_history_line(format!(
                                "{}›{} {}",
                                style::CYAN,
                                style::RESET,
                                trimmed
                            ));
                            state.queue_prompt(trimmed.clone());
                            state.prompt_history.push(trimmed);
                            state.input_buffer.clear();
                            state.cursor_pos = 0;
                            changed = true;
                        }
                    }
                    CtKeyCode::Esc => {
                        state.input_buffer.clear();
                        state.cursor_pos = 0;
                        changed = true;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    changed
}

fn render_ratatui_panel(title: &str, body: &str) -> String {
    let (width, _) = terminal_size();
    let width = width.min(140).max(20);
    let inner_width = width.saturating_sub(2).max(1);
    let wrapped = wrap(body, WrapOptions::new(inner_width));
    let height = wrapped.len().saturating_add(2).max(3);
    let rect = Rect::new(0, 0, width as u16, height as u16);
    let mut buf = Buffer::empty(rect);
    let paragraph = Paragraph::new(wrapped.join("\n"))
        .block(Block::default().title(title).borders(Borders::ALL));
    paragraph.render(rect, &mut buf);
    buffer_to_lines(buf, rect).join("\n")
}

fn buffer_to_lines(buf: Buffer, rect: Rect) -> Vec<String> {
    let mut lines = Vec::new();
    for y in 0..rect.height {
        let mut line = String::new();
        for x in 0..rect.width {
            let cell = buf.get(x, y);
            line.push_str(cell.symbol());
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

/// Strip ANSI escape sequences for length calculation
fn strip_ansi(s: &str) -> String {
    let mut result = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            result.push(c);
            continue;
        }

        // Escape sequence.
        match it.next() {
            Some('[') => {
                // CSI: ESC [ ... <final byte>
                while let Some(ch) = it.next() {
                    if ('@'..='~').contains(&ch) {
                        break;
                    }
                }
            }
            Some(']') => {
                // OSC: ESC ] ... BEL | ESC \
                while let Some(ch) = it.next() {
                    if ch == '\x07' {
                        break;
                    }
                    if ch == '\x1b' {
                        if let Some('\\') = it.peek().copied() {
                            let _ = it.next();
                            break;
                        }
                    }
                }
            }
            Some(_) | None => {}
        }
    }
    result
}

#[allow(dead_code)]
fn print_box(lines: &[String]) {
    let out = format_box_string(lines);
    if !out.is_empty() {
        println!("{out}");
    }
}

fn truncate_line(input: &str, max: usize) -> String {
    if input.len() <= max {
        return input.to_string();
    }
    input.chars().take(max).collect()
}

fn truncate_middle(input: &str, max: usize) -> String {
    if input.len() <= max {
        return input.to_string();
    }
    let total = max;
    let head_len = total / 2;
    let tail_len = total - head_len;
    let head: String = input.chars().take(head_len).collect();
    let tail: String = input
        .chars()
        .rev()
        .take(tail_len)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{}{}", head, tail)
}

fn format_root_path(path: &Path) -> String {
    let mut out = path.display().to_string();
    if let Ok(home) = std::env::var("HOME") {
        if out.starts_with(&home) {
            out = out.replacen(&home, "~", 1);
        }
    }
    truncate_middle(&out, 70)
}

fn terminal_size() -> (usize, usize) {
    let term = ConsoleTerm::stdout();
    let (rows, cols) = term.size();
    let width = cols.max(1) as usize;
    let height = rows.max(1) as usize;
    (width, height)
}

fn format_mode(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Read => "read",
        RunMode::Work => "work",
    }
}

fn format_approvals(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Ask => "ask",
        ApprovalMode::Auto => "auto",
    }
}

fn format_approvals_display(chat: &eli_core::config::ChatConfig) -> String {
    let cmds = chat.resolved_command_approvals();
    let diffs = chat.resolved_diff_approvals();
    if cmds == diffs {
        return format_approvals(cmds).to_string();
    }
    format!(
        "cmd:{} diff:{}",
        format_approvals(cmds),
        format_approvals(diffs)
    )
}

fn parse_bool(val: &str) -> Result<bool> {
    match val.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => anyhow::bail!("invalid boolean value: {other}"),
    }
}

async fn run_commands_with_policy(
    profile: AgentProfile,
    command_runner: &CommandRunner,
    commands: &[String],
    parallelism: usize,
) -> Vec<CommandResult> {
    let _ = profile;
    command_runner
        .run_commands_with_parallelism(commands, parallelism)
        .await
}

fn shadow_large_tool_outputs(
    project_root: &Path,
    session_id: &str,
    step: u32,
    results: &[CommandResult],
) -> Vec<CommandResult> {
    const MAX_INLINE_JSON_BYTES: usize = 2 * 1024;
    const MAX_TOTAL_BYTES: u64 = 1_000_000_000;
    const MAX_AGE_DAYS: i64 = 45;
    const MAX_FILES_PER_SESSION: usize = 200;

    let last_output_path = project_root
        .join("eli_research")
        .join("data")
        .join(".last_tool_output.json");
    let rel_last_output_path = last_output_path
        .strip_prefix(project_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| last_output_path.display().to_string());
    let tool_output_root = project_root
        .join("eli_research")
        .join("data")
        .join("tool_outputs");
    let index_path = tool_output_root.join("index.jsonl");

    let mut out = Vec::with_capacity(results.len());
    for (idx, r) in results.iter().enumerate() {
        let mut rr = r.clone();

        let cmd0 = rr
            .command
            .trim_start()
            .split_whitespace()
            .next()
            .unwrap_or("");
        let is_eli = cmd0 == "eli" || cmd0.ends_with("/eli") || cmd0.ends_with("\\eli");
        if !is_eli || !rr.allowed || rr.returncode != 0 {
            out.push(rr);
            continue;
        }

        if is_suppression_exempt(&rr.command) {
            out.push(rr);
            continue;
        }

        let stdout = rr.stdout.trim();
        if stdout.as_bytes().len() <= MAX_INLINE_JSON_BYTES {
            out.push(rr);
            continue;
        }
        let Some(value) = extract_json_from_stdout(stdout) else {
            out.push(rr);
            continue;
        };

        if let Some(parent) = last_output_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                rr.stderr = format!(
                    "{}\n(data shadowing: failed to create dir '{}': {e})",
                    rr.stderr.trim_end(),
                    parent.display()
                )
                .trim()
                .to_string();
                out.push(rr);
                continue;
            }
        }

        if let Err(e) = std::fs::create_dir_all(&tool_output_root) {
            rr.stderr = format!(
                "{}\n(data shadowing: failed to create dir '{}': {e})",
                rr.stderr.trim_end(),
                tool_output_root.display()
            )
            .trim()
            .to_string();
            out.push(rr);
            continue;
        }

        let json = serde_json::to_string_pretty(&value).unwrap_or_else(|_| stdout.to_string());
        let session_dir = tool_output_root.join(session_id);
        if let Err(e) = std::fs::create_dir_all(&session_dir) {
            rr.stderr = format!(
                "{}\n(data shadowing: failed to create dir '{}': {e})",
                rr.stderr.trim_end(),
                session_dir.display()
            )
            .trim()
            .to_string();
            out.push(rr);
            continue;
        }

        let (tool_name, parsed_args) = parse_tool_name_and_arg_pairs_from_command(&rr.command);
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();
        let sha256 = sha256_hex(json.as_bytes());
        let stem = build_programmatic_dataset_stem(&tool_name, &value, &parsed_args, &stamp);
        let mut archive_path = session_dir.join(format!("{stem}.json"));
        if archive_path.exists() {
            archive_path = session_dir.join(format!("{stem}_S{step:03}_I{idx:02}.json"));
        }
        let rel_archive_path = archive_path
            .strip_prefix(project_root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| archive_path.display().to_string());

        let archive_ok = match std::fs::write(&archive_path, &json) {
            Ok(()) => true,
            Err(e) => {
                rr.stderr = format!(
                    "{}\n(data shadowing: failed to write '{}': {e})",
                    rr.stderr.trim_end(),
                    rel_archive_path
                )
                .trim()
                .to_string();
                false
            }
        };
        if archive_ok {
            if let Err(e) =
                write_shadow_meta_for_value(&archive_path, &value, "tool.shadow", &rr.command)
            {
                rr.stderr = format!(
                    "{}\n(data shadowing: failed to write meta for '{}': {e})",
                    rr.stderr.trim_end(),
                    rel_archive_path
                )
                .trim()
                .to_string();
            }
        }

        if let Err(e) = std::fs::write(&last_output_path, &json) {
            rr.stderr = format!(
                "{}\n(data shadowing: failed to write '{}': {e})",
                rr.stderr.trim_end(),
                rel_last_output_path
            )
            .trim()
            .to_string();
        } else if let Err(e) =
            write_shadow_meta_for_value(&last_output_path, &value, "tool.shadow", &rr.command)
        {
            rr.stderr = format!(
                "{}\n(data shadowing: failed to write meta for '{}': {e})",
                rr.stderr.trim_end(),
                rel_last_output_path
            )
            .trim()
            .to_string();
        }

        let bytes = json.as_bytes().len();
        if archive_ok {
            let meta = serde_json::json!({
                "created_at": chrono::Utc::now().to_rfc3339(),
                "session_id": session_id,
                "step": step,
                "command_index": idx,
                "command": rr.command,
                "path": rel_archive_path,
                "latest_path": rel_last_output_path,
                "bytes": bytes,
                "sha256": sha256,
            });
            if let Ok(line) = serde_json::to_string(&meta) {
                if let Err(e) = append_line(&index_path, &line) {
                    rr.stderr = format!(
                        "{}\n(data shadowing: failed to append index '{}': {e})",
                        rr.stderr.trim_end(),
                        index_path.display()
                    )
                    .trim()
                    .to_string();
                }
            }
            if let Err(e) = prune_tool_outputs(
                &tool_output_root,
                MAX_TOTAL_BYTES,
                MAX_AGE_DAYS,
                MAX_FILES_PER_SESSION,
            ) {
                rr.stderr = format!(
                    "{}\n(data shadowing: prune failed: {e})",
                    rr.stderr.trim_end(),
                )
                .trim()
                .to_string()
            }
        }

        let points = count_data_points(&value);
        let summary = format_suppressed_summary(&rr.command, &value, 8, 160);
        let hint =
            "More detail is available in the saved file; inspect with local tools if needed.";
        let archive_fragment = if archive_ok {
            format!("; saved_copy={rel_archive_path}")
        } else {
            String::new()
        };
        rr.stdout = format!(
            "[OUTPUT SUPPRESSED] saved_to={rel_last_output_path} ({bytes} bytes){archive_fragment}. Data points: {points}.\n[SUMMARY]\n{summary}\n{hint}"
        );
        out.push(rr);
    }

    out
}

fn extract_json_from_stdout(stdout: &str) -> Option<serde_json::Value> {
    if stdout.starts_with('{') || stdout.starts_with('[') {
        return serde_json::from_str(stdout).ok();
    }

    let first_obj = stdout.find('{');
    let first_arr = stdout.find('[');
    let start = match (first_obj, first_arr) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    serde_json::from_str(&stdout[start..]).ok()
}

fn parse_tool_name_and_arg_pairs_from_command(command: &str) -> (String, Vec<String>) {
    let toks = command.split_whitespace().collect::<Vec<_>>();
    if toks.is_empty() {
        return ("tool.shadow".to_string(), Vec::new());
    }
    let first = toks[0];
    let is_eli = first == "eli" || first.ends_with("/eli") || first.ends_with("\\eli");
    let tool_name =
        if is_eli && toks.len() >= 3 && !toks[1].starts_with('-') && !toks[2].starts_with('-') {
            format!("{}.{}", toks[1], toks[2])
        } else {
            "tool.shadow".to_string()
        };

    let mut args = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        let tok = toks[i];
        if let Some(rest) = tok.strip_prefix("--") {
            if let Some((k, v)) = rest.split_once('=') {
                let key = normalize_name_token(k, false, 48).to_ascii_lowercase();
                if !key.is_empty() && !v.trim().is_empty() {
                    args.push(format!("{key}={v}"));
                }
            } else {
                let key = normalize_name_token(rest, false, 48).to_ascii_lowercase();
                let next = toks.get(i + 1).copied();
                if let Some(v) = next {
                    if !v.starts_with('-') {
                        args.push(format!("{key}={v}"));
                        i += 1;
                    } else {
                        args.push(format!("{key}=true"));
                    }
                } else {
                    args.push(format!("{key}=true"));
                }
            }
        }
        i += 1;
    }
    (tool_name, args)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

fn prune_tool_outputs(
    tool_output_root: &Path,
    max_total_bytes: u64,
    max_age_days: i64,
    max_files_per_session: usize,
) -> Result<()> {
    #[derive(Clone)]
    struct Entry {
        path: PathBuf,
        modified: std::time::SystemTime,
        size: u64,
        session: String,
    }

    if !tool_output_root.exists() {
        return Ok(());
    }

    let now = chrono::Utc::now();
    let expiry = now - chrono::Duration::days(max_age_days);
    let mut entries: Vec<Entry> = Vec::new();

    let sessions = match std::fs::read_dir(tool_output_root) {
        Ok(rd) => rd,
        Err(e) => {
            return Err(anyhow::anyhow!(
                "read_dir {}: {e}",
                tool_output_root.display()
            ));
        }
    };

    for session_entry in sessions {
        let session_entry = match session_entry {
            Ok(v) => v,
            Err(_) => continue,
        };
        let session_path = session_entry.path();
        if !session_path.is_dir() {
            continue;
        }
        let session = session_entry.file_name().to_string_lossy().to_string();
        let files = match std::fs::read_dir(&session_path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for file in files {
            let Ok(file) = file else { continue };
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(meta) = file.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let modified_dt: chrono::DateTime<chrono::Utc> = modified.into();
            if modified_dt < expiry {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            entries.push(Entry {
                path,
                modified,
                size: meta.len(),
                session: session.clone(),
            });
        }
    }

    // Per-session cap: keep newest N.
    let mut by_session: std::collections::HashMap<String, Vec<Entry>> =
        std::collections::HashMap::new();
    for entry in entries {
        by_session
            .entry(entry.session.clone())
            .or_default()
            .push(entry);
    }

    let mut remaining: Vec<Entry> = Vec::new();
    for (_session, mut files) in by_session {
        files.sort_by_key(|e| std::cmp::Reverse(e.modified));
        for (idx, entry) in files.into_iter().enumerate() {
            if idx < max_files_per_session {
                remaining.push(entry);
            } else {
                let _ = std::fs::remove_file(&entry.path);
            }
        }
    }

    // Global cap: delete oldest until size <= max_total_bytes.
    let mut total: u64 = remaining.iter().map(|e| e.size).sum();
    if total > max_total_bytes {
        remaining.sort_by_key(|e| e.modified);
        for entry in remaining {
            if total <= max_total_bytes {
                break;
            }
            if std::fs::remove_file(&entry.path).is_ok() {
                total = total.saturating_sub(entry.size);
            }
        }
    }

    // Remove empty session dirs.
    if let Ok(sessions) = std::fs::read_dir(tool_output_root) {
        for session in sessions.flatten() {
            let p = session.path();
            if p.is_dir() {
                let is_empty = std::fs::read_dir(&p)
                    .ok()
                    .and_then(|mut rd| rd.next())
                    .is_none();
                if is_empty {
                    let _ = std::fs::remove_dir(&p);
                }
            }
        }
    }

    Ok(())
}

fn format_suppressed_summary(
    command: &str,
    value: &serde_json::Value,
    max_lines: usize,
    max_field_len: usize,
) -> String {
    fn trunc(s: String, max_len: usize) -> String {
        if s.chars().count() <= max_len {
            return s;
        }
        let mut out: String = s.chars().take(max_len).collect();
        out.push('…');
        out
    }

    fn list_sample(items: Vec<String>, max_items: usize, max_len: usize) -> String {
        let mut out = items
            .into_iter()
            .filter(|s| !s.is_empty())
            .take(max_items)
            .map(|s| trunc(s, max_len))
            .collect::<Vec<_>>()
            .join(", ");
        if out.is_empty() {
            out = "n/a".to_string();
        }
        out
    }

    let mut lines: Vec<String> = Vec::new();

    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(|k| k.as_str()).collect();
            keys.sort();
            lines.push(format!("top_level_keys: {}", keys.join(", ")));

            if let Some(provider) = map.get("provider").and_then(|v| v.as_str()) {
                lines.push(format!("provider: {provider}"));
            }

            if let Some(tickers) = map.get("tickers").and_then(|v| v.as_array()) {
                let tickers = tickers
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>();
                if !tickers.is_empty() {
                    lines.push(format!(
                        "tickers: {}",
                        list_sample(tickers, 10, max_field_len)
                    ));
                }
            }

            if let Some(arr) = map.get("available_events").and_then(|v| v.as_array()) {
                lines.push(format!("available_events: {}", arr.len()));
                let sample = arr
                    .iter()
                    .take(3)
                    .filter_map(|v| {
                        let title = v.get("title").and_then(|s| s.as_str());
                        let ticker = v.get("ticker").and_then(|s| s.as_str());
                        match (ticker, title) {
                            (Some(t), Some(tt)) => Some(format!("{t}: {tt}")),
                            (None, Some(tt)) => Some(tt.to_string()),
                            _ => None,
                        }
                    })
                    .collect::<Vec<_>>();
                if !sample.is_empty() {
                    lines.push(format!(
                        "event_samples: {}",
                        list_sample(sample, 3, max_field_len)
                    ));
                }
            }

            if let Some(arr) = map.get("available_tags").and_then(|v| v.as_array()) {
                lines.push(format!("available_tags: {}", arr.len()));
                let sample = arr
                    .iter()
                    .take(3)
                    .filter_map(|v| {
                        let label = v.get("label").and_then(|s| s.as_str());
                        let slug = v.get("slug").and_then(|s| s.as_str());
                        let id = v.get("id").and_then(|s| s.as_str());
                        match (label, slug, id) {
                            (Some(l), _, _) => Some(l.to_string()),
                            (None, Some(s), _) => Some(s.to_string()),
                            (None, None, Some(i)) => Some(i.to_string()),
                            _ => None,
                        }
                    })
                    .collect::<Vec<_>>();
                if !sample.is_empty() {
                    lines.push(format!(
                        "tag_samples: {}",
                        list_sample(sample, 3, max_field_len)
                    ));
                }
            }

            if let Some(arr) = map.get("markets").and_then(|v| v.as_array()) {
                lines.push(format!("markets: {}", arr.len()));
                let sample = arr
                    .iter()
                    .take(3)
                    .filter_map(|v| {
                        let title = v.get("title").and_then(|s| s.as_str());
                        let ticker = v.get("ticker").and_then(|s| s.as_str());
                        match (ticker, title) {
                            (Some(t), Some(tt)) => Some(format!("{t}: {tt}")),
                            (None, Some(tt)) => Some(tt.to_string()),
                            _ => None,
                        }
                    })
                    .collect::<Vec<_>>();
                if !sample.is_empty() {
                    lines.push(format!(
                        "market_samples: {}",
                        list_sample(sample, 3, max_field_len)
                    ));
                }
            }

            if let Some(arr) = map.get("series").and_then(|v| v.as_array()) {
                let mut tickers = Vec::new();
                let mut total_points = 0usize;
                for s in arr {
                    if let Some(t) = s.get("ticker").and_then(|v| v.as_str()) {
                        tickers.push(t.to_string());
                    }
                    if let Some(candles) = s.get("candles").and_then(|v| v.as_array()) {
                        total_points += candles.len();
                    }
                }
                lines.push(format!("series: {}", arr.len()));
                if !tickers.is_empty() {
                    lines.push(format!(
                        "series_tickers: {}",
                        list_sample(tickers, 10, max_field_len)
                    ));
                }
                if total_points > 0 {
                    lines.push(format!("series_points: {total_points}"));
                }
            }

            if let Some(arr) = map.get("snapshots").and_then(|v| v.as_array()) {
                lines.push(format!("snapshots: {}", arr.len()));
                let sample = arr
                    .iter()
                    .take(3)
                    .filter_map(|v| {
                        let t = v.get("ticker").and_then(|s| s.as_str())?;
                        let p = v.get("current_price").and_then(|s| s.as_f64());
                        Some(match p {
                            Some(px) => format!("{t}={px:.2}"),
                            None => t.to_string(),
                        })
                    })
                    .collect::<Vec<_>>();
                if !sample.is_empty() {
                    lines.push(format!(
                        "snapshot_samples: {}",
                        list_sample(sample, 3, max_field_len)
                    ));
                }
            }

            if let Some(arr) = map.get("prices").and_then(|v| v.as_array()) {
                lines.push(format!("prices: {}", arr.len()));
                let sample = arr
                    .iter()
                    .take(3)
                    .filter_map(|v| {
                        let sym = v.get("symbol").and_then(|s| s.as_str())?;
                        let val = v.get("value").and_then(|s| s.as_f64());
                        Some(match val {
                            Some(px) => format!("{sym}={px:.4}"),
                            None => sym.to_string(),
                        })
                    })
                    .collect::<Vec<_>>();
                if !sample.is_empty() {
                    lines.push(format!(
                        "price_samples: {}",
                        list_sample(sample, 3, max_field_len)
                    ));
                }
            }

            if let Some(arr) = map.get("filings").and_then(|v| v.as_array()) {
                lines.push(format!("filings: {}", arr.len()));
                let sample = arr
                    .iter()
                    .take(3)
                    .filter_map(|v| {
                        let form = v.get("form").and_then(|s| s.as_str())?;
                        let date = v.get("filing_date").and_then(|s| s.as_str());
                        Some(match date {
                            Some(d) => format!("{form} ({d})"),
                            None => form.to_string(),
                        })
                    })
                    .collect::<Vec<_>>();
                if !sample.is_empty() {
                    lines.push(format!(
                        "filing_samples: {}",
                        list_sample(sample, 3, max_field_len)
                    ));
                }
            }

            if let Some(arr) = map.get("indicators").and_then(|v| v.as_array()) {
                lines.push(format!("indicators: {}", arr.len()));
                let sample = arr
                    .iter()
                    .take(3)
                    .filter_map(|v| {
                        let sym = v.get("symbol").and_then(|s| s.as_str())?;
                        let val = v.get("current_value").and_then(|s| s.as_f64());
                        Some(match val {
                            Some(px) => format!("{sym}={px:.3}"),
                            None => sym.to_string(),
                        })
                    })
                    .collect::<Vec<_>>();
                if !sample.is_empty() {
                    lines.push(format!(
                        "indicator_samples: {}",
                        list_sample(sample, 3, max_field_len)
                    ));
                }
            }

            if let Some(data) = map.get("data") {
                if let serde_json::Value::Object(data_obj) = data {
                    let mut child_keys: Vec<&str> = data_obj.keys().map(|k| k.as_str()).collect();
                    child_keys.sort();
                    lines.push(format!("data_keys: {}", child_keys.join(", ")));
                    for key in child_keys.iter().take(4) {
                        if let Some(arr) = data_obj.get(*key).and_then(|v| v.as_array()) {
                            lines.push(format!("data.{key}: {}", arr.len()));
                        }
                    }
                }
            }
        }
        serde_json::Value::Array(arr) => {
            lines.push(format!("top_level: array (len={})", arr.len()));
        }
        _ => {
            lines.push("top_level: scalar".to_string());
        }
    }

    let mut merged = command_summary_lines(command, value, 4);
    merged.extend(schema_pattern_summary_parts(value));
    merged.extend(lines);
    let trimmed = merged.into_iter().take(max_lines).collect::<Vec<_>>();
    trimmed
        .into_iter()
        .map(|l| format!("- {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn augment_tool_errors(results: Vec<CommandResult>) -> Vec<CommandResult> {
    results
        .into_iter()
        .map(|mut r| {
            if !r.allowed || r.returncode == 0 {
                return r;
            }

            if !looks_like_clap_error(&r.stderr) {
                return r;
            }

            let path = match extract_eli_tool_path(&r.command) {
                Some(path) => path,
                None => return r,
            };

            if path.first().map(|p| p.as_str()) == Some("tool-info") {
                return r;
            }

            let info = build_tool_info(&path);
            let info_json = serde_json::to_string_pretty(&info)
                .unwrap_or_else(|_| "<tool-info failed>".to_string());
            let sep = if r.stderr.trim().is_empty() { "" } else { "\n" };
            r.stderr = format!("{}{}[TOOL INFO]\n{}", r.stderr.trim_end(), sep, info_json);
            r
        })
        .collect()
}

fn looks_like_clap_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("error:") && (lower.contains("usage:") || lower.contains("try '--help'"))
}

fn extract_eli_tool_path(command: &str) -> Option<Vec<String>> {
    let mut parts = command.split_whitespace();
    let first = parts.next()?;
    let is_eli = first == "eli" || first.ends_with("/eli") || first.ends_with("\\eli");
    if !is_eli {
        return None;
    }

    let mut path = Vec::new();
    for tok in parts {
        if tok.starts_with('-') {
            break;
        }
        path.push(tok.to_string());
    }

    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

fn is_suppression_exempt(command: &str) -> bool {
    let trimmed = command.trim_start();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    let mut parts = lower.split_whitespace();
    let Some(bin) = parts.next() else {
        return false;
    };

    let is_eli = bin == "eli" || bin.ends_with("/eli") || bin.ends_with("\\eli");
    if !is_eli {
        return false;
    }

    let Some(domain) = parts.next() else {
        return false;
    };
    if domain != "finance" {
        return false;
    }

    let Some(tool) = parts.next() else {
        return false;
    };

    match tool {
        "search" => true,
        "odds" => {
            let rest = parts.collect::<Vec<_>>();
            rest.iter()
                .any(|t| *t == "--list-events" || *t == "--list-series")
        }
        "options" => {
            let rest = parts.collect::<Vec<_>>();
            rest.iter().any(|t| *t == "--expirations")
        }
        _ => false,
    }
}

fn infer_sources(command: &str, stdout: &str) -> Vec<&'static str> {
    let cmd_lower = command.to_ascii_lowercase();
    let mut out: Vec<&'static str> = Vec::new();

    if cmd_lower.contains("eli finance odds") {
        let out_lower = stdout.to_ascii_lowercase();
        if out_lower.contains("kalshi") {
            out.push("Kalshi");
        }
        if out_lower.contains("polymarket") {
            out.push("Polymarket");
        }
        return dedupe_sources(out);
    }

    if cmd_lower.contains("eli finance prices") {
        out.push("Pyth");
        return out;
    }

    if cmd_lower.contains("eli finance") {
        if let Some(source) = infer_sources_from_json(stdout) {
            out.extend(source);
            return dedupe_sources(out);
        }
        if cmd_lower.contains("--provider fred") {
            out.push("FRED");
        } else if cmd_lower.contains("--provider yahoo") {
            out.push("Yahoo Finance");
        } else if cmd_lower.contains("--provider mock") {
            out.push("Mock");
        }
    }

    dedupe_sources(out)
}

fn infer_sources_from_json(stdout: &str) -> Option<Vec<&'static str>> {
    let value: serde_json::Value = serde_json::from_str(stdout).ok()?;
    let mut out: Vec<&'static str> = Vec::new();

    if let Some(provider) = value.get("provider").and_then(|v| v.as_str()) {
        match provider {
            "yahoo" => out.push("Yahoo Finance"),
            "fred" => out.push("FRED"),
            "mock" => out.push("Mock"),
            _ => {}
        }
    }

    if let Some(source) = value.get("source").and_then(|v| v.as_str()) {
        match source {
            "pyth" => out.push("Pyth"),
            "kalshi" => out.push("Kalshi"),
            "polymarket" => out.push("Polymarket"),
            _ => {}
        }
    }

    if let Some(sources) = value.get("sources").and_then(|v| v.as_array()) {
        for s in sources {
            if let Some(name) = s.get("source").and_then(|v| v.as_str()) {
                match name {
                    "kalshi" => out.push("Kalshi"),
                    "polymarket" => out.push("Polymarket"),
                    "pyth" => out.push("Pyth"),
                    "fred" => out.push("FRED"),
                    "yahoo" => out.push("Yahoo Finance"),
                    "mock" => out.push("Mock"),
                    _ => {}
                }
            }
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(dedupe_sources(out))
    }
}

fn dedupe_sources(mut sources: Vec<&'static str>) -> Vec<&'static str> {
    sources.sort_unstable();
    sources.dedup();
    sources
}

fn count_data_points(value: &serde_json::Value) -> usize {
    fn array_len(v: Option<&serde_json::Value>) -> Option<usize> {
        v.and_then(|vv| vv.as_array().map(|a| a.len()))
    }

    match value {
        serde_json::Value::Array(arr) => arr.len(),
        serde_json::Value::Object(map) => {
            if let Some(series) = map.get("series").and_then(|v| v.as_array()) {
                let mut total = 0usize;
                for s in series {
                    total += s
                        .get("candles")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                }
                if total > 0 {
                    return total;
                }
            }

            if let Some(n) = array_len(map.get("snapshots")) {
                return n;
            }
            if let Some(n) = array_len(map.get("prices")) {
                return n;
            }
            if let Some(n) = array_len(map.get("available_events")) {
                return n;
            }
            if let Some(n) = array_len(map.get("available_tags")) {
                return n;
            }
            if let Some(n) = array_len(map.get("events")) {
                return n;
            }
            if let Some(n) = array_len(map.get("markets")) {
                return n;
            }
            if let Some(n) = array_len(map.get("results")) {
                return n;
            }

            map.len()
        }
        _ => 1,
    }
}

fn build_observation(
    read_mode: bool,
    approvals_ask_commands: bool,
    approvals_ask_diffs: bool,
    diffs: &[DiffResult],
    commands: &[CommandResult],
) -> String {
    let mode = if read_mode { "read" } else { "work" };
    let approvals_cmds = if approvals_ask_commands {
        "ask"
    } else {
        "auto"
    };
    let approvals_diffs = if approvals_ask_diffs { "ask" } else { "auto" };

    let mut out = String::new();
    out.push_str(&format!(
        "mode={mode}, approvals_cmds={approvals_cmds}, approvals_diffs={approvals_diffs}\n"
    ));

    if !diffs.is_empty() {
        out.push_str("diffs:\n");
        for r in diffs {
            out.push_str(&format!(
                "- {op} {path}: {status} {msg}\n",
                op = r.op,
                path = r.path,
                status = if r.success { "OK" } else { "ERR" },
                msg = r.message
            ));
        }
    }

    if !commands.is_empty() {
        out.push_str("commands:\n");
        for r in commands {
            out.push_str(&format!(
                "- `{cmd}` => {code} ({ms}ms)\n",
                cmd = r.command,
                code = r.returncode,
                ms = r.duration_ms
            ));
            let digest = build_command_digest(r);
            if !digest.trim().is_empty() {
                out.push_str(&format!("  digest: {digest}\n"));
            }
            if !r.stdout.trim().is_empty() {
                out.push_str(&format!("  stdout:\n{}\n", truncate(&r.stdout, 400000)));
            }
            if !r.stderr.trim().is_empty() {
                out.push_str(&format!("  stderr:\n{}\n", truncate(&r.stderr, 400000)));
            }
        }
    }

    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for (idx, ch) in s.char_indices() {
        if idx >= max {
            break;
        }
        out.push(ch);
    }
    out
}

fn insert_system_context_before_conversation(messages: &mut Vec<ChatMessage>, extra: ChatMessage) {
    // Keep the contract/system prompt first, but insert this near the top
    // (after any initial system messages like date/summary/brain).
    let mut idx = 0usize;
    while idx < messages.len() {
        if !matches!(messages[idx].role, eli_core::types::Role::System) {
            break;
        }
        idx += 1;
    }
    messages.insert(idx, extra);
}

fn discover_recent_research(project_root: &Path, max_items: usize) -> Vec<ResearchArtifact> {
    if max_items == 0 {
        return Vec::new();
    }

    let dir = project_root.join("eli_research");
    let entries = match std::fs::read_dir(&dir) {
        Ok(it) => it,
        Err(_) => return Vec::new(),
    };

    #[derive(Clone)]
    struct Candidate {
        path: PathBuf,
        modified: std::time::SystemTime,
    }

    let mut files: Vec<Candidate> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.file_name().and_then(|s| s.to_str()) == Some("ELI.md") {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        files.push(Candidate { path, modified });
    }

    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    files.truncate(max_items);

    let mut out = Vec::new();
    for cand in files {
        let rel = cand
            .path
            .strip_prefix(project_root)
            .unwrap_or(&cand.path)
            .to_string_lossy()
            .to_string();

        let title = read_markdown_title(&cand.path).unwrap_or_else(|| {
            cand.path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("research")
                .to_string()
        });

        let created_utc = chrono::DateTime::<chrono::Utc>::from(cand.modified).to_rfc3339();

        out.push(ResearchArtifact {
            rel_path: rel,
            title,
            status: String::new(),
            created_utc,
            answer_hint: None,
        });
    }

    out
}

fn read_markdown_title(path: &Path) -> Option<String> {
    use std::io::Read;

    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    // Titles are at the top for Eli reports; keep this cheap.
    let mut reader = f.take(2048);
    reader.read_to_end(&mut buf).ok()?;
    let s = String::from_utf8_lossy(&buf);
    let first = s.lines().next()?.trim();
    let title = first.strip_prefix('#')?.trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

fn is_slash_command_context(line: &str, pos: usize) -> bool {
    if pos != line.len() {
        return false;
    }
    if !line.starts_with('/') {
        return false;
    }
    if line.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let tail = line.get(1..).unwrap_or("");
    if tail.contains('/') {
        return false;
    }
    true
}

fn confirm(prompt: &str) -> Result<bool> {
    use std::io::Write;
    print!(
        "{}?{} {} {}(y/n):{} ",
        style::YELLOW,
        style::RESET,
        prompt,
        style::GRAY,
        style::RESET
    );
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("read confirm input")?;
    let v = input.trim().to_lowercase();
    Ok(v == "y" || v == "yes")
}

fn prompt_user(prompt: &str) -> Result<(String, Vec<String>)> {
    use std::io::Write;
    println!("\n{}?{} {}", style::CYAN, style::RESET, prompt);
    print!("{}›{} ", style::CYAN, style::RESET);
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("read input")?;
    Ok(process_input_for_images(input.trim()))
}

fn colorize_diff(diff: &str) -> String {
    use style::*;

    let mut out = String::new();
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            out.push_str(&format!("{}    {}{}\n", GREEN, line, RESET));
        } else if line.starts_with('-') && !line.starts_with("---") {
            out.push_str(&format!("{}    {}{}\n", RED, line, RESET));
        } else if line.starts_with("@@") {
            out.push_str(&format!("{}    {}{}\n", CYAN, line, RESET));
        } else if line.starts_with("+++") || line.starts_with("---") {
            out.push_str(&format!("{}    {}{}\n", GRAY, line, RESET));
        } else {
            out.push_str(&format!("    {}\n", line));
        }
    }
    out
}

fn diff_line_counts(diff: &str) -> (usize, usize) {
    let mut added = 0usize;
    let mut deleted = 0usize;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            deleted += 1;
        }
    }
    (added, deleted)
}

fn print_diff_results(results: &[DiffResult], preview: bool, brief: bool) {
    use style::*;

    if results.is_empty() {
        return;
    }
    if brief {
        let created = results.iter().filter(|r| r.op == "create").count();
        let modified = results
            .iter()
            .filter(|r| r.op == "replace" || r.op == "patch")
            .count();
        let deleted = results.iter().filter(|r| r.op == "delete").count();

        let mut parts = Vec::new();
        if created > 0 {
            parts.push(format!("{}+{} created{}", GREEN, created, RESET));
        }
        if modified > 0 {
            parts.push(format!("{}~{} modified{}", YELLOW, modified, RESET));
        }
        if deleted > 0 {
            parts.push(format!("{}-{} deleted{}", RED, deleted, RESET));
        }

        let status = if preview {
            format!("{}preview{}", GRAY, RESET)
        } else {
            format!("{}applied{}", GREEN, RESET)
        };
        let count = created + modified + deleted;
        let noun = if count == 1 { "file" } else { "files" };
        print_history_line(format!("edited {count} {noun} ({})", status));
        return;
    }

    let status = if preview { "preview" } else { "applied" };
    println!("{}◆{} diffs: {} ({})", PURPLE, RESET, results.len(), status);
    for r in results {
        let (icon, color) = if r.success {
            ("✓", GREEN)
        } else {
            ("✗", RED)
        };
        println!(
            "  {}{}{} {}{} {}{}{}: {}",
            color, icon, RESET, BLUE, r.op, RESET, WHITE, r.path, RESET,
        );
        if !r.message.is_empty() && r.message != "ok" {
            println!("    {}{}{}", GRAY, r.message, RESET);
        }
        if let Some(d) = &r.diff {
            let (added, deleted) = diff_line_counts(d);
            println!(
                "    LINE CODED ({}{}{} IN GREEN, {}{}{} IN RED)",
                GREEN, added, RESET, RED, deleted, RESET
            );
            println!("{}", colorize_diff(d));
        }
    }
}

fn print_command_results(results: &[CommandResult], brief: bool, full: bool) {
    use style::*;

    if results.is_empty() {
        return;
    }

    if brief {
        for r in results {
            let (icon, color) = if r.returncode == 0 {
                ("✓", GREEN)
            } else {
                ("✗", RED)
            };
            print_history_line(format!(
                "{}{}{} {}${} {}{}",
                color,
                icon,
                RESET,
                GRAY,
                RESET,
                truncate_line(&r.command, 70),
                RESET
            ));
            if r.returncode != 0 && !r.stderr.trim().is_empty() {
                print_history_line(format!(
                    "{}err:{} {}{}",
                    RED,
                    RESET,
                    truncate_line(&r.stderr.replace('\n', " "), 100),
                    RESET
                ));
            }
        }
        return;
    }

    println!("{}◆{} commands: {}", YELLOW, RESET, results.len());
    for r in results {
        let (icon, color) = if r.returncode == 0 {
            ("✓", GREEN)
        } else {
            ("✗", RED)
        };
        println!(
            "  {}{}{} {}${} {} {}{}ms{}",
            color, icon, RESET, GRAY, RESET, r.command, DARK_GRAY, r.duration_ms, RESET
        );
        if full {
            if !r.stdout.trim().is_empty() {
                println!("    {}stdout:{}{}", GRAY, RESET, RESET);
                for line in r.stdout.lines() {
                    println!("    {}{}{}", GRAY, line, RESET);
                }
            }
            if !r.stderr.trim().is_empty() {
                println!("    {}stderr:{}{}", RED, RESET, RESET);
                for line in r.stderr.lines() {
                    println!("    {}{}{}", RED, line, RESET);
                }
            }
        } else {
            if !r.stdout.trim().is_empty() {
                for line in r.stdout.lines().take(20) {
                    println!("    {}{}{}", GRAY, line, RESET);
                }
                if r.stdout.lines().count() > 20 {
                    println!(
                        "    {}... ({} more lines){}",
                        DARK_GRAY,
                        r.stdout.lines().count() - 20,
                        RESET
                    );
                }
            }
            if !r.stderr.trim().is_empty() {
                for line in r.stderr.lines().take(10) {
                    println!("    {}{}{}", RED, line, RESET);
                }
            }
        }
    }
}

fn print_tool_results_debug(results: &[CommandResult]) {
    if results.is_empty() {
        return;
    }

    println!("\n=== TOOL CALL RESULT ===");
    for (idx, r) in results.iter().enumerate() {
        if idx > 0 {
            println!("\n---");
        }
        println!("command: {}", r.command);
        println!("returncode: {}", r.returncode);
        if let Some(reason) = &r.deny_reason {
            println!("deny_reason: {}", reason);
        }
        println!("stdout:");
        print!("{}", r.stdout);
        if !r.stdout.ends_with('\n') {
            println!();
        }
        println!("stderr:");
        print!("{}", r.stderr);
        if !r.stderr.ends_with('\n') {
            println!();
        }
    }
    println!("=== END TOOL CALL RESULT ===");
}

async fn print_screen_results(actions: &[serde_json::Value]) {
    for action in actions {
        let Some(obj) = action.as_object() else {
            continue;
        };
        let Some(kind) = obj.get("action").and_then(|v| v.as_str()) else {
            continue;
        };
        match kind {
            "clipboard" => {
                if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                    let _ = eli_screen::run_action(eli_screen::ScreenAction::Clipboard {
                        text: text.to_string(),
                    })
                    .await;
                    println!("screen: clipboard ({} chars)", text.len());
                }
            }
            "focus_app" => {
                if let Some(name) = obj.get("app").and_then(|v| v.as_str()) {
                    let _ = eli_screen::run_action(eli_screen::ScreenAction::FocusApp {
                        name: name.to_string(),
                    })
                    .await;
                    println!("screen: focus_app {name}");
                }
            }
            other => println!("screen: skipped action {other}"),
        }
    }
}

fn parse_plan_controls(plan: &str) -> (Option<RunMode>, Option<ApprovalMode>) {
    let line = plan.lines().next().unwrap_or("");
    let mut mode = None;
    let mut approvals = None;

    for part in line.split('|').map(|p| p.trim()) {
        let lower = part.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("mode:") {
            let v = rest.trim();
            mode = match v {
                "read" => Some(RunMode::Read),
                "work" => Some(RunMode::Work),
                _ => None,
            };
        } else if let Some(rest) = lower.strip_prefix("approvals:") {
            let v = rest.trim();
            approvals = match v {
                "ask" => Some(ApprovalMode::Ask),
                "auto" => Some(ApprovalMode::Auto),
                _ => None,
            };
        }
    }

    (mode, approvals)
}

fn print_cost_stats(state: &SessionState, chat: &eli_core::config::ChatConfig) {
    use style::*;

    let usage = &state.total_usage;
    let cost = estimate_cost(usage, &chat.model);

    let lines = vec![
        format!("{}{}Cost & Usage{}", BOLD, CYAN, RESET),
        String::new(),
        format!(
            "{}total{} {} tokens  {}│{}  {}${}  {:.4}{}",
            GRAY, RESET, usage.total_tokens, DARK_GRAY, RESET, GREEN, RESET, cost, RESET
        ),
        format!(
            "{}      {} in         {} out",
            GRAY, usage.prompt_tokens, usage.completion_tokens
        ),
    ];

    if let Some(last) = &state.last_usage {
        let last_cost = estimate_cost(last, &chat.model);
        let mut extended = lines;
        extended.push(String::new());
        extended.push(format!(
            "{}last{}  {} tokens     {}${:.4}{}",
            GRAY, RESET, last.total_tokens, YELLOW, last_cost, RESET
        ));
        let out = format_indented_block(&extended);
        println!("{}", out);
    } else {
        let out = format_indented_block(&lines);
        println!("{}", out);
    }
}

fn estimate_cost(usage: &eli_core::types::Usage, model: &str) -> f64 {
    // Very rough estimation based on common OpenRouter/Anthropic pricing
    // Normalize model name
    let m = model.to_lowercase();
    let (input_rate, output_rate) = if m.contains("claude-3-5-sonnet") {
        (3.0, 15.0)
    } else if m.contains("claude-3-5-haiku") {
        (0.8, 4.0)
    } else if m.contains("claude-3-haiku") || m.contains("haiku") {
        (0.25, 1.25)
    } else if m.contains("claude-3-opus") || m.contains("opus") {
        (15.0, 75.0)
    } else if m.contains("gpt-4o-mini") {
        (0.15, 0.60)
    } else if m.contains("gpt-4o") {
        (2.5, 10.0)
    } else if m.contains("o1-mini") {
        (1.1, 4.4)
    } else if m.contains("o1") {
        (15.0, 60.0)
    } else if m.contains("o3-mini") {
        (1.1, 4.4)
    } else if m.contains("gpt-4-turbo") || m.contains("gpt-4") {
        (10.0, 30.0)
    } else if m.contains("deepseek") {
        (0.14, 0.28)
    } else if m.contains("gemini-1.5-flash") {
        (0.075, 0.3)
    } else if m.contains("gemini-1.5-pro") {
        (1.25, 5.0)
    } else if m.contains("llama-3.1-405b") || m.contains("llama-3.3-70b") {
        (1.0, 1.0) // Approx OpenRouter pricing for huge models
    } else if m.contains("llama") || m.contains("mistral") {
        (0.1, 0.1)
    } else if m.contains("devstral") || m.contains("moe") {
        (0.05, 0.22) // $0.22 per 1M output tokens as requested
    } else {
        (3.0, 15.0) // Default to Sonnet
    };

    let input_cost = (usage.prompt_tokens as f64 / 1_000_000.0) * input_rate;
    let output_cost = (usage.completion_tokens as f64 / 1_000_000.0) * output_rate;
    input_cost + output_cost
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn mk_temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{prefix}_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn suppressed_summary_includes_schema_pattern_lines() {
        let value = serde_json::json!({
            "provider": "yahoo",
            "tickers": ["SPY"],
            "series": [{"ticker":"SPY","candles":[{"c":1.0},{"c":2.0}]}]
        });
        let summary = format_suppressed_summary("eli finance timeseries", &value, 12, 200);
        assert!(summary.contains("schema_root="));
        assert!(summary.contains("schema_paths="));
        assert!(summary.contains("nullable_fields="));
    }

    #[test]
    fn data_sidecar_gate_detects_missing_meta() {
        let dir = mk_temp_dir("eli_cli_gate");
        let data = dir.join("payload.json");
        std::fs::write(&data, "{\"x\":1}").expect("write data");
        std::fs::write(dir.join("notes.txt"), "ignore me").expect("write notes");

        let missing_first = missing_data_sidecars(&dir).expect("missing sidecars");
        assert_eq!(missing_first.len(), 1);
        assert_eq!(missing_first[0], data);

        let sidecar = eli_core::meta::sidecar_path_for(&data);
        std::fs::write(&sidecar, "{}").expect("write sidecar");
        assert!(missing_data_sidecars(&dir).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_source_kind_sniffs_json_when_extension_unknown() {
        let dir = mk_temp_dir("eli_cli_probe");
        let path = dir.join("mystery.bin");
        std::fs::write(&path, "{\"alpha\":1,\"beta\":2}").expect("write probe sample");
        let kind = detect_source_kind_for_path(&path);
        assert!(matches!(kind, eli_core::meta::SourceKind::Json));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_json_out_with_meta_writes_sidecar() {
        let dir = mk_temp_dir("eli_cli_out_meta");
        let out = dir.join("payload.json");
        let payload = serde_json::json!({"x": 1, "y": [1,2,3]});
        let wr =
            write_json_out_with_meta(out.clone(), &payload, "test.tool", &["arg=a".to_string()])
                .expect("write out+meta");
        assert!(wr.out_path.exists());
        assert!(wr.meta_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_json_out_with_meta_odds_sidecar_has_units_and_scale_hints() {
        let dir = mk_temp_dir("eli_cli_odds_meta");
        let out = dir.join("odds.json");
        let payload = serde_json::json!({
            "markets": [
                {"probability_yes": 0.23, "yes_price": 23, "volume": 223483}
            ]
        });
        let wr = write_json_out_with_meta(
            out,
            &payload,
            "finance.odds",
            &["provider=polymarket".to_string()],
        )
        .expect("write odds out+meta");
        let raw = std::fs::read_to_string(&wr.meta_path).expect("read sidecar");
        let meta: serde_json::Value = serde_json::from_str(&raw).expect("parse sidecar");
        let paths = meta
            .get("path_index")
            .and_then(|v| v.as_array())
            .expect("path_index");
        let prob = paths
            .iter()
            .find(|e| {
                e.get("path")
                    .and_then(|v| v.as_str())
                    .map(|p| p == "$.markets[].probability_yes")
                    .unwrap_or(false)
            })
            .expect("probability entry");
        assert_eq!(
            prob.get("probability_scale").and_then(|v| v.as_str()),
            Some("0_to_1")
        );
        let volume = paths
            .iter()
            .find(|e| {
                e.get("path")
                    .and_then(|v| v.as_str())
                    .map(|p| p == "$.markets[].volume")
                    .unwrap_or(false)
            })
            .expect("volume entry");
        assert_eq!(volume.get("units").and_then(|v| v.as_str()), Some("cents"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shared_manifest_context_is_prepended() {
        let task = "Compute recession probability.";
        let enriched =
            prepend_shared_manifest_context(task, Path::new("/tmp/shared_manifest.json"));
        assert!(enriched.contains("/tmp/shared_manifest.json"));
        assert!(enriched.contains("artifact paths + sidecars"));
        assert!(enriched.ends_with(task));
    }

    #[test]
    fn auto_out_path_uses_dimensional_timeseries_name() {
        let dir = mk_temp_dir("eli_cli_auto_name");
        let out = dir.join("auto.json");
        let payload = serde_json::json!({
            "provider": "yahoo",
            "tickers": ["NVDA","INTC","AMD"],
            "series": []
        });
        let wr = write_json_out_with_meta(
            out,
            &payload,
            "finance.timeseries",
            &["range=1y".to_string(), "granularity=5min".to_string()],
        )
        .expect("write auto out+meta");
        let name = wr
            .out_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        assert!(name.starts_with("TIMESERIES_AMD_INTC_NVDA_1YR_5MIN_YAHOO_"));
        assert!(name.ends_with(".json"));
        assert!(!name.contains("step001"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shadow_pipeline_writes_meta_for_saved_outputs() {
        let dir = mk_temp_dir("eli_cli_shadow");
        let candles = (0..256)
            .map(|i| serde_json::json!({"t": i, "c": i as f64 + 100.0, "v": i + 1}))
            .collect::<Vec<_>>();
        let payload = serde_json::json!({
            "provider": "mock",
            "series": [{"ticker":"SPY","candles": candles}]
        });
        let stdout = serde_json::to_string_pretty(&payload).expect("serialize payload");
        assert!(stdout.len() > 2048, "payload should trigger suppression");

        let result = CommandResult {
            command: "eli finance timeseries --tickers SPY --range 1d --granularity 5min"
                .to_string(),
            returncode: 0,
            stdout,
            stderr: String::new(),
            duration_ms: 1,
            allowed: true,
            deny_reason: None,
        };
        let out = shadow_large_tool_outputs(&dir, "sess_1", 1, &[result]);
        assert_eq!(out.len(), 1);
        assert!(out[0].stdout.contains("[OUTPUT SUPPRESSED]"));
        assert!(out[0].stdout.contains("schema_root="));

        let last = dir.join("eli_research/data/.last_tool_output.json");
        assert!(last.exists());
        assert!(eli_core::meta::sidecar_path_for(&last).exists());

        let archive_dir = dir.join("eli_research/data/tool_outputs/sess_1");
        let mut archive_jsons = std::fs::read_dir(&archive_dir)
            .expect("read archive dir")
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("json")
                    && !p.display().to_string().ends_with(".meta.json")
            })
            .collect::<Vec<_>>();
        archive_jsons.sort();
        assert!(!archive_jsons.is_empty(), "expected archived json output");
        let archive = archive_jsons[0].clone();
        let archive_name = archive
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        assert!(archive_name.starts_with("TIMESERIES_SPY_1D_5MIN_MOCK_"));
        assert!(eli_core::meta::sidecar_path_for(&archive).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunk_text_for_swarm_respects_requested_chunks() {
        let text = "abcdefghijklmnopqrstuvwxyz0123456789";
        let chunks = chunk_text_for_swarm(text, Some(3), 10, 0, 10);
        assert_eq!(chunks.len(), 3);
        let combined = chunks.join("");
        assert_eq!(combined, text);
    }

    #[test]
    fn chunk_text_for_swarm_respects_requested_chunks_with_overlap() {
        let text = "abcdefghijklmnopqrstuvwxyz0123456789";
        let chunks = chunk_text_for_swarm(text, Some(4), 10, 2, 10);
        assert_eq!(chunks.len(), 4);
    }

    #[test]
    fn chunk_text_for_swarm_applies_overlap() {
        let text = "abcdefghij1234567890";
        let chunks = chunk_text_for_swarm(text, None, 10, 2, 10);
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0], "abcdefghij");
        assert!(chunks[1].starts_with("ij"));
    }
}
