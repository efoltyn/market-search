### `eli web read`

Fetch and extract readable content from a URL.

```bash
eli web read --url "https://www.reuters.com/article/..."
```

**Returns:**
- `title`: Page title
- `text`: Extracted article/main content

**Works well on:**
- News articles
- Blog posts
- Static documentation
- SEC filings (HTML versions)

**Limitation:** JS-rendered sites (React SPAs) may return empty or garbage. For those, the content isn't in the initial HTML.

**Tip:** Some sites block bots (403 Forbidden). Try a different source or use `eli web search` to find alternatives.
