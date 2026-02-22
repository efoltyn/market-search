### `eli web read`

Fetch and extract readable content from one or many URLs with structured reliability diagnostics.

```bash
eli web read --url "https://www.reuters.com/article/..."
eli web read --url "https://a.com/x,https://b.com/y" --max-parallel 8
eli web read --urls-file urls.txt --max-parallel 6
eli web read --url "https://docs.rs/spider/latest/spider/" --max-chars 1600   # compact text budget
eli web read --url "https://docs.rs/spider/latest/spider/" --full              # full text payload
```

**Returns:**
- Single URL: `WebReadResponse`
  - `url`, `final_url`, `title`, `text`
  - `fetch_status` (`success|partial|blocked|error`)
  - `blocked_reason`
  - `attempts[]` (fetch + extraction diagnostics)
- Batch mode: `WebReadBatchResponse`
  - aggregate counts + `results[]` per URL

By default, CLI output is compact for token efficiency:
- text is truncated to `--max-chars` (default `2400`)
- includes `text_chars_total` + `text_truncated`
- includes failed-attempt diagnostics
Use `--full` for full text + full attempt details.

**Works well on:**
- News articles
- Blog posts
- Static documentation
- SEC filings (HTML versions)

**Fallback stack:**
1. Primary HTTP fetch
2. Retry on transient failures
3. Readability extraction
4. Semantic fallback extraction (`main/article/body`)

**Known blocked reasons:**
- `auth_required`, `forbidden`, `rate_limited`, `captcha_or_bot_challenge`
- `not_found`, `legal_restriction`, `timeout`, `network_error`, `server_error`, `empty_or_js_rendered`

**Tip:** Use `eli web search --probe-top` to attach read diagnostics directly to search results before ingesting full article text.
