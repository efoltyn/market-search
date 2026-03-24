# Monday Night Tool Expansion Log

## COMPLETE — All 7 Sources Built, Hardened, MCP-Wired

**22 tools total | 23 data providers | 20 MCP tools | 96 unit tests passing**

## Status

| # | Source | Status | Started | Completed | Notes |
|---|--------|--------|---------|-----------|-------|
| 1 | Stooq | HARDENED | 2026-03-23 21:00 | 2026-03-23 21:45 | OHLCV backfill + PE + FX pair fix |
| 2 | Binance | HARDENED | 2026-03-23 21:30 | 2026-03-23 21:50 | BTC/ETH/SOL/AVAX, hourly, 1yr, invalid ticker |
| 3 | EIA | DONE (needs key) | 2026-03-23 22:00 | 2026-03-23 22:20 | Built, clean error when no API key |
| 4 | ECB (SDMX) | HARDENED | 2026-03-23 22:00 | 2026-03-23 22:20 | 7 presets + custom, 1595 obs EURUSD since 2020 |
| 5 | BIS (SDMX v2) | HARDENED | 2026-03-23 23:00 | 2026-03-24 00:00 | US assets fixed (XDC key), 6 countries, all presets |
| 6 | BOJ (REST) | HARDENED | 2026-03-23 23:00 | 2026-03-23 23:30 | All 7 presets tested, USD/JPY=159.22 |
| 7 | BOE (CSV) | HARDENED | 2026-03-23 23:00 | 2026-03-23 23:30 | 5Y/10Y/20Y gilts, custom codes, 1571 obs Bank Rate |

---

## Iteration 1 — 2026-03-23 ~21:00

### API Research (4 parallel subagents)
- Stooq: CSV endpoint, no auth, `?s={ticker}&d1={YYYYMMDD}&d2={YYYYMMDD}&i={d|w|m}`
- Binance: JSON klines, no auth, `?symbol={PAIR}&interval={1d}&limit=1000`, paginate via startTime
- EIA: JSON v2 API, requires API key, `petroleum/stoc/wstk/data/` for inventories
- ECB: SDMX REST, no auth, CSV format, `data/{dataset}/{key}?format=csvdata`

### Stooq Provider (stooq.rs)
- File: `eli/crates/eli-core/src/finance/timeseries/fetch/stooq.rs`
- Ticker format: `STOOQ:AAPL` → `aapl.us`, `STOOQ:^spx` → `^spx`, `STOOQ:AAPL_PE` → `aapl_pe.us`
- Coverage: US equities (back to 1990), indices (1950), forex, gold, PE ratios
- NOT supported: commodity futures (cl.f returns empty), Treasury yields, DXY
- Rate limit: 500ms between requests (conservative)
- Tests passed:
  - AAPL 1yr: 250 candles, $223→$251 ✓
  - SPY 10yr: 2512 candles, $175→$655 ✓
  - AAPL_PE: 250 candles, PE 31.98→31.80 ✓
  - ^SPX index: 21 candles ✓
  - eurusd forex: works ✓

### Binance Provider (binance.rs)
- File: `eli/crates/eli-core/src/finance/timeseries/fetch/binance.rs`
- Uses Binance.us (not geo-blocked from US)
- Ticker format: `BN:BTC` → `BTCUSD`, `BN:ETHUSDT` → `ETHUSDT`
- Pagination: up to 50 pages × 1000 candles = 50,000 candles max
- All Binance native intervals supported (1m through 1M)
- Symbol fix: bare tickers (BTC, ETH, SOL) get USD appended; full pairs (BTCUSDT) used as-is
- Tests passed:
  - BTC 90d: 90 candles, $87K→$71K ✓
  - ETH 90d: 90 candles ✓
  - SOL 90d: 90 candles ✓

### Mixed Provider Routing
- SPY (Yahoo) + STOOQ:AAPL_PE (Stooq) + BN:BTC (Binance) in one call: 3 series returned ✓
- Auto-detection by prefix: STOOQ: → Stooq, BN: → Binance, PYTH: → Pyth, else → Yahoo/FRED

### Infrastructure Changes
- Added ProviderKind::Stooq, Binance, Eia, Ecb to eli-finance-types
- Added stooq.rs, binance.rs to timeseries fetch module
- Updated CLI timeseries.rs: new TimeseriesTickerBucket variants, routing logic, merge blocks
- Updated snapshot fetch.rs: exhaustive match for new provider kinds
- All 96 tests pass, 0 failures

---

## Iteration 2 — next: EIA + ECB SDMX

### EIA Plan
- New standalone tool: `eli finance eia`
- Endpoints: petroleum inventories (crude, gasoline, distillate), natural gas storage
- Requires EIA_API_KEY env var
- Series: WCESTUS1 (crude ex-SPR), WDISTUS1 (distillate), Lower 48 nat gas
- Returns weekly data with period, value, units, product_name

### ECB SDMX Plan
- New standalone tool: `eli finance ecb`
- Presets: eurusd, estr, m3, balance_sheet, euribor, yield_curve
- No auth required
- CSV format with `detail=dataonly` for minimal payload
- One Rust SDMX CSV parser module, reusable for BIS/BOJ/BOE later
