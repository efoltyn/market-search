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

    /// Start MCP (Model Context Protocol) server — exposes eli tools as native Claude Code tools via JSON-RPC stdio.
    Mcp,
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
    /// Fetch broad FX basket performance with USD-relative deltas and biggest move dates.
    Forex(FinanceForexArgs),
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
    /// Local paper trading sandbox using live Kalshi/Polymarket prices.
    Paper(FinancePaperArgs),
}

#[derive(Subcommand, Debug)]
enum WebCommand {
    /// Crawl a website and extract content from all discovered pages.
    Crawl(WebCrawlArgs),
    /// Ingestion-focused web search for URL candidates + diagnostics.
    Search(WebSearchArgs),
    /// Read and extract content from one or many URLs.
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
    /// Full signatures: "pub async fn name(param: Type) -> ReturnType"
    /// Lets an AI caller understand the API contract without reading source.
    function_signatures: Vec<String>,
    structs: usize,
    struct_names: Vec<String>,
    /// Per-struct field list: {struct_name: ["field: Type", ...]}
    struct_fields: std::collections::BTreeMap<String, Vec<String>>,
    enums: usize,
    enum_names: Vec<String>,
    impls: usize,
    impl_targets: Vec<String>,
    /// Methods per impl block: {"LlmAdapter for AnthropicAdapter": ["pub async fn chat(...)  -> ..."]}
    impl_methods: std::collections::BTreeMap<String, Vec<String>>,
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

#[derive(Copy, Clone, Debug, ValueEnum, Eq, PartialEq)]
enum WebSearchModeArg {
    Auto,
    News,
    Finance,
    Research,
    Tech,
    Encyclopedia,
}

#[derive(Copy, Clone, Debug, ValueEnum, Eq, PartialEq)]
enum WebSearchRecencyArg {
    Day,
    Week,
    Month,
    Year,
}

#[derive(Copy, Clone, Debug, ValueEnum, Eq, PartialEq)]
enum FinancePaperCommandArg {
    Trade,
    Positions,
    Trades,
    Mark,
    Reset,
}

#[derive(Copy, Clone, Debug, ValueEnum, Eq, PartialEq)]
enum FinancePaperModeArg {
    Simulated,
    #[value(alias = "live_like")]
    LiveLike,
    #[value(alias = "kalshi_demo")]
    KalshiDemo,
    #[value(alias = "polymarket_demo")]
    PolymarketDemo,
}

#[derive(Copy, Clone, Debug, ValueEnum, Eq, PartialEq)]
enum FinancePaperSideArg {
    Yes,
    No,
}

#[derive(Copy, Clone, Debug, ValueEnum, Eq, PartialEq)]
enum FinancePaperOrderActionArg {
    Buy,
    Sell,
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

    /// Require the final report to cite these path prefixes (comma-separated).
    #[arg(long = "must-cite", value_delimiter = ',')]
    must_cite: Vec<String>,
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

    /// Require each successful worker report to cite these path prefixes (comma-separated).
    #[arg(long = "must-cite", value_delimiter = ',')]
    must_cite: Vec<String>,
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
    #[arg(long = "max-attempts", default_value_t = 4)]
    max_attempts: usize,

    /// Require successful stage reports to cite these path prefixes (comma-separated).
    #[arg(long = "must-cite", value_delimiter = ',')]
    must_cite: Vec<String>,
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
    #[arg(long = "max-attempts", default_value_t = 2)]
    max_attempts: usize,

    /// Require each successful worker report to cite these path prefixes (comma-separated).
    #[arg(long = "must-cite", value_delimiter = ',')]
    must_cite: Vec<String>,
}

#[derive(clap::Args, Debug)]
struct CodeArgs {
    /// Path to Rust source file or directory to analyze.
    path: PathBuf,

    /// Also generate code (e.g., getter methods for structs).
    #[arg(long, default_value_t = false)]
    generate: bool,

    /// Minimum line count filter (directory mode only).
    #[arg(long, default_value_t = 0)]
    min_loc: usize,

    /// Maximum number of files to analyze after sorting by path (directory mode only).
    #[arg(long)]
    max_files: Option<usize>,

    /// Parallel worker count for Rust parsing (directory mode only).
    #[arg(long)]
    workers: Option<usize>,

    /// Number of rows to include for each hotspot ranking (directory mode only).
    #[arg(long, default_value_t = 20)]
    top: usize,

    /// Include per-file metrics in response (directory mode only).
    #[arg(long, default_value_t = false)]
    include_files: bool,

    /// Search for symbol usages across all .rs files in path (comma-separated).
    /// Uses multi-pattern matching. Returns every line containing any of the symbols
    /// with file path and line number. Works on files and directories.
    #[arg(long, value_delimiter = ',')]
    find: Vec<String>,

