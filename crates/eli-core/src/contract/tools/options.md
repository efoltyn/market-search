### `eli finance options`

Options chains with IV, bid/ask, volume, open interest.

```bash
eli finance options --ticker AAPL
eli finance options --ticker NVDA --expiry 2026-03-20
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

**Note:** OPTIONS ≠ ODDS. Options are derivatives contracts. Odds are prediction markets.
