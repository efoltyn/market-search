### `eli finance odds`

Prediction market odds. Real money = real belief.

**Providers:** `kalshi` (default), `polymarket`, `auto`

### Discovery workflow (IMPORTANT)

**Step 1: Find the series ticker**
```bash
eli finance odds --list-series --search "shutdown"
eli finance odds --list-series --search "rate cut"
eli finance odds --list-series --search "khamenei"
eli finance odds --list-series --search "S&P"
```

**Step 2: Fetch odds by series**
```bash
eli finance odds --series KXGOVSHUTLENGTH
eli finance odds --series KXRATECUTS
eli finance odds --series KXKHAMENEIOUT
eli finance odds --series KXINX
```

**DO NOT** use `--list-events | grep` or `--category X | grep`. Use `--list-series --search` instead.

### Common tickers

| Topic | Ticker | Example |
|-------|--------|---------|
| Gov shutdown | KXGOVSHUTLENGTH | >3 days: 94% |
| Fed rate cuts | KXRATECUTS | # of cuts in 2026 |
| S&P 500 daily | KXINX | Range brackets |
| S&P 500 yearly | INXY | Year-end targets |
| Khamenei | KXKHAMENEIOUT | Leave by Sep: 47% |
| Greenland | KXGREENLAND | Trump buys: 31% |
| Super Bowl | KXSB | Team odds |
| Trump markets | search "trump" | 45+ events |

### Fetching odds

```bash
# By series (after discovery)
eli finance odds --series KXGREENLAND

# By event (specific instance)
eli finance odds --event KXGREENLAND-29

# By market (with orderbook depth)
eli finance odds --market KXGREENLAND-29-27 --orderbook

# Browse all events
eli finance odds --list-events --limit 50

# Polymarket tags
eli finance odds --list-tags --provider polymarket

# Local CSV search (breadth preserved by default)
eli finance odds --search "recession"

# Optional opt-in filters for local CSV search
eli finance odds --search "unemployment" --country US --min-volume 10000 --top 5
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
