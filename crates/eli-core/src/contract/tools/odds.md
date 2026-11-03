### `eli finance odds`

Prediction market odds. Real money = real belief.

**Providers:** `kalshi` (default), `polymarket`, `auto`

### Search (start here)

`--search` is live on both venues. Kalshi discovery uses Kalshi's own search
index, so new and topical markets show up without a sync. Polymarket runs its
search by relevance, lifetime volume and 24h volume, then ranks the union by
match and depth; combo/parlay legs (`combo: true`) rank below single questions.

```bash
eli finance odds --search "ohio senate"            # that race's events, deepest first
eli finance odds --search "fed" --provider kalshi
eli finance odds --search "fed decision" --status settled   # settled legs carry result + settled_at
eli finance odds --search "cpi" --closing-within 7d         # legs closing inside 7 days
eli finance odds --search CONTROLS-2026-D                   # exact Kalshi market/event/series ticker
```

- `events[]` groups each Kalshi or Polymarket event with all its legs (`legs[]`: ticker,
  outcome, p), so a Fed decision or a strike ladder reads as one
  distribution. `mutually_exclusive: true` means at most one leg pays.
  Past 12 legs the deepest are kept (`legs_omitted` counts the rest).
- `markets[]` is leg-level: `ticker` joins `finance_timeseries` directly
  for Kalshi and Polymarket alike. Rows add `chg_24h_pp` (the venue's own
  day-ago move) when it moved. `volume_usd` is lifetime volume on both
  venues; `volume_24h_usd` is the last day.
- `--status`: `open` (default), `closed`, `settled`, `all` (both venues).
- `--category` on Polymarket also matches event tags (`--category crypto`).
- `--orderbook --depth N`: Kalshi rows get `orderbook.yes_bids` /
  `orderbook.no_bids` as `[price, contracts]`, best first. Kalshi books hold
  bids only; a NO bid at q is a YES offer at 1-q.

### Kalshi trades

```bash
eli finance odds --market CONTROLS-2026-D --trades --since 6h --min-contracts 5000
eli finance odds --market KXFEDDECISION-26JUL-H0 --trades --since 2026-07-29T17:30:00Z --until 2026-07-29T18:10:00Z
```

Tick-level prints (UTC timestamp, yes price, contracts, taker side, block
flag). `_summary` totals contracts and taker-yes vs taker-no contracts over
the window; `trades[]` lists the largest prints (`--limit`, default 50) in
time order. Prints older than Kalshi's historical cutoff come from its
archive automatically.

### Polymarket market and trades

```bash
eli finance odds --market 562793                                  # one market, every sibling leg
eli finance odds --market 562793 --trades --since 6h --min-trade-usd 5000
eli finance odds --market 542538 --trades --since 2025-09-17 --until 2025-09-17
```

`--market <id>` returns bid/ask, lifetime and 24h volume, open interest,
1h/24h/1w/1mo moves, close or settlement time and result, the resolution
rules, every sibling leg, and `timeseries` (which ticker returns which
outcome: `562793` is Yes, `562793:NO` the No token). `--trades` uses the
Kalshi trades shape (`t`, `yes_price`, `contracts`, `taker`) plus `usd` and a
short `wallet`, so repeat prints by one wallet show; `taker: yes` means the
taker bought Yes or sold No.

### Fetching by ticker

```bash
eli finance odds --series KXFEDDECISION
eli finance odds --event KXFEDDECISION-26DEC
eli finance odds --market CONTROLS-2026-D --orderbook
eli finance odds --list-tags --provider polymarket
```

### Reading the output

- `field_semantics` is the schema contract for units/scales.
- `yes_price: 31` means 31% probability (price in cents = probability)
- `probability_yes` / `probability` are decimals in `[0,1]` (`0.31` = 31%).
- `volume` is total traded in cents
- Scalar markets have brackets (e.g., ">$500B", "$100-500B") - highest yes_price = market consensus
- `status: active` = tradeable, `status: finalized` = settled
- Local CSV search now adds:
  - `match_score`
  - `match_terms`
  - `country_hints`
  - `volume_usd`
- Breadth is preserved by default; `--country` filtering is opt-in.

### CSV parsing safety

- `eli finance sync` writes RFC4180 CSV with quoted fields.
- `eli finance sync` excludes sports by default; use `--include-sports` when needed.
- `title` often contains commas.
- Do **not** parse with `awk -F,`, `cut -d,`, or `split(',')`.
- Use a CSV-aware parser (`python csv`, pandas, etc.) when reading `all_markets.csv`.
