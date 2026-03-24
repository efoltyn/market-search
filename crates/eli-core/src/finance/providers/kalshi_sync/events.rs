use serde::de::DeserializeOwned;

const KALSHI_EVENTS_PAGE_LIMIT: usize = 200;
const KALSHI_MARKETS_PAGE_LIMIT: usize = 1000;
const KALSHI_NON_SPORTS_CATEGORY_CONCURRENCY: usize = 8;
const KALSHI_NON_SPORTS_UNCATEGORIZED_EVENT_CONCURRENCY: usize = 6;
const KALSHI_MAX_RETRIES: usize = 6;

#[derive(Default, Clone, Copy)]
struct RetryMetrics {
    attempts: usize,
    retry_429: usize,
    retry_5xx: usize,
}

impl RetryMetrics {
    fn merge(&mut self, other: RetryMetrics) {
        self.attempts = self.attempts.saturating_add(other.attempts);
        self.retry_429 = self.retry_429.saturating_add(other.retry_429);
        self.retry_5xx = self.retry_5xx.saturating_add(other.retry_5xx);
    }
}

#[derive(Deserialize)]
struct EventsResp {
    #[serde(default)]
    events: Vec<EventRow>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Clone, Deserialize)]
struct EventRow {
    event_ticker: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    series_ticker: Option<String>,
}

#[derive(Deserialize)]
struct NestedEventsResp {
    #[serde(default)]
    events: Vec<NestedEventRow>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Clone, Deserialize)]
struct NestedEventRow {
    event_ticker: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    series_ticker: Option<String>,
    #[serde(default)]
    markets: Vec<NestedMarketRow>,
}

#[derive(Clone, Deserialize)]
struct NestedMarketRow {
    ticker: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    event_ticker: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    volume: Option<i64>,
    #[serde(default)]
    volume_24h: Option<i64>,
    #[serde(default)]
    yes_bid_dollars: Option<String>,
    #[serde(default)]
    yes_ask_dollars: Option<String>,
    #[serde(default)]
    last_price_dollars: Option<String>,
    #[serde(default)]
    yes_bid: Option<i64>,
    #[serde(default)]
    yes_ask: Option<i64>,
    #[serde(default, rename = "last_price", alias = "yes_price")]
    last_price: Option<i64>,
}

#[derive(Deserialize)]
struct MarketsResp {
    #[serde(default)]
    markets: Vec<MarketRow>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Deserialize)]
struct HistoricalCutoffResp {
    #[serde(default)]
    market_settled_ts: Option<String>,
}

#[derive(Clone, Deserialize)]
struct MarketRow {
    ticker: String,
    #[serde(default)]
    title: Option<String>,
    event_ticker: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    volume: Option<i64>,
    #[serde(default)]
    volume_24h: Option<i64>,
    #[serde(default)]
    yes_bid_dollars: Option<String>,
    #[serde(default)]
    yes_ask_dollars: Option<String>,
    #[serde(default)]
    last_price_dollars: Option<String>,
    #[serde(default)]
    yes_bid: Option<i64>,
    #[serde(default)]
    yes_ask: Option<i64>,
    #[serde(default, rename = "last_price", alias = "yes_price")]
    last_price: Option<i64>,
}

struct EventsFetchResult {
    rows: Vec<EventRow>,
    pages_fetched: usize,
    exhausted: bool,
    metrics: RetryMetrics,
}

struct MarketsFetchResult {
    pages_fetched: usize,
    exhausted: bool,
    metrics: RetryMetrics,
}

struct NestedSyncResult {
    events: Vec<OddsListedEvent>,
    markets: Vec<OddsListedMarket>,
    category_map: HashMap<String, Option<String>>,
    seen_market_tickers: HashSet<String>,
    pages_fetched: usize,
    exhausted: bool,
    metrics: RetryMetrics,
}

struct StreamRowsFetchResult {
    stream_key: String,
    rows: Vec<MarketRow>,
    pages_fetched: usize,
    exhausted: bool,
    metrics: RetryMetrics,
}

struct CategoryFilterProbeResult {
    supported: bool,
    metrics: RetryMetrics,
}

/// Sync events/markets from Kalshi with explicit dual-path ingestion:
///   - include_sports=true: global `/markets` cursor pagination (full breadth).
///   - include_sports=false:
///       * capped mode (`max_pages` set): single-pass `/events?with_nested_markets=true`
///         so one cursor walk yields both events and markets
///       * unbounded mode: parallel category streams + uncategorized event fallback
pub(crate) async fn sync_kalshi_events(
    limiter: &RateLimiter,
    max_pages: Option<usize>,
    include_sports: bool,
    include_historical: bool,
) -> std::result::Result<
    (
        Vec<OddsListedEvent>,
        Vec<OddsListedMarket>,
        OddsSyncCoverage,
    ),
    String,
