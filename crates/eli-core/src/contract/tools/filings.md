### `eli finance filings`

SEC filings by ticker: metadata and SEC URLs, local raw files, and precise reading of one filing by section or as structured numbers.

**Read a filing precisely (TOC, then sections):**

```bash
eli finance filings --ticker AMZN --forms 10-Q --toc                     # every section with its size in chars
eli finance filings --ticker AMZN --forms 10-Q --section cash_flow       # exactly the cash flow statement
eli finance filings --ticker INTC --forms 10-Q --section "note:goodwill,mda"
eli finance filings --ticker AAPL --forms 8-K --section ex99.1           # the earnings press release
eli finance filings --ticker AAPL --forms 8-K --section cash_flow        # the cash flow table inside it
eli finance filings --ticker AAPL --forms 8-K --xbrl                     # its tables as numbers, earnings night
eli finance filings --ticker AAPL --forms 10-K --section notes           # all notes to the financial statements
eli finance filings --ticker AMZN --forms 10-Q --xbrl                    # numbers with qoq / yoy changes
eli finance filings --ticker INTC --forms 10-Q --xbrl --xbrl-concepts Inventory,Goodwill
eli finance filings --ticker AAPL --accession 0000320193-26-000024 --toc # a specific filing
eli finance filings --ticker AAPL --forms 10-Q --section risk_factors,mda --diff   # what changed vs the previous filing
eli finance filings --ticker AAPL --accession 0000320193-26-000020 --section risk_factors --diff-against 0000320193-26-000013
```

- `--toc`: one cheap call. Returns `toc[]` = `{id, title, kind, level, parent, chars}` in document order: 10-K/10-Q items (`item1a`, `item7`, `part1_item2`, `part2_item1a`, ...), each primary statement (`income_statement`, `comprehensive_income`, `balance_sheet`, `cash_flow`, `equity`), the `notes` container and every numbered note under its own title (`note:5` = "Note 5. Inventories"), `auditor_report`, 8-K items (`item2.02`), and exhibits (`ex99.1`; 8-K exhibits carry `chars`, 10-K/10-Q exhibits raw `bytes`). On an 8-K / 6-K, the tables inside each EX-99 exhibit follow it at level 2: the statements (`ex99.1:income_statement`, `ex99.1:balance_sheet`, `ex99.1:cash_flow`, kind `statement`) and other titled tables (`ex99.1:segment_information`, kind `subsection`), each `chars` = exactly what `--section` returns (title, units line, table). Filers that drop "Item N." headings (Intel, JPMorgan) are mapped from their own table of contents; `structure_note` says which method was used.
- `--section <ids>`: comma-separated TOC ids or aliases. Returns `sections[]` = `{requested, id, title, chars, text}` with exactly that text and nothing else; no cap unless `--max-chars` is given. Aliases: `mda`, `risk_factors`, `business`, `legal_proceedings`, `market_risk`, `controls`, `financial_statements`, `notes`, `note:<number or words>` (`note:inventories`, `note:7`), `auditor_report`, `exhibits`, `press_release`, `document` (whole filing). On an 8-K, `cash_flow`, `balance_sheet`, `income_statement` reach the statement inside the earnings release; `ex99.1:cash_flow` names it explicitly. Unknown ids return an `error` listing the available ids.
- Text is clean: paragraphs one per line, tables as aligned rows with ` | ` between columns (`$`, `(`, `)` and `%` fragments merged into their numbers; multi-line column headers kept), page numbers, running headers/footers and "Table of Contents" back-links removed.
- `--xbrl`: `xbrl.facts[]` = `{concept, label, section, dims, period, unit, value, qoq, yoy, vs_fye}` from the inline XBRL in the primary document. `label` is the filing's own row label, `section` the TOC id the number sits in (fetch its prose with `--section`), `period` like `3M ended 2026-09-30` or `at 2026-09-30`, `unit` like `USD millions` (values shown at the filing's scale, with the sign as printed). Comparisons `{period, value, change, pct, from}` come from the filing's own columns, else the previous 10-Q/10-K's inline XBRL, else SEC companyfacts (`from` names the source; a Q4 never reported alone is derived as FY minus 9M YTD). Default selection: every line item of the primary statements (balance at period end; flows for the quarter where reported, else year-to-date) plus up to 25 facts elsewhere that moved at least `--xbrl-min-change` percent (default 25). `--xbrl-concepts` (substrings of concept or label; `*` = all) returns every matching fact, all periods.
- `--xbrl` on an earnings 8-K / 6-K: the press release is the only document out that night and has no XBRL, so its tables come back as data in the same shape, `source` "press release EX-99.1 tables". Each row of each statement: `label` as printed (with its heading in brackets when the label repeats, e.g. `Products [Cost of sales]`), `section` the table id, `period` from the column headers (`3M ended 2026-09-26`, `12M ended ...`, `at 2026-09-26`, `Q3 2026`), `unit` from the caption (`USD millions`, `USD per share`, `shares thousands`), `value` exactly as printed (parentheses = negative). `concept` is `release:<label>`, never a us-gaap tag. Comparisons only between printed values: `yoy` and `vs_fye` from the release's other columns, `qoq` from the same table and row label (case, spacing, punctuation and footnote markers folded) in the company's previous earnings release (`from` = "8-K 2026-07-30"); when there is no confident match, `missing` says why. Rows of other tables (segments, products, reconciliations) that moved at least `--xbrl-min-change` percent are added as movers. A table whose columns can't be read (no printed span, repeated periods) is left out and named in `notes`; read it with `--section`.
- `--diff` (with `--section`): each section compared with the same section of the previous filing of the same form; `sections[].diff` replaces `text` (call again without `--diff` for the text). Prose paragraphs only (tables are numbers; `--xbrl` compares those). Every string returned is exact text from one of the two filings:
  - `base` `{form, filing_date, report_date, accession_number, section_id, section_title}`; items match by id, notes and subsections by title (note numbers shift).
  - `added[]` / `removed[]` `{under, text}`: paragraphs none of whose sentences appear in the other filing. `under` = the nearest subheading.
  - `changed[]` `{under, at, added_sentences, removed_sentences}`: paragraphs that differ by whole sentences; `at` = the paragraph's opening words. A sentence counts as added only if no sentence anywhere in the base section matches it, so moved text is not reported.
  - `figures_only` (+ `figures_only_under`): paragraphs whose only differences are numbers, dates, months and period words ("third quarter", "nine months"); counted, not printed. `moved`: paragraphs split, merged or reordered with no new sentence. `unchanged`, `paragraphs` [this, base], `chars` (size of the diff).
  - Matching ignores whitespace, case, curly quotes, dashes and trademark signs; sentences split around abbreviations (U.S., Inc., No., e.g.).
  - 10-Q risk factors: compared with the 10-K when the 10-K is newer than the previous 10-Q, or when the 10-Q's section refers to the annual report and is under half its size (it lists only updates); then 10-K paragraphs it does not restate are counted in `not_restated`, not listed. `note` says which. `--diff-against <accession>` picks the base filing.
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
- `toc`, `sections` (with `diff` under `--diff`), `xbrl`, `structure_note`: see above
- `--single-file`: Downloads only the primary document for the latest matching filing and skips index/exhibits.
- `items`: For 8-K, item codes (e.g. "2.02,9.01" = earnings + exhibits)

**8-K item codes:**
- 2.02 = Results of Operations (earnings)
- 5.02 = Departure/Election of Directors
- 8.01 = Other Events
- 9.01 = Financial Statements and Exhibits

**Combine with:** `eli web read --url <filing_url>` for documents outside EDGAR.
