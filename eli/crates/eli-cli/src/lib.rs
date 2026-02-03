#![forbid(unsafe_code)]

mod chat_ui;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use eli_core::config::{self, ApprovalMode, AutoMode, ConfigFile, DisplayMode, Paths, RunMode};
use eli_core::contract::{self, StepStatus};
use eli_core::diff::engine::{DiffEngine, DiffResult};
use eli_core::diff::engine::UndoManager;
use eli_core::executor::command_runner::{CommandResult, CommandRunner};
use eli_core::orchestrator::{compact_memory_now, maybe_compact_memory, run_subagents, SubagentResult};
use eli_core::persistence::{EventKind, SessionEvent, SessionStore};
use eli_core::types::{ChatMessage, ChatRequest, ProviderKind};
use eli_core::LlmAdapter;
use futures::StreamExt;
use console::Term as ConsoleTerm;
use crossterm::cursor;
use crossterm::event::{self as ct_event, Event as CtEvent, KeyCode as CtKeyCode, KeyEventKind, KeyModifiers as CtKeyModifiers};
use crossterm::queue;
use crossterm::style::{Attribute, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::history::DefaultHistory;
use rustyline::hint::Hinter;
use rustyline::{ 
    Cmd, CompletionType, ConditionalEventHandler, Config, Context as RustyContext, Editor, Event, 
    EventHandler, Helper, KeyCode, KeyEvent, Modifiers, 
};
use rustyline::validate::Validator;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::path::{Path, PathBuf};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::Widget;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use serde::Serialize;
use termimad::MadSkin;
use textwrap::{wrap, Options as WrapOptions};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use std::time::{Duration, Instant};
use tracing::{info, warn};

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

impl FooterUi {
    fn enable() -> Self {
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
        let bottom = self
            .term_height
            .saturating_sub(self.height as usize)
            .max(1);
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
            write!(out, "\x1b[{};1H", clear_from + 1).ok();  // Move to row
            write!(out, "\x1b[J").ok();  // Clear from cursor to end of screen (ED0)
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

fn apply_prompt_mode(_mode: PromptMode, state: &mut SessionState, chat: &mut eli_core::config::ChatConfig) {
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
        let tokens = self.last_input_tokens.load(std::sync::atomic::Ordering::Relaxed);
        if tokens > 0 {
            // "Input: ~X tokens"
            return Some(format!("  {}Input: ~{} tokens{}", style::DARK_GRAY, tokens, style::RESET));
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

    /// Launch the (early) ratatui interface
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
    /// Latest spot prices from Pyth Hermes (REST).
    Prices(FinancePricesArgs),
    /// Prediction market discovery + pricing (Kalshi default; falls back to Polymarket).
    Odds(FinanceOddsArgs),
    /// Listed options chains with IV/skew summaries (Yahoo Finance).
    Options(FinanceOptionsArgs),
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
    /// Output format (json only).
    #[arg(long, default_value = "json")]
    pub format: String,
    /// Output file path.
    #[arg(short, long)]
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

    /// Output format (currently: json).
    #[arg(long, default_value = "json")]
    format: String,

    /// Write full JSON output to a file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct FinanceOddsArgs {
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
    #[arg(long)]
    search: Option<String>,

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

    /// End timestamp for the window (RFC3339). If you pass YYYY-MM-DD, it's treated as end-of-day UTC. Defaults to now (UTC).
    #[arg(long)]
    as_of: Option<String>,

    /// Data provider (mock | yahoo | fred).
    #[arg(long, default_value = "yahoo")]
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
            std::env::var("RUST_LOG").unwrap_or_else(|_| "eli=info,eli_cli=info".to_string()),
        )
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
        Some(Command::Tui) => cmd_tui().await,
        Some(Command::Finance { cmd }) => cmd_finance(cmd).await,
        Some(Command::Web { cmd }) => cmd_web(cmd).await,
    }
}

async fn cmd_research(query: String, provider: Option<String>, model: Option<String>) -> Result<()> {
    let paths = Paths::discover().context("discover paths")?;
    let mut cfg = config::load_or_create(&paths).context("load/create config")?;
    apply_overrides(&mut cfg, provider, model)?;

    // Research defaults: safe, autonomous, non-destructive.
    cfg.chat.mode = RunMode::Read;
    cfg.chat.approvals = ApprovalMode::Auto;
    cfg.chat.auto = true;

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

    print_banner(&cfg.chat, &project_root, &state);

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

    print_cost_stats(&state, &cfg.chat);

    Ok(())
}

async fn cmd_finance(cmd: FinanceCommand) -> Result<()> {
    match cmd {
        FinanceCommand::Timeseries(args) => cmd_finance_timeseries(args).await,
        FinanceCommand::Snapshot(args) => cmd_finance_snapshot(args).await,
        FinanceCommand::Fundamentals(args) => cmd_finance_fundamentals(args).await,
        FinanceCommand::Search(args) => cmd_finance_search(args).await,
        FinanceCommand::Filings(args) | FinanceCommand::Sec(args) => cmd_finance_filings(args).await,
        FinanceCommand::News(args) => cmd_finance_news(args).await,
        FinanceCommand::Macro(args) => cmd_finance_macro(args).await,
        FinanceCommand::Prices(args) => cmd_finance_prices(args).await,
        FinanceCommand::Odds(args) => cmd_finance_odds(args).await,
        FinanceCommand::Options(args) => cmd_finance_options(args).await,
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

async fn cmd_web_crawl(args: WebCrawlArgs) -> Result<()> {
    let req = eli_core::web::CrawlRequest {
        url: args.url,
        max_pages: Some(args.max_pages),
        respect_robots: args.respect_robots,
        include_subdomains: args.subdomains,
    };

    let resp = eli_core::web::crawl_website(req)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
        .context("crawl website")?;

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, &json).context("write --out")?;
        println!(
            "{{\"ok\":true,\"path\":{}}}",
            serde_json::to_string(&out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    println!("{json}");
    Ok(())
}

async fn cmd_web_search(args: WebSearchArgs) -> Result<()> {
    let hits = eli_core::web::providers::general::search_general(&args.query)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
        .context("web search")?;

    let resp = eli_core::web::WebSearchResponse { hits };
    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, &json).context("write --out")?;
        println!(
            "{{\"ok\":true,\"path\":{}}}",
            serde_json::to_string(&out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    println!("{json}");
    Ok(())
}

async fn cmd_web_read(args: WebReadArgs) -> Result<()> {
    let article = eli_core::web::providers::read::read_url(&args.url)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
        .context("read url")?;

    let json = serde_json::to_string_pretty(&article).context("serialize response")?;

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, &json).context("write --out")?;
        println!(
            "{{\"ok\":true,\"path\":{}}}",
            serde_json::to_string(&out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

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

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, &json).context("write --out")?;
        println!(
            "{{\"ok\":true,\"path\":{}}}",
            serde_json::to_string(&out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    println!("{json}");
    Ok(())
}

/// Redirect JSON output files to eli_research/data/ if they're in the project root.
fn redirect_finance_output(path: std::path::PathBuf) -> std::path::PathBuf {
    // Only redirect if it's a bare filename (no directory component)
    if path.parent().map(|p| p == std::path::Path::new("") || p == std::path::Path::new(".")).unwrap_or(true) {
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

    let req = eli_core::finance::MacroRequest { range };
    let resp = eli_core::finance::fetch_macro(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch macro")?;

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        std::fs::write(&out_path, &json).context("write output file")?;
    }

    println!("{json}");
    Ok(())
}

async fn cmd_finance_prices(args: FinancePricesArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let req = eli_core::finance::PricesRequest {
        query: args.query,
        asset_type: args.asset_type,
        ids: args.ids,
    };

    let resp = eli_core::finance::fetch_prices(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch prices")?;

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, json).context("write --out")?;
        println!(
            "{{\"ok\":true,\"path\":{}}}",
            serde_json::to_string(&out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    println!("{json}");
    Ok(())
}

async fn cmd_finance_odds(args: FinanceOddsArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let provider = args.provider.as_ref().map(|s| s.trim().to_ascii_lowercase());
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

    let resp = eli_core::finance::fetch_odds(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch odds")?;

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, json).context("write --out")?;
        println!(
            "{{\"ok\":true,\"path\":{}}}",
            serde_json::to_string(&out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    println!("{json}");
    Ok(())
}

async fn cmd_finance_options(args: FinanceOptionsArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    if args.summary && args.expirations {
        anyhow::bail!("use only one of --summary or --expirations");
    }

    let option_type = match args.option_type.as_deref().map(|s| s.trim().to_ascii_lowercase()) {
        None => None,
        Some(t) if t == "both" || t.is_empty() => None,
        Some(t) if t == "calls" || t == "puts" => Some(t),
        Some(other) => anyhow::bail!("invalid --type '{other}' (expected calls|puts|both)"),
    };

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

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, json).context("write --out")?;
        println!(
            "{{\"ok\":true,\"path\":{}}}",
            serde_json::to_string(&out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    println!("{json}");
    Ok(())
}

async fn cmd_finance_news(args: FinanceNewsArgs) -> Result<()> {
    let req = eli_core::finance::NewsRequest {
        ticker: args.ticker,
        date: args.date,
    };
    
    let resp = eli_core::finance::fetch_news(req).await
        .map_err(|e| anyhow::anyhow!(e))?;
    
    let json = serde_json::to_string_pretty(&resp)?;

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, &json).context("write --out")?;
        println!(
            "{{\"ok\":true,\"path\":{}}}",
            serde_json::to_string(&out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    println!("{}", json);
    Ok(())
}

async fn cmd_finance_snapshot(args: FinanceSnapshotArgs) -> Result<()> {
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

    let provider = match args.provider.trim().to_ascii_lowercase().as_str() {
        "mock" => eli_core::finance::ProviderKind::Mock,
        "yahoo" => eli_core::finance::ProviderKind::Yahoo,
        other => anyhow::bail!("unsupported --provider '{other}' (supported: mock, yahoo)"),
    };

    let req = eli_core::finance::SnapshotRequest { tickers, provider };
    let resp = eli_core::finance::fetch_snapshot(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch snapshot")?;

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, json).context("write --out")?;
        println!(
            "{{\"ok\":true,\"path\":{}}}",
            serde_json::to_string(&out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    println!("{json}");
    Ok(())
}

async fn cmd_finance_fundamentals(args: FinanceFundamentalsArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let req = eli_core::finance::FundamentalsRequest { ticker: args.ticker };
    let resp = eli_core::finance::fetch_fundamentals(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch fundamentals")?;

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, json).context("write --out")?;
        println!(
            "{{\"ok\":true,\"path\":{}}}",
            serde_json::to_string(&out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    println!("{json}");
    Ok(())
}

async fn cmd_finance_search(args: FinanceSearchArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let req = eli_core::finance::SearchRequest { query: args.query };
    let resp = eli_core::finance::fetch_search(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch search")?;

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, json).context("write --out")?;
        println!(
            "{{\"ok\":true,\"path\":{}}}",
            serde_json::to_string(&out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

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

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, json).context("write --out")?;
        println!(
            "{{\"ok\":true,\"path\":{}}}",
            serde_json::to_string(&out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

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

    let range = eli_core::finance::Span::parse(&args.range)
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse --range")?;
    let granularity = eli_core::finance::Span::parse(&args.granularity)
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse --granularity")?;

    let as_of = match args.as_of {
        Some(raw) => Some(
            eli_core::finance::parse_as_of(&raw)
                .map_err(|e| anyhow::anyhow!(e))
                .context("parse --as-of")?,
        ),
        None => None,
    };

    let provider = match args.provider.trim().to_ascii_lowercase().as_str() {
        "mock" => eli_core::finance::ProviderKind::Mock,
        "yahoo" => eli_core::finance::ProviderKind::Yahoo,
        "fred" => eli_core::finance::ProviderKind::Fred,
        other => anyhow::bail!(
            "unsupported --provider '{other}' (supported: mock, yahoo, fred)"
        ),
    };

    let cache_dir = if let Some(path) = args.cache_dir {
        path
    } else {
        let paths = Paths::discover().context("discover paths")?;
        paths.ensure_dirs().context("ensure dirs")?;
        paths.cache_dir
    };

    let req = eli_core::finance::TimeseriesRequest {
        tickers,
        range,
        granularity,
        as_of,
        provider,
        max_points_per_ticker: args.max_points_per_ticker,
    };

    let resp = eli_core::finance::fetch_timeseries(req, &cache_dir)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch timeseries")?;

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, json).context("write --out")?;
        println!(
            "{{\"ok\":true,\"path\":{},\"cache\":{}}}",
            serde_json::to_string(&out_path.display().to_string()).unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&resp.cache).unwrap_or_else(|_| "null".to_string())
        );
        return Ok(());
    }

    println!("{json}");
    Ok(())
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
    std::io::stdin().read_line(&mut input).context("read provider choice")?;
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
    std::io::stdin().read_line(&mut input).context("read model")?;
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
        std::io::stdin().read_line(&mut input).context("read api key")?;
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
    println!("{}", toml::to_string_pretty(&cfg).context("serialize config")?);
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
                cfg.chat.compact_trigger = Some(val.parse::<usize>().context("compact_trigger must be a number")?);
                println!("Set compact_trigger = {}", cfg.chat.compact_trigger.unwrap_or(0));
            }
            "compact_keep" => {
                cfg.chat.compact_keep = Some(val.parse::<usize>().context("compact_keep must be a number")?);
                println!("Set compact_keep = {}", cfg.chat.compact_keep.unwrap_or(0));
            }
            "summary_model" => {
                cfg.chat.summary_model = if val.trim().is_empty() { None } else { Some(val.clone()) };
                println!("Set summary_model = {}", cfg.chat.summary_model.clone().unwrap_or_else(|| "none".to_string()));
            }
            "parallel_commands" | "parallel_cmds" => {
                cfg.chat.parallel_commands = val.parse::<u32>().context("parallel_commands must be a number")?;
                println!("Set parallel_commands = {}", cfg.chat.parallel_commands);
            }
            "parallel_subagents" | "parallel_agents" => {
                cfg.chat.parallel_subagents = val.parse::<u32>().context("parallel_subagents must be a number")?;
                println!("Set parallel_subagents = {}", cfg.chat.parallel_subagents);
            }
            "scrollback_max_lines" | "scrollback" => {
                cfg.chat.scrollback_max_lines = val.parse::<usize>().context("scrollback_max_lines must be a number")?;
                println!("Set scrollback_max_lines = {}", cfg.chat.scrollback_max_lines);
            }
            other => {
                anyhow::bail!("Unknown config key: {}. Valid keys: provider, model, mem_steps, key, anthropic_key, openai_key, openrouter_key, sec_user_agent, compact, compact_trigger, compact_keep, summary_model, parallel_commands, parallel_subagents, scrollback_max_lines", other);
            }
        }

        config::save(&paths, &cfg).context("save config")?;
        return Ok(())
    }

    // Otherwise, print current config
    let cfg = config::load_or_default(&paths).context("load config")?;
    println!("Config file: {}", paths.config_file().display());
    println!("{}", toml::to_string_pretty(&cfg).context("serialize config")?);
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

            let possible_values = arg.get_value_parser().possible_values().map(|vals| {
                vals.map(|v| v.get_name().to_string()).collect::<Vec<_>>()
            });

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

            if let ValueHint::FilePath
            | ValueHint::DirPath
            | ValueHint::ExecutablePath = arg.get_value_hint()
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
    use crossterm::event::{Event, KeyEventKind};

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
                        if let Some(input) = ui.handle_key(key.code, key.modifiers) {
                            let trimmed = input.trim();

                            // Handle slash commands
                            if trimmed == "/exit" || trimmed == "/quit" {
                                break;
                            }
                            if trimmed == "/help" {
                                ui.add_message(
                                    "System",
                                    "Commands: /exit, /help, /model, /compact, /reset, /copy, /status\n/copy [scope] [> file] - Copy session: all, last, user, tools, N, -data\nKeys: Esc interrupt, ↑↓ history, PgUp/PgDn scroll",
                                );
                                continue;
                            }
                            if trimmed == "/model" || trimmed.starts_with("/model ") {
                                let model = trimmed.strip_prefix("/model").unwrap_or("").trim();
                                if model.is_empty() {
                                    ui.add_message("System", &format!("model: {}", cfg.chat.model));
                                } else {
                                    cfg.chat.model = model.to_string();
                                    ui.add_message("System", &format!("(model: {})", cfg.chat.model));
                                }
                                continue;
                            }
                            if trimmed == "/models" {
                                ui.add_message("System", &format!("model: {}\nset with: /model <name>", cfg.chat.model));
                                continue;
                            }
                            if trimmed == "/compact" {
                                match compact_memory_now(adapter.clone(), &cfg.chat, memory).await {
                                    Ok(Some(compaction)) => {
                                        let note = format!(
                                            "memory_compaction: dropped {} messages\n{}",
                                            compaction.dropped,
                                            compaction.summary
                                        );
                                        let brain_entry = format!(
                                            "\n### {} (session {})\n{}\n",
                                            chrono::Utc::now().to_rfc3339(),
                                            session_id,
                                            note
                                        );
                                        if let Err(e) = append_eli_brain(project_root, &brain_entry) {
                                            ui.add_message("System", &format!("(compacted, but failed to write brain: {e})"));
                                        } else {
                                            ui.add_message("System", &format!("memory: compacted ({} msgs)", compaction.dropped));
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
                                    Err(e) => ui.add_message("Error", &format!("compact failed: {e}")),
                                }
                                continue;
                            }
                            if trimmed == "/tip" {
                                ui.show_tips = !ui.show_tips;
                                ui.add_message(
                                    "System",
                                    if ui.show_tips { "Tips shown." } else { "Tips hidden." },
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
                                    &format!("Mode: AUTO | Tokens: {} | Time: {}s", ui.total_tokens, ui.elapsed_secs),
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
                        ).await;

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
    use eli_core::types::ChatStreamEvent;
    use futures::StreamExt;
    use crossterm::event as ct_event;
    use crossterm::event::{Event as CtEvent, KeyCode as CtKeyCode, KeyEventKind as CtKeyEventKind};

    let max_iters = if chat.auto { chat.max_auto.max(1) } else { 1 };
    let mut current_message = initial_message.clone();
    let mut current_images = images;

    for step in 1..=max_iters {
        // Update UI
        ui.tick_spinner();
        terminal.draw(ui)?;

        // Add message to memory
        if !current_images.is_empty() {
            memory.push(ChatMessage::user_with_images(current_message.clone(), current_images.clone()));
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
                let Ok(ev) = ct_event::read() else { continue; };
                match ev {
                    CtEvent::Key(key) => {
                        if key.kind != CtKeyEventKind::Press {
                            continue;
                        }
                        if key.code == CtKeyCode::Esc {
                            return true;
                        }
                        if let Some(input) = ui.handle_key(key.code, key.modifiers) {
                            let trimmed = input.trim();
                            if trimmed.eq_ignore_ascii_case("/exit") || trimmed.eq_ignore_ascii_case("/quit") {
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
            Ok(m) => m,
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
                break;
            }
        };

        // Add response to memory
        memory.push(ChatMessage::assistant(full_response.clone()));
        store
            .append(
                session_id,
                &SessionEvent {
                    ts: chrono::Utc::now(),
                    kind: EventKind::AssistantMessage {
                        content: full_response.clone(),
                    },
                },
            )
            .await
            .ok();

        // Show the answer/notes
        if let Some(synthesis) = &model.synthesis {
            if !synthesis.answer.trim().is_empty() {
                ui.add_message("Eli", synthesis.answer.trim());
            }
        } else if !model.notes.trim().is_empty() {
            ui.add_message("Eli", model.notes.trim());
        }

        // Execute commands if any
        if !model.commands.is_empty() && !matches!(chat.mode, RunMode::Read) {
            let mut all_tool_output = String::new();

            for cmd in &model.commands {
                ui.add_message("Tool", &format!("$ {}", cmd));
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
                terminal.draw(ui)?;

                let results = command_runner
                    .run_commands(&[cmd.clone()])
                    .await;

                for r in &results {
                    let icon = if r.returncode == 0 { "✓" } else { "✗" };
                    let output = if !r.stdout.trim().is_empty() {
                        r.stdout.lines().take(3).collect::<Vec<_>>().join("\n")
                    } else if !r.stderr.trim().is_empty() {
                        r.stderr.lines().take(2).collect::<Vec<_>>().join("\n")
                    } else {
                        String::new()
                    };
                    let line = format!("{} {}", icon, output);
                    ui.add_message("Tool", &line);
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

                    // Build full tool output for memory (LLM needs the actual data!)
                    all_tool_output.push_str(&format!("Command: {}\n", cmd));
                    all_tool_output.push_str(&format!("Return code: {}\n", r.returncode));
                    all_tool_output.push_str(&format!("Digest: {}\n", build_command_digest(r)));
                    if !r.stdout.trim().is_empty() {
                        all_tool_output.push_str(&format!("Output:\n{}\n", r.stdout));
                    }
                    if !r.stderr.trim().is_empty() {
                        all_tool_output.push_str(&format!("Stderr:\n{}\n", r.stderr));
                    }
                    all_tool_output.push('\n');

                    // Infer sources (never invent a generic "eli finance" source)
                    for source in infer_sources(cmd, &r.stdout) {
                        ui.add_source(source);
                    }
                    ui.last_tool_ok = Some(r.returncode == 0);
                }
                terminal.draw(ui)?;
            }

            // CRITICAL: Feed tool results back to memory so LLM can use the actual values!
            if !all_tool_output.is_empty() {
                memory.push(ChatMessage::user(format!("Tool execution results:\n{}", all_tool_output)));
            }
        }

        // Check if done
        if matches!(model.status, StepStatus::Done) {
            break;
        }

        // Continue with KEEP WORKING
        current_message = "KEEP WORKING".to_string();
    }

    Ok(())
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
        ).await;
    }

    // Non-TUI modes (Brain/Debug/Raw) use the old CLI approach
    print_banner(&cfg.chat, &project_root, &state);

    loop {
        // Show queue status if there are queued prompts
        let queue_len = state.prompt_queue.len();

        // Update token hint for the upcoming prompt
        if let Some(usage) = &state.last_usage {
            shared_input_tokens.store(usage.prompt_tokens as usize, std::sync::atomic::Ordering::Relaxed);
        }
        
        let (line, from_boxed_prompt) = if let Some(queued) = state.next_prompt() {
            print_history_line(format!("{}›{} {}", style::CYAN, style::RESET, queued));
            (queued, false)
        } else if matches!(state.display_mode, DisplayMode::Standard) && !force_plain_prompt {
            let Some(line) = read_line_boxed(&mut state, &mut cfg.chat, queue_len).context("boxed prompt")? else {
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
                        compaction.dropped,
                        compaction.summary
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
            println!("(bot: exec=work, approvals={})", format_approvals_display(&cfg.chat));
            continue;
        }
        if trimmed == "/yolo" {
            cfg.chat.mode = RunMode::Work;
            cfg.chat.approvals = ApprovalMode::Auto;
            cfg.chat.approvals_commands = None;
            cfg.chat.approvals_diffs = None;
            println!("(yolo: exec=work, approvals={})", format_approvals_display(&cfg.chat));
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
            print_history_line(format!("{}›{} {}", style::CYAN, style::RESET, queued_prompt));
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

    render(&mut footer, spinner_idx, &input_buffer, cursor_pos, state, chat);

    let maybe_line = loop {
        if esc_armed && Instant::now() > esc_deadline {
            esc_armed = false;
        }
        if last_anim.elapsed() > Duration::from_millis(120) {
            spinner_idx = (spinner_idx + 1) % FOOTER_SPINNER.len();
            render(&mut footer, spinner_idx, &input_buffer, cursor_pos, state, chat);
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
                render(&mut footer, spinner_idx, &input_buffer, cursor_pos, state, chat);
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

                render(&mut footer, spinner_idx, &input_buffer, cursor_pos, state, chat);
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
        format!("  {}/brain{}      full output (tools, history, details)", WHITE, RESET),
        format!("  {}/debug{}      debug output (raw request/response + tool output + observation)", WHITE, RESET),
        format!("  {}/standard{}   brief output (recent stream, summary)", WHITE, RESET),
        String::new(),
        format!("{}Execution{}", PURPLE, RESET),
        format!("  {}/mode{}       set exec mode (read/work)", WHITE, RESET),
        format!("  {}/read{}       set exec mode to read", WHITE, RESET),
        format!("  {}/work{}       set exec mode to work", WHITE, RESET),
        format!("  {}/bot{}        work; cmds auto, diffs ask", WHITE, RESET),
        format!("  {}/yolo{}       work; auto approvals", WHITE, RESET),
        String::new(),
        format!("{}Configuration{}", PURPLE, RESET),
        format!("  {}/model{}      set or show model for this session", WHITE, RESET),
        format!("  {}/key{}        set API key for current provider", WHITE, RESET),
        String::new(),
        format!("{}Queue{}", PURPLE, RESET),
        format!("  {}/queue /q{}   show queued prompts", WHITE, RESET),
        format!("  {}/cq{}         clear queue", WHITE, RESET),
        format!("  {}+<prompt>{}   queue a prompt for later", WHITE, RESET),
        String::new(),
        format!("{}Keyboard{}", PURPLE, RESET),
        format!("  {}Esc{}         interrupt current run (standard mode)", WHITE, RESET),
        format!("  {}Esc Esc{}     clear input (standard mode)", WHITE, RESET),
        format!("  {}Ctrl+C{}      clear input (standard mode)", WHITE, RESET),
        format!("  {}Ctrl+D{}      quit (standard mode)", WHITE, RESET),
        String::new(),
        format!("{}Session{}", PURPLE, RESET),
        format!("  {}/status /s{}  show current mode/stats", WHITE, RESET),
        format!("  {}/compact{}    summarize older context (reduce tokens)", WHITE, RESET),
        format!("  {}/reset{}      clear conversation", WHITE, RESET),
        format!("  {}/new{}        alias for /reset", WHITE, RESET),
        format!("  {}/tip{}        toggle tips (standard mode)", WHITE, RESET),
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
                kind: EventKind::Note { content: observation },
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

fn read_eli_brain_context(project_root: &Path, pinned_max: usize, tail_max: usize) -> Result<Option<String>> {
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
        (Some(pinned), Some(tail)) => Ok(Some(format!("ELI.md (pinned):\n{pinned}\n\nELI.md (recent):\n{tail}"))),
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
            messages.iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .into_iter()
                .collect()
        }
        "all" => {
            // All non-system messages
            messages.iter()
                .filter(|m| m.role != Role::System)
                .collect()
        }
        "user" => {
            messages.iter()
                .filter(|m| m.role == Role::User)
                .collect()
        }
        "assistant" => {
            messages.iter()
                .filter(|m| m.role == Role::Assistant)
                .collect()
        }
        "tools" => {
            messages.iter()
                .filter(|m| m.role == Role::Tool)
                .collect()
        }
        n if n.parse::<usize>().is_ok() => {
            // Last N turns (user + assistant pairs)
            let n: usize = n.parse().unwrap();
            let non_system: Vec<_> = messages.iter()
                .filter(|m| m.role != Role::System)
                .collect();
            non_system.into_iter().rev().take(n * 2).collect::<Vec<_>>().into_iter().rev().collect()
        }
        _ => {
            return Err(anyhow::anyhow!("unknown scope '{}'. Use: all, last, user, assistant, tools, or N", scope));
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
            format!("[output: {} chars, omitted with -data]\n", msg.content.len())
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
        eli_screen::clipboard_set(&output).await
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
        } else if matches!(c, ' ' | '-' | '_' | '.' | '/' | '\\' | ':' | ';' | ',' | '|') {
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

    let title = prompt.trim();
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
    md.push_str(prompt.trim());
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

fn slash_menu_lines() -> Vec<String> {
    use style::*;

    let mut lines = Vec::new();
    lines.push(format!("{}{}Slash Commands{} {}(↑/↓ to cycle){}", BOLD, CYAN, RESET, GRAY, RESET));
    lines.push(String::new());
    for cmd in SLASH_COMMANDS {
        lines.push(format!(
            "{}{:<14}{} {}{}{}",
            WHITE, cmd.name, RESET,
            GRAY, cmd.desc, RESET
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

async fn cmd_tui() -> Result<()> {
    let paths = Paths::discover().context("discover paths")?;
    let mut cfg = config::load_or_create(&paths).context("load/create config")?;
    ensure_tui_default_model(&mut cfg.chat);
    let adapter = eli_adapters::build_from_chat_config(&cfg.chat).context("build adapter")?;
    let adapter: Arc<dyn LlmAdapter> = Arc::from(adapter);

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
        project_root,
    );
    let store = SessionStore::new(&paths);
    let session_id = uuid::Uuid::new_v4().to_string();

    eli_tui::run(cfg.chat, adapter, diff_engine, command_runner, store, session_id)
        .await
        .context("run tui")?;
    Ok(())
}

fn apply_overrides(cfg: &mut ConfigFile, provider: Option<String>, model: Option<String>) -> Result<()> {
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
    let debug = matches!(state.display_mode, DisplayMode::Debug) || matches!(chat.display_mode, DisplayMode::Debug);
    let brief = matches!(state.display_mode, DisplayMode::Standard) && !matches!(chat.display_mode, DisplayMode::Debug);
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

    for step in 1..=max_iters {
        let step_start = Instant::now();
        state.step_count += 1;
        let mut step_observation: Option<String> = None;

        // Sequence fix: only push "KEEP WORKING" if the last message wasn't a tool observation.
        // This avoids double-user messages which crash some providers.
        let skip_keep_working = step > 1 && current_message == "KEEP WORKING" && memory.last_role() == Some(eli_core::types::Role::Tool);

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
                memory.push(ChatMessage::user_with_images(current_message.clone(), current_images.clone()));
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
                compaction.dropped,
                compaction.summary
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
                        kind: EventKind::Note { content: note.clone() },
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
        } else {
            print!("{}eli[{}]>{} connecting...", style::CYAN, step, style::RESET);
            std::io::stdout().flush().ok();
        }

        let stream_opt = if brief {
            let mut fut = Box::pin(adapter.chat_stream(req));
            loop {
                let changed = drain_run_key_events(state, &mut interrupted, &mut interrupted_by_esc);
                if last_anim.elapsed() > Duration::from_millis(120) || changed {
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

                let changed = drain_run_key_events(state, &mut interrupted, &mut interrupted_by_esc);
                if interrupted {
                    break;
                }

                if last_anim.elapsed() > Duration::from_millis(120) || changed {
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

        memory.push(ChatMessage::assistant(out.clone()));
        store
            .append(
                session_id,
                &SessionEvent {
                    ts: chrono::Utc::now(),
                    kind: EventKind::AssistantMessage { content: out.clone() },
                },
            )
            .await
            .ok();

        let model = match contract::validate_model_response(&out) {
            Ok(m) => m,
            Err(e) => {
                println!("eli: invalid response ({})", e);
                if !brief {
                    println!("{}", out);
                }
                break;
            }
        };

        // Track step time
        let step_elapsed = step_start.elapsed();

        // Print step summary (brief vs full)
        if brief {
            if step == 1 {
                // Force a scroll line so the first prompt is not overwritten.
                print_history_line(String::new());
            }
            print_step_summary_brief(step, step_elapsed, &model);
            render_footer(&mut footer, "ready", spinner_idx, Duration::ZERO, state, None);
        } else {
            print_step_summary(step, &model);
        }

        let mut read_mode = matches!(chat.mode, RunMode::Read);
        let mut approvals_ask_commands = matches!(chat.resolved_command_approvals(), ApprovalMode::Ask);
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

        if debug {
            println!("\n=== TOOL CALL ATTEMPTED ===");
            if model.commands.is_empty() && model.diffs.is_empty() && model.subagents.is_empty() && model.screen.is_empty() {
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
                        println!("  {} (model: {})", agent.name, agent.model.as_deref().unwrap_or("default"));
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
                    print_diff_results(&diff_results, true, brief);
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
                    print_diff_results(&diff_results, !apply, brief);
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
                if debug {
                    print_tool_results_debug(&command_results);
                } else {
                    print_command_results(
                        &command_results,
                        brief,
                        matches!(state.display_mode, DisplayMode::Brain),
                    );
                }
                if brief {
                    render_footer(&mut footer, "ready", spinner_idx, Duration::ZERO, state, None);
                }
            }

            if !model.screen.is_empty() && !read_mode && !brief {
                print_screen_results(&model.screen).await;
            }

            let command_results_for_llm = shadow_large_tool_outputs(project_root, &command_results);

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
                            kind: EventKind::Note { content: observation },
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

            let mut fut = Box::pin(run_subagents(adapter.clone(), chat, memory, &model.subagents));
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
            if !brief {
                print_subagent_results(&subagent_results);
            } else {
                println!("  subagents: {} completed", subagent_results.len());
            }
            if brief {
                render_footer(&mut footer, "ready", spinner_idx, Duration::ZERO, state, None);
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
                        kind: EventKind::Note { content: observation },
                    },
                )
                .await
                .ok();
        }

        // Capture trajectory
        let _ = trajectory_logger.append(&eli_core::trajectory::TrajectoryStep {
            session_id: session_id.to_string(),
            step_index: step as usize,
            timestamp: chrono::Utc::now(),
            input_messages: trajectory_input,
            model_output_raw: out.clone(),
            observation: step_observation,
            usage: state.last_usage.clone(),
        }).await;

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
	                            print_synthesis_box(&synthesis_title, synthesis);
	                        }
	                        // Skip print_answer_line - step summary already showed the answer
	                    }
	                    // Skip print_answer_line for notes - step summary already showed them
	                }

	                if profile == AgentProfile::Research {
	                    let status = if wants_user_input { "needs_user_input" } else { "done" };
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
                                    warn!("eli brain: failed to persist research pointer (ignored): {e}");
                                }
	                        }
	                        Ok(None) => {}
	                        Err(e) => warn!("failed to write research report (ignored): {e}"),
	                    }
	                }

	                // Show final summary for brief mode
	                let task_elapsed = task_start.elapsed();
	                state.total_work_time += task_elapsed;
	                if brief && step > 1 {
                    println!(
                        "\n{}✓{} done in {} ({} steps)",
                        style::GREEN, style::RESET,
                        format_duration(task_elapsed),
                        step
                    );
                }
                break;
            }
            StepStatus::KeepWorking => {
                if step == max_iters {
                    println!("(stopped: max autonomous steps reached)");
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

    println!(
        "{}({} / {}){}",
        GRAY,
        chat.provider,
        model,
        RESET
    );
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
            CYAN, step, RESET,
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
        lines.push(format!("{}◆{} focus: {}", YELLOW, RESET, model.focus.trim()));
    }

    if !model.checklist.is_empty() {
        lines.push(format!("{}checklist:{}", GRAY, RESET));
        for item in model.checklist.iter().take(4) {
            if !item.trim().is_empty() {
                lines.push(format!("  {}•{} {}", GREEN, RESET, item.trim()));
            }
        }
        if model.checklist.len() > 4 {
            lines.push(format!("  {}... +{} more{}", DARK_GRAY, model.checklist.len() - 4, RESET));
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
fn print_step_summary_brief(_step: u32, elapsed: Duration, model: &eli_core::contract::ModelResponse) {
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
            print_history_line(format!(
                "→ {}",
                focus
            ));
        }
        StepStatus::Done => {
            // Show the actual response/answer unboxed
            let answer = model
                .synthesis
                .as_ref()
                .map(|s| s.answer.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| model.notes.trim());
            if answer.is_empty() { return; }
            
            print_history_line(String::new());
            print_markdown(answer);
        }
    };
}

fn extract_insight(command_results: &[CommandResult], diff_results: &[DiffResult]) -> Option<String> {
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
    let stderr = result.stderr.trim();
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
        if let Some(saved_to) = stdout.split("saved_to=").nth(1).and_then(|s| s.split_whitespace().next()) {
            parts.push(format!("saved_to={saved_to}"));
        }
        if let Some(bytes) = stdout.split('(').nth(1).and_then(|s| s.split(" bytes").next()) {
            if bytes.chars().all(|c| c.is_ascii_digit()) {
                parts.push(format!("bytes={bytes}"));
            }
        }
        if let Some(points) = stdout.split("Data points: ").nth(1).and_then(|s| s.split('.').next()) {
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

    let looks_like_json = stdout.starts_with('{') || stdout.starts_with('[');
    if looks_like_json {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout) {
            return digest_from_json(&value, stdout_bytes);
        }
    }

    let lines = stdout.lines().count();
    format!("stdout_bytes={} lines={}", stdout_bytes, lines)
}

fn digest_from_json(value: &serde_json::Value, bytes: usize) -> String {
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

    parts.join(" ")
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

    let mut seen = std::collections::HashSet::new();
    let summary: Vec<String> = synthesis
        .summary
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.to_string()))
        .take(6)
        .map(|s| format!("{}•{} {}", GREEN, RESET, s))
        .collect();
    if !summary.is_empty() {
        if !lines.is_empty() { lines.push(String::new()); }
        lines.extend(summary);
    }

    if !synthesis.answer.trim().is_empty() {
        if !lines.is_empty() { lines.push(String::new()); }
        
        let answer = synthesis.answer.trim();
        // Ensure answer lines are formatted nicely? 
        // Just push it, format_indented_block handles wrapping.
        // We use a bullet for the answer block? Or just text?
        // Logic before was: format!("{}◆{} {}", CYAN, RESET, answer)
        // User wants unboxed.
        // If it's multi-line, prefixing with ◆ might look odd if not handled.
        // format_indented_block handles bullet indentation.
        
        // Let's just push lines, maybe with a bullet for the first line?
        // Or "◆ " prefix.
        
        lines.push(format!("{}◆{} {}", CYAN, RESET, answer));
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
        if !lines.is_empty() { lines.push(String::new()); }
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
            lines.push(format!("{}✗{} {}: {}error{} {}", RED, RESET, result.name, RED, RESET, err));
            continue;
        }
        if result.output.trim().is_empty() {
            lines.push(format!("{}✓{} {}: {}(no output){}", GREEN, RESET, result.name, GRAY, RESET));
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
    pub const TL: &str = "╭";  // top-left
    pub const TR: &str = "╮";  // top-right
    pub const BL: &str = "╰";  // bottom-left
    pub const BR: &str = "╯";  // bottom-right
    pub const H: &str = "─";   // horizontal
    pub const V: &str = "│";   // vertical

    // Colors (ANSI 256 / RGB where supported)
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";

    // Gradient palette for eli branding
    pub const CYAN: &str = "\x1b[38;5;51m";       // bright cyan
    pub const BLUE: &str = "\x1b[38;5;39m";       // bright blue
    pub const PURPLE: &str = "\x1b[38;5;141m";    // lavender
    pub const PINK: &str = "\x1b[38;5;213m";      // pink
    pub const GREEN: &str = "\x1b[38;5;120m";     // mint green
    pub const YELLOW: &str = "\x1b[38;5;227m";    // soft yellow
    pub const ORANGE: &str = "\x1b[38;5;215m";    // peach
    pub const RED: &str = "\x1b[38;5;203m";       // coral red
    pub const GRAY: &str = "\x1b[38;5;245m";      // medium gray
    pub const DARK_GRAY: &str = "\x1b[38;5;238m"; // dark gray
    pub const WHITE: &str = "\x1b[38;5;255m";     // bright white

    // Semantic colors
    pub const SUCCESS: &str = "\x1b[38;5;120m";   // mint
    pub const ERROR: &str = "\x1b[38;5;203m";     // coral
    pub const WARN: &str = "\x1b[38;5;215m";      // peach
    pub const INFO: &str = "\x1b[38;5;111m";      // soft blue
    pub const MUTED: &str = "\x1b[38;5;245m";     // gray

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
                            print_history_line(format!("{}›{} {}", style::CYAN, style::RESET, trimmed));
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
                            print_history_line(format!("{}›{} {}", style::CYAN, style::RESET, trimmed));
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
    let tail: String = input.chars().rev().take(tail_len).collect::<String>().chars().rev().collect();
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
    format!("cmd:{} diff:{}", format_approvals(cmds), format_approvals(diffs))
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

fn shadow_large_tool_outputs(project_root: &Path, results: &[CommandResult]) -> Vec<CommandResult> {
    const MAX_INLINE_JSON_BYTES: usize = 2 * 1024;

    let out_path = project_root
        .join("eli_research")
        .join("data")
        .join(".last_tool_output.json");
    let rel_path = out_path
        .strip_prefix(project_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| out_path.display().to_string());

    let mut out = Vec::with_capacity(results.len());
    for r in results {
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
        if !(stdout.starts_with('{') || stdout.starts_with('[')) {
            out.push(rr);
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(stdout) {
            Ok(v) => v,
            Err(_) => {
                out.push(rr);
                continue;
            }
        };

        if let Some(parent) = out_path.parent() {
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

        let json = serde_json::to_string_pretty(&value).unwrap_or_else(|_| stdout.to_string());
        if let Err(e) = std::fs::write(&out_path, &json) {
            rr.stderr = format!(
                "{}\n(data shadowing: failed to write '{}': {e})",
                rr.stderr.trim_end(),
                rel_path
            )
            .trim()
            .to_string();
            out.push(rr);
            continue;
        }

        let audit_path = {
            let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S%3f");
            out_path
                .parent()
                .unwrap_or(project_root)
                .join(format!("tool_output_{stamp}.json"))
        };
        let rel_audit_path = audit_path
            .strip_prefix(project_root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| audit_path.display().to_string());
        let audit_ok = match std::fs::write(&audit_path, &json) {
            Ok(()) => true,
            Err(e) => {
                rr.stderr = format!(
                    "{}\n(data shadowing: failed to write '{}': {e})",
                    rr.stderr.trim_end(),
                    rel_audit_path
                )
                .trim()
                .to_string();
                false
            }
        };

        let points = count_data_points(&value);
        let summary = format_suppressed_summary(&value, 8, 160);
        let hint = "More detail is available in the saved file; inspect with local tools if needed.";
        let bytes = json.as_bytes().len();
        let audit_fragment = if audit_ok {
            format!("; saved_copy={rel_audit_path}")
        } else {
            String::new()
        };
        rr.stdout = format!(
            "[OUTPUT SUPPRESSED] saved_to={rel_path} ({bytes} bytes){audit_fragment}. Data points: {points}.\n[SUMMARY]\n{summary}\n{hint}"
        );
        out.push(rr);
    }

    out
}

fn format_suppressed_summary(
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
                    lines.push(format!("event_samples: {}", list_sample(sample, 3, max_field_len)));
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
                    lines.push(format!("tag_samples: {}", list_sample(sample, 3, max_field_len)));
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
                    lines.push(format!("market_samples: {}", list_sample(sample, 3, max_field_len)));
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
                    lines.push(format!("snapshot_samples: {}", list_sample(sample, 3, max_field_len)));
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
                    lines.push(format!("price_samples: {}", list_sample(sample, 3, max_field_len)));
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
                    lines.push(format!("filing_samples: {}", list_sample(sample, 3, max_field_len)));
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
                    lines.push(format!("indicator_samples: {}", list_sample(sample, 3, max_field_len)));
                }
            }

            if let Some(data) = map.get("data") {
                if let serde_json::Value::Object(data_obj) = data {
                    let mut child_keys: Vec<&str> =
                        data_obj.keys().map(|k| k.as_str()).collect();
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

    let trimmed = lines.into_iter().take(max_lines).collect::<Vec<_>>();
    trimmed.into_iter().map(|l| format!("- {l}")).collect::<Vec<_>>().join("\n")
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
            let info_json =
                serde_json::to_string_pretty(&info).unwrap_or_else(|_| "<tool-info failed>".to_string());
            let sep = if r.stderr.trim().is_empty() { "" } else { "\n" };
            r.stderr = format!(
                "{}{}[TOOL INFO]\n{}",
                r.stderr.trim_end(),
                sep,
                info_json
            );
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
            rest.iter().any(|t| *t == "--list-events" || *t == "--list-series")
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
    let approvals_cmds = if approvals_ask_commands { "ask" } else { "auto" };
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
        style::YELLOW, style::RESET,
        prompt,
        style::GRAY, style::RESET
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
    println!(
        "\n{}?{} {}",
        style::CYAN, style::RESET,
        prompt
    );
    print!("{}›{} ", style::CYAN, style::RESET);
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).context("read input")?;
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
        let modified = results.iter().filter(|r| r.op == "replace" || r.op == "patch").count();
        let deleted = results.iter().filter(|r| r.op == "delete").count();

        let mut parts = Vec::new();
        if created > 0 { parts.push(format!("{}+{} created{}", GREEN, created, RESET)); }
        if modified > 0 { parts.push(format!("{}~{} modified{}", YELLOW, modified, RESET)); }
        if deleted > 0 { parts.push(format!("{}-{} deleted{}", RED, deleted, RESET)); }

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
        let (icon, color) = if r.success { ("✓", GREEN) } else { ("✗", RED) };
        println!(
            "  {}{}{} {}{} {}{}{}: {}",
            color, icon, RESET,
            BLUE, r.op, RESET,
            WHITE, r.path, RESET,
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
            let (icon, color) = if r.returncode == 0 { ("✓", GREEN) } else { ("✗", RED) };
            print_history_line(format!(
                "{}{}{} {}${} {}{}",
                color, icon, RESET,
                GRAY, RESET,
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
        let (icon, color) = if r.returncode == 0 { ("✓", GREEN) } else { ("✗", RED) };
        println!(
            "  {}{}{} {}${} {} {}{}ms{}",
            color, icon, RESET,
            GRAY, RESET,
            r.command,
            DARK_GRAY, r.duration_ms, RESET
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
                    println!("    {}... ({} more lines){}", DARK_GRAY, r.stdout.lines().count() - 20, RESET);
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
            GRAY, RESET,
            usage.total_tokens,
            DARK_GRAY, RESET,
            GREEN, RESET, cost, RESET
        ),
        format!(
            "{}      {} in         {} out",
            GRAY, usage.prompt_tokens,
            usage.completion_tokens
        ),
    ];

    if let Some(last) = &state.last_usage {
        let last_cost = estimate_cost(last, &chat.model);
        let mut extended = lines;
        extended.push(String::new());
        extended.push(format!(
            "{}last{}  {} tokens     {}${:.4}{}",
            GRAY, RESET,
            last.total_tokens,
            YELLOW, last_cost, RESET
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
mod tests {}