> {
    eprintln!(
        "[kalshi] starting sync (max_pages={}, include_sports={}, include_historical={})",
        max_pages
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unbounded".to_string()),
        include_sports,
        include_historical
    );

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(StdDuration::from_secs(30))
        .tcp_nodelay(true)
        .build()
        .map_err(|e| format!("Kalshi client init failed: {e}"))?;

    let page_cap = max_pages.unwrap_or(usize::MAX);

    let events_pages_fetched;
    let events_exhausted;
    let events_requests;
    let mut markets_pages_fetched = 0usize;
    let mut markets_exhausted = false;
    let mut markets_requests = 0usize;
    let mut retry_count_429 = 0usize;
    let mut retry_count_5xx = 0usize;

    let mut category_map: HashMap<String, Option<String>> = HashMap::new();
    let mut non_sports_categories: BTreeSet<String> = BTreeSet::new();
    let mut non_sports_category_counts: HashMap<String, usize> = HashMap::new();
    let mut uncategorized_non_sports_events: Vec<String> = Vec::new();
    let mut all_events: Vec<OddsListedEvent> = Vec::new();
    let mut all_markets: Vec<OddsListedMarket> = Vec::new();
    let mut seen_market_tickers: HashSet<String> = HashSet::new();

    if !include_sports && max_pages.is_some() {
        eprintln!("[kalshi] mode=nested-events (non-sports, capped)");
        let nested = run_capped_nested_sync(&client, limiter, page_cap, false).await?;
        events_pages_fetched = nested.pages_fetched;
        events_exhausted = nested.exhausted;
        events_requests = nested.metrics.attempts;
        markets_pages_fetched = nested.pages_fetched;
        markets_exhausted = nested.exhausted;
        markets_requests = nested.metrics.attempts;
        retry_count_429 = retry_count_429.saturating_add(nested.metrics.retry_429);
        retry_count_5xx = retry_count_5xx.saturating_add(nested.metrics.retry_5xx);
        category_map = nested.category_map;
        all_events = nested.events;
        all_markets = nested.markets;
        seen_market_tickers = nested.seen_market_tickers;
    } else {
        let events_fetch = fetch_open_events(&client, limiter, page_cap).await?;
        events_pages_fetched = events_fetch.pages_fetched;
        events_exhausted = events_fetch.exhausted;
        events_requests = events_fetch.metrics.attempts;
        retry_count_429 = retry_count_429.saturating_add(events_fetch.metrics.retry_429);
        retry_count_5xx = retry_count_5xx.saturating_add(events_fetch.metrics.retry_5xx);

        for row in events_fetch.rows {
            let event_ticker = row.event_ticker.clone();
            let category = normalize_event_category(row.category.as_deref())
                .or_else(|| infer_category_from_event_ticker(&event_ticker));
            let is_sports = category.as_deref().map_or(false, is_sports_category);
            category_map.insert(event_ticker.clone(), category.clone());

            if !include_sports {
                if is_sports {
                    continue;
                }
                if let Some(cat) = category.clone() {
                    non_sports_categories.insert(cat.clone());
                    let key = cat;
                    *non_sports_category_counts.entry(key).or_insert(0) += 1;
                } else {
                    uncategorized_non_sports_events.push(event_ticker);
                }
            }

            all_events.push(build_event_record(row, category));
        }
    }

    if !include_sports && max_pages.is_some() {
        // Bounded non-sports mode already populated markets via nested events.
    } else if include_sports {
        eprintln!("[kalshi] mode=global-markets (sports included)");
        let global = run_global_markets_sync(
            &client,
            limiter,
            page_cap,
            include_sports,
            &category_map,
            &mut seen_market_tickers,
            &mut all_markets,
        )
        .await?;
        markets_pages_fetched = markets_pages_fetched.saturating_add(global.pages_fetched);
        markets_exhausted = global.exhausted;
        markets_requests = markets_requests.saturating_add(global.metrics.attempts);
        retry_count_429 = retry_count_429.saturating_add(global.metrics.retry_429);
        retry_count_5xx = retry_count_5xx.saturating_add(global.metrics.retry_5xx);
    } else {
        let probe = probe_category_filter_support(
            &client,
            limiter,
            &non_sports_category_counts,
            &category_map,
        )
        .await?;
        markets_requests = markets_requests.saturating_add(probe.metrics.attempts);
        retry_count_429 = retry_count_429.saturating_add(probe.metrics.retry_429);
        retry_count_5xx = retry_count_5xx.saturating_add(probe.metrics.retry_5xx);

        if probe.supported {
            // Fast path for non-sports sync when category filtering is functioning.
            eprintln!(
                "[kalshi] mode=category-streams (non-sports) categories={} uncategorized_events={}",
                non_sports_categories.len(),
                uncategorized_non_sports_events.len()
            );
            let by_category = run_non_sports_category_sync(
                &client,
                limiter,
                &non_sports_categories,
                &category_map,
                &mut seen_market_tickers,
                &mut all_markets,
            )
            .await?;
            markets_pages_fetched = markets_pages_fetched.saturating_add(by_category.pages_fetched);
            markets_requests = markets_requests.saturating_add(by_category.metrics.attempts);
            retry_count_429 = retry_count_429.saturating_add(by_category.metrics.retry_429);
            retry_count_5xx = retry_count_5xx.saturating_add(by_category.metrics.retry_5xx);

            let uncategorized = run_uncategorized_event_fallback_sync(
                &client,
                limiter,
                &uncategorized_non_sports_events,
                &category_map,
                &mut seen_market_tickers,
                &mut all_markets,
            )
            .await?;
            markets_pages_fetched =
                markets_pages_fetched.saturating_add(uncategorized.pages_fetched);
            markets_requests = markets_requests.saturating_add(uncategorized.metrics.attempts);
            retry_count_429 = retry_count_429.saturating_add(uncategorized.metrics.retry_429);
            retry_count_5xx = retry_count_5xx.saturating_add(uncategorized.metrics.retry_5xx);
            markets_exhausted = by_category.exhausted && uncategorized.exhausted;
        } else {
            eprintln!(
                "[kalshi] category filter probe failed; falling back to global non-sports pagination"
            );
            let global = run_global_markets_sync(
                &client,
                limiter,
                page_cap,
                false,
                &category_map,
                &mut seen_market_tickers,
                &mut all_markets,
            )
            .await?;
            markets_pages_fetched = markets_pages_fetched.saturating_add(global.pages_fetched);
            markets_exhausted = global.exhausted;
            markets_requests = markets_requests.saturating_add(global.metrics.attempts);
            retry_count_429 = retry_count_429.saturating_add(global.metrics.retry_429);
            retry_count_5xx = retry_count_5xx.saturating_add(global.metrics.retry_5xx);
        }
    }

    if include_historical {
        eprintln!("[kalshi] mode=historical-markets");
        let historical = run_historical_markets_sync(
            &client,
            limiter,
            page_cap,
            include_sports,
            &mut category_map,
            &mut seen_market_tickers,
            &mut all_markets,
        )
        .await?;
        markets_pages_fetched = markets_pages_fetched.saturating_add(historical.pages_fetched);
        markets_requests = markets_requests.saturating_add(historical.metrics.attempts);
        retry_count_429 = retry_count_429.saturating_add(historical.metrics.retry_429);
        retry_count_5xx = retry_count_5xx.saturating_add(historical.metrics.retry_5xx);
        markets_exhausted = markets_exhausted && historical.exhausted;
    }

    all_events.sort_by(|a, b| a.ticker.cmp(&b.ticker));
    all_markets.sort_by(|a, b| a.ticker.cmp(&b.ticker));

    let coverage_warning = max_pages.map(|cap| {
        format!(
            "debug frontier sample requested via max_pages={cap}; results are not full-provider coverage"
        )
    });
    let mut strict_fail_reasons = Vec::new();
    if !events_exhausted {
        if let Some(cap) = max_pages {
            strict_fail_reasons.push(format!(
                "events cursor not exhausted within debug frontier sample max_pages={cap}"
            ));
        } else {
            strict_fail_reasons.push("events pagination not exhausted".to_string());
        }
    }
    if !markets_exhausted {
        if let Some(cap) = max_pages {
            strict_fail_reasons.push(format!(
                "markets cursor not exhausted within debug frontier sample max_pages={cap}"
            ));
        } else {
            strict_fail_reasons.push("markets pagination not exhausted".to_string());
        }
    }

    let coverage = OddsSyncCoverage {
        sync_mode: if max_pages.is_some() {
            OddsSyncMode::FrontierSample
        } else {
            OddsSyncMode::Exhaustive
        },
        requested_max_pages: max_pages,
        events_pages_fetched,
        events_exhausted,
        markets_pages_fetched,
        markets_exhausted,
        events_requests,
        markets_requests,
        retry_count_429,
        retry_count_5xx,
        series_backfill_calls: None,
        series_backfill_cap: None,
        series_backfill_truncated: None,
        coverage_warning,
        strict_pass: strict_fail_reasons.is_empty(),
        strict_fail_reasons,
    };

    eprintln!(
        "[kalshi] sync complete: {} markets, {} events",
        all_markets.len(),
        all_events.len()
    );

    Ok((all_events, all_markets, coverage))
}

