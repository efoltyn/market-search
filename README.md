# market-search

Your AI has web search. Ask it about recession risk or oil, and it summarizes articles.

This gives it market search: the live odds of recession on Kalshi and Polymarket, and the price of oil, as structured data it can read and line up on one axis.

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

21 tools, one timeseries axis. Stocks, options, futures curves, crypto, forex, prediction markets, FRED macro, Treasury auctions, SEC filings, and central banks (Fed, ECB, BOJ, BOE, BIS). Runs on your machine. No API keys for the core data.

---

## Install

**Cargo** (if you have Rust):

```bash
cargo install market-search
```

**Prebuilt binary** (no toolchain, macOS / Linux). Download the binary for your platform from the [latest release](https://github.com/efoltyn/market-search/releases/latest) and put it on your PATH, or:

```bash
curl -fsSL https://eliterminal.com/install.sh | sh
```

**From source:**

```bash
git clone https://github.com/efoltyn/market-search.git
cd market-search/eli && cargo build --release
```

**Or let your AI set it up.** Tell Claude Code (or any coding agent):

> Install market-search and add it as an MCP server.

It installs the binary and runs `claude mcp add market-search -- market-search mcp`. The source is open Rust, so an agent can also read it, change tools, and add providers.

---

## Connect it

One line in Claude Code:

```bash
claude mcp add market-search -- market-search mcp
```

Or add it to your client config (`.mcp.json`, Claude Desktop, Codex, Cursor, Gemini CLI):

```json
{ "mcpServers": { "market-search": { "command": "market-search", "args": ["mcp"] } } }
```

**From claude.ai or your phone** (optional): run `market-search mcp share` on a machine that stays on. It opens a tunnel and prints a URL to paste into a claude.ai custom connector. The link lives only while that process runs, so use a box that stays awake (there's no background service yet).

---

## Tools

### Market data

| Tool | What it does | Example |
|------|-------------|---------|
| `finance_timeseries` | OHLCV on one axis across stocks, FRED macro, futures, crypto, and prediction markets, auto-routed by ticker. 17 presets pull a whole domain in one call. | `--preset macro --tickers NVDA,SMH --range 90d` |
| `finance_movers` | Largest day gainers / losers / most-active, or screen a list by % move, price, volume, or market cap | `--direction gainers --min-cap 10B` |
| `finance_options` | Full options chain: IV, max pain, put/call ratio, skew. `--all` for every expiration | `--ticker SPY --all` |
| `finance_fundamentals` | Income statement, P/E, margins, ROE, debt/equity | `--ticker NVDA` |
| `finance_curve` | Futures forward curve: every contract month, front/back spread. Oil, brent, gold, silver, natgas, copper, more | `--commodity wti` |
| `finance_search` | Ticker lookup + FRED macro series discovery | `--query "semiconductor"` |

### Prediction markets

| Tool | What it does | Example |
|------|-------------|---------|
| `finance_odds` | Search Kalshi + Polymarket at once: probabilities, volume, both venues merged | `--search "taiwan" --live` |
| `finance_rate_path` | Per-meeting Fed hold / cut / hike probabilities from live Kalshi markets | (no args) |

### Macro & government

| Tool | What it does | Example |
|------|-------------|---------|
| `finance_schedule` | Earnings calendar + macro release dates | `--kind macro --from 2026-04-01 --to 2026-04-30 --major` |
| `finance_cot` | CFTC Commitments of Traders: speculator vs commercial positioning | `--query crude --weeks 12` |
| `finance_auctions` | US Treasury auctions: bid-to-cover, high yield, bidder breakdown | `--security-type note` |
| `finance_nyfed` | NY Fed: SOFR, EFFR, reverse repo, SOMA holdings, dealer positions | `--kind rates` |
| `finance_fiscal` | National debt, Treasury cash balance, average interest rates | `--kind debt` |
| `finance_stress` | OFR Financial Stress Index, decomposed into credit / equity / funding / vol | `--range 90` |
| `finance_volsurface` | CBOE vol indices: VIX, VIX9D, VIX3M, VIX6M, VIX1Y, VVIX, OVX, GVZ, SKEW | `--symbols VIX,VVIX,SKEW` |
| `finance_filings` | SEC filings by ticker: URLs, primary docs, optional inline text | `--ticker NVDA --forms 10-K --limit 3` |

### Central banks

| Tool | What it does | Example |
|------|-------------|---------|
| `finance_ecb` | EUR rates, Euro STR, M3, EURIBOR, euro yield curve, balance sheet | `--preset yield_curve` |
| `finance_bis` | Global policy rates, total assets, credit-to-GDP gaps | `--preset policy_rates --countries US,XM,JP,GB` |
| `finance_boj` | Monetary base, TANKAN, BOJ balance sheet, call rate, USD/JPY | `--preset tankan` |
| `finance_boe` | Bank Rate, SONIA, gilt yields, M4, GBP rates | `--preset all` |
| `finance_eia` | US crude, gasoline, distillate, natural gas storage | `--preset crude` |

---

## Timeseries provider routing

`finance_timeseries` picks the source from the ticker:

| Ticker | Provider | Coverage |
|--------|----------|----------|
| `AAPL`, `SPY`, `CL=F`, `BTC-USD` | Yahoo Finance | Stocks, ETFs, futures, crypto, FX |
| `UNRATE`, `DGS10`, `CPIAUCSL` | FRED | 800,000+ macro series, no key |
| `PYTH:BTC`, `PYTH:OIL` | Pyth Network | 24/7 crypto & commodity oracles |
| `BN:BTC`, `BN:ETH` | Binance | Crypto OHLCV since 2019 |
| `KXRECSSNBER-26` | Kalshi | Probability + volume candles |
| `609655` | Polymarket | Probability + volume candles |

Mix them in one call:

```bash
market-search finance timeseries --tickers SPY,DGS10,BN:BTC,KXRECSSNBER-26,609655 --range 30d
```

---

## Data sources

Core tools need no API keys.

| Provider | Key |
|----------|-----|
| Yahoo Finance | None |
| Kalshi | None |
| Polymarket | None |
| FRED | None |
| CFTC | None |
| SEC EDGAR | None |
| NY Fed | None |
| US Treasury | None |
| OFR | None |
| CBOE | None |
| Pyth Network | None |
| Binance | None |
| ECB · BIS · BOJ · BOE | None |
| EIA | Free key ([register](https://www.eia.gov/opendata/register.php)) |

---

Runs on your own machine and IP, making the same requests your browser would, at human rates. That is why it is a local binary and not a hosted service.

[eliterminal.com](https://eliterminal.com) · Licensed under [AGPL-3.0](https://www.gnu.org/licenses/agpl-3.0.html).
