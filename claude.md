# Eli - Data Tools for AI Agents

## What is Eli?

**Eli is a Rust CLI that gives AI agents native data access.**

Claude Code, Codex, Cursor - they're incredible agents. People should pay for them. But when they need market data, prediction markets, or structured web content, all they have is Google. Eli fixes that.

Eli is a single binary with native Rust tools that any AI agent can call:

```bash
# Stock prices, options chains, fundamentals, SEC filings
eli finance snapshot --ticker NVDA,AAPL,MSFT,GOOGL,AMZN,META,TSLA

# Full macro dashboard - 31 indicators in ~2 seconds (inflation, rates, debt-to-GDP, housing, etc.)
eli finance macro

# Options chains with IV, put/call ratios, max pain
eli finance options --ticker SPY --summary --near-money 5

# Scrape ALL of Kalshi + Polymarket in ~10 seconds → 22,500 markets
eli finance sync

# News without websearch - direct API, no LLM tokens burned
eli finance news --ticker NVDA --date 2026-02-05

# Web scraping, search, content extraction
eli web crawl <url>
eli web search "query"
eli web read <url>
```

**The agent is Claude Code. The data is Eli.**

---

## The Problem

AI agents today have one way to get information: websearch.

Websearch is:
- **Expensive** - burns LLM tokens to parse HTML
- **Slow** - multiple round-trips through a search engine
- **Unstructured** - you get HTML, not JSON
- **Limited** - no options chains, no FRED macro data, no prediction markets

When Claude Code needs the price of NVDA, it googles "NVDA stock price" and scrapes a webpage. When it needs unemployment data, it googles "US unemployment rate" and hopes for the best.

**Eli gives agents structured data access instead.**

---

## What Eli Provides

### Financial Data (Native Rust)
| Command | What It Does |
|---------|-------------|
| `eli finance snapshot --ticker NVDA,AAPL` | Price, market cap, shares, daily returns, relative strength |
| `eli finance timeseries --ticker AMD --range 30d` | OHLCV time series |
| `eli finance fundamentals --ticker INTC` | Income statement, balance sheet, cash flow |
| `eli finance filings --ticker TSLA` | Recent SEC filings (8-K, 10-K, 10-Q) |
| `eli finance options --ticker SPY --summary` | Options chain with IV, skew, put/call ratio, max pain |
| `eli finance search "tesla"` | Ticker symbol search |

### Macro Economics (FRED)
| Command | What It Does |
|---------|-------------|
| `eli finance macro` | 31 macro indicators: inflation, employment, GDP, rates, debt-to-GDP, Fed balance sheet, consumer, housing, credit, commodities, FX |

### Prediction Markets
| Command | What It Does |
|---------|-------------|
| `eli finance sync` | Scrape ALL of Kalshi (15,000) + Polymarket (7,500) → local CSV in ~10 seconds |
| `eli finance odds --search "recession"` | Search local CSV cache instantly (no API call) |
| `eli finance odds --search "recession" --live` | CSV discovery → fresh bid/ask/volume from live API |
| `eli finance odds --event KXNBERRECESSQ` | Fetch a single event directly from the exchange API |
| `eli finance odds --list-events` | Browse events (live API) |
| `eli finance odds --list-markets` | Browse markets (live API) |
| `eli finance odds where` | Print local cache paths for odds CSVs |

The odds tool scales from a single event lookup (no sync needed) to full 22,500-market sync. The CSV is an index of IDs/slugs/titles — use `--live` for fresh prices, or `--event` for direct API hits.

### Rates & Yield
| Command | What It Does |
|---------|-------------|
| `eli finance rate-path` | Implied Fed policy trajectory from prediction market cache (hold/cut/hike probabilities per meeting) |
| `eli finance yield-curve` | US Treasury yield curve (1mo–30y) with key spreads (2s10s, 10s2s, term premium) |

### Dashboard
| Command | What It Does |
|---------|-------------|
| `eli finance dashboard --preset recession` | Multi-tool preset: runs macro + yield-curve + rate-path + odds search in one call |

### News (Direct API, No Websearch)
| Command | What It Does |
|---------|-------------|
| `eli finance news --ticker NVDA --date 2026-02-05` | Headlines for a ticker on a date |

