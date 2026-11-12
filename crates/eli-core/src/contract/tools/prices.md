### Live prices around the clock

There is no separate prices command. For the current price of crypto, gold, silver, oil, natural gas, copper, the Nasdaq-100 (Kraken's US Tech 100 index) or EUR and GBP at any hour, pull one-minute candles from Kraken Futures (no key):

```bash
eli finance timeseries --tickers KF:BTC,KF:ETH,KF:GOLD,KF:OIL --range 1h --granularity 1m
```

The last candle is the current minute. `KF:<NAME>` is the market's index price; `KF:<NAME>:TRADE` is the traded perpetual, with volume.

`PYTH:<NAME>` still works and returns the same Kraken Futures index: Pyth ended keyless access on 2026-08-26.

**Also open at all hours:**
- `HL:xyz:SP500`, `HL:xyz:NVDA`: Hyperliquid perps on indexes and US stocks
- `BN:BTCUSDT`: Binance spot candles with volume

**For stocks:** `eli finance timeseries --tickers AAPL --range 1d --granularity 1m`; intraday series include pre-market and after-hours bars.
