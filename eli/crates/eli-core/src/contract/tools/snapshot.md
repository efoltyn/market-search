### `eli finance snapshot`

Current price, market cap, shares, EV for one or more tickers.

```bash
eli finance snapshot --tickers AAPL,MSFT,GOOGL
eli finance snapshot --tickers SPY,QQQ,IWM --returns 1mo,3mo,1y
eli finance snapshot --tickers AAPL,MSFT --provider ibkr --ibkr-account U1234567
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

**Optional trend block:**
- `trailing_returns` - nested map shape `ticker -> period -> decimal_return`
- Enabled with `--returns` and supported windows: `1mo,3mo,6mo,1y`

**Use cases:**
- Compare valuations across tickers
- Get current price for any stock
- Calculate portfolio weights
- Rank stocks by daily performance
