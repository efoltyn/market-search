### `eli web search`

Agent-grade evidence ingestion search with deterministic filtering, scoring, and diagnostics.

```bash
eli web search --query "fed decision" --mode news --top 10
eli web search --query "yen intervention" --domains reuters.com,bloomberg.com --recency week
eli web search --query "fomc preview" --since 2026-01-01 --until 2026-01-31 --probe-top 3
eli web search --query "usd weakness" --track-key usd-daily
eli web search --query "spider docs" --mode tech --full   # include verbose snippets + all score fields
```

**Returns:**
- `query`, `mode`, `generated_at`
- `providers[]` fetch diagnostics per backend
- `items[]` ranked records with `scores` and optional `read_probe`
- `stats` counts after dedupe/filter/probe
- `run_delta` when `--track-key` is set

By default, CLI output is compact for token efficiency (keeps ranking/probe essentials).
Use `--full` for verbose payloads.

**Power uses:**
- Deterministic evidence retrieval for downstream agents
- Strict domain/time filtering for reproducible runs
- Probe top URLs for blocked/readability status without full article ingest
- Track URL/rank changes between runs (`run_delta`)

**When to use web search:**
- Need reproducible ingestion, not generic browsing
- Need source filtering and run-to-run comparability
- Need diagnostics attached to search hits

**When NOT to use:**
- Stock prices → use `snapshot` or `timeseries`
- Event odds → use `odds` (once you have the ticker)
- Economic data → use `macro`
