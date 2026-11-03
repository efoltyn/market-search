### `eli finance filings`

SEC filings by ticker: metadata and SEC URLs, local raw files, and precise reading of one filing by section or as structured numbers.

**Read a filing precisely (TOC, then sections):**

```bash
eli finance filings --ticker AMZN --forms 10-Q --toc                     # every section with its size in chars
eli finance filings --ticker AMZN --forms 10-Q --section cash_flow       # exactly the cash flow statement
eli finance filings --ticker INTC --forms 10-Q --section "note:goodwill,mda"
eli finance filings --ticker AAPL --forms 8-K --section ex99.1           # the earnings press release
eli finance filings --ticker AAPL --forms 10-K --section notes           # all notes to the financial statements
eli finance filings --ticker AMZN --forms 10-Q --xbrl                    # numbers with qoq / yoy changes
eli finance filings --ticker INTC --forms 10-Q --xbrl --xbrl-concepts Inventory,Goodwill
eli finance filings --ticker AAPL --accession 0000320193-26-000024 --toc # a specific filing
```

- `--toc`: one cheap call. Returns `toc[]` = `{id, title, kind, level, parent, chars}` in document order: 10-K/10-Q items (`item1a`, `item7`, `part1_item2`, `part2_item1a`, ...), each primary statement (`income_statement`, `comprehensive_income`, `balance_sheet`, `cash_flow`, `equity`), the `notes` container and every numbered note under its own title (`note:5` = "Note 5. Inventories"), `auditor_report`, 8-K items (`item2.02`), and exhibits (`ex99.1`; 8-K exhibits carry `chars`, 10-K/10-Q exhibits raw `bytes`). Filers that drop "Item N." headings (Intel, JPMorgan) are mapped from their own table of contents; `structure_note` says which method was used.
- `--section <ids>`: comma-separated TOC ids or aliases. Returns `sections[]` = `{requested, id, title, chars, text}` with exactly that text and nothing else; no cap unless `--max-chars` is given. Aliases: `mda`, `risk_factors`, `business`, `legal_proceedings`, `market_risk`, `controls`, `financial_statements`, `notes`, `note:<number or words>` (`note:inventories`, `note:7`), `auditor_report`, `exhibits`, `press_release`, `document` (whole filing). Unknown ids return an `error` listing the available ids.
- Text is clean: paragraphs one per line, tables as aligned rows with ` | ` between columns (`$`, `(`, `)` and `%` fragments merged into their numbers; multi-line column headers kept), page numbers, running headers/footers and "Table of Contents" back-links removed.
- `--xbrl`: `xbrl.facts[]` = `{concept, label, section, dims, period, unit, value, qoq, yoy, vs_fye}` from the inline XBRL in the primary document. `label` is the filing's own row label, `section` the TOC id the number sits in (fetch its prose with `--section`), `period` like `3M ended 2026-09-30` or `at 2026-09-30`, `unit` like `USD millions` (values shown at the filing's scale, with the sign as printed). Comparisons `{period, value, change, pct, from}` come from the filing's own columns, else the previous 10-Q/10-K's inline XBRL, else SEC companyfacts (`from` names the source; a Q4 never reported alone is derived as FY minus 9M YTD). Default selection: every line item of the primary statements (balance at period end; flows for the quarter where reported, else year-to-date) plus up to 25 facts elsewhere that moved at least `--xbrl-min-change` percent (default 25). `--xbrl-concepts` (substrings of concept or label; `*` = all) returns every matching fact, all periods.
- `--accession`: read one specific filing; works beyond the submissions window via the filing's own SGML header.
- toc / section / xbrl read one filing (`--limit 1`) unless `--limit` is set, and cache the parsed documents under the cache dir (filings are immutable).

**List / download (unchanged):**

```bash
eli finance filings --ticker AAPL --limit 5
eli finance filings --ticker AAPL --single-file
eli finance filings --ticker AAPL --limit 5 --primary-only
eli finance filings --ticker AAPL --forms 10-K,10-Q --limit 3 --download-all
eli finance filings --ticker AAPL --forms 10-K --limit 1 --raw-text --max-chars 20000
eli finance filings --ticker AAPL --forms 8-K --limit 1 --press-release-text
eli finance filings --search-text "liquid cooling" --forms 8-K --from 2026-10-01
eli finance filings --ticker AAPL --user-agent "eli-cli (mailto:me@example.com)"
```

**Returns per filing:**
- `form`, `filing_date`, `report_date`, `accession_number`, `acceptance_datetime`
- `url`: Direct link to the primary document; `filing_index_url`
- `primary_doc_path`, `index_json_path`, `downloaded_files`: Local raw-file paths (index JSON, primary filing, filtered useful exhibits by default; `--primary-only` skips exhibits, `--download-all` takes every attachment)
- `raw_text` (`--raw-text`): the primary document as clean text inline; `raw_text_bytes`, `raw_text_truncated` with `--max-chars`
- `press_release_text` (`--press-release-text`): EX-99.1 for earnings 8-Ks, the document body for 10-Q/10-K
- `toc`, `sections`, `xbrl`, `structure_note`: see above
- `--single-file`: Downloads only the primary document for the latest matching filing and skips index/exhibits.
- `items`: For 8-K, item codes (e.g. "2.02,9.01" = earnings + exhibits)

**8-K item codes:**
- 2.02 = Results of Operations (earnings)
- 5.02 = Departure/Election of Directors
- 8.01 = Other Events
- 9.01 = Financial Statements and Exhibits

**Combine with:** `eli web read --url <filing_url>` for documents outside EDGAR.