No LLM tokens burned. No websearch. Direct REST calls.

### Economic Calendar
| Command | What It Does |
|---------|-------------|
| `eli finance schedule --kind all --from 2026-02-17 --to 2026-02-21` | Earnings + macro release calendar (FRED) |

### Code Analysis (syn + quote)
| Command | What It Does |
|---------|-------------|
| `eli code <path>` | Parse Rust source → structural map (functions, structs, enums, impls, traits) |

### Multi-Agent Orchestration
| Command | What It Does |
|---------|-------------|
| `eli agent run --task "..."` | Single autonomous worker |
| `eli agent fanout --task-template "..." --vars vars.json` | Parallel workers from template |
| `eli agent swarm --task "..." --input file.md` | Map/reduce over large inputs |
| `eli agent critique/evidence/compete/debate` | Specialized analysis modes |

### Web Tools
| Command | What It Does |
|---------|-------------|
| `eli web crawl <url>` | Crawl a site, extract content from all pages |
| `eli web search "query"` | DuckDuckGo search |
| `eli web read <url>` | Read and extract content from a URL |
| `eli web extract` | Extract key facts from content |

### Spot Prices (Pyth Hermes)
| Command | What It Does |
|---------|-------------|
| `eli finance prices` | Latest crypto + commodity spot prices |

---

## Real Example: What Just Happened

A user asked Claude Code for "a full market snapshot." Claude Code ran these Eli commands in parallel:

```
eli finance sync                                    → 22,500 prediction markets cached
eli finance macro                                   → CPI, unemployment, Fed funds, oil, M2
eli finance snapshot --ticker SPY,QQQ,DIA,IWM,...   → Index ETFs
eli finance snapshot --ticker NVDA,AAPL,MSFT,...    → Mega-cap tech
eli finance options --ticker SPY --summary          → SPY options (IV, P/C ratio, max pain)
eli finance options --ticker NVDA --summary         → NVDA options
eli finance options --ticker AAPL --summary         → AAPL options
eli finance options --ticker GLD --summary          → Gold options
eli finance options --ticker SLV --summary          → Silver options
eli finance options --ticker TSLA --summary         → Tesla options
eli finance options --ticker MSFT --summary         → Microsoft options
eli finance options --ticker GOOGL --summary        → Alphabet options
eli finance news --ticker SPY --date 2026-02-05     → Market news
eli finance news --ticker NVDA --date 2026-02-05    → NVDA news
eli finance news --ticker AMZN --date 2026-02-05    → AMZN news
eli finance news --ticker MSFT --date 2026-02-05    → MSFT news
eli finance news --ticker GOOGL --date 2026-02-05   → GOOGL news
```

**Total time: ~20 seconds.** Claude Code synthesized everything into a full market report with macro indicators, equity prices, options flow, prediction market odds, and news context.

Without Eli, Claude Code would have to websearch each data point individually. That's 17+ Google searches, HTML parsing, and hoping the data is structured. With Eli, it's 17 parallel CLI calls returning clean JSON.

**Claude Code is the agent. Eli is the data.**

---

## Why Rust?

### Single Binary
```bash
cargo build --release
./target/release/eli
```
No Python. No pip install. No dependency hell. One binary, all tools included.

### Parallel Fetching
Fetch 10 tickers simultaneously. Sync 22,500 prediction markets across two exchanges. Rust's async runtime (tokio) handles it all without data races.

### Structured JSON Output
Every command returns clean JSON. AI agents parse JSON natively. No HTML scraping, no regex, no guessing.

### Fast
Native HTTP clients, no subprocess overhead, smart caching with TTL-based invalidation. Repeat queries are instant.

---

## How Agents Use Eli

### Claude Code
Claude Code sees `bin/eli` in the project and calls it via Bash. It reads the JSON output, reasons about it, and synthesizes reports. It can run multiple Eli commands in parallel.

### Codex / Any Terminal Agent
Any agent that can run shell commands can use Eli. The interface is just CLI → JSON.

### Eli's Own Chat Loop (Demo)
Eli also includes its own autonomous iteration loop (`eli chat`) that uses a free LLM (OpenRouter) to orchestrate the tools. This is a demo of what the tools can do - it re-prompts itself iteratively, fetches data, writes Python for calculations, and generates markdown reports. But the real value is the tools themselves, not the orchestration layer. Claude Code and Codex are better agents.

