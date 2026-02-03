### `eli web crawl`

Crawl a website and extract content from all pages.

```bash
eli web crawl --url "https://investor.apple.com" --max-pages 20
```

**Options:**
- `--max-pages`: Limit pages (default: 50)
- `--subdomains`: Include subdomains
- `--out`: Save to file

**Returns:**
- `pages[]` with `url`, `title`, `text_preview`, `links_found`
- `pages_crawled`, `duration_ms`

**Works well on:**
- Static HTML sites
- News sites, blogs
- Investor relations pages
- Documentation (if server-rendered)

**Limitation:** JS-heavy sites (React, Next.js, Mintlify) return JavaScript garbage instead of content. For those, try `eli web search` to find the right page, then read specific URLs.
