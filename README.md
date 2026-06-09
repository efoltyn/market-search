# market search

Your AI has web search. Ask about recession risk or oil prices, it summarizes articles.

Now it has **market search**. It sees the odds of recession on Kalshi and Polymarket, and the live price of oil.

```
> finance_odds --search "recession" --live

  kalshi        36.5%  vol=82,231,200   Will there be a recession in 2026?
  polymarket    36.5%  vol=1,824,634    US recession by end of 2026
  kalshi        22.5%  vol=2,965,000    Will the IMF declare a global recession before 2027?
```

```
> finance_timeseries --tickers SPY,CL=F,^VIX,KXRECSSNBER-26 --range 30d

  SPY                          22 candles   $682 → $655   -4.0%
  CL=F                         22 candles    $66 → $89   +33.8%
  ^VIX                         22 candles     21 → 26    +22.5%
  KALSHI:KXRECSSNBER-26:YES    30 candles    22% → 36%   +63.6%
```

21 tools. 23 data providers. Stocks, options, crypto, futures, forex, prediction markets, FRED macro, Treasury auctions, SEC filings, central bank data from the Fed, ECB, BOJ, BOE, and BIS. Runs locally. No paid API keys.

## Install

Three ways:

**1. Cargo (recommended)**

```bash
cargo install market-search
```

