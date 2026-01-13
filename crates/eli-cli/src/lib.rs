#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use eli_adapters::LlmAdapter;
use eli_core::config::{self, ApprovalMode, AutoMode, ConfigFile, DisplayMode, Paths, RunMode};
use eli_core::contract::{self, StepStatus};
use eli_core::diff::engine::{DiffEngine, DiffResult};
use eli_core::diff::engine::UndoManager;
use eli_core::executor::command_runner::{CommandResult, CommandRunner};
use eli_core::orchestrator::{maybe_compact_memory, run_subagents, SubagentResult};
use eli_core::persistence::{EventKind, SessionEvent, SessionStore};
use eli_core::types::{ChatMessage, ChatRequest, ProviderKind};
use futures::StreamExt;
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
use std::sync::{Arc, Mutex};
use std::path::{Path, PathBuf};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use std::time::{Duration, Instant};
use tracing::{info, warn};
use crossterm::event::{self as ct_event, Event as CtEvent, KeyCode as CtKeyCode, KeyEventKind, KeyModifiers as CtKeyModifiers};

#[derive(Clone, Debug)]
struct ResearchArtifact {
    rel_path: String,
    title: String,
    status: String,
    created_utc: String,
    answer_hint: Option<String>,
}

/// Runtime session state (not persisted to config)
struct SessionState {
    display_mode: DisplayMode,
    auto_mode: AutoMode,
    total_work_time: Duration,
    step_count: u32,
    last_run_was_research: bool,
    prompt_queue: Vec<String>,
    input_buffer: String,
    prompt_history: Vec<String>,
    history_cursor: Option<usize>,
    recent_research: Vec<ResearchArtifact>,
    total_usage: eli_core::types::Usage,
    last_usage: Option<eli_core::types::Usage>,
}

struct RawModeGuard {
    active: bool,
}

impl RawModeGuard {
    fn enable() -> Self {
        use std::io::Write;

        crossterm::terminal::enable_raw_mode().ok();
        // Hide cursor while we render our own input UI.
        print!("\x1b[?25l");
        std::io::stdout().flush().ok();
        Self { active: true }
    }

    fn disable(&mut self) {
        if !self.active {
            return;
        }
        use std::io::Write;

        self.active = false;
        // Always restore cursor visibility.
        print!("\x1b[?25h");
        std::io::stdout().flush().ok();
        crossterm::terminal::disable_raw_mode().ok();
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        self.disable();
    }
}

struct StickyFooter {
    #[allow(dead_code)]
    raw: RawModeGuard,
    width: usize,
    height: usize,
    footer_lines: usize,
}

impl StickyFooter {
    fn enable(footer_lines: usize) -> Self {
        let raw = RawModeGuard::enable();
        let (width, height) = terminal_size();
        let this = Self {
            raw,
            width,
            height,
            footer_lines: footer_lines.max(1),
        };
        this.apply_scroll_region();
        this
    }

    fn apply_scroll_region(&self) {
        use std::io::Write;
        let bottom = self.scroll_bottom();
        // Set scroll region to exclude the footer area at the bottom.
        // 1-based rows: [1, bottom] scroll; [bottom+1, height] fixed footer.
        print!("\x1b[1;{bottom}r");
        // Keep the cursor in the scrollable region so normal println! output doesn't overwrite the footer.
        print!("\x1b[{bottom};1H");
        std::io::stdout().flush().ok();
    }

    fn reset_scroll_region(&self) {
        use std::io::Write;
        // Reset scroll region to full screen.
        print!("\x1b[r");
        std::io::stdout().flush().ok();
    }

    fn scroll_bottom(&self) -> usize {
        // Ensure there is always at least 1 scrollable row.
        self.height.saturating_sub(self.footer_lines).max(1)
    }

    fn footer_top(&self) -> usize {
        self.scroll_bottom().saturating_add(1)
    }

    fn update_layout(&mut self, footer_lines: usize) {
        let (width, height) = terminal_size();
        let footer_lines = footer_lines.max(1);
        if width == self.width && height == self.height && footer_lines == self.footer_lines {
            return;
        }
        self.width = width;
        self.height = height;
        self.footer_lines = footer_lines;
        self.apply_scroll_region();
    }

    fn render(&mut self, lines: &[String]) {
        use std::io::Write;

        let top = self.footer_top();
        let mut row = top;

        for line in lines {
            if row > self.height {
                break;
            }
            print!("\x1b[{row};1H\x1b[K{line}", row = row, line = line);
            row += 1;
        }

        // Clear any remaining footer rows (when the footer shrinks).
        while row <= self.height {
            print!("\x1b[{row};1H\x1b[K", row = row);
            row += 1;
        }

        // Return cursor to bottom of scroll region for normal printing.
        let bottom = self.scroll_bottom();
        print!("\x1b[{bottom};1H", bottom = bottom);
        std::io::stdout().flush().ok();
    }
}

impl Drop for StickyFooter {
    fn drop(&mut self) {
        use std::io::Write;

        // Clear the footer region so we don't leave a stale box on screen when returning to normal output.
        let top = self.footer_top();
        for row in top..=self.height {
            print!("\x1b[{row};1H\x1b[K", row = row);
        }
        self.reset_scroll_region();
        std::io::stdout().flush().ok();
        // `RawModeGuard` drop restores cursor + raw mode.
    }
}

fn build_processing_footer_lines(
    spinner: usize,
    queue_len: usize,
    phase: &str,
    elapsed: Duration,
    input_buffer: &str,
    usage: &eli_core::types::Usage,
) -> Vec<String> {
    let (width, _) = terminal_size();
    let spinner = style::SPINNER[spinner % style::SPINNER.len()];
    let elapsed_display = format!("[{}s]", elapsed.as_secs());
    let queue_indicator = if queue_len > 0 {
        format!(" [{}Q]", queue_len)
    } else {
        String::new()
    };
    let total_tokens = usage.total_tokens;

    if width < 12 {
        let line_raw = format!("{spinner} {phase}{queue_indicator} {elapsed_display}  t:{total_tokens}");
        let line = truncate_to_visible_width(&line_raw, width.saturating_sub(1));
        return vec![line];
    }

    let inner_width = width.saturating_sub(2).max(1);
    let h = "─".repeat(inner_width);
    let top = format!("{}{}{}{}{}", style::BLUE, style::TL, h, style::TR, style::RESET);
    let bottom = format!("{}{}{}{}{}", style::BLUE, style::BL, h, style::BR, style::RESET);

    let top_status_raw = format!(
        "{} {spinner} {phase}{queue_indicator} {elapsed_display}{}",
        style::DARK_GRAY,
        style::RESET
    );
    let top_status = truncate_to_visible_width(&top_status_raw, width + 10);

    let bottom_status_raw = format!("{} {total_tokens} tokens{}", style::DARK_GRAY, style::RESET);
    let bottom_status = truncate_to_visible_width(&bottom_status_raw, width + 10);

    let input_lines_raw = wrap_with_prefix(input_buffer, " → ", "   ", inner_width);
    let mut input_lines: Vec<String> = Vec::with_capacity(input_lines_raw.len());
    for raw in input_lines_raw {
        let text = truncate_to_visible_width(&raw, inner_width);
        let visible = text.width().min(inner_width);
        let pad = " ".repeat(inner_width.saturating_sub(visible));
        input_lines.push(format!(
            "{}{}{}{}{}{}{}{}",
            style::BLUE,
            style::V,
            style::RESET,
            text,
            pad,
            style::BLUE,
            style::V,
            style::RESET
        ));
    }

    let mut lines: Vec<String> = Vec::new();
    lines.push(top_status);
    lines.push(top);
    lines.extend(input_lines);
    lines.push(bottom);
    lines.push(bottom_status);
    lines
}

