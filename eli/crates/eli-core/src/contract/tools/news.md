### `eli finance news`

Dated headlines for a ticker.

```bash
eli finance news --ticker NVDA --date 2026-01-31
```

**Returns:** Array of headlines with title, link, and timestamp.

**What you get:** headlines around that date with publication timestamps so you know the sequence of events.

**Caveat:** ambiguous tickers are noisy, and even obvious liquid tickers can occasionally return sparse results. Use `web search` as the narrative fallback when the feed is thin.

**Combine with:** `timeseries` (see the move), `web search` (dig into a specific headline), `snapshot` (current state).