fn build_event_record(row: EventRow, category: Option<String>) -> OddsListedEvent {
    let title = row
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&row.event_ticker)
        .to_string();

    OddsListedEvent {
        ticker: row.event_ticker,
        title,
        category,
        series_ticker: row.series_ticker,
        source: Some("kalshi".to_string()),
        event_id: None,
        slug: None,
        tags: None,
    }
}

async fn fetch_open_events(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    page_cap: usize,
) -> std::result::Result<EventsFetchResult, String> {
    let mut rows = Vec::new();
    let mut pages_fetched = 0usize;
    let mut exhausted = false;
    let mut metrics = RetryMetrics::default();
    let mut cursor: Option<String> = None;
    let mut seen_cursors: HashSet<String> = HashSet::new();
    let mut page = 0usize;

    while page < page_cap {
        let mut url = reqwest::Url::parse(&format!("{}/events", KALSHI_BASE_URL))
            .map_err(|e| format!("Kalshi events url parse failed: {e}"))?;
        url.query_pairs_mut()
            .append_pair("status", "open")
            .append_pair("limit", &KALSHI_EVENTS_PAGE_LIMIT.to_string());
        if let Some(ref c) = cursor {
            if !c.trim().is_empty() {
                url.query_pairs_mut().append_pair("cursor", c.trim());
            }
        }

        let (body, req_metrics) =
            fetch_json_with_retry::<EventsResp>(client, limiter, url, "Kalshi events").await?;
        metrics.merge(req_metrics);

        let count = body.events.len();
        pages_fetched = pages_fetched.saturating_add(1);
        if page == 0 || (page + 1) % 10 == 0 || count == 0 {
            eprintln!(
                "[kalshi] events page {}: {} rows (total: {})",
                page + 1,
                count,
                rows.len() + count
            );
        }

        if count == 0 {
            exhausted = true;
            break;
        }

        rows.extend(body.events.into_iter());

        let next_cursor = body.cursor.filter(|c| !c.trim().is_empty());
        if let Some(ref c) = next_cursor {
            if !seen_cursors.insert(c.clone()) {
                eprintln!("[kalshi] events stop: cursor cycle detected");
                exhausted = true;
                break;
            }
        }
        cursor = next_cursor;
        if cursor.is_none() {
            exhausted = true;
            break;
        }

        page = page.saturating_add(1);
    }

    Ok(EventsFetchResult {
        rows,
        pages_fetched,
        exhausted,
        metrics,
    })
}