---

## Quick Start

### 1. One-time dependency fetch
```bash
cd eli
CARGO_HOME=$(pwd)/.cargo_local_local CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse cargo fetch
```

### 2. Build (use this after Rust changes)
This keeps a single build output in `eli/target_local` and refreshes the stable symlink at `bin/eli`.
```bash
CARGO_HOME=$(pwd)/.cargo_local_local \
CARGO_TARGET_DIR=$(pwd)/target_local \
CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse \
cargo build -p eli-cli --bin eli

ln -sf $(pwd)/target_local/debug/eli ../bin/eli
```

### 3. Use the tools
```bash
# From any agent, or from the command line:
bin/eli finance snapshot --ticker NVDA
bin/eli finance macro
bin/eli finance options --ticker SPY --summary
bin/eli finance sync
bin/eli finance news --ticker AAPL --date 2026-02-05
bin/eli web search "tariff impact on semiconductors"
```

### 4. Demo: Eli's own chat loop
```bash
bin/eli chat
> Analyze AMD vs INTC correlation over 30 days
# Eli autonomously fetches data, writes Python, calculates, generates report
```

---

## Tool Reference

### `eli finance snapshot`
Point-in-time snapshot: price, market cap, shares outstanding, daily returns, relative strength.
```bash
eli finance snapshot --ticker NVDA,AAPL,MSFT,GOOGL,AMZN,META,TSLA
eli finance snapshot --ticker INTC --returns 1mo,3mo,6mo,1y     # trailing return windows
eli finance snapshot --tickers-file watchlist.txt                # load tickers from file
eli finance snapshot --ticker SPY --out spy.json                 # write to file
```

### `eli finance timeseries`
OHLCV time series for one or more tickers. Auto-detects provider — works for both stock tickers (Yahoo) and FRED macro series (e.g., `GFDEGDQ188S` for debt-to-GDP) without needing `--provider`.
```bash
eli finance timeseries --ticker AMD --range 30d --granularity 1h
eli finance timeseries --ticker GFDEGDQ188S --range 5y          # FRED series, auto-detected
eli finance timeseries --ticker AAPL,UNRATE --range 1y           # mixed stock + FRED
eli finance timeseries --ticker SPY --start 2025-01-01 --end 2025-06-30  # explicit date window
eli finance timeseries --ticker NVDA --as-of 2025-12-31 --range 1y      # historical window ending at date
```

### `eli finance fundamentals`
Quarterly financials: income statement, balance sheet, cash flow.
```bash
eli finance fundamentals --ticker INTC
```

### `eli finance filings` / `eli finance sec`
Recent SEC filings (8-K, 10-K, 10-Q). Can download and inline document text.
```bash
eli finance filings --ticker TSLA                                    # default: 5 most recent 8-K/10-K/10-Q
eli finance filings --ticker AAPL --forms 10-K --limit 3             # only 10-Ks, last 3
eli finance filings --ticker NVDA --include-text                     # download docs + include text excerpt
eli finance filings --ticker INTC --include-text --max-chars 5000    # cap excerpt length
```

### `eli finance options`
Options chains with IV, skew, put/call ratios, max pain.
```bash
eli finance options --ticker SPY --summary --near-money 5            # summary metrics, strikes within 5%
eli finance options --ticker NVDA --expirations                      # list available expiry dates
eli finance options --ticker AAPL --expiry 2026-03-20                # full chain for specific expiry
eli finance options --ticker TSLA --expiry 2026-03-20 --type calls   # calls only
eli finance options --ticker SPY --expiry 2026-03-20 --type puts --near-money 10  # puts within 10%
```

### `eli finance macro`
Full macro dashboard — 31 indicators across 9 categories: inflation (CPI, Core CPI, Core PCE, PPI, breakeven), employment (unemployment, payrolls, claims, JOLTS), GDP & output, rates & yields (Fed funds, 2Y/10Y/30Y Treasury, TIPS, mortgage), debt & fiscal (debt-to-GDP, total federal debt), money supply & Fed balance sheet, consumer & housing (sentiment, retail sales, savings rate, Case-Shiller, housing starts, vehicle sales), credit spreads, commodities & FX (oil, dollar index). All fetched in parallel from FRED in ~2 seconds.
```bash
eli finance macro --range 1y
```

