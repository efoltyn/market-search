### `eli finance filings`

Download recent SEC filings with direct SEC URLs and local raw-file paths. The tool does not parse or summarize filing text.

```bash
eli finance filings --ticker AAPL --limit 5
eli finance filings --ticker AAPL --forms 10-K,10-Q --limit 3 --download-all
eli finance filings --ticker AAPL --user-agent "eli-cli (mailto:me@example.com)"
```

**Returns per filing:**
- `form`: 8-K, 10-K, 10-Q
- `filing_date`, `report_date`
- `url`: Direct link to SEC filing
- `items`: For 8-K, shows item codes (e.g., "2.02,9.01" = earnings + exhibits)

**8-K item codes:**
- 2.02 = Results of Operations (earnings)
- 5.02 = Departure/Election of Directors
- 8.01 = Other Events
- 9.01 = Financial Statements and Exhibits

**Combine with:** `eli web read --url <filing_url>` to get the actual content.
