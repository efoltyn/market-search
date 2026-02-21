# eli

Native data tools for AI agents. Stock prices, macro indicators, prediction markets, options chains, SEC filings, web scraping — all as a single Rust binary returning structured JSON.

**The agent is Claude Code, Codex, Gemini CLI, or any AI agent. The data is Eli.**

---

## Install

**Requires Rust:** https://rustup.rs

```bash
# From repo root
cargo install --path crates/eli-cli
```

Installs `eli` to `~/.cargo/bin/eli`.

---

## Use as MCP Server (Claude Code, Codex, Gemini CLI, any MCP-compatible agent)

Any AI agent that supports MCP gets eli tools natively — no Bash calls, no parsing CLI output, just structured tool calls.

Add to your project's `.mcp.json`:

```json
{
  "mcpServers": {
    "eli": {
      "command": "eli",
      "args": ["mcp"]
    }
  }
}
```

Restart your agent. Tools appear natively alongside the agent's built-in tools.

**21 tools exposed:**
- `finance_macro` — 31 FRED macro indicators in ~2s
- `finance_snapshot` — price, market cap, daily returns
- `finance_timeseries` — OHLCV for stocks or FRED series
- `finance_yield_curve` — US Treasury curve with spreads
- `finance_rate_path` — implied Fed trajectory from prediction markets
- `finance_odds` — Kalshi + Polymarket live prices
- `finance_options` — options chain with IV, put/call ratio, max pain
- `finance_news` — headlines by ticker and date
- `finance_prices` — crypto/commodity spot prices (Pyth)
- `finance_fundamentals` — income statement, balance sheet, cash flow
- `finance_sync` — bulk sync 22,500 prediction markets to local CSV
- `finance_search` — ticker/series search
- `finance_filings` — SEC filings (8-K, 10-K, 10-Q)
- `finance_schedule` — earnings + macro release calendar
- `finance_dashboard` — preset aggregate tools (recession, tech_megacap)
- `web_search` — DuckDuckGo search
- `web_read` — fetch and extract content from a URL
- `web_crawl` — crawl a site, extract all pages
- `web_extract` — extract key facts from URL, file, or text
- `code_analyze` — parse Rust source: signatures, pub API surface, symbol search
- `agent_run` — spawn an autonomous eli research worker

---

## Use from the CLI

```bash
# Financial data
eli finance snapshot --ticker NVDA,AAPL,MSFT
eli finance macro
eli finance options --ticker SPY --summary
eli finance sync
eli finance odds --search "recession" --live
eli finance yield-curve --compare 3mo,1y

# Web
eli web search "tariff impact semiconductors"
eli web crawl --url https://example.com

# Codebase analysis
eli code src/                        # hotspot ranking
eli code src/ --pub-api              # complete public API surface
eli code src/ --find "fetch_snapshot,SnapshotRequest"  # symbol search

# Multi-agent
eli agent run --task "Analyze AMD vs INTC correlation"
eli agent swarm --task "Extract key claims" --input large_doc.md
```

---

## Keys

```bash
export OPENROUTER_API_KEY=...
export OPENAI_API_KEY=...
export ANTHROPIC_API_KEY=...
```

Or set in `~/.config/eli/config.toml`.

---

## Build from source (dev)

```bash
cd eli
CARGO_HOME=$(pwd)/.cargo_local_local \
CARGO_TARGET_DIR=$(pwd)/target_local \
cargo build -p eli-cli --bin eli

ln -sf $(pwd)/target_local/debug/eli ../bin/eli
```