### `eli finance news`
Headlines for a ticker on a specific date. Direct API, no websearch.
```bash
eli finance news --ticker NVDA --date 2026-02-05
```

### `eli finance search`
Search for ticker symbols or macro series IDs.
```bash
eli finance search "tesla"
eli finance search "unemployment"
```

### `eli finance sync`
Bulk-sync Kalshi + Polymarket prediction markets to local CSV cache. Auto-emits `.meta.json` sidecar with sync stats.
```bash
eli finance sync                           # both sources, 10 pages each (~10s)
eli finance sync --max-pages 15            # deeper scrape
eli finance sync --sources kalshi          # Kalshi only
eli finance sync --sources polymarket      # Polymarket only
eli finance sync --strict                  # fail if pagination indicates incomplete coverage
eli finance sync --cache-dir /tmp/odds     # custom cache directory
```

### `eli finance odds`
Prediction market discovery + pricing. Scales from a single event lookup (no sync needed) to full CSV search.

**Three operating modes:**

1. **CSV search** (instant, ~170ms) — searches local CSV cache from `sync`. No API call.
```bash
eli finance odds --search "recession"
eli finance odds --search "federal reserve" --min-volume 1000 --top 10
```

2. **Live search** (`--live`, ~2s) — CSV discovers matching events, then fetches fresh bid/ask from exchange APIs.
```bash
eli finance odds --search "recession" --live
eli finance odds --search "tariff" --live --top 5
```

3. **No CSV fallback** (automatic, ~4s) — if no CSV cache exists, searches live APIs directly (Kalshi events + Polymarket search).
```bash
# Just works — no sync needed
eli finance odds --search "recession"
```

**Direct event/market lookup** (no sync or CSV needed):
```bash
eli finance odds --event KXNBERRECESSQ                     # Kalshi event by ticker
eli finance odds --event KXNBERRECESSQ --provider kalshi   # explicit provider
eli finance odds --event 48802 --provider polymarket       # Polymarket event by numeric ID
eli finance odds --market KXNBERRECESSQ-Q1-2026            # specific Kalshi market
eli finance odds --series KXFEDDECISION                    # all events in a Kalshi series
```

**Browse and discover** (live API):
```bash
eli finance odds --list-events                             # browse events
eli finance odds --list-events --search "fed" --limit 20   # filtered event browse
eli finance odds --list-markets --event KXFEDDECISION-26MAR19  # markets in an event
eli finance odds --list-series                             # Kalshi series (8400+)
eli finance odds --list-tags --provider polymarket          # Polymarket tags
eli finance odds --list-events --category "Economics"       # Kalshi category filter
```

**Orderbook depth** (heavier call):
```bash
eli finance odds --market KXNBERRECESSQ-Q1-2026 --orderbook --depth 5
```

**Cache info:**
```bash
eli finance odds where                                     # print CSV cache paths
```

### `eli finance prices`
Latest spot prices from Pyth Hermes (crypto, commodities, FX, rates).
```bash
eli finance prices                                       # default set (BTC, ETH, SOL, gold, silver, oil, etc.)
eli finance prices --query "pepe"                        # discover price feeds by name
eli finance prices --asset-type crypto                   # filter by asset type (crypto, equity, fx, metal, rates)
eli finance prices --ids <feed_id1>,<feed_id2>           # explicit Pyth feed IDs
eli finance prices --query "bitcoin" --auto-select       # auto-pick top match when ambiguous
```

### `eli web crawl`
Crawl a website and extract content from all discovered pages.
```bash
eli web crawl --url https://example.com                              # default: up to 50 pages
eli web crawl --url https://example.com --max-pages 10               # limit pages
eli web crawl --url https://example.com --smart                      # HTTP first, JS render only when needed
eli web crawl --url https://example.com --sitemap                    # discover via sitemap.xml
eli web crawl --url https://example.com --subdomains                 # include subdomains
eli web crawl --url https://example.com --view raw                   # raw output (summary | raw | path)
```

### `eli web search`
Search the web via DuckDuckGo.
```bash
eli web search "Fed rate decision February 2026"
```

