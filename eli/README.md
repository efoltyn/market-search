# eli

Agent-native market data tools as one Rust binary.

Eli gives agents structured live market data from public APIs so they can reason from numbers, not just narrative search output.

For launch, the two pillar tools are:
- `eli finance timeseries`
- `eli finance odds`

The rest of the finance surface supports those two.

---

## Install

**Requires Rust:** https://rustup.rs

```bash
cargo install eli
```

Installs `eli` to `~/.cargo/bin/eli`.

Local dev install from source:

```bash
# From the repo's eli/ workspace
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

MCP currently exposes **15 finance tools**:

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

## Positioning

Eli complements built-in websearch in Claude/Codex/Gemini/Cursor/OpenClaw.

- Use websearch for broad discovery and narrative context.
- Use Eli for structured, reproducible, low-token data ingestion and deltas.

---

## Zero-Key Core

Core finance tools use public endpoints including:
- Yahoo Finance
- FRED
- Kalshi
- Polymarket
- US Treasury / FiscalData
- New York Fed
- OFR
- CFTC
- SEC EDGAR
- Pyth Hermes

No paid market-data subscription is required for normal Eli usage. Optional attachments can add providers like IBKR or keyed FRED enhancements later.

---

## CLI Examples

```bash
# Core movement/context tool
eli finance timeseries --tickers SPY,UNRATE --range 1y --granularity 1d

# Core expectations tool
eli finance odds --search "recession" --live --top 10
eli finance odds --event KXFEDDECISION-26APR29

# Supporting finance tools
eli finance snapshot --tickers NVDA,AAPL,SPY
eli finance options --ticker SPY --summary --near-money 5
eli finance search --query "crude oil"
eli finance schedule --kind macro --from 2026-03-23 --to 2026-04-03 --major
eli finance filings --ticker NVDA --forms 10-K --limit 3
eli finance cot --query "crude" --weeks 12
eli finance nyfed --kind rates
eli finance fiscal --kind debt
eli finance stress --range 30
eli finance volatility --symbols VIX,VVIX,SKEW --history 5
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
cargo check -p eli --bin eli
cargo build -p eli --bin eli

ln -sf $(pwd)/target/debug/eli ../bin/eli
```
