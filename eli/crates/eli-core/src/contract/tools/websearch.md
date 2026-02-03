### `eli web search`

Search the web via DuckDuckGo.

```bash
eli web search --query "kalshi greenland prediction market"
eli web search --query "NVDA earnings January 2026"
```

**Returns:**
- `hits[]` with `title`, `url`, `snippet`, `source`, `score`

**Power uses:**
- Find prediction market tickers (Kalshi series like KXGREENLAND)
- Get recent news on a topic
- Find primary sources for claims
- Discover URLs to read with `eli web read`

**When to use web search:**
- No structured data exists for the topic
- Need to find a specific document/page
- Discovering prediction market tickers (API search is unreliable)

**When NOT to use:**
- Stock prices → use `snapshot` or `timeseries`
- Event odds → use `odds` (once you have the ticker)
- Economic data → use `macro`
