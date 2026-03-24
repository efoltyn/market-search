# eli

Native market data tools for AI agents.

Use Eli with Claude Code, Codex, Gemini CLI, Cursor, OpenClaw, or any MCP-compatible agent to fetch live prices, timeseries, prediction-market odds, filings, and market structure as JSON.

[eliterminal.com](https://eliterminal.com)

---

## What Eli Is

Eli is a **market data layer** for agents.

- Your agent already has websearch for broad discovery and narrative context.
- Eli gives your agent **direct, structured access** to live market and macro APIs.
- Output is JSON, so agents can reason, rank, compare deltas, and run calculations without scraping random HTML.

Think of it as: **websearch for stories, Eli for numbers**.

For launch, the two pillar tools are:
- `eli finance timeseries` for movement, context, and timestamps
- `eli finance odds` for market-implied expectations

Everything else supports those two.

See [docs/core-vs-attachments.md](./docs/core-vs-attachments.md) for the zero-key core vs attachment model.

---

## Why It Matters

Built-in websearch is strong for:
- "What happened?"
- "What are people saying?"

Eli is strong for:
- "What is the price now?"
- "How did it move over time?"
- "What does the market imply for Fed/rates/recession?"
- "What do filings, auctions, Fed plumbing, and fiscal data say right now?"

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

Current MCP surface exposes **15 finance tools**:

- `finance_snapshot`
- `finance_timeseries`
- `finance_rate_path`
- `finance_odds`
- `finance_options`
- `finance_fundamentals`
- `finance_search`
- `finance_filings`
- `finance_schedule`
- `finance_auctions`
- `finance_cot`
- `finance_nyfed`
- `finance_volsurface`
- `finance_stress`
- `finance_fiscal`

---

## Zero-Key Core

Eli’s core market tools work without API keys, so agents can start immediately.

Primary public sources used across the core:
- Yahoo Finance
- FRED
- Kalshi public API
- Polymarket public market APIs
- US Treasury / FiscalData
- New York Fed
- OFR
- CFTC
- SEC EDGAR
- Pyth Hermes

Optional attachments can layer in providers like IBKR or keyed FRED enhancements later without changing the core tools.

---

## Quick Command Examples

```bash
# Mixed market + macro time series (auto provider)
eli finance timeseries --tickers SPY,UNRATE --range 1y --granularity 1d

# Prediction markets (search + fresh prices)
eli finance odds --search "recession" --live --top 10
eli finance odds --event KXFEDDECISION-26APR29
eli finance odds --market KXRECSSNBER-26

# Live stock/ETF snapshot
eli finance snapshot --tickers NVDA,AAPL,SPY

# Options summary
eli finance options --ticker SPY --summary --near-money 5

# Search before timeseries when you do not know the symbol
eli finance search --query "crude oil"

# Calendar / filings / positioning
eli finance schedule --kind macro --from 2026-03-23 --to 2026-04-03 --major
eli finance filings --ticker NVDA --forms 10-K --limit 3
eli finance cot --query "crude" --weeks 12

# Fed / fiscal / stress
eli finance nyfed --kind rates
eli finance fiscal --kind debt
eli finance stress --range 30

# Volatility indices
eli finance volatility --symbols VIX,VVIX,SKEW --history 5
```

---

## Design Goals

1. Structured first: JSON outputs tuned for agent pipelines.
2. Relativity first: compare now vs prior sync/windows; surface deltas.
3. Token efficiency: compact defaults, with full payloads available when needed.
4. Breadth without bloat: wide data access + controllable output budgets.
5. Launch around the core: `timeseries` + `odds` first, everything else second.

---

## Contributing

```bash
cd eli
cargo check -p eli --bin eli
cargo build -p eli --bin eli
ln -sf $(pwd)/target/debug/eli ../bin/eli
```

MIT licensed.

---

**[eliterminal.com](https://eliterminal.com)** — agent-native market data tools.