Installs `market-search` to `~/.cargo/bin/`. Requires Rust ([rustup.rs](https://rustup.rs)).

**2. Build from source**

```bash
git clone https://github.com/efoltyn/market-search.git
cd market-search
cargo build --release
./target/release/market-search --help
```

**3. Let your AI install it**

Paste this into Claude Code, Codex, or any coding agent:

> Install market-search from https://github.com/efoltyn/market-search — clone the repo, build the Rust binary, and add it as an MCP server in `.mcp.json`.

It will read the repo, run cargo build, and configure itself. The codebase is open Rust — coding agents can also read the source, modify tools, and add new providers.

## MCP setup (local stdio)

Add to your agent's MCP config (`.mcp.json` or equivalent):

```json
{
  "mcpServers": {
    "market-search": {
      "command": "market-search",
      "args": ["mcp"]
    }
  }
}
```

Works with Claude Code, Codex, Gemini CLI, Cursor, Claude Desktop, or any MCP-compatible agent.

## Public URL for claude.ai web / ChatGPT custom apps

For claude.ai or ChatGPT (which need a public HTTPS URL to reach your local server), market-search ships a built-in tunnel command. Three providers ship today:

```bash
market-search mcp share --provider cloudflare                            # temporary, instant (default)
market-search mcp share --provider tunnelmole                            # temporary, instant (less reliable)
market-search mcp share --provider ngrok --domain mysub.ngrok-free.dev   # permanent, requires free ngrok account
```

The command boots the local MCP server, spawns the tunnel binary, parses the public URL from its output, and prints a paste-ready block for claude.ai's connector dialog or ChatGPT's apps & connectors page.

> **Self-host / sovereign mode is NOT implemented yet** — `--provider self-host` is a placeholder that errors out. The planned architecture (laptop holds TLS keys, gateway only routes encrypted bytes) is described in [SELFHOST.md](SELFHOST.md) as a design spec. For sensitive use today, run `market-search mcp` locally over stdio (no public URL exposure).

## Tools

The 21 tools below are the MCP surface — what your AI sees when you connect via `market-search mcp`. They're the core of the project.

The CLI binary also ships two **experimental** non-MCP subcommands: `market-search web` (basic crawl/search/read tools) and `market-search picks` (logs report picks to track performance over time). Neither is exposed via MCP and neither is core. They're shipped as-is; **contributors welcome to improve or replace them**. If you're here to use Market Search with an AI, ignore both.

### Market data
| Tool | What it does | Example |
|---|---|---|
| `finance_timeseries` | OHLCV candles from 9 providers. Auto-routes by ticker prefix. Mix stocks, ETFs, crypto, futures, FX, FRED macro, prediction markets in one call. | `--tickers SPY,DGS10,BN:ETH,CL=F --range 90d` |
| `finance_movers` | Largest day movers by percent, market cap, dollar volume, or estimated market-cap value change. Uses IBKR when configured, Yahoo otherwise. | `--sort-by value_change --min-market-cap 2B` |
| `finance_options` | Full options chain: IV, max pain, put/call ratio, skew. `--all` parallelizes every expiration. | `--ticker SPY --all` |
| `finance_fundamentals` | Income statement, P/E, margins, ROE, debt/equity, dividend yield | `--ticker NVDA` |
| `finance_search` | Ticker symbol lookup + FRED macro series discovery | `--query "semiconductor"` |

### Prediction markets
| Tool | What it does | Example |
|---|---|---|
| `finance_odds` | Search Kalshi + Polymarket simultaneously. Probabilities, volume, both sources merged. | `--search "taiwan" --live` |
| `finance_rate_path` | Per-meeting Fed hold/cut/hike probabilities aggregated from Polymarket + Kalshi | (no args needed) |

### Macro & government
| Tool | What it does | Example |
|---|---|---|
| `finance_schedule` | Earnings calendar + macro release dates (BEA, FRED, official sources) | `--kind macro --major --from 2026-04-01 --to 2026-04-30` |
| `finance_cot` | CFTC Commitments of Traders: speculator vs commercial positioning | `--query crude --weeks 12` |
| `finance_auctions` | US Treasury auction results: bid-to-cover, high yield, bidder breakdown | `--security-type note` |
| `finance_nyfed` | NY Fed: SOFR, EFFR, reverse repo, SOMA holdings, dealer positions | `--kind rates` |
| `finance_fiscal` | National debt, Treasury cash balance, average interest rates by security | `--kind debt` |
| `finance_stress` | OFR Financial Stress Index with credit/equity/funding/vol decomposition | `--range 90` |
| `finance_volsurface` | CBOE vol indices: VIX, VIX9D, VIX3M, VIX6M, VIX1Y, VVIX, OVX, GVZ, SKEW | `--symbols VIX,VVIX,SKEW` |
| `finance_filings` | Download recent SEC filings by ticker; returns URLs, local primary docs, and saved index JSON | `--ticker NVDA --forms 10-K --limit 3` |
| `finance_curve` | Futures forward curves for energy, metals, grains | `--commodity gold` |

### Central banks
| Tool | What it does | Example |
|---|---|---|
| `finance_ecb` | EUR/USD, Euro STR, M3, EURIBOR term structure, euro yield curve | `--preset yield_curve` |
| `finance_bis` | Global central bank policy rates, total assets, credit-to-GDP gaps | `--preset policy_rates --countries US,XM,JP,GB` |
| `finance_boj` | BOJ uncollateralized overnight call rate, TANKAN, balance sheet, JPY pairs | `--preset tankan` |
| `finance_boe` | Bank Rate, SONIA, gilt yields (5Y/10Y/20Y), M4, GBP pairs | `--preset all` |
| `finance_eia` | US crude, gasoline, distillate, natural gas storage + spot prices | `--preset crude` |

## Timeseries provider routing

`finance_timeseries` auto-detects the data source from the ticker:

| Pattern | Provider | Coverage |
|---|---|---|
| `AAPL`, `SPY`, `CL=F`, `BTC-USD` | Yahoo Finance | Stocks, ETFs, futures, crypto, FX |
| `UNRATE`, `DGS10`, `FRED:CPIAUCSL` | FRED | 800,000+ macro series |
| `PYTH:BTC`, `PYTH:OIL` | Pyth Network | 24/7 crypto and commodity oracles |
| `BN:BTC`, `BN:ETH` | Binance | Crypto OHLCV since 2019 |
| `IBKR:FUT:CL:NYMEX` | IBKR (free, more setup) | Intraday futures incl. overnight extremes — free 15-min delayed institutional data once IB Gateway is running locally |
| `CLEV:CPI`, `CLEV:COREPCE` | Cleveland Fed | Real-time inflation nowcasts |
| `KXRECSSNBER-26` | Kalshi | Probability + volume candles |
| `609655` (numeric) | Polymarket | Probability + volume candles |
| `RATEPATH:JUN2026:hold` | Aggregated rate path | Per-meeting Fed hold/cut/hike historical curves |

Mix any of them in one call:

```bash
market-search finance timeseries --tickers SPY,DGS10,BN:BTC,KXRECSSNBER-26,609655 --range 30d
```

## Data sources

All core tools work without API keys.

| Provider | Auth |
|---|---|
| Yahoo Finance, Kalshi, Polymarket, CFTC, SEC EDGAR | None |
| NY Fed, US Treasury, OFR, CBOE, Nasdaq, BEA | None |
| Pyth Network, Binance | None |
| ECB, BIS, BOJ, BOE, Cleveland Fed | None |
| FRED | Free key ([register](https://fred.stlouisfed.org/docs/api/api_key.html)) |
| EIA | Free key ([register](https://www.eia.gov/opendata/register.php)) |
| IBKR | Free IBKR account + IB Gateway running locally. 15-min delayed institutional-quality data is free (no fee). Real-time data costs from IBKR if you want it (~$10/mo for most US markets, free if you trade through them). Set IB Gateway to refresh every 24h so you don't re-sign-in daily. |

## Positioning

market-search complements the built-in web search in Claude/Codex/Gemini/Cursor.

- Use **web search** for broad discovery and narrative context.
- Use **market-search** for structured, reproducible, low-token data ingestion and deltas.

The two are designed to be used together. Web search finds the article. market-search shows the chart.

## Demo

[Eli Terminal](https://eliterminal.com) is the daily demo — every report on the site is produced by Claude using market-search, autonomously, on a 30-minute loop.

## Workspace crates

`market-search` is the install target. Other workspace crates are internal components published for dependency resolution:

`eli-cli`, `eli-core`, `eli-finance-types`

## License

AGPL-3.0-or-later for individuals, research, and OSS projects.

For firm and commercial use: licensing@eliterminal.com.
