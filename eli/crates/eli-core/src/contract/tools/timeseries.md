### `eli finance timeseries`

OHLCV candles for one or more tickers.

```bash
eli finance timeseries --tickers AAPL,MSFT --range 5d --granularity 1d
eli finance timeseries --tickers NVDA --range 1y --granularity 1mo
eli finance timeseries --tickers BTC-USD --range 30d --granularity 1h
eli finance timeseries --tickers AAPL --range 1mo --granularity 1h --provider ibkr --ibkr-account U1234567
```

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