    /// Emit the complete public API surface for a directory: every pub fn (with full
    /// signature), pub struct (with field types), pub enum (with variants), pub trait
    /// (with method signatures), grouped by file. Ideal for understanding a module's
    /// contract before writing new code.
    #[arg(long, default_value_t = false)]
    pub_api: bool,

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

    /// Search mode tuned for different ingestion workflows.
    #[arg(long, value_enum, default_value_t = WebSearchModeArg::Auto)]
    mode: WebSearchModeArg,

    /// Include only these domains (comma-separated).
    #[arg(long, value_delimiter = ',')]
    domains: Vec<String>,

    /// Exclude these domains (comma-separated).
    #[arg(long = "exclude-domains", value_delimiter = ',')]
    exclude_domains: Vec<String>,

    /// Recency hint (day, week, month, year).
    #[arg(long, value_enum)]
    recency: Option<WebSearchRecencyArg>,

    /// Earliest publication date (YYYY-MM-DD).
    #[arg(long)]
    since: Option<String>,

    /// Latest publication date (YYYY-MM-DD).
    #[arg(long)]
    until: Option<String>,

    /// Maximum items to return.
    #[arg(long, default_value_t = 15)]
    top: usize,

    /// Number of top results to probe with web read diagnostics.
    #[arg(long = "probe-top", default_value_t = 4)]
    probe_top: usize,

    /// Maximum parallel network operations.
    #[arg(long = "max-parallel", default_value_t = 6)]
    max_parallel: usize,

    /// Optional run-tracking key for delta comparisons.
    #[arg(long = "track-key")]
    track_key: Option<String>,

    /// Emit full verbose payload (snippets + detailed score components).
    #[arg(long, default_value_t = false)]
    full: bool,