async fn run_capped_nested_sync(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    page_cap: usize,
    include_sports: bool,
) -> std::result::Result<NestedSyncResult, String> {
    let mut cursor: Option<String> = None;
    let mut seen_cursors: HashSet<String> = HashSet::new();
    let mut page = 0usize;
    let mut pages_fetched = 0usize;
    let mut metrics = RetryMetrics::default();
    let mut exhausted = false;
    let mut category_map: HashMap<String, Option<String>> = HashMap::new();
    let mut seen_market_tickers: HashSet<String> = HashSet::new();
    let mut seen_event_tickers: HashSet<String> = HashSet::new();
    let mut all_events: Vec<OddsListedEvent> = Vec::new();
    let mut all_markets: Vec<OddsListedMarket> = Vec::new();

    while page < page_cap {
        let mut url = reqwest::Url::parse(&format!("{}/events", KALSHI_BASE_URL))
            .map_err(|e| format!("Kalshi nested events url parse failed: {e}"))?;
        url.query_pairs_mut()
            .append_pair("status", "open")
            .append_pair("limit", &KALSHI_EVENTS_PAGE_LIMIT.to_string())
            .append_pair("with_nested_markets", "true");
        if let Some(ref c) = cursor {
            if !c.trim().is_empty() {
                url.query_pairs_mut().append_pair("cursor", c.trim());
            }
        }

        let (body, req_metrics) =
            fetch_json_with_retry::<NestedEventsResp>(client, limiter, url, "Kalshi nested events")
                .await?;
        metrics.merge(req_metrics);
        pages_fetched = pages_fetched.saturating_add(1);

        let event_count = body.events.len();
        let mut page_markets: Vec<MarketRow> = Vec::new();
        for event in body.events {
            let category = normalize_event_category(event.category.as_deref())
                .or_else(|| infer_category_from_event_ticker(&event.event_ticker));
            category_map.insert(event.event_ticker.clone(), category.clone());
            if !include_sports && category.as_deref().map_or(false, is_sports_category) {
                continue;
            }
            if seen_event_tickers.insert(event.event_ticker.clone()) {
                all_events.push(build_event_record(
                    EventRow {
                        event_ticker: event.event_ticker.clone(),
                        title: event.title.clone(),
                        category: event.category.clone(),
                        series_ticker: event.series_ticker.clone(),
                    },
                    category.clone(),
                ));
            }
            page_markets.extend(event.markets.into_iter().map(|market| MarketRow {
                ticker: market.ticker,
                title: market.title,
                event_ticker: market.event_ticker.unwrap_or(event.event_ticker.clone()),
                status: market.status,
                volume: market.volume,
                volume_24h: market.volume_24h,
                yes_bid_dollars: market.yes_bid_dollars,
                yes_ask_dollars: market.yes_ask_dollars,
                last_price_dollars: market.last_price_dollars,
                yes_bid: market.yes_bid,
                yes_ask: market.yes_ask,
                last_price: market.last_price,
            }));
        }
        let fetched_market_rows = page_markets.len();
        let kept_this_page = append_market_rows(
            page_markets,
            &category_map,
            include_sports,
            true,
            &mut seen_market_tickers,
            &mut all_markets,
        );
        let dropped_this_page = fetched_market_rows.saturating_sub(kept_this_page);
        eprintln!(
            "[kalshi] nested events page {}: events={} kept_events_total={} fetched_markets={} kept_total={} kept_this_page={} dropped_this_page={}",
            page + 1,
            event_count,
            all_events.len(),
            fetched_market_rows,
            all_markets.len(),
            kept_this_page,
            dropped_this_page
        );

        if event_count == 0 {
            exhausted = true;
            break;
        }

        let next_cursor = body.cursor.filter(|c| !c.trim().is_empty());
        if let Some(ref c) = next_cursor {
            if !seen_cursors.insert(c.clone()) {
                eprintln!("[kalshi] nested events stop: cursor cycle detected");
                exhausted = true;
                break;
            }
        }
        cursor = next_cursor;
        if cursor.is_none() {
            exhausted = true;
            break;
        }

        page = page.saturating_add(1);
    }

    Ok(NestedSyncResult {
        events: all_events,
        markets: all_markets,
        category_map,
        seen_market_tickers,
        pages_fetched,
        exhausted,
        metrics,
    })
}

