### `eli web crawl`

Crawl a website and extract content from all pages.

```bash
eli web crawl --url "https://investor.apple.com" --max-pages 20
```

**Options:**
- `--max-pages`: Limit pages (default: 50)
- `--subdomains`: Include subdomains
- `--sitemap`: Crawl via sitemap discovery
- `--smart`: HTTP-first crawl with JS rendering fallback (for JS-heavy docs)
- `--view`: `summary` (default), `raw`, or `path`
- `--save`: `auto` (default) or `off` when `--out` is omitted
- `--out`: Save to file

**Returns:**
- `pages[]` with `url`, `title`, `text_preview`, `links_found`
- `pages_crawled`, `duration_ms`

**Works well on:**
- Static HTML sites
- News sites, blogs
- Investor relations pages
- Documentation (if server-rendered)

For JS-heavy docs (React/Next.js/Mintlify), use `--smart`.
Default behavior now auto-saves full raw output while showing a compact terminal summary.