fn render_processing_footer(footer: &mut Option<StickyFooter>, lines: Vec<String>) {
    if footer.is_none() {
        *footer = Some(StickyFooter::enable(lines.len()));
    }
    if let Some(footer) = footer.as_mut() {
        footer.update_layout(lines.len());
        footer.render(&lines);
    }
}

fn drain_run_key_events(
    state: &mut SessionState,
    interrupted: &mut bool,
    interrupted_by_esc: &mut bool,
) -> bool {
    let mut changed = false;
    while ct_event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let Ok(CtEvent::Key(key)) = ct_event::read() else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            CtKeyCode::Char(c) => {
                state.input_buffer.push(c);
                changed = true;
            }
            CtKeyCode::Backspace => {
                state.input_buffer.pop();
                changed = true;
            }
            CtKeyCode::Enter => {
                let trimmed = state.input_buffer.trim().to_string();
                if !trimmed.is_empty() {
                    if trimmed == "/stop" || trimmed == "/interrupt" {
                        *interrupted = true;
                        state.input_buffer.clear();
                        changed = true;
                        break;
                    }

                    println!("{}  ›{} {}", style::CYAN, style::RESET, trimmed);
                    state.queue_prompt(trimmed.clone());
                    state.prompt_history.push(trimmed);
                    state.input_buffer.clear();
                    changed = true;
                }
            }
            CtKeyCode::Esc => {
                *interrupted = true;
                *interrupted_by_esc = true;
                state.input_buffer.clear();
                changed = true;
                break;
            }
            _ => {}
        }
    }
    changed
}

fn drain_run_key_events_queue_only(state: &mut SessionState) -> bool {
    let mut changed = false;
    while ct_event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let Ok(CtEvent::Key(key)) = ct_event::read() else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            CtKeyCode::Char(c) => {
                state.input_buffer.push(c);
                changed = true;
            }
            CtKeyCode::Backspace => {
                state.input_buffer.pop();
                changed = true;
            }
            CtKeyCode::Enter => {
                let trimmed = state.input_buffer.trim().to_string();
                if !trimmed.is_empty() {
                    println!("{}  ›{} {}", style::CYAN, style::RESET, trimmed);
                    state.queue_prompt(trimmed.clone());
                    state.prompt_history.push(trimmed);
                    state.input_buffer.clear();
                    changed = true;
                }
            }
            CtKeyCode::Esc => {
                state.input_buffer.clear();
                changed = true;
            }
            _ => {}
        }
    }
    changed
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
            last_run_was_research: false,
            prompt_queue: Vec::new(),
            input_buffer: String::new(),
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
        name: "/standard",
        desc: "brief output (recent stream, summary)",
    },
    SlashCommand {
        name: "/brief",
        desc: "alias for /standard",
    },
    SlashCommand {
        name: "/plan",
        desc: "require human approval for plans",
    },
    SlashCommand {
        name: "/auto",
        desc: "autonomous: AI self-reviews until done",
    },
    SlashCommand {
        name: "/autonomous",
        desc: "alias for /auto",
    },
    SlashCommand {
        name: "/normal",
        desc: "default execution mode",
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
        name: "/quant",
        desc: "market/time-series research",
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
        name: "/reset",
        desc: "clear conversation",
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
        /// Set a config value: provider, model, mem_steps, key, compact, compact_trigger, compact_keep, summary_model, parallel_commands, parallel_subagents
        #[arg(long)]
        set: Option<String>,

        /// Value to set
        #[arg(long)]
        value: Option<String>,
    },

    /// Chat in a readline loop (default)
    Chat,

    /// One-shot quantitative research loop (no web; data via finance tool)
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
    #[arg(long)]
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
    #[arg(long, value_delimiter = ',')]
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
    #[arg(long)]
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
struct FinanceFilingsArgs {
    /// Ticker to fetch filings for.
    #[arg(long)]
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
    #[arg(long, value_delimiter = ',')]
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

    let cli = Cli::parse();

