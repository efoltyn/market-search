### `eli finance snapshot`

Current price, market cap, shares, EV for one or more tickers.

```bash
eli finance snapshot --tickers AAPL,MSFT,GOOGL
```

**Returns per ticker:**
- `current_price`, `previous_close`, `open`, `day_low`, `day_high`
- `market_cap`, `enterprise_value`
- `shares_outstanding`, `float_shares`
- `last_split_factor`, `last_split_date`

**Analytics block (multi-ticker):**
- `market_cap_weights` - each ticker's weight in the set
- `daily_returns` - today's return per ticker
- `relative_strength` - each ticker vs the group mean

**Use cases:**
- Compare valuations across tickers
- Get current price for any stock
- Calculate portfolio weights
- Rank stocks by daily performance