async fn run_global_markets_sync(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    page_cap: usize,
    include_sports: bool,
    category_map: &HashMap<String, Option<String>>,
    seen_market_tickers: &mut HashSet<String>,
    all_markets: &mut Vec<OddsListedMarket>,
) -> std::result::Result<MarketsFetchResult, String> {
    let mut market_cursor: Option<String> = None;
    let mut seen_market_cursors: HashSet<String> = HashSet::new();
    let mut page = 0usize;
    let mut pages_fetched = 0usize;
    let mut fetched_market_rows = 0usize;
    let mut metrics = RetryMetrics::default();
    let mut exhausted = false;

    while page < page_cap {
        let mut url = reqwest::Url::parse(&format!("{}/markets", KALSHI_BASE_URL))
            .map_err(|e| format!("Kalshi markets url parse failed: {e}"))?;
        url.query_pairs_mut()
            .append_pair("status", "open")
            .append_pair("limit", &KALSHI_MARKETS_PAGE_LIMIT.to_string());
        if !include_sports {
            // Default non-sports mode excludes multivariate combo markets, which are
            // overwhelmingly sports-heavy and dominate sync runtime/noise.
            url.query_pairs_mut().append_pair("mve_filter", "exclude");
        }
        if let Some(ref c) = market_cursor {
            if !c.trim().is_empty() {
                url.query_pairs_mut().append_pair("cursor", c.trim());
            }
        }

        let (body, req_metrics) =
            fetch_json_with_retry::<MarketsResp>(client, limiter, url, "Kalshi markets").await?;
        metrics.merge(req_metrics);
        pages_fetched = pages_fetched.saturating_add(1);

        let count = body.markets.len();
        fetched_market_rows = fetched_market_rows.saturating_add(count);
        let kept_this_page = append_market_rows(
            body.markets,
            category_map,
            include_sports,
            true,
            seen_market_tickers,
            all_markets,
        );
        let dropped_this_page = count.saturating_sub(kept_this_page);
        eprintln!(
            "[kalshi] markets page {}: fetched={} fetched_total={} kept_total={} kept_this_page={} dropped_this_page={}",
            page + 1,
            count,
            fetched_market_rows,
            all_markets.len(),
            kept_this_page,
            dropped_this_page
        );

        if count == 0 {
            exhausted = true;
            break;
        }

        let next_cursor = body.cursor.filter(|c| !c.trim().is_empty());
        if let Some(ref c) = next_cursor {
            if !seen_market_cursors.insert(c.clone()) {
                eprintln!("[kalshi] markets stop: cursor cycle detected");
                exhausted = true;
                break;
            }
        }
        market_cursor = next_cursor;
        if market_cursor.is_none() {
            exhausted = true;
            break;
        }

        page = page.saturating_add(1);
    }

    Ok(MarketsFetchResult {
        pages_fetched,
        exhausted,
        metrics,
    })
}

async fn run_historical_markets_sync(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    page_cap: usize,
    include_sports: bool,
    category_map: &HashMap<String, Option<String>>,
    seen_market_tickers: &mut HashSet<String>,
    all_markets: &mut Vec<OddsListedMarket>,
) -> std::result::Result<MarketsFetchResult, String> {
    let mut metrics = RetryMetrics::default();
    if let Ok((cutoff, cutoff_metrics)) = fetch_json_with_retry::<HistoricalCutoffResp>(
        client,
        limiter,
        reqwest::Url::parse(&format!("{}/historical/cutoff", KALSHI_BASE_URL))
            .map_err(|e| format!("Kalshi historical cutoff url parse failed: {e}"))?,
        "Kalshi historical cutoff",
    )
    .await
    {
        metrics.merge(cutoff_metrics);
        if let Some(ts) = cutoff.market_settled_ts {
            eprintln!("[kalshi] historical cutoff market_settled_ts={ts}");
        }
    }

    let mut cursor: Option<String> = None;
    let mut seen_cursors: HashSet<String> = HashSet::new();
    let mut page = 0usize;
    let mut pages_fetched = 0usize;
    let mut fetched_market_rows = 0usize;
    let mut exhausted = false;

    while page < page_cap {
        let mut url = reqwest::Url::parse(&format!("{}/historical/markets", KALSHI_BASE_URL))
            .map_err(|e| format!("Kalshi historical markets url parse failed: {e}"))?;
        url.query_pairs_mut()
            .append_pair("status", "settled")
            .append_pair("limit", &KALSHI_MARKETS_PAGE_LIMIT.to_string());
        if !include_sports {
            url.query_pairs_mut().append_pair("mve_filter", "exclude");
        }
        if let Some(ref c) = cursor {
            if !c.trim().is_empty() {
                url.query_pairs_mut().append_pair("cursor", c.trim());
            }
        }

        let (body, req_metrics) =
            fetch_json_with_retry::<MarketsResp>(client, limiter, url, "Kalshi historical markets")
                .await?;
        metrics.merge(req_metrics);
        pages_fetched = pages_fetched.saturating_add(1);

        let count = body.markets.len();
        fetched_market_rows = fetched_market_rows.saturating_add(count);
        let kept_this_page = append_market_rows(
            body.markets,
            category_map,
            include_sports,
            true,
            seen_market_tickers,
            all_markets,
        );
        let dropped_this_page = count.saturating_sub(kept_this_page);
        eprintln!(
            "[kalshi] historical markets page {}: fetched={} fetched_total={} kept_total={} kept_this_page={} dropped_this_page={}",
            page + 1,
            count,
            fetched_market_rows,
            all_markets.len(),
            kept_this_page,
            dropped_this_page
        );

        if count == 0 {
            exhausted = true;
            break;
        }

        let next_cursor = body.cursor.filter(|c| !c.trim().is_empty());
        if let Some(ref c) = next_cursor {
            if !seen_cursors.insert(c.clone()) {
                eprintln!("[kalshi] historical markets stop: cursor cycle detected");
                exhausted = true;
                break;
            }
        }
        cursor = next_cursor;
        if cursor.is_none() {
            exhausted = true;
            break;
        }

        page = page.saturating_add(1);
    }

    Ok(MarketsFetchResult {
        pages_fetched,
        exhausted,
        metrics,
    })
}