    /// Output file path (JSON).
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct WebReadArgs {
    /// URL(s) to read content from (repeatable or comma-separated).
    #[arg(long = "url", value_delimiter = ',')]
    url: Vec<String>,

    /// Optional file containing URLs (one per line, '#' comments allowed).
    #[arg(long = "urls-file")]
    urls_file: Option<PathBuf>,

    /// Maximum parallel URL fetches.
    #[arg(long = "max-parallel", default_value_t = 6)]
    max_parallel: usize,

    /// Max chars to keep per article text in default compact mode.
    #[arg(long = "max-chars", default_value_t = 2400)]
    max_chars: usize,

    /// Emit full verbose payload (full text + attempt details).
    #[arg(long, default_value_t = false)]
    full: bool,

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
pub struct FinanceForexArgs {
    /// Time range for FX performance (e.g. 1y, 6mo).
    #[arg(long, default_value = "1y")]
    pub range: String,
    /// Candle granularity (e.g. 1d, 4h).
    #[arg(long, default_value = "1d")]
    pub granularity: String,
    /// Optional explicit Yahoo FX tickers (repeatable or comma-separated), e.g. EURUSD=X.
    #[arg(long = "pairs", value_delimiter = ',')]
    pub pairs: Vec<String>,
    /// Optional currency filter (comma-separated), e.g. CAD,JPY,EUR.
    #[arg(long = "currencies", value_delimiter = ',')]
    pub currencies: Vec<String>,
    /// Optional country filter (comma-separated), e.g. US,CA,JP,GB,EU.
    #[arg(long = "countries", value_delimiter = ',')]
    pub countries: Vec<String>,
    /// Optional preset groups (comma-separated): majors,g10,em,europe,americas,asia,commodity.
    #[arg(long = "groups", value_delimiter = ',')]
    pub groups: Vec<String>,
    /// Include selected EM FX pairs in the default basket.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub include_em: bool,
    /// Optional as-of date/time (YYYY-MM-DD or RFC3339).
    #[arg(long = "as-of")]
    pub as_of: Option<String>,
    /// Optional event timestamp for pre/post analysis (YYYY-MM-DD or RFC3339).
    #[arg(long = "event-at")]
    pub event_at: Option<String>,
    /// Optional pre/post window around --event-at (e.g. 6h,12h,1d,3d).
    #[arg(long = "event-window")]
    pub event_window: Option<String>,
    /// Optional historical comparison anchors (comma-separated YYYY-MM-DD or RFC3339).
    #[arg(long = "compare-as-of", value_delimiter = ',')]
    pub compare_as_of: Vec<String>,
    /// Optional horizon windows for USD deltas (comma-separated), e.g. 1d,1w,1mo,3mo,1y.
    #[arg(long = "horizons", value_delimiter = ',')]
    pub horizons: Vec<String>,
    /// Optional max number of resolved pairs after filtering.
    #[arg(long = "max-pairs")]
    pub max_pairs: Option<usize>,
    /// Include the latest N close points per pair (compact timeseries context).
    #[arg(long = "recent-points", default_value_t = 0)]
    pub recent_points: usize,
    /// Number of largest daily USD-impact moves to include.
    #[arg(long, default_value_t = 12)]
    pub top: usize,
    /// Optional cache directory for timeseries fetches.
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
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
    /// Macro filtering profile: broad | market | major.
    #[arg(long = "macro-profile", default_value = "market")]
    pub macro_profile: String,
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
    /// Tickers to fetch (repeatable or comma-separated).
    #[arg(long, visible_alias = "ticker", value_delimiter = ',')]
    tickers: Vec<String>,

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

    /// Category filter (Kalshi list endpoints, and local CSV search when --search is used).
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

    /// Return top N markets after ranking (local CSV search).
    #[arg(long)]
    top: Option<usize>,

    /// Ranking for local CSV search output.
    /// Options: relevance, volume, delta_prob, delta_yes_price, delta_volume.
    #[arg(long = "sort-by", default_value = "relevance")]
    sort_by: String,
    /// Query profile: auto | macro | broad.
    #[arg(long, default_value = "auto")]
    profile: String,

    /// Only include markets that changed since the last sync (requires sync delta index).
    #[arg(long, default_value_t = false)]
    deltas_only: bool,

    /// Minimum absolute probability move (percentage points) since last sync.
    /// Requires sync delta index.
    #[arg(long = "min-delta-pp")]
    min_delta_pp: Option<f64>,

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

    /// Optional page cap per source. Omit for full exhaustion.
    #[arg(long)]
    max_pages: Option<usize>,

    /// Fail if pagination/coverage checks indicate incomplete source exhaustion.
    #[arg(long)]
    strict: bool,

    /// Include sports markets/events in sync output (default: false).
    #[arg(long)]
    include_sports: bool,

    /// Include Kalshi historical markets (archived/settled tier). Default: false.
    #[arg(long)]
    include_historical: bool,

    /// Fast refresh from Kalshi websocket ticker stream using cached baseline (no full re-pagination).
    #[arg(long)]
    stream_refresh: bool,

    /// Breadth heartbeat in hours for stream refresh mode. If cached baseline is older, force strict REST anchor sync (default: 6).
    #[arg(long)]
    refresh_heartbeat_hours: Option<u64>,

    /// WebSocket listen window in seconds for stream refresh mode (default: 300).
    #[arg(long)]
    stream_refresh_timeout_secs: Option<u64>,

    /// Cache directory for CSV files.
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Output format (currently: json).
    #[arg(long, default_value = "json")]
    format: String,

    /// Emit full verbose payload on stdout (default is compact/token-efficient).
    #[arg(long)]
    full: bool,

    /// Write full JSON output to a file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct FinancePaperArgs {
    /// Command: trade | positions | trades | mark | reset
    #[arg(long, value_enum, default_value = "trade")]
    command: FinancePaperCommandArg,

    /// Execution mode (v1: simulated local fills)
    #[arg(long, value_enum, default_value = "simulated")]
    mode: FinancePaperModeArg,

    /// Paper account name.
    #[arg(long, default_value = "default")]
    account: String,

    /// Provider for trade/mark pricing (kalshi|polymarket).
    #[arg(long)]
    provider: Option<String>,

    /// Market ticker or market id (required for --command trade).
    #[arg(long)]
    market: Option<String>,

    /// Side (yes|no) for --command trade.
    #[arg(long, value_enum)]
    side: Option<FinancePaperSideArg>,

    /// Order action (buy|sell) for --command trade.
    #[arg(long, value_enum)]
    action: Option<FinancePaperOrderActionArg>,

    /// Quantity/contracts for --command trade.
    #[arg(long)]
    qty: Option<f64>,

    /// Optional manual fill price in probability units [0,1]. If omitted, uses live midpoint.
    #[arg(long)]
    price: Option<f64>,

    /// Starting paper cash for account init/reset (default: 10000).
    #[arg(long)]
    starting_cash: Option<f64>,

    /// Trade history limit for --command trades (default: 50).
    #[arg(long)]
    limit: Option<usize>,

    /// Optional custom cache dir or full state file path.
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Output format (json only).
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

    /// Optional prediction market provider to pair with timeseries (kalshi | polymarket).
    #[arg(long)]
    odds_provider: Option<String>,

    /// Optional prediction market identifier. Kalshi: market ticker. Polymarket: market ID or slug.
    #[arg(long)]
    odds_market: Option<String>,

    /// Prediction market side to pair (yes | no). Defaults to yes.
    #[arg(long, default_value = "yes")]
    odds_side: String,

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
