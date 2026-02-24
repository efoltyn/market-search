# eli

Structured real-time data tools for AI agents.

Use Eli with Claude Code, Codex, Gemini CLI, Cursor, OpenClaw, or any MCP-compatible agent to fetch live market state and prediction-market expectations as JSON.

[eli-terminal.com](https://eli-terminal.com)

---

## What Eli Is

Eli is a **data ingestion layer** for agents.

- Your agent already has websearch for broad discovery and narrative context.
- Eli gives your agent **direct, structured access** to live public market APIs.
- Output is JSON, so agents can reason, rank, compare deltas, and run calculations without scraping random HTML.

Think of it as: **websearch for stories, Eli for state**.

---

## Why It Matters

Built-in websearch is strong for:
- "What happened?"
- "What are people saying?"

Eli is strong for:
- "What is the price now?"
- "What changed since last sync?"
- "What does the market imply for Fed/rates/recession?"
- "Give me exact article text with fetch diagnostics, not model narrative"

Eli is additive, not a replacement.

---

## Install

```bash
cargo install eli
```

Build locally from source:

```bash
cd eli
cargo build --release
```

---

## MCP Setup (Any Agent)

Add to your `.mcp.json`:

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

Restart your agent. Eli tools appear as native MCP tools.

---

## MCP Tools Exposed

Current MCP surface in code (`eli/crates/eli-cli/src/cmd/mcp.rs`) exposes **22 tools**:

- Finance: `finance_macro`, `finance_forex`, `finance_snapshot`, `finance_timeseries`, `finance_yield_curve`, `finance_rate_path`, `finance_odds`, `finance_options`, `finance_news`, `finance_prices`, `finance_fundamentals`, `finance_sync`, `finance_search`, `finance_filings`, `finance_schedule`, `finance_dashboard`
- Web ingestion: `web_search`, `web_read`, `web_crawl`, `web_extract`
- Code/agent: `code_analyze`, `agent_run`

---

## No-Auth Data By Default

Eli’s data tools target public endpoints, so agents can start immediately without paid data subscriptions.

Primary public sources used across tools:
- Yahoo Finance
- FRED
- Kalshi public API
- Polymarket public market APIs
- SEC EDGAR
- Google News RSS
- Pyth Hermes

No auth is required for these data fetches in normal use.

Note:
- API keys are only needed if you use Eli’s own chat/tui model providers (`openrouter`, `openai`, `anthropic`, etc.).

---

## Quick Command Examples

```bash
# Live stock/ETF snapshot
eli finance snapshot --tickers NVDA,AAPL,SPY

# Mixed market + macro time series (auto provider)
eli finance timeseries --tickers SPY,UNRATE --range 1y --granularity 1d

# Full macro pack (31 indicators)
eli finance macro --range 1y

# Broad FX regime read (USD-relative deltas)
eli finance forex --range 1y --horizons 1w,1mo,3mo,1y

# Prediction markets (search + fresh prices)
eli finance odds --search "federal reserve" --live --top 10

# Bulk sync Kalshi + Polymarket for local fast discovery (non-sports by default)
eli finance sync --sources kalshi,polymarket

# Bound runtime explicitly when you want a fixed budget
eli finance sync --sources kalshi,polymarket --max-pages 25

# Include sports too when needed
eli finance sync --sources kalshi,polymarket --include-sports

# Paper trading sandbox (local simulated fills, live pricing)
eli finance paper --command reset --account sandbox --starting-cash 10000
eli finance paper --command trade --account sandbox --provider kalshi --market KXBTCD-26FEB2400-T59999.99 --side yes --action buy --qty 5
eli finance paper --command positions --account sandbox
# Note: --mode kalshi-demo is reserved for upcoming signed demo-order routing; current v1 is local simulated paper execution.

# Web ingestion search (deterministic filters, probes)
eli web search --query "fed meeting march 2026" --mode news --recency week --top 15

# Read one or many URLs with diagnostics
eli web read --url https://example.com/article --max-chars 2400
eli web read --url https://a.com,https://b.com --max-parallel 6

# Extract key facts from URL/file/text
eli web extract --url https://example.com/article --bullets 8 --focus "policy"
```

---

## Design Goals

1. Structured first: JSON outputs tuned for agent pipelines.
2. Relativity first: compare now vs prior sync/windows; surface deltas.
3. Token efficiency: compact defaults, with full payloads available when needed.
4. Breadth without bloat: wide data access + controllable output budgets.

---

## Contributing

```bash
cd eli
cargo test
cargo build
```

MIT licensed.

---

**[eli-terminal.com](https://eli-terminal.com)** — agent-native market and web ingestion.