async fn probe_category_filter_support(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    category_counts: &HashMap<String, usize>,
    category_map: &HashMap<String, Option<String>>,
) -> std::result::Result<CategoryFilterProbeResult, String> {
    let Some((sample_category, _)) = category_counts.iter().max_by_key(|(_, count)| *count) else {
        return Ok(CategoryFilterProbeResult {
            supported: false,
            metrics: RetryMetrics::default(),
        });
    };

    let mut url = reqwest::Url::parse(&format!("{}/markets", KALSHI_BASE_URL))
        .map_err(|e| format!("Kalshi category probe url parse failed: {e}"))?;
    url.query_pairs_mut()
        .append_pair("status", "open")
        .append_pair("limit", "60")
        .append_pair("mve_filter", "exclude")
        .append_pair("category", sample_category);

    let (body, metrics) =
        fetch_json_with_retry::<MarketsResp>(client, limiter, url, "Kalshi category probe").await?;
    if body.markets.is_empty() {
        return Ok(CategoryFilterProbeResult {
            supported: false,
            metrics,
        });
    }

    let mut mapped = 0usize;
    let mut matched = 0usize;
    for row in body.markets.into_iter().take(60) {
        let category = category_map
            .get(&row.event_ticker)
            .cloned()
            .flatten()
            .or_else(|| infer_category_from_event_ticker(&row.event_ticker));
        let Some(cat) = category else {
            continue;
        };
        mapped = mapped.saturating_add(1);
        if cat.eq_ignore_ascii_case(sample_category) {
            matched = matched.saturating_add(1);
        }
    }

    // Require at least modestly reliable targeting before enabling category fanout.
    let supported = mapped >= 8 && ((matched as f64 / mapped as f64) >= 0.8);
    if !supported {
        eprintln!(
            "[kalshi] category probe sample={} mapped={} matched={} (unsupported)",
            sample_category, mapped, matched
        );
    }

    Ok(CategoryFilterProbeResult { supported, metrics })
}

