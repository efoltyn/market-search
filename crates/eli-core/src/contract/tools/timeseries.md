### `eli finance timeseries`

OHLCV candles for one or more tickers.

```bash
eli finance timeseries --tickers AAPL,MSFT --range 5d --granularity 1d
eli finance timeseries --tickers NVDA --range 1y --granularity 1mo
eli finance timeseries --tickers BTC-USD --range 30d --granularity 1h
eli finance timeseries --tickers GOVT:FR10Y,GOVT:DE10Y,GOVT:US10Y --range 5d --granularity 1h   # sovereign yields (%)
eli finance timeseries --preset sovereigns --range 1y --granularity 1d
eli finance timeseries --tickers AAPL --range 1mo --granularity 1h --provider ibkr --ibkr-account U1234567
```

**Sovereign yields (`GOVT:`):** country code + tenor (`GOVT:US10Y`, `GOVT:FR2Y`, `GOVT:JP30Y`, `GOVT:IT10Y`; raw `GOVT:FR10Y-FR` also accepted). Source CNBC, no key. Candle values are yields in percent. Daily back to 2000 (stamped 00:00 UTC of the trade date); intraday 1m/5m/30m/1h reliably for ~the last year, patchy before.

**Plumbing / credit / leverage prefixes (no key; mix freely with everything else):**

```bash
eli finance timeseries --tickers "PNAV:OBDC,BOND:21873SAG3:YLD,NYFED:SOFR_P99,OFR:FSI,ETF:JNK:FLOW,CBOE:VXCONTANGO" --range 3mo --granularity 1d
eli finance timeseries --preset stress_plumbing --range 1mo --granularity 1d
eli finance bonds --issuer "ORACLE CORP"     # list an issuer's CUSIPs (exact FINRA name) to feed BOND:
```

| Prefix | Grammar / examples | Source | Caveats |
|---|---|---|---|
| `NAV:` `PNAV:` | `NAV:OBDC` (NAV/share), `PNAV:FSK` (price ÷ latest NAV) | SEC XBRL | NAV stamped 00:00 UTC of the FILED date (no look-ahead). Needs `ELI_SEC_USER_AGENT` or config `sec_user_agent`. Quarterly/semiannual steps. |
| `BOND:` | `BOND:<CUSIP\|FINRA symbol>[:PX\|:YLD]` e.g. `BOND:21873SAG3:YLD`, `BOND:68389XEC5:PX` | FINRA TRACE | ≥1d = end-of-day last sale (~5y); <1d = trade OHLC (~1y, 15-min delay, size capped so `v` is a floor). |
| `BUYBACK:` | `BUYBACK:<20-30Y\|10-20Y\|…\|TIPS-1-10Y\|ALL\|LIQ\|CASH>[:OFFERED\|ACCEPTED\|MAX\|COVER\|FILL\|ISSUES]` e.g. `BUYBACK:20-30Y:COVER` | Treasury Fiscal Data | One point per operation at op close (ET→UTC); sums within a bucket. |
| `NYFED:` | `NYFED:SRF`, `NYFED:RRP`, `NYFED:SOFR_P99`, `NYFED:EFFR`, `NYFED:TGCR_VOL` | NY Fed Markets API | Rates stamped at next-business-day PUBLICATION (~08:00/09:00 ET), one day after FRED's date. |
| `OFR:` | `OFR:FSI` (+ `_CREDIT`/`_FUNDING`/…), `OFR:REPO-DVP_OV_TOT-P`, `OFR:FICC-SPONSORED_REPO_VOL` | OFR | Stamped at OBSERVATION date, released T+1 or later (FPF months later): no same-day causality. Raw units (dollars, %). |
| `ETF:` | `ETF:<TICKER>[:SO\|NAV\|AUM\|FLOW\|FLOW5\|FLOW20]` e.g. `ETF:JNK:FLOW`, `ETF:SPY:SO` | SSGA, ProShares, iShares, Direxion | DAILY regardless of granularity (flows summed). FLOW in $mn. iShares/Direxion are snapshot-only (one point). |
| `CBOE:` | `CBOE:VX1`, `CBOE:VX2-VX1`, `CBOE:VXCONTANGO`, `CBOE:PCR_EQUITY`; snapshots `CBOE:GEX:SPX`, `ZEROGAMMA`, `PCR_OI`, `0DTE_SHARE`, `IV30`, `SPOT` | CBOE CDN | DAILY regardless of granularity. Chain snapshots are LIVE-ONLY (one point; error if as_of is >1h past). GEX uses a naive dealer-sign convention. |
| `HL:` | `HL:BTC:FUNDING_APR`, `HL:ETH:PX`, `HL:BTC:OI_USD` | Hyperliquid | OI / OI_USD LIVE-ONLY. Funding stamped at hourly settlement. |
| `DERIBIT:` | `DERIBIT:DVOL:BTC`, `DERIBIT:FUNDING_APR:BTC`, `DERIBIT:SKEW25:BTC:30D` | Deribit | SKEW25 LIVE-ONLY. Funding stamped at end of the hour it covers. |

**Prediction markets (Kalshi `KX*` / `KALSHI:<ticker>`, Polymarket numeric id):** Kalshi supports every granularity. `1m` is native; `5m`/`15m`/`30m` are resampled from 1m bars with the same semantics as Kalshi's native hourly bars: o/h/l/c come from trades in the bucket, `v` sums contracts, a bucket with no trade shows the yes bid/ask mid, and minutes with no activity carry the last close forward (`v: 0`). Sub-hour candles are stamped at bucket START; native `1h`/`1d` Kalshi bars are stamped at bar END. Kalshi caps a request at 10,000 candles, so long 1m windows are paged (max ~187 days of minutes per call). Kalshi tickers that don't start with `KX` need the prefix: `KALSHI:CONTROLS-2026-D`.

**Range/granularity units:**
- Minutes: `30min`, `1h`
- Days: `1d`, `5d`
- Months: `1mo`, `6mo`
- Years: `1y`, `5y`

**Returns per ticker:**
- `candles[]` with `t` (timestamp), `o`, `h`, `l`, `c`, `v`

**Analytics block (multi-ticker):**
- `stats` per ticker: `total_return`, `annualized_vol`, `sharpe_ratio`
- `correlation_matrix` between all tickers

**Power move:** One call with multiple tickers auto-aligns them. Use this for correlation analysis, relative performance, pairs trading research.