### `eli web read`
Read and extract content from a single URL.
```bash
eli web read https://example.com/article
```

### `eli web extract`
Extract key facts from content (URL, file, or text).
```bash
eli web extract --url https://example.com/article                    # extract from URL
eli web extract --file report.md                                     # extract from local file
eli web extract --text "long content here..."                        # extract from inline text
eli web extract --url https://example.com --bullets 5                # number of bullet points (default: 10)
eli web extract --url https://example.com --focus "earnings"         # focus extraction on a topic
```

### `eli finance schedule`
Earnings calendar (Nasdaq) + macro release calendar (FRED). See what data drops are coming.
```bash
eli finance schedule --kind all --from 2026-02-17 --to 2026-02-21
eli finance schedule --kind earnings --date 2026-02-14
eli finance schedule --kind macro --from 2026-02-14 --to 2026-02-28
eli finance schedule --kind macro --from 2026-02-14 --to 2026-02-28 --major  # major releases only (CPI, PCE, GDP, jobs, FOMC, claims)
eli finance schedule --kind earnings --date 2026-02-14 --ticker NVDA,AMD     # filter to specific tickers
```

### `eli finance rate-path`
Aggregate implied Fed policy trajectory from local prediction-market cache. Shows hold/cut/hike probabilities per FOMC meeting.
```bash
eli finance rate-path                                # auto-detect source (meeting markets or fallback)
eli finance rate-path --source-mode meeting          # force meeting-date markets only
eli finance rate-path --source-mode fallback         # force fallback estimation
```
Requires prediction market CSV cache (from `eli finance sync`).

### `eli finance yield-curve`
Fetch the live US Treasury yield curve (1mo through 30y) with key spread calculations.
```bash
eli finance yield-curve                              # current curve
eli finance yield-curve --compare 3mo,1y             # compare to 3 months ago and 1 year ago
eli finance yield-curve --strict                     # fail if any tenor is missing
```

### `eli finance dashboard`
Run a preset multi-tool macro dashboard that combines several eli commands into one call.
```bash
eli finance dashboard --preset recession             # macro + yield curve + rate path + odds search
eli finance dashboard --preset recession --max-ms 30000  # with timeout budget
```

### `eli code`
Parse Rust source into a structural map using `syn`. Returns function/struct/enum/impl/trait counts and names. Useful for understanding large Rust files without reading them into context.
```bash
eli code <path>                # structural summary (functions, structs, enums, impls, traits)
eli code <path> --generate     # also generate getter methods for structs via quote!
eli code <path> --out out.json # write to file
```

### `eli agent run`
Run a single autonomous Eli worker from a natural-language task. Uses the configured LLM with fallback models.
```bash
eli agent run --task "Analyze AMD vs INTC correlation" --max-ms 45000
```

### `eli agent fanout`
Run many workers in parallel from a task template with variable substitution. Workers can share a manifest file.
```bash
eli agent fanout --task-template "Analyze {{ticker}} outlook" --vars vars.json --max-parallel 4
```
The `--vars` file is a JSON array of objects, e.g. `[{"ticker":"NVDA"},{"ticker":"AMD"}]`.

Convenience modes that wrap fanout with specialized system prompts:
```bash
eli agent critique --prompt "Is recession coming?" --lead report.md --vars vars.json
eli agent evidence --prompt "Bull case for NVDA" --vars vars.json
eli agent compete --prompt "Best semiconductor stock" --vars vars.json
eli agent debate --prompt "US fiscal dominance" --vars vars.json
```

### `eli agent swarm`
Map/reduce over large inputs. Chunks a file, runs parallel map workers, then reduces + synthesizes.
```bash
eli agent swarm --task "Extract key claims" --input large_doc.md --chunks 5 --max-parallel 3
```

---

## Tool Usage Tips for Agents

### Best practices
- **Use Eli for structured data, WebSearch for narrative context.** They complement each other.
- **Run multiple Eli commands in parallel** — they're independent and fast.
- **Pick the right weight class for prediction markets:**
  - Quick single-event lookup → `eli finance odds --event <ticker>` (no sync needed, ~1s)
  - Search with fresh prices → `eli finance odds --search "query" --live` (~2s)
  - Broad discovery → `eli finance odds --search "query"` on CSV cache (~170ms, requires prior sync)
  - Full scrape → `eli finance sync` (~10s, caches 22,500 markets)