    match cli.cmd {
        None => cmd_chat(cli.provider, cli.model).await,
        Some(Command::Setup) => cmd_setup().await,
        Some(Command::Init) => cmd_init().await,
        Some(Command::Config { set, value }) => cmd_config(set, value).await,
        Some(Command::Chat) => cmd_chat(cli.provider, cli.model).await,
        Some(Command::Research { query }) => cmd_research(query, cli.provider, cli.model).await,
        Some(Command::Tui) => cmd_tui().await,
        Some(Command::Finance { cmd }) => cmd_finance(cmd).await,
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
    memory.set_system(eli_core::contract::quant_system_prompt());

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
    }
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
        std::fs::write(out_path, &json).context("write output file")?;
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
            other => {
                anyhow::bail!("Unknown config key: {}. Valid keys: provider, model, mem_steps, key, anthropic_key, openai_key, openrouter_key, compact, compact_trigger, compact_keep, summary_model, parallel_commands, parallel_subagents", other);
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

async fn cmd_chat(provider: Option<String>, model: Option<String>) -> Result<()> {
    let paths = Paths::discover().context("discover paths")?;
    let mut cfg = config::load_or_create(&paths).context("load/create config")?;
    apply_overrides(&mut cfg, provider, model)?;

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
    let mut quant_memory: Option<eli_core::memory::Memory> = None;
    let mut undo_stack: Vec<Vec<DiffResult>> = Vec::new();
    let mut state = SessionState::new(&cfg.chat);
    state.load_recent_research(&project_root, 12);

    print_banner(&cfg.chat, &project_root, &state);

    loop {
        // Show queue status if there are queued prompts
        let queue_len = state.prompt_queue.len();

        // Update token hint for the upcoming prompt
        if let Some(usage) = &state.last_usage {
            shared_input_tokens.store(usage.prompt_tokens as usize, std::sync::atomic::Ordering::Relaxed);
        }
        
        let (line, from_boxed_prompt) = if let Some(queued) = state.next_prompt() {
            println!("{}›{} {}", style::CYAN, style::RESET, queued);
            (queued, false)
        } else if matches!(state.display_mode, DisplayMode::Standard) {
            let Some(line) = read_line_boxed(&mut state, queue_len).context("boxed prompt")? else {
                break;
            };
            (line, true)
        } else {
            // Plain prompt - colors break readline cursor positioning
            let prompt_prefix = if queue_len > 0 {
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
            println!("{}›{} {}", style::CYAN, style::RESET, trimmed);
            state.prompt_history.push(trimmed.to_string());
        } else {
            // Add to history
            editor.add_history_entry(trimmed)?;
        }

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
        if trimmed == "/reset" {
            memory = eli_core::memory::Memory::new(cfg.chat.mem_steps);
            memory.set_system(eli_core::contract::system_prompt());
            ensure_eli_research_brain(&project_root).ok();
            quant_memory = None;
            state.total_work_time = Duration::ZERO;
            state.step_count = 0;
            println!("(reset)");
            continue;
        }
        if trimmed == "/quant" || trimmed.starts_with("/quant ") {
            let query = trimmed.strip_prefix("/quant").unwrap_or("").trim();
            if query.is_empty() {
                println!("usage: /quant <question>");
                continue;
            }

            let (q_clean, q_images) = process_input_for_images(query);
            let mut quant_chat = cfg.chat.clone();
            quant_chat.mode = RunMode::Read;
            quant_chat.approvals = ApprovalMode::Auto;
            quant_chat.auto = true;

	            let qmem = quant_memory.get_or_insert_with(|| {
	                let mut m = eli_core::memory::Memory::new(cfg.chat.mem_steps);
	                m.set_system(eli_core::contract::quant_system_prompt());
	                m
	            });

	            run_agent_steps(
	                &quant_chat,
	                adapter.clone(),
	                &diff_engine,
	                &command_runner,
	                &store,
                    &paths.data_dir,
	                &session_id,
	                &project_root,
	                qmem,
	                &mut undo_stack,
	                &mut state,
	                AgentProfile::Research,
	                q_clean,
                q_images,
            )
            .await?;
            print_cost_stats(&state, &cfg.chat);
            continue;
        }
        if trimmed == "/brain" {
            state.display_mode = DisplayMode::Brain;
            println!("(brain mode: full output)");
            continue;
        }
        if trimmed == "/standard" || trimmed == "/brief" {
            state.display_mode = DisplayMode::Standard;
            println!("(standard mode: brief output)");
            continue;
        }
        if trimmed == "/plan" {
            state.auto_mode = AutoMode::Plan;
            println!("(plan mode: human reviews plans)");
            continue;
        }
        if trimmed == "/auto" || trimmed == "/autonomous" {
            state.auto_mode = AutoMode::Autonomous;
            println!("(autonomous mode: AI self-reviews until done)");
            continue;
        }
        if trimmed == "/normal" {
            state.auto_mode = AutoMode::Normal;
            println!("(normal mode)");
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
                println!("model: {}", cfg.chat.model);
            } else {
                cfg.chat.model = model.to_string();
                println!("(model: {})", cfg.chat.model);
            }
            continue;
        }
        if trimmed == "/models" {
            println!("model: {}", cfg.chat.model);
            println!("set with: /model <name>");
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
        if trimmed == "/queue" || trimmed == "/q" {
            if state.prompt_queue.is_empty() {
                println!("(queue empty)");
            } else {
                println!("queued ({}):", state.queue_len());
                for (i, p) in state.prompt_queue.iter().enumerate() {
                    println!("  {}. {}", i + 1, truncate(p, 60));
                }
            }
            continue;
        }
        if trimmed == "/clear-queue" || trimmed == "/cq" {
            state.prompt_queue.clear();
            println!("(queue cleared)");
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

        editor.add_history_entry(trimmed).ok();

        // Process images
        let (clean_prompt, images) = process_input_for_images(trimmed);
        let use_quant =
            looks_like_quant_query(&clean_prompt) || looks_like_quant_follow_up(&clean_prompt, &state);

        if use_quant {
            let mut quant_chat = cfg.chat.clone();
            quant_chat.mode = RunMode::Read;
            quant_chat.approvals = ApprovalMode::Auto;
            quant_chat.auto = true;

		            let qmem = quant_memory.get_or_insert_with(|| {
		                let mut m = eli_core::memory::Memory::new(cfg.chat.mem_steps);
		                m.set_system(eli_core::contract::quant_system_prompt());
		                m
		            });

	            run_agent_steps(
	                &quant_chat,
	                adapter.clone(),
	                &diff_engine,
	                &command_runner,
	                &store,
                    &paths.data_dir,
	                &session_id,
	                &project_root,
	                qmem,
	                &mut undo_stack,
	                &mut state,
	                AgentProfile::Research,
	                clean_prompt,
                images,
            )
            .await?;
        } else {
            // Run agent for this prompt
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
        }

        // Process queue automatically
        while let Some(queued_prompt) = state.next_prompt() {
            println!("{}  ›{} {}", style::CYAN, style::RESET, queued_prompt);
            // Queue currently supports text only (no image dragging into queue command yet, 
            // though one could theoretically type the path, but process_input_for_images handles paths in string)
            let (q_clean, q_images) = process_input_for_images(&queued_prompt);
            
            let use_quant =
                looks_like_quant_query(&q_clean) || looks_like_quant_follow_up(&q_clean, &state);
            if use_quant {
                let mut quant_chat = cfg.chat.clone();
                quant_chat.mode = RunMode::Read;
                quant_chat.approvals = ApprovalMode::Auto;
                quant_chat.auto = true;

		                let qmem = quant_memory.get_or_insert_with(|| {
		                    let mut m = eli_core::memory::Memory::new(cfg.chat.mem_steps);
		                    m.set_system(eli_core::contract::quant_system_prompt());
		                    m
		                });

	                run_agent_steps(
	                    &quant_chat,
	                    adapter.clone(),
	                    &diff_engine,
	                    &command_runner,
	                    &store,
                        &paths.data_dir,
	                    &session_id,
	                    &project_root,
	                    qmem,
	                    &mut undo_stack,
	                    &mut state,
	                    AgentProfile::Research,
	                    q_clean,
                    q_images,
                )
                .await?;
            } else {
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
    }

    Ok(())
}

fn read_line_boxed(state: &mut SessionState, queue_len: usize) -> Result<Option<String>> {
    use std::io::Write;

    let prompt_history = &state.prompt_history;
    let total_tokens = state.total_usage.total_tokens;

    let mut input_buffer = std::mem::take(&mut state.input_buffer);
    let mut history_cursor = state.history_cursor;

    // Start with a single-line input box and grow as the user types (wrapping).
    let mut reserved_lines: usize = 5;
    for _ in 0..reserved_lines.saturating_sub(1) {
        println!();
    }
    std::io::stdout().flush().ok();

    crossterm::terminal::enable_raw_mode().ok();
    // Hide cursor while we render a "fake" input line.
    print!("\x1b[?25l");
    std::io::stdout().flush().ok();

    let start = Instant::now();
    let mut dots = 0usize;
    let mut last_anim = Instant::now();

    let needed_lines = |input: &str| -> usize {
        let (width, _) = terminal_size();
        if width < 12 {
            return 1;
        }
        let inner_width = width.saturating_sub(2).max(1);
        let input_lines = wrap_with_prefix(input, " → ", "   ", inner_width).len().max(1);
        input_lines + 4 // top_status + top + input lines + bottom + bottom_status
    };

    let render = |reserved_lines: usize,
                  dots: usize,
                  phase: &str,
                  input_buffer: &str,
                  history_cursor: Option<usize>|
     -> String {
        let (width, _) = terminal_size();
        let spinner = style::SPINNER[dots];
        let elapsed_display = format!("[{}s]", start.elapsed().as_secs());
        let queue_indicator = if queue_len > 0 {
            format!(" [{}Q]", queue_len)
        } else {
            String::new()
        };

        if width < 12 {
            let line_raw = format!("{spinner} {phase}{queue_indicator} {elapsed_display}  tokens:{total_tokens}");
            let line = truncate_to_visible_width(&line_raw, width.saturating_sub(1));

            let top_padding = reserved_lines.saturating_sub(1);
            let mut lines: Vec<String> = Vec::with_capacity(reserved_lines);
            for _ in 0..top_padding {
                lines.push(String::new());
            }
            lines.push(line);

            let up = reserved_lines.saturating_sub(1);
            let mut out = format!("\r\x1b[{up}A");
            for (idx, l) in lines.iter().enumerate() {
                out.push_str("\x1b[K");
                out.push_str(l);
                if idx + 1 < lines.len() {
                    out.push_str("\r\n");
                }
            }
            return out;
        }

        let inner_width = width.saturating_sub(2).max(1);
        let h = "─".repeat(inner_width);
        let top = format!("{}{}{}{}{}", style::BLUE, style::TL, h, style::TR, style::RESET);
        let bottom =
            format!("{}{}{}{}{}", style::BLUE, style::BL, h, style::BR, style::RESET);

        let input_tokens_est = input_buffer.len().saturating_div(4);
        let input_tokens_est = if input_buffer.trim().is_empty() {
            0
        } else {
            input_tokens_est.max(1)
        };
        let history_hint = match history_cursor {
            Some(idx) => format!("  hist:{}/{}", idx + 1, prompt_history.len()),
            None => String::new(),
        };

        // Top status: spinner + phase + time (Dark Gray)
        let top_status_raw = format!("{} {spinner} {phase}{queue_indicator} {elapsed_display}{}", style::DARK_GRAY, style::RESET);
        let top_status = truncate_to_visible_width(&top_status_raw, width + 10); // +10 for ansi

        // Bottom status: tokens (Dark Gray)
        let bottom_status_raw = if input_tokens_est > 0 {
            format!("{} tokens:{total_tokens}  input:~{input_tokens_est}{history_hint}{}", style::DARK_GRAY, style::RESET)
        } else {
            format!("{} tokens:{total_tokens}{history_hint}{}", style::DARK_GRAY, style::RESET)
        };
        let bottom_status = truncate_to_visible_width(&bottom_status_raw, width + 10);

        let input_lines_raw = wrap_with_prefix(input_buffer, " → ", "   ", inner_width);
        let mut input_lines: Vec<String> = Vec::with_capacity(input_lines_raw.len());
        for raw in input_lines_raw {
            let text = truncate_to_visible_width(&raw, inner_width);
            let visible = text.width().min(inner_width);
            let pad = " ".repeat(inner_width.saturating_sub(visible));
            input_lines.push(format!(
                "{}{}{}{}{}{}{}{}",
                style::BLUE,
                style::V,
                style::RESET,
                text,
                pad,
                style::BLUE,
                style::V,
                style::RESET
            ));
        }

        let mut all_lines: Vec<String> = Vec::new();
        all_lines.push(top_status);
        all_lines.push(top);
        all_lines.extend(input_lines);
        all_lines.push(bottom);
        all_lines.push(bottom_status);

        let top_padding = reserved_lines.saturating_sub(all_lines.len());
        let mut lines: Vec<String> = Vec::with_capacity(reserved_lines);
        for _ in 0..top_padding {
            lines.push(String::new());
        }
        lines.extend(all_lines);

        let up = reserved_lines.saturating_sub(1);
        let mut out = format!("\r\x1b[{up}A");
        for (idx, l) in lines.iter().enumerate() {
            out.push_str("\x1b[K");
            out.push_str(l);
            if idx + 1 < lines.len() {
                out.push_str("\r\n");
            }
        }
        out
    };

    // Initial render.
    let initial_needed = needed_lines(&input_buffer);
    if initial_needed > reserved_lines {
        for _ in 0..(initial_needed - reserved_lines) {
            println!();
        }
        std::io::stdout().flush().ok();
        reserved_lines = initial_needed;
    }
    print!("{}", render(reserved_lines, 0, "ready", &input_buffer, history_cursor));
    std::io::stdout().flush().ok();

    let maybe_line = loop {
        // Lightweight idle animation so the box feels alive.
        if last_anim.elapsed() > Duration::from_millis(120) {
            dots = (dots + 1) % style::SPINNER.len();
            let n = needed_lines(&input_buffer);
            if n > reserved_lines {
                for _ in 0..(n - reserved_lines) {
                    println!();
                }
                std::io::stdout().flush().ok();
                reserved_lines = n;
            }
            print!("{}", render(reserved_lines, dots, "ready", &input_buffer, history_cursor));
            std::io::stdout().flush().ok();
            last_anim = Instant::now();
        }

        if !ct_event::poll(Duration::from_millis(40)).unwrap_or(false) {
            continue;
        }

        let Ok(CtEvent::Key(key)) = ct_event::read() else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Ctrl+C = cancel current input (stay in prompt), Ctrl+D = EOF (exit).
        if key.modifiers.contains(CtKeyModifiers::CONTROL) {
            match key.code {
                CtKeyCode::Char('c') => {
                    input_buffer.clear();
                    history_cursor = None;
                    break Some(String::new());
                }
                CtKeyCode::Char('d') => break None,
                _ => {}
            }
        }

        match key.code {
            CtKeyCode::Char(c) => {
                history_cursor = None;
                input_buffer.push(c);
            }
            CtKeyCode::Backspace => {
                history_cursor = None;
                input_buffer.pop();
            }
            CtKeyCode::Up => {
                let Some(last_idx) = prompt_history.len().checked_sub(1) else {
                    // no history
                    continue;
                };
                let next = match history_cursor {
                    None => Some(last_idx),
                    Some(idx) => idx.checked_sub(1),
                };
                if let Some(idx) = next {
                    history_cursor = Some(idx);
                    input_buffer = prompt_history[idx].clone();
                }
            }
            CtKeyCode::Down => {
                let Some(idx) = history_cursor else {
                    continue;
                };
                let next = idx.saturating_add(1);
                if next >= prompt_history.len() {
                    history_cursor = None;
                    input_buffer.clear();
                } else {
                    history_cursor = Some(next);
                    input_buffer = prompt_history[next].clone();
                }
            }
            CtKeyCode::Esc => {
                history_cursor = None;
                input_buffer.clear();
            }
            CtKeyCode::Enter => {
                let line = input_buffer.clone();
                history_cursor = None;
                input_buffer.clear();
                break Some(line);
            }
            _ => {}
        }

        // Re-render immediately after input changes.
        let n = needed_lines(&input_buffer);
        if n > reserved_lines {
            for _ in 0..(n - reserved_lines) {
                println!();
            }
            std::io::stdout().flush().ok();
            reserved_lines = n;
        }
        print!("{}", render(reserved_lines, dots, "ready", &input_buffer, history_cursor));
        std::io::stdout().flush().ok();
    };

    state.input_buffer = input_buffer;
    state.history_cursor = history_cursor;

    // Clear the footer box before returning to normal printing.
    let up = reserved_lines.saturating_sub(1);
    print!("\r\x1b[{up}A\x1b[J\x1b[?25h");
    std::io::stdout().flush().ok();
    crossterm::terminal::disable_raw_mode().ok();

    Ok(maybe_line)
}

fn print_mode_status(state: &SessionState, chat: &eli_core::config::ChatConfig) {
    use style::*;

    let display = match state.display_mode {
        DisplayMode::Standard => "standard",
        DisplayMode::Brain => "brain",
    };
    let agent = match state.auto_mode {
        AutoMode::Normal => "normal",
        AutoMode::Plan => "plan",
        AutoMode::Autonomous => "autonomous",
    };
    let exec = format_mode(chat.mode);
    let approvals = format_approvals_display(chat);
    let auto_run = if chat.auto { "on" } else { "off" };
    let time = format_duration(state.total_work_time);

    let lines = vec![
        format!("{}status{}", CYAN, RESET),
        format!(
            "{}◆{} display {}{}{}  {}│{}  agent {}{}{}  {}│{}  exec {}{}{}",
            PURPLE, RESET, WHITE, display, RESET,
            DARK_GRAY, RESET, WHITE, agent, RESET,
            DARK_GRAY, RESET, WHITE, exec, RESET
        ),
        format!(
            "{}◆{} approvals {}{}{}  {}│{}  auto-run {}{}{}",
            GREEN, RESET, WHITE, approvals, RESET,
            DARK_GRAY, RESET, WHITE, auto_run, RESET
        ),
        format!(
            "{}◆{} steps {}{}{}  {}│{}  time {}{}{}",
            YELLOW, RESET, WHITE, state.step_count, RESET,
            DARK_GRAY, RESET, WHITE, time, RESET
        ),
    ];

    let out = format_indented_block(&lines);
    println!("{}", out);
}

fn print_help() {
    use style::*;

    let lines = vec![
        format!("{}{}Commands{}", BOLD, CYAN, RESET),
        String::new(),
        format!("{}Display{}", PURPLE, RESET),
        format!("  {}/brain{}      full output (tools, history, details)", WHITE, RESET),
        format!("  {}/standard{}   brief output (recent stream, summary)", WHITE, RESET),
        String::new(),
        format!("{}Agent Mode{}", PURPLE, RESET),
        format!("  {}/plan{}       require human approval for plans", WHITE, RESET),
        format!("  {}/auto{}       autonomous: AI self-reviews until done", WHITE, RESET),
        format!("  {}/normal{}     default execution mode", WHITE, RESET),
        String::new(),
        format!("{}Execution{}", PURPLE, RESET),
        format!("  {}/mode{}       set exec mode (read/work)", WHITE, RESET),
        format!("  {}/read{}       set exec mode to read", WHITE, RESET),
        format!("  {}/work{}       set exec mode to work", WHITE, RESET),
        format!("  {}/bot{}        work; cmds auto, diffs ask", WHITE, RESET),
        format!("  {}/yolo{}       work; auto approvals", WHITE, RESET),
        String::new(),
        format!("{}Quant{}", PURPLE, RESET),
        format!("  {}/quant <question>{} run market/time-series research", WHITE, RESET),
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
        format!("  {}Esc Esc Esc{} undo last edit after interrupt", WHITE, RESET),
        String::new(),
        format!("{}Session{}", PURPLE, RESET),
        format!("  {}/status /s{}  show current mode/stats", WHITE, RESET),
        format!("  {}/reset{}      clear conversation", WHITE, RESET),
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
    let brain = ensure_eli_research_brain(project_root)?;
    let content = std::fs::read_to_string(&brain).context("read eli_research/ELI.md")?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    if max_chars == 0 {
        return Ok(Some(trimmed.to_string()));
    }

    let total = trimmed.chars().count();
    if total <= max_chars {
        return Ok(Some(trimmed.to_string()));
    }

    let tail: String = trimmed.chars().skip(total - max_chars).collect();
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
    let cfg = config::load_or_create(&paths).context("load/create config")?;
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

    eli_tui::run(cfg.chat, adapter, diff_engine, command_runner, store, session_id).await.context("run tui")?;
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
    let brief = matches!(state.display_mode, DisplayMode::Standard);
    let mut sticky_footer: Option<StickyFooter> = None;
    let synthesis_title = format_synthesis_title(&initial_user_message);
    let mut task_had_actions = false;
    let mut task_insights: Vec<String> = Vec::new();
    let mut saw_finance_timeseries = false;
    let mut saw_finance_snapshot = false;
    let mut current_message = initial_user_message;
    let mut current_images = initial_images;
    let root_prompt = current_message.clone();
    state.last_run_was_research = profile == AgentProfile::Research;

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
                        kind: EventKind::Note { content: note },
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
        if let Some(ctx) = state.recent_research_context(5, 1800) {
            insert_system_context_before_conversation(&mut messages, ChatMessage::system(ctx));
        }
        let trajectory_input = messages.clone();

        let req = ChatRequest {
            model: chat.model.clone(),
            messages,
            temperature: chat.temperature,
            max_tokens: chat.max_tokens,
            stream: true,
        };

        use std::io::Write;
        let mut out = String::new();
        let mut interrupted = false;
        let mut interrupted_by_esc = false;
        let mut last_anim = Instant::now();
        let mut spinner = 0usize;

        let connect_start = Instant::now();
        if brief {
            render_processing_footer(
                &mut sticky_footer,
                build_processing_footer_lines(
                    spinner,
                    state.queue_len(),
                    "connecting",
                    connect_start.elapsed(),
                    &state.input_buffer,
                    &state.total_usage,
                ),
            );
        } else {
            print!("{}eli[{}]>{} connecting...", style::CYAN, step, style::RESET);
            std::io::stdout().flush().ok();
        }

        let stream_opt = if brief {
            let mut fut = Box::pin(adapter.chat_stream(req));
            loop {
                let changed = drain_run_key_events(state, &mut interrupted, &mut interrupted_by_esc);
                if last_anim.elapsed() > Duration::from_millis(80) || changed {
                    if last_anim.elapsed() > Duration::from_millis(80) {
                        spinner = (spinner + 1) % style::SPINNER.len();
                        last_anim = Instant::now();
                    }
                    render_processing_footer(
                        &mut sticky_footer,
                        build_processing_footer_lines(
                            spinner,
                            state.queue_len(),
                            "connecting",
                            connect_start.elapsed(),
                            &state.input_buffer,
                            &state.total_usage,
                        ),
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
                render_processing_footer(
                    &mut sticky_footer,
                    build_processing_footer_lines(
                        spinner,
                        state.queue_len(),
                        "thinking",
                        thinking_start.elapsed(),
                        &state.input_buffer,
                        &state.total_usage,
                    ),
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
                    _ = tokio::time::sleep(Duration::from_millis(30)) => {}
                }

                let changed = drain_run_key_events(state, &mut interrupted, &mut interrupted_by_esc);
                if interrupted {
                    break;
                }

                if last_anim.elapsed() > Duration::from_millis(80) || changed {
                    if last_anim.elapsed() > Duration::from_millis(80) {
                        spinner = (spinner + 1) % style::SPINNER.len();
                        last_anim = Instant::now();
                    }
                    // Re-render after input/animation so the footer never flickers between tool output.
                    // (Keeps the cursor in the scroll region too.)
                    render_processing_footer(
                        &mut sticky_footer,
                        build_processing_footer_lines(
                            spinner,
                            state.queue_len(),
                            "thinking",
                            thinking_start.elapsed(),
                            &state.input_buffer,
                            &state.total_usage,
                        ),
                    );
                }
            }
        }

        let mut undo_after_interrupt = false;
        if brief && interrupted_by_esc {
            // Triple-ESC workflow:
            // 1) ESC interrupts the current run
            // 2) ESC shows warning (arms undo)
            // 3) ESC confirms undo
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
                                "\r\x1b[K  {}!{} press {}Esc{} again to undo last edit",
                                style::YELLOW,
                                style::RESET,
                                style::WHITE,
                                style::RESET
                            );
                            std::io::stdout().flush().ok();
                            deadline = Instant::now() + Duration::from_secs(2);
                        } else {
                            undo_after_interrupt = true;
                            break;
                        }
                    }
                    CtKeyCode::Char(c) => {
                        state.input_buffer.push(c);
                        break;
                    }
                    CtKeyCode::Backspace => {
                        state.input_buffer.pop();
                        break;
                    }
                    _ => break,
                }
            }

            // Clear any arming hint before returning to the normal prompt.
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
            if undo_after_interrupt {
                perform_undo(undo_stack, memory, store, session_id).await?;
            }
            break; 
        }

        if out.trim().is_empty() {
            warn!("empty assistant message");
            break;
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
            print_step_summary_brief(step, step_elapsed, &model);
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
                            drop(sticky_footer.take());
                        }
                        let ans = confirm("Apply diffs?")?;
                        if brief {
                            render_processing_footer(
                                &mut sticky_footer,
                                build_processing_footer_lines(
                                    spinner,
                                    state.queue_len(),
                                    "exec",
                                    Duration::ZERO,
                                    &state.input_buffer,
                                    &state.total_usage,
                                ),
                            );
                        }
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
                        render_processing_footer(
                            &mut sticky_footer,
                            build_processing_footer_lines(
                                spinner,
                                state.queue_len(),
                                "exec",
                                exec_start.elapsed(),
                                &state.input_buffer,
                                &state.total_usage,
                            ),
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
                                _ = tokio::time::sleep(Duration::from_millis(40)) => {}
                            }

                            let changed = drain_run_key_events_queue_only(state);
                            if last_anim.elapsed() > Duration::from_millis(80) || changed {
                                if last_anim.elapsed() > Duration::from_millis(80) {
                                    spinner = (spinner + 1) % style::SPINNER.len();
                                    last_anim = Instant::now();
                                }
                                render_processing_footer(
                                    &mut sticky_footer,
                                    build_processing_footer_lines(
                                        spinner,
                                        state.queue_len(),
                                        "exec",
                                        exec_start.elapsed(),
                                        &state.input_buffer,
                                        &state.total_usage,
                                    ),
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
                            drop(sticky_footer.take());
                        }
                        let ans = confirm("Run commands?")?;
                        if brief {
                            render_processing_footer(
                                &mut sticky_footer,
                                build_processing_footer_lines(
                                    spinner,
                                    state.queue_len(),
                                    "exec",
                                    Duration::ZERO,
                                    &state.input_buffer,
                                    &state.total_usage,
                                ),
                            );
                        }
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
                            render_processing_footer(
                                &mut sticky_footer,
                                build_processing_footer_lines(
                                    spinner,
                                    state.queue_len(),
                                    "exec",
                                    exec_start.elapsed(),
                                    &state.input_buffer,
                                    &state.total_usage,
                                ),
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
                                    _ = tokio::time::sleep(Duration::from_millis(40)) => {}
                                }

                                let changed = drain_run_key_events_queue_only(state);
                                if last_anim.elapsed() > Duration::from_millis(80) || changed {
                                    if last_anim.elapsed() > Duration::from_millis(80) {
                                        spinner = (spinner + 1) % style::SPINNER.len();
                                        last_anim = Instant::now();
                                    }
                                    render_processing_footer(
                                        &mut sticky_footer,
                                        build_processing_footer_lines(
                                            spinner,
                                            state.queue_len(),
                                            "exec",
                                            exec_start.elapsed(),
                                            &state.input_buffer,
                                            &state.total_usage,
                                        ),
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

            let insight = extract_insight(&command_results, &diff_results);
            if let Some(ref line) = insight {
                if task_insights.last().map(|s| s != line).unwrap_or(true) {
                    if task_insights.len() < 6 {
                        task_insights.push(line.to_string());
                    }
                }
            }

            if !command_results.is_empty() {
                print_command_results(&command_results, brief);
            }

            if !model.screen.is_empty() && !read_mode && !brief {
                print_screen_results(&model.screen).await;
            }

                            if !diff_results.is_empty()
                                || !command_results.is_empty()
                                || !model.screen.is_empty()
                            {
                                task_had_actions = true;
                                let observation =
                                    build_observation(read_mode, approvals_ask_commands, approvals_ask_diffs, &diff_results, &command_results);
                                step_observation = Some(observation.clone());
                                memory.push(ChatMessage::tool(observation.clone(), "eli"));                store
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
            render_processing_footer(
                &mut sticky_footer,
                build_processing_footer_lines(
                    spinner,
                    state.queue_len(),
                    "agents",
                    agents_start.elapsed(),
                    &state.input_buffer,
                    &state.total_usage,
                ),
            );

            let mut fut = Box::pin(run_subagents(adapter.clone(), chat, memory, &model.subagents));
            let results = loop {
                tokio::select! {
                    res = &mut fut => {
                        break res;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(40)) => {}
                }

                let changed = drain_run_key_events_queue_only(state);
                if last_anim.elapsed() > Duration::from_millis(80) || changed {
                    if last_anim.elapsed() > Duration::from_millis(80) {
                        spinner = (spinner + 1) % style::SPINNER.len();
                        last_anim = Instant::now();
                    }
                    render_processing_footer(
                        &mut sticky_footer,
                        build_processing_footer_lines(
                            spinner,
                            state.queue_len(),
                            "agents",
                            agents_start.elapsed(),
                            &state.input_buffer,
                            &state.total_usage,
                        ),
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
            let observation = build_subagent_observation(&subagent_results);
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

        if profile == AgentProfile::Research && matches!(model.status, StepStatus::Done) && !wants_user_input {
            if !saw_finance_timeseries && !saw_finance_snapshot {
                let msg = "policy_violation: research mode requires at least one market data fetch via `eli finance timeseries` or `eli finance snapshot` before answering. If analyzing price action, zoom out first (e.g., --range 5y --granularity 1mo) and include correlates.";
                if !brief {
                    println!("{msg}");
                } else {
                    println!("  (policy) forcing market data fetch");
                }
                memory.push(ChatMessage::tool(msg.to_string(), "eli.policy"));
                store
                    .append(
                        session_id,
                        &SessionEvent {
                            ts: chrono::Utc::now(),
                            kind: EventKind::Note {
                                content: msg.to_string(),
                            },
                        },
                    )
                    .await
                    .ok();

                current_message = "KEEP WORKING".to_string();
                continue;
            }
        }

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
                    drop(sticky_footer.take());
                }
                let (msg, imgs) = prompt_user(ask.trim())?;
                current_message = msg;
                current_images = imgs;
                continue;
            }
        }

        current_message = "KEEP WORKING".to_string();
    }

    Ok(())
}

fn print_banner(chat: &eli_core::config::ChatConfig, project_root: &Path, state: &SessionState) {
    use style::*;

    // ASCII art logo with gradient
    let logo = format!(
        r#"
{CYAN}{BOLD}  ███████╗██╗     ██╗{RESET}
{BLUE}{BOLD}  ██╔════╝██║     ██║{RESET}     {WHITE}financial coding agent{RESET}
{PURPLE}{BOLD}  █████╗  ██║     ██║{RESET}     {GRAY}v0.1.0{RESET}
{PINK}{BOLD}  ██╔══╝  ██║     ██║{RESET}
{CYAN}{BOLD}  ███████╗███████╗██║{RESET}
{BLUE}{BOLD}  ╚══════╝╚══════╝╚═╝{RESET}
"#
    );
    println!("{}", logo);

    let mode = format_mode(chat.mode);
    let approvals = format_approvals_display(chat);
    let auto = if chat.auto { "on" } else { "off" };
    let compact = if chat.compact { "on" } else { "off" };
    let model = truncate_middle(&chat.model, 42);
    let root = format_root_path(project_root);
    let display = match state.display_mode {
        DisplayMode::Standard => "standard",
        DisplayMode::Brain => "brain",
    };
    let agent = match state.auto_mode {
        AutoMode::Normal => "normal",
        AutoMode::Plan => "plan",
        AutoMode::Autonomous => "autonomous",
    };

    // Status section with styled labels
    let lines = vec![
        format!("{CYAN}●{RESET} {WHITE}{}{RESET} {GRAY}›{RESET} {BLUE}{}{RESET}", chat.provider, model),
        format!("{GRAY}  cwd{RESET} {}", root),
        String::new(),
        format!(
            "{PURPLE}◆{RESET} display {WHITE}{}{RESET}  {GRAY}│{RESET}  agent {WHITE}{}{RESET}  {GRAY}│{RESET}  exec {WHITE}{}{RESET}",
            display, agent, mode
        ),
        format!(
            "{GREEN}◆{RESET} approvals {WHITE}{}{RESET}  {GRAY}│{RESET}  auto-run {WHITE}{}{RESET}  {GRAY}│{RESET}  compact {WHITE}{}{RESET}",
            approvals, auto, compact
        ),
        format!(
            "{YELLOW}◆{RESET} memory {WHITE}{}{RESET} steps  {GRAY}│{RESET}  parallel {WHITE}{}{RESET} cmds (cap) / {WHITE}{}{RESET} agents (cap)",
            chat.mem_steps, chat.parallel_commands, chat.parallel_subagents
        ),
        String::new(),
        format!(
            "{DARK_GRAY}shortcuts:{RESET} {MUTED}Esc  /help  /status  /mode  /model  /undo  /exit{RESET}"
        ),
    ];

    let out = format_indented_block(&lines);
    println!("{}", out);
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
    use style::*;
    use std::io::Write;

    let secs = elapsed.as_secs_f32();
    let time_str = format!("{:.1}s", secs);

    match model.status {
        StepStatus::KeepWorking => {
            // Show focus/plan when still working
            let focus = if model.focus.trim().is_empty() {
                model.notes.lines().next().unwrap_or("").trim()
            } else {
                model.focus.trim()
            };
            println!(
                "  {}→{} {} {}{}{}",
                BLUE, RESET,
                truncate(focus, 65),
                DARK_GRAY, time_str, RESET
            );
        }
        StepStatus::Done => {
            // Show the actual response/answer unboxed
            let answer = model.notes.trim();
            if answer.is_empty() { return; }
            
            println!(); // Ensure one empty line before response
            
            let mut lines = Vec::new();
            for line in answer.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() { 
                    lines.push(String::new());
                    continue; 
                }
                
                // If it already starts with a bullet/dash, preserve it but colored
                // Otherwise add a bullet
                if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("• ") {
                    lines.push(format!("{}{}", style::RESET, trimmed));
                } else {
                    lines.push(format!("{}•{} {}", style::DARK_GRAY, style::RESET, trimmed));
                }
            }
            
            let out = format_indented_block(&lines);
            print!("{}", out);
            std::io::stdout().flush().ok();
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

fn synthesis_has_content(synthesis: &eli_core::contract::Synthesis) -> bool {
    !synthesis.summary.is_empty()
        || !synthesis.next_steps.is_empty()
        || !synthesis.answer.trim().is_empty()
}

fn format_synthesis_title(_user_message: &str) -> String {
    String::new()
}

fn print_synthesis_box(title: &str, synthesis: &eli_core::contract::Synthesis) {
    use style::*;

    let mut lines = Vec::new();
    // Header removed as per user request ("eli" name gone)
    if !title.trim().is_empty() {
         lines.push(format!("{}{}{}", GRAY, title, RESET));
    }

    let summary: Vec<String> = synthesis
        .summary
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
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

    // Spinner frames (Braille-based)
    pub const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    // Alternative spinners
    pub const DOTS: &[&str] = &["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];
    pub const PULSE: &[&str] = &["◜", "◠", "◝", "◞", "◡", "◟"];
}

fn format_box_string(lines: &[String]) -> String {
    format_indented_block(lines)
}

fn format_indented_block(lines: &[String]) -> String {
    if lines.is_empty() {
        return String::new();
    }

    if lines.is_empty() {
        return String::new();
    }

    // Use terminal width minus 2 (margin) minus 1 (safety)
    let (term_width, _term_height) = terminal_size();
    
    // Threshold Check
    if term_width < 40 {
        return lines.join("\n");
    }

    let term_width = term_width.min(140);
    // Margin on left
    let margin = 2;
    let max_content_width = term_width.saturating_sub(margin + 1);

    if max_content_width == 0 {
        return lines.join("\n");
    }

    let mut wrapped_lines = Vec::new();
    for line in lines {
        let line = sanitize_for_box(line);
        if line.is_empty() {
            wrapped_lines.push(String::new());
            continue;
        }

        // Strip ANSI for processing, but keep track of original
        let visible_line = strip_ansi(&line);

        // Find prefix length (bullet points or numbers) based on visible text
        let mut prefix_visible_len = if visible_line.starts_with("- ") {
            2
        } else if visible_line.starts_with("• ") {
            2
        } else if visible_line.starts_with("=> ") {
            3
        } else if visible_line.starts_with("→ ") {
            2 // → is one char visually
        } else if visible_line.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
            if let Some(pos) = visible_line.find(". ") {
                pos + 2
            } else {
                0
            }
        } else {
            0
        };
        // Don't let indent consume the entire line in narrow terminals.
        prefix_visible_len = prefix_visible_len.min(max_content_width.saturating_sub(1));

        // If line fits, just add it
        if visible_line.width() <= max_content_width {
            wrapped_lines.push(line);
            continue;
        }

        // Need to wrap - work on the original line (preserving ANSI codes).
        let indent = " ".repeat(prefix_visible_len);
        let mut current_visible_len = 0usize;
        let mut current_line = String::new();
        let mut current_prefix_len = 0usize; // 0 for first line, prefix_visible_len for continuations
        let mut is_first_wrapped_line = true;

        for word in line.split_whitespace() {
            let word_visible_len = strip_ansi(word).width();

            // If we're at the start of the line (only prefix), don't insert a leading space.
            let space_needed = if current_visible_len == current_prefix_len { 0 } else { 1 };
            if current_visible_len + space_needed + word_visible_len <= max_content_width {
                if space_needed == 1 {
                    current_line.push(' ');
                    current_visible_len += 1;
                }
                current_line.push_str(word);
                current_visible_len += word_visible_len;
                continue;
            }

            // Word doesn't fit; flush the current line if it has content beyond the prefix.
            if current_visible_len > current_prefix_len {
                wrapped_lines.push(current_line);
                is_first_wrapped_line = false;
            }

            // Start (or restart) the next line (continuation lines are indented).
            current_line = if is_first_wrapped_line {
                String::new()
            } else {
                indent.clone()
            };
            current_prefix_len = if is_first_wrapped_line { 0 } else { prefix_visible_len };
            current_visible_len = current_prefix_len;

            let available_width = max_content_width.saturating_sub(current_prefix_len).max(1);
            if word_visible_len <= available_width {
                current_line.push_str(word);
                current_visible_len += word_visible_len;
                continue;
            }

            // Hard-wrap long "words" (paths, code, etc) to prevent the right border from wrapping.
            let chunks = split_ansi_chunks(word, available_width);
            for (idx, chunk) in chunks.iter().enumerate() {
                if idx > 0 {
                    wrapped_lines.push(current_line);
                    is_first_wrapped_line = false;
                    current_line = indent.clone();
                    current_visible_len = prefix_visible_len;
                    current_prefix_len = prefix_visible_len;
                }
                current_line.push_str(chunk);
                current_visible_len += strip_ansi(chunk).width();
            }
        }
        if current_visible_len > current_prefix_len {
            wrapped_lines.push(current_line);
        }
    }

    // Just print as indented block
    let mut out = String::new();
    let margin_str = " ".repeat(margin);

    // One blank line top?
    // out.push('\n'); 

    for line in &wrapped_lines {
        // strip ansi for checking emptiness? no, keep it simple
        out.push_str(&margin_str);
        out.push_str(line);
        out.push('\n');
    }
    
    out
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

fn truncate_to_visible_width(input: &str, max_visible_width: usize) -> String {
    if max_visible_width == 0 {
        return String::new();
    }

    let mut out = String::new();
    let mut width = 0usize;
    for c in input.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if width + w > max_visible_width {
            break;
        }
        out.push(c);
        width += w;
    }
    out
}

fn wrap_to_visible_width(input: &str, max_visible_width: usize) -> Vec<String> {
    if max_visible_width == 0 {
        return vec![String::new()];
    }

    let mut out: Vec<String> = Vec::new();
    for raw_line in input.split('\n') {
        let mut current = String::new();
        let mut width = 0usize;
        for ch in raw_line.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if width + w > max_visible_width && !current.is_empty() {
                out.push(current);
                current = String::new();
                width = 0;
            }
            current.push(ch);
            width = width.saturating_add(w);
            if width >= max_visible_width {
                out.push(current);
                current = String::new();
                width = 0;
            }
        }
        out.push(current);
    }

    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn wrap_with_prefix(
    input: &str,
    prefix_first: &str,
    prefix_cont: &str,
    inner_width: usize,
) -> Vec<String> {
    if inner_width == 0 {
        return vec![String::new()];
    }

    let prefix_width = prefix_first.width();
    if inner_width <= prefix_width {
        return vec![truncate_to_visible_width(prefix_first, inner_width)];
    }

    let content_width = inner_width.saturating_sub(prefix_width).max(1);
    let wrapped = wrap_to_visible_width(input, content_width);
    if wrapped.is_empty() {
        return vec![truncate_to_visible_width(prefix_first, inner_width)];
    }

    let mut out = Vec::with_capacity(wrapped.len());
    for (idx, line) in wrapped.into_iter().enumerate() {
        let prefix = if idx == 0 { prefix_first } else { prefix_cont };
        let combined = format!("{prefix}{line}");
        out.push(truncate_to_visible_width(&combined, inner_width));
    }
    out
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
    crossterm::terminal::size()
        .map(|(cols, rows)| (cols as usize, rows as usize))
        .ok()
        .or_else(|| {
            let cols = std::env::var("COLUMNS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok());
            let rows = std::env::var("LINES")
                .ok()
                .and_then(|v| v.parse::<usize>().ok());
            
            match (cols, rows) {
                (Some(c), Some(r)) => Some((c, r)),
                _ => None,
            }
        })
        .unwrap_or((80, 24))
}


fn sanitize_for_box(s: &str) -> String {
    // Keep printable text; avoid control chars that can break box alignment (e.g. \r, \t).
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\r' => {}
            '\t' => out.push_str("    "),
            '\x1b' => out.push(c), // keep ANSI escapes
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.trim_end().to_string()
}

fn split_ansi_chunks(s: &str, max_visible_width: usize) -> Vec<String> {
    if max_visible_width == 0 {
        return vec![String::new()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            // Copy the entire escape sequence without counting it toward visible width.
            current.push(c);
            match it.next() {
                Some('[') => {
                    current.push('[');
                    while let Some(ch) = it.next() {
                        current.push(ch);
                        if ('@'..='~').contains(&ch) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    current.push(']');
                    while let Some(ch) = it.next() {
                        current.push(ch);
                        if ch == '\x07' {
                            break;
                        }
                        if ch == '\x1b' {
                            if let Some('\\') = it.peek().copied() {
                                current.push(it.next().unwrap());
                                break;
                            }
                        }
                    }
                }
                Some(other) => {
                    current.push(other);
                }
                None => break,
            }
            continue;
        }

        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if current_width + w > max_visible_width && current_width > 0 {
            chunks.push(current);
            current = String::new();
            current_width = 0;
        }
        current.push(c);
        current_width += w;
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
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

fn looks_like_quant_query(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();

    for tok in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        match tok {
            "stock"
            | "stocks"
            | "ticker"
            | "tickers"
            | "shares"
            | "earnings"
            | "dividend"
            | "dividends"
            | "etf"
            | "etfs"
            | "sp500"
            | "nasdaq"
            | "dow" => return true,
            _ => {}
        }
    }

    if lower.contains("price history")
        || lower.contains("price action")
        || lower.contains("market data")
        || lower.contains("correlation")
        || lower.contains("correlate")
        || lower.contains("yahoo")
        || lower.contains("fred")
    {
        return true;
    }

    lower.contains("=f")
        || lower.contains("-usd")
        || prompt
            .split_whitespace()
            .any(|t| t.starts_with('^') && t.len() > 1)
}

fn looks_like_coding_intent(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();

    let strong = [
        "cargo", "rust", "compile", "build", "test", "clippy", "rustfmt", "fmt", "panic", "stack trace",
        "segfault", "backtrace", "crate", "module", "struct", "function", "impl ", "diff", "patch",
        "apply_patch", "subcommand", "cli", "tui", "cursor", "input box", "blue box", "ui", "terminal",
        "repo", "source code", "codebase",
    ];
    if strong.iter().any(|w| lower.contains(w)) {
        return true;
    }

    let file_exts = [
        ".rs", ".toml", ".json", ".yaml", ".yml", ".md", ".py", ".js", ".ts", ".tsx", ".jsx", ".go",
        ".java", ".c", ".h", ".cpp", ".hpp",
    ];
    for tok in prompt.split_whitespace() {
        let t = tok.trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '"' | '\''));
        let t_lower = t.to_ascii_lowercase();
        if (t.contains('/') || t.contains('\\')) && t.contains('.') {
            return true;
        }
        if file_exts.iter().any(|ext| t_lower.contains(ext)) {
            return true;
        }
    }

    false
}

fn looks_like_quant_follow_up(prompt: &str, state: &SessionState) -> bool {
    if !state.last_run_was_research {
        return false;
    }

    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        return false;
    }

    // If the user clearly switched back to coding, don't keep the research persona sticky.
    if looks_like_coding_intent(trimmed) {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();

    // Common "follow-up" / trading language.
    let follow = [
        "tradable", "trade", "play", "idea", "thesis", "strategy", "signal", "setup", "entry", "exit",
        "long", "short", "hedge", "risk", "stop", "lead", "lag", "predict", "forward", "regime",
        "correlation", "correlate", "pairs", "spread",
    ];
    if follow.iter().any(|w| lower.contains(w)) {
        return true;
    }

    // Very short replies that usually refer to the immediately preceding analysis.
    if lower.chars().count() <= 24 {
        return true;
    }

    let pronouns = [
        "that", "this", "it", "those", "earlier", "previous", "last", "same", "again", "still", "why",
        "how", "more", "explain", "continue",
    ];
    pronouns.iter().any(|w| lower.contains(w))
}

fn deny_reason_for_research_command(command: &str) -> Option<String> {
    let cmd = command.trim();
    if cmd.is_empty() {
        return Some("empty command".to_string());
    }

    let lower = cmd.to_ascii_lowercase();
    let first = lower.split_whitespace().next().unwrap_or("");

    let banned_bins = [
        "curl",
        "wget",
        "http",
        "https",
        "lynx",
        "links",
        "w3m",
        "open",
        "xdg-open",
    ];
    if banned_bins.contains(&first) {
        return Some(format!("'{first}' disabled in research mode (no web/news)"));
    }

    // Disallow URLs anywhere unless it's explicitly an Eli finance command (which should not embed URLs anyway).
    if (lower.contains("http://") || lower.contains("https://")) && !lower.starts_with("eli finance") {
        return Some("URLs disabled in research mode (no web/news)".to_string());
    }

    // Heuristic block: prevent easy network fetches from scripting runtimes.
    let runtime_bins = ["python", "python3", "node", "nodejs"];
    if runtime_bins.contains(&first)
        && (lower.contains("http://")
            || lower.contains("https://")
            || lower.contains("requests")
            || lower.contains("urllib")
            || lower.contains("fetch("))
    {
        return Some("network access via scripting runtime disabled in research mode".to_string());
    }

    None
}

async fn run_commands_with_policy(
    profile: AgentProfile,
    command_runner: &CommandRunner,
    commands: &[String],
    parallelism: usize,
) -> Vec<CommandResult> {
    if profile != AgentProfile::Research {
        return command_runner
            .run_commands_with_parallelism(commands, parallelism)
            .await;
    }

    let mut allowed_idx = Vec::new();
    let mut allowed_cmds = Vec::new();
    let mut indexed_results: Vec<(usize, CommandResult)> = Vec::new();

    for (idx, cmd) in commands.iter().enumerate() {
        if let Some(reason) = deny_reason_for_research_command(cmd) {
            indexed_results.push((
                idx,
                CommandResult {
                    command: cmd.clone(),
                    returncode: -1,
                    stdout: String::new(),
                    stderr: format!("Denied (research policy): {reason}"),
                    duration_ms: 0,
                    allowed: false,
                    deny_reason: Some(reason),
                },
            ));
            continue;
        }

        allowed_idx.push(idx);
        allowed_cmds.push(cmd.clone());
    }

    if !allowed_cmds.is_empty() {
        let results = command_runner
            .run_commands_with_parallelism(&allowed_cmds, parallelism)
            .await;
        for (i, r) in results.into_iter().enumerate() {
            let idx = allowed_idx.get(i).copied().unwrap_or(i);
            indexed_results.push((idx, r));
        }
    }

    indexed_results.sort_by_key(|(idx, _)| *idx);
    indexed_results.into_iter().map(|(_, r)| r).collect()
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
        println!("  {}◆{} diffs: {} ({})", PURPLE, RESET, parts.join(", "), status);
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
        if preview {
            if let Some(d) = &r.diff {
                println!("{}", colorize_diff(d));
            }
        }
    }
}

fn print_command_results(results: &[CommandResult], brief: bool) {
    use style::*;

    if results.is_empty() {
        return;
    }

    if brief {
        for r in results {
            let (icon, color) = if r.returncode == 0 { ("✓", GREEN) } else { ("✗", RED) };
            println!(
                "  {}{}{} {}${} {}{}",
                color, icon, RESET,
                GRAY, RESET,
                truncate_line(&r.command, 70),
                RESET
            );
            if r.returncode != 0 && !r.stderr.trim().is_empty() {
                println!(
                    "    {}err: {}{}",
                    RED,
                    truncate_line(&r.stderr.replace('\n', " "), 100),
                    RESET
                );
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