async fn run_non_sports_category_sync(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    categories: &BTreeSet<String>,
    category_map: &HashMap<String, Option<String>>,
    seen_market_tickers: &mut HashSet<String>,
    all_markets: &mut Vec<OddsListedMarket>,
) -> std::result::Result<MarketsFetchResult, String> {
    if categories.is_empty() {
        return Ok(MarketsFetchResult {
            pages_fetched: 0,
            exhausted: true,
            metrics: RetryMetrics::default(),
        });
    }

    let mut stream_results = futures::stream::iter(categories.iter().cloned())
        .map(|category| {
            let client = client.clone();
            async move {
                fetch_markets_stream_with_filters(
                    &client,
                    limiter,
                    usize::MAX,
                    format!("category={category}"),
                    vec![("category".to_string(), category)],
                )
                .await
            }
        })
        .buffer_unordered(KALSHI_NON_SPORTS_CATEGORY_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    let mut aggregate = MarketsFetchResult {
        pages_fetched: 0,
        exhausted: true,
        metrics: RetryMetrics::default(),
    };

    for result in stream_results.drain(..) {
        let stream = result?;
        aggregate.pages_fetched = aggregate.pages_fetched.saturating_add(stream.pages_fetched);
        aggregate.exhausted &= stream.exhausted;
        aggregate.metrics.merge(stream.metrics);

        let fetched = stream.rows.len();
        let kept = append_market_rows(
            stream.rows,
            category_map,
            false,
            true,
            seen_market_tickers,
            all_markets,
        );
        eprintln!(
            "[kalshi] markets stream {}: fetched={} kept={} kept_total={}",
            stream.stream_key,
            fetched,
            kept,
            all_markets.len()
        );
    }

    Ok(aggregate)
}

async fn run_uncategorized_event_fallback_sync(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    event_tickers: &[String],
    category_map: &HashMap<String, Option<String>>,
    seen_market_tickers: &mut HashSet<String>,
    all_markets: &mut Vec<OddsListedMarket>,
) -> std::result::Result<MarketsFetchResult, String> {
    let mut seen_events: HashSet<String> = HashSet::new();
    let unique_events: Vec<String> = event_tickers
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| seen_events.insert(s.clone()))
        .collect();

    if unique_events.is_empty() {
        return Ok(MarketsFetchResult {
            pages_fetched: 0,
            exhausted: true,
            metrics: RetryMetrics::default(),
        });
    }

    let mut stream_results = futures::stream::iter(unique_events.into_iter())
        .map(|event_ticker| {
            let client = client.clone();
            async move {
                fetch_markets_stream_with_filters(
                    &client,
                    limiter,
                    usize::MAX,
                    format!("event={event_ticker}"),
                    vec![("event_ticker".to_string(), event_ticker)],
                )
                .await
            }
        })
        .buffer_unordered(KALSHI_NON_SPORTS_UNCATEGORIZED_EVENT_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    let mut aggregate = MarketsFetchResult {
        pages_fetched: 0,
        exhausted: true,
        metrics: RetryMetrics::default(),
    };

    for result in stream_results.drain(..) {
        let stream = result?;
        aggregate.pages_fetched = aggregate.pages_fetched.saturating_add(stream.pages_fetched);
        aggregate.exhausted &= stream.exhausted;
        aggregate.metrics.merge(stream.metrics);

        let fetched = stream.rows.len();
        let kept = append_market_rows(
            stream.rows,
            category_map,
            false,
            true,
            seen_market_tickers,
            all_markets,
        );
        eprintln!(
            "[kalshi] markets fallback {}: fetched={} kept={} kept_total={}",
            stream.stream_key,
            fetched,
            kept,
            all_markets.len()
        );
    }

    Ok(aggregate)
}

async fn fetch_markets_stream_with_filters(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    page_cap: usize,
    stream_key: String,
    extra_filters: Vec<(String, String)>,
) -> std::result::Result<StreamRowsFetchResult, String> {
    let request_label = format!("Kalshi markets [{stream_key}]");
    let mut rows: Vec<MarketRow> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors: HashSet<String> = HashSet::new();
    let mut page = 0usize;
    let mut pages_fetched = 0usize;
    let mut metrics = RetryMetrics::default();
    let mut exhausted = false;

    while page < page_cap {
        let mut url = reqwest::Url::parse(&format!("{}/markets", KALSHI_BASE_URL))
            .map_err(|e| format!("{request_label} url parse failed: {e}"))?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("status", "open")
                .append_pair("limit", &KALSHI_MARKETS_PAGE_LIMIT.to_string())
                .append_pair("mve_filter", "exclude");
            for (k, v) in &extra_filters {
                if !v.trim().is_empty() {
                    query.append_pair(k, v.trim());
                }
            }
            if let Some(ref c) = cursor {
                if !c.trim().is_empty() {
                    query.append_pair("cursor", c.trim());
                }
            }
        }

        let (body, req_metrics) =
            fetch_json_with_retry::<MarketsResp>(client, limiter, url, &request_label).await?;
        metrics.merge(req_metrics);
        pages_fetched = pages_fetched.saturating_add(1);

        let count = body.markets.len();
        if count == 0 {
            exhausted = true;
            break;
        }
        rows.extend(body.markets.into_iter());

        let next_cursor = body.cursor.filter(|c| !c.trim().is_empty());
        if let Some(ref c) = next_cursor {
            if !seen_cursors.insert(c.clone()) {
                eprintln!("[kalshi] markets stream {stream_key} stop: cursor cycle detected");
                exhausted = true;
                break;
            }
        }
        cursor = next_cursor;
        if cursor.is_none() {
            exhausted = true;
            break;
        }

        page = page.saturating_add(1);
    }

    Ok(StreamRowsFetchResult {
        stream_key,
        rows,
        pages_fetched,
        exhausted,
        metrics,
    })
}

fn append_market_rows(
    rows: Vec<MarketRow>,
    category_map: &HashMap<String, Option<String>>,
    include_sports: bool,
    allow_unknown_non_sports: bool,
    seen_market_tickers: &mut HashSet<String>,
    all_markets: &mut Vec<OddsListedMarket>,
) -> usize {
    let mut kept = 0usize;
    for row in rows {
        if !seen_market_tickers.insert(row.ticker.clone()) {
            continue;
        }

        let category = category_map
            .get(&row.event_ticker)
            .cloned()
            .flatten()
            .or_else(|| infer_category_from_event_ticker(&row.event_ticker));
        if !include_sports && !allow_unknown_non_sports && category.is_none() {
            continue;
        }
        if !include_sports && category.as_deref().map_or(false, is_sports_category) {
            continue;
        }

        let probability_yes = probability_from_kalshi_fields(
            row.last_price_dollars.as_deref(),
            row.last_price,
            row.yes_bid_dollars.as_deref(),
            row.yes_ask_dollars.as_deref(),
            row.yes_bid,
            row.yes_ask,
        );
        let yes_price = probability_yes.map(probability_to_cents);
        let title = row
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&row.ticker)
            .to_string();

        all_markets.push(OddsListedMarket {
            ticker: row.ticker,
            title,
            event_ticker: row.event_ticker,
            freshness: Freshness::new(
                Utc::now(),
                Utc::now(),
                FreshnessState::Unknown,
                FreshnessOrigin::TransportReceived,
                FreshnessQuality::Estimated,
            ),
            yes_price,
            volume: row.volume.or(row.volume_24h),
            status: normalize_kalshi_status(row.status.as_deref()),
            source: Some("kalshi".to_string()),
            market_id: None,
            event_id: None,
            slug: None,
            outcomes: None,
            outcome_prices: None,
            clob_token_ids: None,
            probability_yes,
            category,
        });
        kept = kept.saturating_add(1);
    }
    kept
}

