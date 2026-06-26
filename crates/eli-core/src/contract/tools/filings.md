### `eli finance filings`

Download recent SEC filings with direct SEC URLs and local raw-file paths. The tool does not parse or summarize filing text.

```bash
eli finance filings --ticker AAPL --limit 5
eli finance filings --ticker AAPL --single-file
eli finance filings --ticker AAPL --limit 5 --primary-only
eli finance filings --ticker AAPL --forms 10-K,10-Q --limit 3 --download-all
eli finance filings --ticker AAPL --forms 10-K --limit 1 --raw-text --max-chars 20000
eli finance filings --ticker AAPL --user-agent "eli-cli (mailto:me@example.com)"
```

**Returns per filing:**
- `form`: 8-K, 10-K, 10-Q
- `filing_date`, `report_date`
- `url`: Direct link to SEC filing
- `primary_doc_path`, `index_json_path`: Local raw-file paths
- `downloaded_files`: Local paths for index JSON, primary filing, and filtered useful exhibits by default. Use `--primary-only` to skip exhibits, or `--download-all` for every attachment.
- `raw_text`: Present only with `--raw-text`; contains the primary document's raw decoded text inline. Use `--max-chars` to cap response size.
- `raw_text_bytes`, `raw_text_truncated`: Source byte count and cap indicator for inline raw text.
- `--single-file`: Downloads only the primary document for the latest matching filing and skips index/exhibits.
- `items`: For 8-K, shows item codes (e.g., "2.02,9.01" = earnings + exhibits)

**8-K item codes:**
- 2.02 = Results of Operations (earnings)
- 5.02 = Departure/Election of Directors
- 8.01 = Other Events
- 9.01 = Financial Statements and Exhibits

**Combine with:** `eli web read --url <filing_url>` to get the actual content.
