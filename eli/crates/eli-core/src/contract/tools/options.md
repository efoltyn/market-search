### `eli finance options`

Options chains with IV, bid/ask, volume, open interest.

```bash
eli finance options --ticker AAPL
eli finance options --ticker NVDA --expiry 2026-03-20
eli finance options --ticker SPY --provider ibkr --ibkr-account U1234567
```

**Returns:**
- `underlying_price`
- `expirations[]`: All available expiry dates
- `calls[]` and `puts[]` with:
  - `strike`, `bid`, `ask`, `last`
  - `volume`, `open_interest`
  - `implied_volatility`
  - `in_the_money`

**Use cases:**
- Find IV for a strike
- See where volume is concentrated
- Identify unusual options activity
- Get all available expirations

**Provider notes:**
- Default provider is Yahoo.
- `--provider ibkr` uses native TWS / IB Gateway data and is preferable when you have an IBKR account connected locally.
- IBKR support currently covers expiry discovery, near-money chain reconstruction, IV, volume, open interest, and summary metrics.

**Note:** OPTIONS ≠ ODDS. Options are derivatives contracts. Odds are prediction markets.
