# eli

Agent-native market and web ingestion tools as one Rust binary.

Eli gives agents structured live data from public APIs (prices, macro, prediction markets, filings, calendars, web ingestion) so they can reason from state, not just narrative search output.

---

## Install

**Requires Rust:** https://rustup.rs

```bash
cargo install eli
```

Installs `eli` to `~/.cargo/bin/eli`.

Local dev install from source:

```bash
# From /Users/elifoltyn/Desktop/eli-code/eli
cargo install --path .
```

---

## MCP Setup

Add to `.mcp.json`:

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

Restart your agent. Eli tools become native MCP tools.

---

## Tool Surface

MCP currently exposes **22 tools**:

- Finance: `finance_macro`, `finance_forex`, `finance_snapshot`, `finance_timeseries`, `finance_yield_curve`, `finance_rate_path`, `finance_odds`, `finance_options`, `finance_news`, `finance_prices`, `finance_fundamentals`, `finance_sync`, `finance_search`, `finance_filings`, `finance_schedule`, `finance_dashboard`
- Web ingestion: `web_search`, `web_read`, `web_crawl`, `web_extract`
- Code/agent: `code_analyze`, `agent_run`

---

## Positioning

Eli complements built-in websearch in Claude/Codex/Gemini/Cursor/OpenClaw.

- Use websearch for broad discovery and narrative context.
- Use Eli for structured, reproducible, low-token data ingestion and deltas.

---

## Data Access (No Paid Data Keys Required)

Finance/data tools use public endpoints including:
- Yahoo Finance
- FRED
- Kalshi
- Polymarket
- SEC EDGAR
- Google News RSS
- Pyth Hermes

No paid market-data subscription is required for normal Eli usage.

---

## CLI Examples

```bash
# Financial state
eli finance snapshot --tickers NVDA,AAPL,SPY
eli finance timeseries --tickers SPY,UNRATE --range 1y --granularity 1d
eli finance macro --range 1y
eli finance forex --range 1y --horizons 1w,1mo,3mo,1y

# Prediction markets
eli finance sync --sources kalshi,polymarket --max-pages 10
eli finance odds --search "recession" --live --top 10

# Web ingestion
eli web search --query "fed rate decision" --mode news --recency week
eli web read --url https://example.com/article
eli web extract --url https://example.com/article --bullets 8

# Code analysis
eli code src/ --pub-api
```

---

## Workspace Crates

`eli` is the install target. Other workspace crates are internal components published for dependency resolution:

- [`eli-cli`](./crates/eli-cli/README.md)
- [`eli-core`](./crates/eli-core/README.md)
- [`eli-adapters`](./crates/eli-adapters/README.md)
- [`eli-finance-types`](./crates/eli-finance-types/README.md)
- [`eli-screen`](./crates/eli-screen/README.md)

---

## Build from Source (Dev)

```bash
cd eli
CARGO_HOME=$(pwd)/.cargo_local_local \
CARGO_TARGET_DIR=$(pwd)/target_local \
cargo build -p eli --bin eli

ln -sf $(pwd)/target_local/debug/eli ../bin/eli
```
