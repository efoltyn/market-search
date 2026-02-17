# Eli — Data Tools for AI Agents

**Eli is a Rust CLI that gives AI agents native data access.** Instead of websearch, you get structured JSON from native APIs. One binary, all tools included.

The binary is at `bin/eli`. Call it via shell. Every command returns JSON to stdout.

---

## Complete Tool Reference

### Financial Data

#### `eli finance snapshot`
Point-in-time snapshot: price, market cap, shares outstanding, daily returns, relative strength.
```bash
eli finance snapshot --ticker NVDA,AAPL,MSFT,GOOGL,AMZN,META,TSLA
eli finance snapshot --ticker INTC --returns 1mo,3mo,6mo,1y     # trailing return windows
eli finance snapshot --tickers-file watchlist.txt                # load tickers from file
eli finance snapshot --ticker SPY --out spy.json                 # write to file
```

#### `eli finance timeseries`
OHLCV time series. Auto-detects provider — works for both stock tickers (Yahoo) and FRED macro series without needing `--provider`.
```bash
eli finance timeseries --ticker AMD --range 30d --granularity 1h
eli finance timeseries --ticker GFDEGDQ188S --range 5y          # FRED series (debt-to-GDP), auto-detected
eli finance timeseries --ticker AAPL,UNRATE --range 1y           # mixed stock + FRED in one call
eli finance timeseries --ticker SPY --start 2025-01-01 --end 2025-06-30  # explicit date window
eli finance timeseries --ticker NVDA --as-of 2025-12-31 --range 1y      # historical window ending at date
```

#### `eli finance fundamentals`
Quarterly financials: income statement, balance sheet, cash flow (Yahoo Finance).
```bash
eli finance fundamentals --ticker INTC
```
**Note:** ETFs (SPY, QQQ, TLT) return empty — use `snapshot` instead.

#### `eli finance filings` / `eli finance sec`
Recent SEC filings (8-K, 10-K, 10-Q). Can download and inline document text.
```bash
eli finance filings --ticker TSLA                                    # default: 5 most recent 8-K/10-K/10-Q
eli finance filings --ticker AAPL --forms 10-K --limit 3             # only 10-Ks, last 3
eli finance filings --ticker NVDA --include-text                     # download docs + include text excerpt
eli finance filings --ticker INTC --include-text --max-chars 5000    # cap excerpt length
```

#### `eli finance options`
Options chains with IV, skew, put/call ratios, max pain.
```bash
eli finance options --ticker SPY --summary --near-money 5            # summary metrics, strikes within 5%
eli finance options --ticker NVDA --expirations                      # list available expiry dates
eli finance options --ticker AAPL --expiry 2026-03-20                # full chain for specific expiry
eli finance options --ticker TSLA --expiry 2026-03-20 --type calls   # calls only
eli finance options --ticker SPY --expiry 2026-03-20 --type puts --near-money 10  # puts within 10%
```

#### `eli finance search`
Search for ticker symbols or macro series IDs.
```bash
eli finance search --query "tesla"
eli finance search --query "unemployment"
```

#### `eli finance prices`
Latest spot prices from Pyth Hermes (crypto, commodities, FX, rates).
```bash
eli finance prices                                       # default set (BTC, ETH, SOL, gold, silver, oil, etc.)
eli finance prices --query "pepe"                        # discover price feeds by name
eli finance prices --asset-type crypto                   # filter by asset type (crypto, equity, fx, metal, rates)
eli finance prices --ids <feed_id1>,<feed_id2>           # explicit Pyth feed IDs
eli finance prices --query "bitcoin" --auto-select       # auto-pick top match when ambiguous
```

---

### Macro Economics (FRED)

#### `eli finance macro`
Full macro dashboard — **31 indicators across 9 categories**, all fetched in parallel from FRED in ~2 seconds:

| Category | Indicators |
|----------|-----------|
| Inflation | CPI, Core CPI, Core PCE (Fed preferred), PPI, 10Y Breakeven Inflation |
| Employment | Unemployment Rate, Non-farm Payrolls, Initial Jobless Claims, Job Openings (JOLTS) |
| GDP & Output | Real GDP, Industrial Production |
| Rates & Yields | Fed Funds, 2Y/10Y/30Y Treasury, 10Y-2Y Spread, 10Y TIPS Real Yield, 30Y Mortgage |
| Debt & Fiscal | **Federal Debt to GDP**, Federal Debt Total |
| Money & Fed | M2 Money Supply, Fed Balance Sheet Total Assets |
| Consumer & Housing | Consumer Sentiment (UMich), Retail Sales, Personal Savings Rate, Case-Shiller Home Prices, Housing Starts, Total Vehicle Sales |
| Credit & Risk | High Yield Credit Spread |
| Commodities & FX | WTI Oil Price, Trade-Weighted Dollar Index |

