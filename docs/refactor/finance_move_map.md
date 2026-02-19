# Finance Move Map

Source: `crates/eli-core/src/finance/mod.rs` (`6,936` LOC)

## Provider Odds

Move to `crates/eli-core/src/finance/providers/odds_kalshi.rs`

- `fetch_odds_kalshi`

Move to `crates/eli-core/src/finance/providers/odds_polymarket.rs`

- `fetch_odds_polymarket`
- `fetch_polymarket_books_ws`

Move to `crates/eli-core/src/finance/providers/odds_common.rs`

- `json_value_to_string`
- `parse_json_array_strings`
- `parse_json_value_strings`
- `parse_probability`
- `probability_yes_from_outcomes`
- `build_odds_analytics`
- `build_odds_analytics_from_listed`

## Public Odds Entry

Move to `crates/eli-core/src/finance/odds/fetch.rs`

- `fetch_odds`

## Timeseries

Move to `crates/eli-core/src/finance/timeseries/fetch.rs`

- `fetch_timeseries`

Move to `crates/eli-core/src/finance/timeseries/analytics.rs`

- `build_timeseries_analytics`
- `correlation`

Move to `crates/eli-core/src/finance/timeseries/resample.rs`

- `resample_candles`
- `aggregate_bucket`
- `round_4`

Move to `crates/eli-core/src/finance/timeseries/mock.rs`

- `generate_mock_series`
- `generate_mock_candles`
- `seed_from_str`
- `base_price_from_seed`
- `generate_mock_snapshots`

Move to `crates/eli-core/src/finance/timeseries/yahoo.rs`

- `fetch_yahoo_snapshots`
- `yahoo_alias_ticker`
- `fetch_yahoo_series`
- `yahoo_fetch_quotes_retry`
- `yahoo_base_interval`

Move to `crates/eli-core/src/finance/timeseries/fred.rs`

- `fetch_fred_series`

## Snapshot/Prices/Fundamentals/Search

Move to `crates/eli-core/src/finance/snapshot/fetch.rs`

- `fetch_snapshot`
- `build_snapshot_analytics`

Move to `crates/eli-core/src/finance/prices/fetch.rs`

- `fetch_prices`

Move to `crates/eli-core/src/finance/fundamentals/fetch.rs`

- `fetch_fundamentals`

Move to `crates/eli-core/src/finance/search/fetch.rs`

- `fetch_search`

## Options

Move to `crates/eli-core/src/finance/options/fetch.rs`

- `fetch_options`

## Filings + SEC

Move to `crates/eli-core/src/finance/filings/fetch.rs`

- `fetch_filings`

Move to `crates/eli-core/src/finance/filings/sec_client.rs`

- `sec_user_agent`
- `sec_client`
- `sec_get_json`
- `sec_get_text`

Move to `crates/eli-core/src/finance/filings/sec_lookup.rs`

- `sec_lookup_cik`
- `sec_fetch_submissions`

Move to `crates/eli-core/src/finance/filings/excerpt.rs`

- `sanitize_for_filename`
- `truncate_chars`
- `best_effort_sec_filing_excerpt`
- `html_to_text`

Move to `crates/eli-core/src/finance/filings/cache.rs`

- `cache_key`
- `cache_path`
- `file_is_fresh`

## Insider

Move to `crates/eli-core/src/finance/insider/fetch.rs`

- `fetch_insider`

Move to `crates/eli-core/src/finance/insider/form4.rs`

- `fetch_form4_xml`
- `parse_form4_xml`
- `parse_single_transaction`
- `find_form4_xml_from_index`
- `extract_xml_tag`

## News/Macro/Schedule

Move to `crates/eli-core/src/finance/news/fetch.rs`

- `fetch_news`

Move to `crates/eli-core/src/finance/macro_data/fetch.rs`

- `fetch_macro`
- `fetch_fred_macro_counts`
- `fetch_fred_macro_for_day`

Move to `crates/eli-core/src/finance/schedule/fetch.rs`

- `fetch_schedule`

Move to `crates/eli-core/src/finance/schedule/nasdaq.rs`

- `parse_schedule_date`
- `collapse_ws`
- `parse_nasdaq_time`
- `fetch_nasdaq_earnings_for_date`

## Utilities

Move to `crates/eli-core/src/finance/util/debug.rs`

- `debug_dir`
- `write_debug_payload`

Move to `crates/eli-core/src/finance/util/math.rs`

- `periods_per_year`
- `default_risk_free_rate_annual`