- **The CSV is an index, not the data.** Stale CSV is fine for discovering event IDs/slugs/titles. Use `--live` or `--event` for fresh prices.
- **No sync? No problem.** `--search` auto-falls back to live API when no CSV exists.
- **Weekend/holiday: snapshot daily_returns will be 0.0** — the `market_note` field warns about this.
- **ETF fundamentals return empty** — the `note` field explains why and suggests `eli finance snapshot` instead.
- **News tool adds "stock" to queries** — but Google RSS can still be noisy for obscure tickers.

### Common mistakes
- Don't `sync` when you just need one event — use `eli finance odds --event <ticker>` for direct API lookup.
- Don't call `eli finance fundamentals` on ETFs (SPY, QQQ, TLT) — use `snapshot` instead.
- News for short tickers (AI, SPY) can return unrelated results — check relevance.
- `eli finance macro` may return empty on weekends/holidays — FRED API limitation.
- `eli finance odds --search` uses word-boundary matching — specific terms work well.
- Don't assume Polymarket event IDs are slugs — they're numeric (e.g., `48802`). Use `--provider polymarket` when querying Polymarket events directly.

---

## Configuration

**File:** `~/.config/eli/config.toml`

```toml
[chat]
# Only needed for eli chat demo loop
model = "arcee-ai/trinity-large-preview:free"
provider = "openrouter"
openrouter_api_key = "sk-or-v1-..."

[finance]
cache_dir = "~/.eli/cache/finance"
cache_ttl_hours = 24
yahoo_timeout_secs = 30
fred_api_key = "optional"   # For higher FRED rate limits
news_api_key = "optional"   # For news fetching
```

---

## Research Artifacts

When used (by any agent), analysis outputs go to `eli_research/`:

```
eli_research/
├── amd_intc_correlation_20260113.md
├── semiconductor_sector_20260112.md
├── market_snapshot_20260205.md
└── data/
    └── (CSVs, charts, raw data)
```

---

## Design Philosophy

### 1. Tools, Not Agent
Eli is a data toolkit. The agent is Claude Code, Codex, Cursor, or whatever you prefer. Don't rebuild what already exists.

### 2. Data Access, Not Websearch
Structured JSON from native APIs. Not "google it and parse HTML."

### 3. Native Rust, Not Python Wrappers
Yahoo Finance, FRED, Kalshi, Polymarket - all native Rust HTTP clients. Fast, type-safe, single binary.

### 4. Parallel by Default
Fetch 10 tickers simultaneously. Sync 22,500 prediction markets in 10 seconds. Agents can call multiple Eli commands in parallel.

### 5. Cache Everything
SHA256-hashed query params, TTL-based invalidation, local disk cache. Repeat queries are instant.

### 6. Direct APIs, Not LLM Websearch
News from direct REST APIs. Prediction markets from exchange APIs. No LLM tokens burned for data fetching.

---

## Limitations & Roadmap

### Current
- Yahoo Finance + FRED + Google News RSS + Kalshi + Polymarket + DuckDuckGo
- Basic options chain data (no Greeks calculation yet)
- No visualization (agents can write Python for charts)
- News via Google RSS can be noisy for some ticker symbols

### Planned
- Bloomberg / Alpha Vantage / SEC Edgar integration
- Options Greeks (Black-Scholes, binomial)
- Technical indicators (RSI, MACD, Bollinger)
- Real-time WebSocket feeds
- Portfolio analytics (VaR, Sharpe, drawdown)
- Better news sources (dedicated financial news APIs)

---

## Summary

**Eli gives AI agents native data access.**

Instead of websearch, agents get:
- Stock prices, fundamentals, SEC filings (Yahoo Finance)
- Macro economics (FRED)
- Options chains with IV and flow metrics (Yahoo Finance)
- 22,500 prediction markets synced in 10 seconds (Kalshi + Polymarket)
- News headlines without websearch (direct API)
- Web scraping and search (DuckDuckGo, crawl, read)

All as a single Rust binary returning structured JSON.

**The agent is Claude Code. The data is Eli.**