```bash
eli finance macro
eli finance macro --range 1y
eli finance macro --compare-to 2025-01-01            # compare current to a historical date
```

Each indicator includes `current_value` and `change_1y` (percent change year-over-year).

**Why this matters:** AI models hallucinate macro data. Training data is stale — models commonly cite US debt-to-GDP as ~98-101% when the real number is 121%. Always use `eli finance macro` for macro data instead of relying on model knowledge.

---

### Prediction Markets

#### `eli finance sync`
Bulk-sync Kalshi + Polymarket prediction markets to local CSV cache. Auto-emits `.meta.json` sidecar with sync stats.
```bash
eli finance sync                           # both sources, 10 pages each (~10s)
eli finance sync --max-pages 15            # deeper scrape
eli finance sync --sources kalshi          # Kalshi only
eli finance sync --sources polymarket      # Polymarket only
eli finance sync --strict                  # fail if pagination indicates incomplete coverage
eli finance sync --cache-dir /tmp/odds     # custom cache directory
```

#### `eli finance odds`
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

---

### Rates & Yield

#### `eli finance rate-path`
Aggregate implied Fed policy trajectory from local prediction-market cache. Shows hold/cut/hike probabilities per FOMC meeting.
```bash
eli finance rate-path                                # auto-detect source (meeting markets or fallback)
eli finance rate-path --source-mode meeting          # force meeting-date markets only
eli finance rate-path --source-mode fallback         # force fallback estimation
```
Requires prediction market CSV cache (from `eli finance sync`).

#### `eli finance yield-curve`
Fetch the live US Treasury yield curve (1mo through 30y) with key spread calculations.
```bash
eli finance yield-curve                              # current curve
eli finance yield-curve --compare 3mo,1y             # compare to 3 months ago and 1 year ago
eli finance yield-curve --strict                     # fail if any tenor is missing
```

---

### Dashboard

#### `eli finance dashboard`
Run a preset multi-tool macro dashboard that combines several eli commands into one call.
```bash
eli finance dashboard --preset recession             # macro + yield curve + rate path + odds search
eli finance dashboard --preset recession --max-ms 30000  # with timeout budget
```

---

### News

#### `eli finance news`
Headlines for a ticker on a specific date. Direct Google News RSS API, no websearch, no LLM tokens burned.
```bash
eli finance news --ticker NVDA --date 2026-02-05
```
**Note:** Short tickers that are common English words (SPY, AI, TLT) can return noisy results.

---

### Economic Calendar

#### `eli finance schedule`
Earnings calendar (Nasdaq) + macro release calendar (FRED). See what data drops are coming.
```bash
eli finance schedule --kind all --from 2026-02-17 --to 2026-02-21
eli finance schedule --kind earnings --date 2026-02-14
eli finance schedule --kind macro --from 2026-02-14 --to 2026-02-28
eli finance schedule --kind macro --from 2026-02-14 --to 2026-02-28 --major  # major releases only (CPI, PCE, GDP, jobs, FOMC, claims)
eli finance schedule --kind earnings --date 2026-02-14 --ticker NVDA,AMD     # filter to specific tickers
```

---

### Web Tools

#### `eli web crawl`
Crawl a website and extract content from all discovered pages.
```bash
eli web crawl --url https://example.com                              # default: up to 50 pages
eli web crawl --url https://example.com --max-pages 10               # limit pages
eli web crawl --url https://example.com --smart                      # HTTP first, JS render only when needed
eli web crawl --url https://example.com --sitemap                    # discover via sitemap.xml
eli web crawl --url https://example.com --subdomains                 # include subdomains
eli web crawl --url https://example.com --view raw                   # raw output (summary | raw | path)
```

#### `eli web search`
Search the web via DuckDuckGo.
```bash
eli web search "Fed rate decision February 2026"
```

#### `eli web read`
Read and extract content from a single URL.
```bash
eli web read https://example.com/article
```

#### `eli web extract`
Extract key facts from content (URL, file, or text).
```bash
eli web extract --url https://example.com/article                    # extract from URL
eli web extract --file report.md                                     # extract from local file
eli web extract --text "long content here..."                        # extract from inline text
eli web extract --url https://example.com --bullets 5                # number of bullet points (default: 10)
eli web extract --url https://example.com --focus "earnings"         # focus extraction on a topic
```

---

### Code Analysis (syn + quote)

#### `eli code`
Parse Rust source into a structural map using `syn`. Returns function/struct/enum/impl/trait counts and names. Useful for understanding large Rust files without reading them.
```bash
eli code <path>                # structural summary
eli code <path> --generate     # also generate getter methods for structs
eli code <path> --out out.json # write to file
```

---

### Multi-Agent Orchestration