fn normalize_event_category(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

async fn fetch_json_with_retry<T: DeserializeOwned>(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    url: reqwest::Url,
    label: &str,
) -> std::result::Result<(T, RetryMetrics), String> {
    let mut attempt = 0usize;
    let mut metrics = RetryMetrics::default();

    loop {
        limiter.wait().await;
        attempt = attempt.saturating_add(1);
        metrics.attempts = metrics.attempts.saturating_add(1);

        let resp = match client.get(url.clone()).send().await {
            Ok(resp) => resp,
            Err(err) => {
                if attempt < KALSHI_MAX_RETRIES {
                    // Network transport errors are usually transient on long full-breadth runs.
                    metrics.retry_5xx = metrics.retry_5xx.saturating_add(1);
                    limiter.on_rate_limited();
                    sleep(backoff_for_attempt(attempt)).await;
                    continue;
                }
                return Err(format!("{label} fetch failed: {err}"));
            }
        };

        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < KALSHI_MAX_RETRIES {
            metrics.retry_429 = metrics.retry_429.saturating_add(1);
            limiter.on_rate_limited();
            sleep(retry_delay(status, resp.headers(), attempt)).await;
            continue;
        }
        if status.is_server_error() && attempt < KALSHI_MAX_RETRIES {
            metrics.retry_5xx = metrics.retry_5xx.saturating_add(1);
            limiter.on_rate_limited();
            sleep(retry_delay(status, resp.headers(), attempt)).await;
            continue;
        }
        if !status.is_success() {
            return Err(format!("{label} fetch failed: http {status}"));
        }

        let body = match resp.json::<T>().await {
            Ok(body) => body,
            Err(err) => {
                if attempt < KALSHI_MAX_RETRIES {
                    metrics.retry_5xx = metrics.retry_5xx.saturating_add(1);
                    limiter.on_rate_limited();
                    sleep(backoff_for_attempt(attempt)).await;
                    continue;
                }
                return Err(format!("{label} parse failed: {err}"));
            }
        };
        limiter.on_success();
        return Ok((body, metrics));
    }
}

fn retry_delay(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    attempt: usize,
) -> TokioDuration {
    let base = backoff_for_attempt(attempt);
    if status != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return base;
    }
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| TokioDuration::from_secs(secs.min(60)));
    retry_after.map_or(base, |d| d.max(base))
}

fn parse_prob_from_dollars(raw: Option<&str>) -> Option<f64> {
    raw.and_then(|s| s.trim().parse::<f64>().ok())
        .map(|v| v.clamp(0.0, 1.0))
}

fn parse_prob_from_cents(raw: Option<i64>) -> Option<f64> {
    raw.map(|v| (v as f64 / 100.0).clamp(0.0, 1.0))
}

fn midpoint(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(((x + y) / 2.0).clamp(0.0, 1.0)),
        (Some(x), None) | (None, Some(x)) => Some(x.clamp(0.0, 1.0)),
        _ => None,
    }
}

fn probability_from_kalshi_fields(
    last_price_dollars: Option<&str>,
    last_price: Option<i64>,
    yes_bid_dollars: Option<&str>,
    yes_ask_dollars: Option<&str>,
    yes_bid: Option<i64>,
    yes_ask: Option<i64>,
) -> Option<f64> {
    parse_prob_from_dollars(last_price_dollars)
        .or_else(|| parse_prob_from_cents(last_price))
        .or_else(|| {
            midpoint(
                parse_prob_from_dollars(yes_bid_dollars),
                parse_prob_from_dollars(yes_ask_dollars),
            )
        })
        .or_else(|| {
            midpoint(
                parse_prob_from_cents(yes_bid),
                parse_prob_from_cents(yes_ask),
            )
        })
}

fn probability_to_cents(probability_yes: f64) -> i64 {
    (probability_yes * 100.0).round() as i64
}

fn normalize_kalshi_status(status: Option<&str>) -> Option<String> {
    let normalized = status
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())?;

    if normalized.contains("open") || normalized.contains("active") {
        return Some("open".to_string());
    }
    if normalized.contains("close")
        || normalized.contains("settled")
        || normalized.contains("final")
    {
        return Some("closed".to_string());
    }
    Some(normalized)
}

fn infer_category_from_event_ticker(event_ticker: &str) -> Option<String> {
    let upper = event_ticker.to_ascii_uppercase();
    let looks_sports = upper.contains("SPORT")
        || upper.starts_with("KXNBA")
        || upper.starts_with("KXNFL")
        || upper.starts_with("KXMLB")
        || upper.starts_with("KXNHL")
        || upper.starts_with("KXNCAA")
        || upper.starts_with("KXSOCCER")
        || upper.starts_with("KXEPL")
        || upper.starts_with("KXMLS")
        || upper.starts_with("KXATP")
        || upper.starts_with("KXWTA")
        || upper.starts_with("KXGOLF")
        || upper.starts_with("KXF1")
        || upper.starts_with("KXNASCAR")
        || upper.starts_with("KXMMA")
        || upper.starts_with("KXUFC")
        || upper.starts_with("KXCRICKET")
        || upper.starts_with("KXNCAAB")
        || upper.starts_with("KXNCAAF")
        || upper.starts_with("KXESPORT");
    if looks_sports {
        Some("Sports".to_string())
    } else {
        None
    }
}

fn backoff_for_attempt(attempt: usize) -> TokioDuration {
    let seconds = 2u64.saturating_mul(1u64 << ((attempt.saturating_sub(1)) as u32));
    TokioDuration::from_secs(seconds.min(30))
}
