# eli

**Free market data CLI for AI agents.**

Ask a question. Your AI pulls the data, runs the analysis, gives you an answer.

Works with Claude Code, Codex, Gemini CLI, or `eli tui` with OpenRouter.

[eli-terminal.com](https://eli-terminal.com)

---

## What is Eli?

Eli is a data layer for AI agents. It works alongside your web search tool, not instead of it.

**Web search is great for the past.** Articles, context, narratives, human sentiment. But it's inefficient for what's happening NOW and what's EXPECTED to happen. You burn tokens parsing HTML articles that describe yesterday's move.

**Eli handles present and future:**
- **Present** — What IS the price right now? `eli finance snapshot`
- **Future** — What does the market EXPECT? `eli finance odds` (Kalshi, Polymarket)
- **Structured past** — What ACTUALLY happened? `eli finance timeseries` (not a journalist's summary)

**Use both.** Eli gives you prices, options IV, prediction market odds, macro data. Web search gives you the human narrative — why people think something happened, political context, sentiment. Your AI synthesizes both into a complete picture.

Example: "What's happening with silver?"
- **Eli** → SLV dropped 30%, IV at 147%, SPY/TLT/BTC flat, Kalshi prices 2 Fed cuts
- **Web search** → Government shutdown context, margin call rumors, analyst takes
- **AI synthesizes** → "Margin-call liquidation, not macro crisis. Bounce likely."

One tool for hard data. One tool for human context. Your AI connects them.

---

## Install

```bash
cargo install eli
```

Or build from source:

```bash
cd eli
cargo build --release
```

---

## Quick Start

### Use with Claude Code / Codex / Gemini CLI

Your AI agent calls eli tools directly:

```bash
# Current price
eli finance snapshot --tickers NVDA

# Historical data
eli finance timeseries --tickers AAPL --range 30d --granularity 1h

# Options chain with IV
eli finance options --ticker SPY --summary

# Prediction markets
eli finance odds --series KXRATECUTCOUNT --list-markets

# Macro indicators
eli finance macro
```

### Use with eli tui

If you don't have a CLI agent, use Eli's built-in TUI with free OpenRouter models:

```bash
eli tui
```

Then ask questions in natural language. Eli pulls the data and runs the analysis.

---

## Tools

| Tool | What it does | Source |
|------|--------------|--------|
| `timeseries` | OHLCV candles with range and granularity | Yahoo Finance |
| `snapshot` | Current price, market cap, shares, EV | Yahoo Finance |
| `fundamentals` | Income statement, balance sheet, cash flow | Yahoo Finance |
| `filings` | SEC filings with full text | SEC EDGAR |
| `news` | Date-specific headlines | Google News RSS |
| `odds` | Prediction market prices and orderbooks | Kalshi, Polymarket |
| `options` | Options chains with IV and skew | Yahoo Finance |
| `macro` | CPI, unemployment, Fed funds, yield curve | FRED |
| `search` | Ticker and macro ID lookup | Yahoo + FRED |

All tools return structured JSON. No HTML parsing. No token-heavy article text.

---

## Why Eli?

### 1. Raw Data First

If Yahoo says $72.44, your AI works from that number. No hallucinated prices. No "approximately" or "around."

### 2. Compute Locally

LLMs can't do math. Eli fetches data with Rust, your AI writes Python to calculate correlations and returns, executes it locally. The math is real.

### 3. Options as Signal

IV at 147% tells you more than price action. Put/call ratios, skew, max pain — positioning data that news doesn't capture.

### 4. Prediction Markets

Kalshi odds are forward-looking. "56% Japan recession" beats yesterday's headlines. Read the future, not the past.

---

## Example: Cross-Asset Analysis

User asks: "Silver crashed 30% — what happened?"

Eli-powered AI autonomously:

```bash
# 1. See the crash
eli finance timeseries --tickers SLV --range 5d --granularity 1h
# SLV: $109.53 → $68.26 low → $72.44 current

# 2. Check options fear
eli finance options --ticker SLV --summary
# ATM IV: 147% calls / 126% puts — extreme

# 3. Check Fed expectations
eli finance odds --series KXRATECUTCOUNT --list-markets
# 2 cuts (25%), 3 cuts (22%), 0 cuts (8%)

# 4. Check if it's macro or isolated
eli finance snapshot --tickers SPY,TLT,BTC-USD
# SPY +0.5%, TLT flat, BTC flat — not macro

# 5. Synthesize
# "Silver's 30% drop is margin-call liquidation, not macro crisis.
#  Equities/bonds/crypto flat. SLV IV at 147% = overdone.
#  Bounce to $78-85 likely."
```

One question. Autonomous research. Cited numbers.

---

## Configuration

Config file: `~/.config/eli/config.toml`

```toml
[chat]
provider = "openrouter"
model = "anthropic/claude-3.5-sonnet"

[finance]
cache_dir = "~/.eli/cache/finance"
cache_ttl_hours = 24
```

For eli tui, set your OpenRouter API key:

```bash
export OPENROUTER_API_KEY=sk-or-...
```

---

## Tool Reference

### timeseries

```bash
eli finance timeseries --tickers AMD,INTC --range 30d --granularity 1h
```

Options:
- `--tickers` — Comma-separated or repeatable
- `--range` — e.g., `1d`, `30d`, `12mo`, `5y`
- `--granularity` — e.g., `1h`, `1d`, `1w`
- `--provider` — `yahoo` (default) or `fred`

### snapshot

```bash
eli finance snapshot --tickers NVDA,AMD,INTC
```

Returns current price, previous close, day range, market cap.

### options

```bash
eli finance options --ticker SPY --summary
eli finance options --ticker SPY --expiry 2024-03-15 --near-money 10
```

Returns IV, put/call ratios, max pain, full chain.

### odds

```bash
# List available series
eli finance odds --list-series --category Economics

# Get markets in a series
eli finance odds --series KXRATECUTCOUNT --list-markets

# Polymarket
eli finance odds --provider polymarket --list-events
```

### macro

```bash
eli finance macro
```

Returns CPI, unemployment, Fed funds, 10Y-2Y spread, M2, WTI.

### filings

```bash
eli finance filings --ticker AAPL --include-text
```

Returns recent 8-K, 10-K, 10-Q filings from SEC EDGAR.

---

## How It Works

1. **Your AI decides what data to pull** — based on the user's question
2. **Eli fetches from free APIs** — Yahoo Finance, FRED, Kalshi, SEC EDGAR
3. **Returns structured JSON** — no HTML, no ads, no parsing
4. **Your AI analyzes and synthesizes** — writes Python if needed, forms a view
5. **User gets an answer with cited numbers**

The AI orchestrates. Eli provides the data pipe.

---

## Free Forever

Eli uses free public APIs:
- Yahoo Finance (free)
- FRED (free)
- Kalshi public API (free)
- Polymarket (free)
- SEC EDGAR (free)
- Google News RSS (free)

No paid API keys required. No subscription.

---

## Contributing

PRs welcome. The codebase is Rust with a focus on:
- Fast, reliable data fetching
- Structured JSON output
- No dependencies on paid services

```bash
cd eli
cargo test
cargo build
```

---

## License

MIT

---

**[eli-terminal.com](https://eli-terminal.com)** — Connect your AI to the market.