#### `eli agent run`
Run a single autonomous Eli worker from a natural-language task.
```bash
eli agent run --task "Analyze AMD vs INTC correlation" --max-ms 45000
```

#### `eli agent fanout`
Run many workers in parallel from a task template with variable substitution.
```bash
eli agent fanout --task-template "Analyze {{ticker}} outlook" --vars vars.json --max-parallel 4
```
The `--vars` file is a JSON array of objects, e.g. `[{"ticker":"NVDA"},{"ticker":"AMD"}]`.

#### `eli agent swarm`
Map/reduce over large inputs. Chunks a file, runs parallel map workers, then reduces + synthesizes.
```bash
eli agent swarm --task "Extract key claims" --input large_doc.md --chunks 5 --max-parallel 3
```

#### Specialized analysis modes
Convenience wrappers around fanout with specialized system prompts:
```bash
eli agent critique --prompt "Is recession coming?" --lead report.md --vars vars.json
eli agent evidence --prompt "Bull case for NVDA" --vars vars.json
eli agent compete --prompt "Best semiconductor stock" --vars vars.json
eli agent debate --prompt "US fiscal dominance" --vars vars.json
```

---

## Tool Usage Tips

### Best practices
- **Use Eli for structured data, your own websearch for narrative context.** They complement each other.
- **Run multiple Eli commands in parallel** — they're independent and fast.
- **`eli finance macro` is the first thing to run** for any macro/economic question — 31 real indicators in 2 seconds beats hallucinating stale numbers.
- **Pick the right weight class for prediction markets:**
  - Quick single-event lookup → `eli finance odds --event <ticker>` (no sync needed, ~1s)
  - Search with fresh prices → `eli finance odds --search "query" --live` (~2s)
  - Broad discovery → `eli finance odds --search "query"` on CSV cache (~170ms, requires prior sync)
  - Full scrape → `eli finance sync` (~10s, caches 22,500 markets)
- **The CSV is an index, not the data.** Stale CSV is fine for discovering event IDs/slugs/titles. Use `--live` or `--event` for fresh prices.
- **No sync? No problem.** `--search` auto-falls back to live API when no CSV exists.
- **`eli finance timeseries` auto-detects Yahoo vs FRED** — just pass any ticker or FRED series ID.
- **Combine tools for a full picture:** `macro` for economy, `snapshot` for prices, `options` for flow, `odds --search` for market expectations, `news` for headlines.

### Common mistakes
- Don't hallucinate macro data — always use `eli finance macro`. AI training data for debt-to-GDP, unemployment, etc. is stale.
- Don't call `eli finance fundamentals` on ETFs (SPY, QQQ, TLT) — use `snapshot` instead.
- Don't `sync` when you just need one event — use `eli finance odds --event <ticker>` for direct API lookup.
- Don't use `--provider fred` or `--provider yahoo` — the default `auto` handles both.
- News for short tickers (AI, SPY) can return unrelated results — check relevance.
- `eli finance macro` may return empty on weekends/holidays — FRED API limitation.
- Don't assume Polymarket event IDs are slugs — they're numeric (e.g., `48802`). Use `--provider polymarket` when querying Polymarket events directly.

---

## Build & Run

### Build (after Rust changes)
```bash
cd eli
CARGO_HOME=$(pwd)/.cargo_local_local \
CARGO_TARGET_DIR=$(pwd)/target_local \
CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse \
cargo build -p eli-cli --bin eli

ln -sf $(pwd)/target_local/debug/eli ../bin/eli
```

### Run tools
```bash
bin/eli finance snapshot --ticker NVDA
bin/eli finance macro
bin/eli finance options --ticker SPY --summary
bin/eli finance sync
bin/eli finance odds --search "recession" --live
bin/eli finance news --ticker AAPL --date 2026-02-05
bin/eli web search "tariff impact on semiconductors"
```

---

## Configuration

**File:** `~/.config/eli/config.toml`

```toml
[chat]
model = "arcee-ai/trinity-large-preview:free"
provider = "openrouter"
openrouter_api_key = "sk-or-v1-..."

[finance]
cache_dir = "~/.eli/cache/finance"
cache_ttl_hours = 24
yahoo_timeout_secs = 30
fred_api_key = "optional"   # For higher FRED rate limits
```

---

## Repo Layout

- `eli/` — Rust workspace (the `eli` binary + internal crates)
- `eli website/` — Static landing page + local demo server
- `eli_research/` — Generated research reports (created at runtime)
- `bin/eli` — Symlink to built binary

## Key implementation files

- Tool contract + system prompt: `eli/crates/eli-core/src/contract/mod.rs`
- Main agent loop: `eli/crates/eli-cli/src/lib.rs`
- Finance tools: `eli/crates/eli-core/src/finance/`
- Provider adapters: `eli/crates/eli-adapters/`
